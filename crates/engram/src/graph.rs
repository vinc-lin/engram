//! graph.rs — Pure edge extraction for the engram code-graph.
//!
//! `extract_edges` is the single public entry point. It parses `content` via the tree-sitter
//! grammar already loaded by `treesit::parse`, walks the AST for Rust and C/C++, and returns
//! a `Vec<RawEdge>` with confidence scores assigned by in-file resolution:
//!
//!   1.0 — dst symbol is in `file_syms` (defined in this file)
//!   0.6 — dst symbol is named in a `use`/`#include` import of this file
//!   0.3 — bare unresolved name (exists in neither set)
//!
//! The caller (`ingest::ingest_document`) is responsible for applying `min_confidence` filtering
//! before passing edges to `commit_ingest`.
//!
//! # Known misses (by design — structural extraction only)
//!
//! ## Rust
//! - **Macro-generated calls**: `vec![]`, `println!`, proc-macro invocations expand at compile
//!   time; tree-sitter sees only the macro call expression, not the generated call sites inside.
//! - **Virtual / trait-object dispatch**: `dyn Trait` method calls resolve to the trait method
//!   name, not the concrete impl. The edge `sym:method_name` is recorded but points at the trait,
//!   not the concrete receiver.
//! - **Method calls on variables**: `x.foo()` emits `sym:foo` without the receiver type; if
//!   `foo` is common (e.g. `to_string`, `clone`) this creates noisy low-confidence edges.
//!   The IDF-gated `code_def_boost` in `retrieve.rs` mitigates this for search.
//! - **Calls inside `macro_definition` body**: tree-sitter's Rust grammar puts the body of a
//!   `macro_definition` in a `token_tree` node, not individual `call_expression` nodes.
//!
//! ## C/C++
//! - **Overloaded functions**: multiple definitions share one symbol name; edges are name-correct
//!   but resolve ambiguously across overloads.
//! - **Virtual method dispatch**: calls via pointer/reference to a base class emit the
//!   declared method name, not the vtable target.
//! - **Function-pointer calls**: `(*fp)(args)` — the callee is a dereferenced expression, not
//!   an identifier; these are skipped.
//! - **Template specialisations**: the template name is extracted; instantiated specialisation
//!   names (e.g. `vector<int>::push_back`) are not separately recorded.
//! - **Preprocessor macro expansion**: call sites inside macro bodies are not visited by
//!   tree-sitter at the unexpanded source level.
//!
//! ## Both
//! - Cross-file confidence (0.3) is a heuristic — it can be spuriously high for common names
//!   that appear in many files, and spuriously low for rare names imported via a wildcard.
//! - `dst_doc_id` resolution is deferred to query time via `chunk_entities`; see `graph_query.rs`.

use crate::model::{EdgeKind, RawEdge};
use crate::treesit::{declarator_name, parse, Lang};

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

/// Extract raw call/type/import edges from `content` for the given language.
///
/// `file_syms` is the slice of **bare symbol names** (without the `sym:` prefix) that are
/// *defined* in this file. Edges whose `dst_sym` (bare name) appears in `file_syms` get
/// confidence 1.0; edges whose name appears as an import target in the file get 0.6; everything
/// else gets 0.3. Returns an empty `Vec` for any unsupported language or parse failure.
pub fn extract_edges(content: &str, lang: Lang, file_syms: &[&str]) -> Vec<RawEdge> {
    match lang {
        Lang::Rust => extract_rust(content, file_syms),
        Lang::C | Lang::Cpp => extract_c_cpp(content, lang, file_syms),
        // v1 scope: Rust + C/C++ only.
        // Follow-up: Python, TypeScript/JavaScript, Go, Java/Kotlin.
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Confidence helper
// ---------------------------------------------------------------------------

fn confidence(name: &str, file_syms: &[&str], import_names: &[String]) -> f32 {
    if file_syms.contains(&name) {
        1.0
    } else if import_names.iter().any(|i| i == name) {
        0.6
    } else {
        0.3
    }
}

// ---------------------------------------------------------------------------
// Rust extraction
// ---------------------------------------------------------------------------

fn extract_rust(content: &str, file_syms: &[&str]) -> Vec<RawEdge> {
    let tree = match parse(Lang::Rust, content) {
        Some(t) => t,
        None => return vec![],
    };
    let src = content.as_bytes();
    let mut edges: Vec<RawEdge> = Vec::new();

    let import_names = rust_collect_imports(&tree, src);

    for name in &import_names {
        if name.is_empty() || name == "self" || name == "super" || name == "crate" {
            continue;
        }
        edges.push(RawEdge {
            dst_sym: format!("sym:{name}"),
            edge_kind: EdgeKind::Imports,
            src_line: None,
            confidence: confidence(name, file_syms, &import_names),
        });
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "call_expression" => {
                if let Some(callee_name) =
                    rust_callee_name(node.child_by_field_name("function"), src)
                {
                    let line = node.start_position().row as i64 + 1;
                    edges.push(RawEdge {
                        dst_sym: format!("sym:{callee_name}"),
                        edge_kind: EdgeKind::Calls,
                        src_line: Some(line),
                        confidence: confidence(&callee_name, file_syms, &import_names),
                    });
                }
            }
            "type_identifier" => {
                if let Ok(name) = node.utf8_text(src) {
                    if !name.is_empty() {
                        let line = node.start_position().row as i64 + 1;
                        edges.push(RawEdge {
                            dst_sym: format!("sym:{name}"),
                            edge_kind: EdgeKind::UsesType,
                            src_line: Some(line),
                            confidence: confidence(name, file_syms, &import_names),
                        });
                    }
                }
            }
            _ => {}
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }

    dedup_edges(edges)
}

fn rust_collect_imports(tree: &tree_sitter::Tree, src: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" {
            if let Some(arg) = node.child_by_field_name("argument") {
                rust_collect_use_tree(arg, src, &mut names);
            }
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn rust_collect_use_tree(node: tree_sitter::Node, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "scoped_identifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                if let Ok(t) = name_node.utf8_text(src) {
                    out.push(t.to_string());
                }
            }
        }
        "identifier" => {
            if let Ok(t) = node.utf8_text(src) {
                out.push(t.to_string());
            }
        }
        "use_list" => {
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                rust_collect_use_tree(child, src, out);
            }
        }
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                rust_collect_use_tree(list, src, out);
            }
        }
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                if let Ok(t) = alias.utf8_text(src) {
                    out.push(t.to_string());
                }
            }
        }
        _ => {}
    }
}

fn rust_callee_name(node: Option<tree_sitter::Node>, src: &[u8]) -> Option<String> {
    let node = node?;
    match node.kind() {
        "identifier" => node
            .utf8_text(src)
            .ok()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(src).ok())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        "generic_function" => rust_callee_name(node.child_by_field_name("function"), src),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// C / C++ extraction
// ---------------------------------------------------------------------------

fn extract_c_cpp(content: &str, lang: Lang, file_syms: &[&str]) -> Vec<RawEdge> {
    let tree = match parse(lang, content) {
        Some(t) => t,
        None => return vec![],
    };
    let src = content.as_bytes();
    let mut edges: Vec<RawEdge> = Vec::new();

    let import_names = cpp_collect_includes(&tree, src);

    for name in &import_names {
        if name.is_empty() {
            continue;
        }
        edges.push(RawEdge {
            dst_sym: format!("sym:{name}"),
            edge_kind: EdgeKind::Imports,
            src_line: None,
            confidence: confidence(name, file_syms, &import_names),
        });
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "call_expression" => {
                if let Some(callee_name) =
                    cpp_callee_name(node.child_by_field_name("function"), src)
                {
                    let line = node.start_position().row as i64 + 1;
                    edges.push(RawEdge {
                        dst_sym: format!("sym:{callee_name}"),
                        edge_kind: EdgeKind::Calls,
                        src_line: Some(line),
                        confidence: confidence(&callee_name, file_syms, &import_names),
                    });
                }
            }
            "type_identifier" => {
                if let Ok(name) = node.utf8_text(src) {
                    if !name.is_empty() {
                        let line = node.start_position().row as i64 + 1;
                        edges.push(RawEdge {
                            dst_sym: format!("sym:{name}"),
                            edge_kind: EdgeKind::UsesType,
                            src_line: Some(line),
                            confidence: confidence(name, file_syms, &import_names),
                        });
                    }
                }
            }
            _ => {}
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }

    dedup_edges(edges)
}

fn cpp_collect_includes(tree: &tree_sitter::Tree, src: &[u8]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "preproc_include" {
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                let raw = match child.utf8_text(src) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let stripped = raw
                    .trim_matches(|c| c == '<' || c == '>' || c == '"')
                    .trim();
                let filename = stripped.rsplit('/').next().unwrap_or(stripped);
                let stem = filename
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(filename);
                if !stem.is_empty() {
                    names.push(stem.to_string());
                }
            }
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn cpp_callee_name(node: Option<tree_sitter::Node>, src: &[u8]) -> Option<String> {
    let node = node?;
    match node.kind() {
        "identifier" => node
            .utf8_text(src)
            .ok()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        "qualified_identifier" => declarator_name(node, src),
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(src).ok())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        "template_function" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared post-processing
// ---------------------------------------------------------------------------

/// Deduplicate edges: keep the highest-confidence occurrence of each (dst_sym, edge_kind) pair.
fn dedup_edges(mut edges: Vec<RawEdge>) -> Vec<RawEdge> {
    edges.sort_by(|a, b| {
        a.dst_sym
            .cmp(&b.dst_sym)
            .then(a.edge_kind.as_str().cmp(b.edge_kind.as_str()))
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    edges.dedup_by(|later, first| {
        later.dst_sym == first.dst_sym && later.edge_kind.as_str() == first.edge_kind.as_str()
    });
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treesit::Lang;

    #[test]
    fn rust_same_file_call_confidence_1() {
        let src = r#"
fn helper() {}
fn caller() {
    helper();
}
"#;
        let edges = extract_edges(src, Lang::Rust, &["helper", "caller"]);
        let calls: Vec<&RawEdge> = edges
            .iter()
            .filter(|e| e.dst_sym == "sym:helper" && matches!(e.edge_kind, EdgeKind::Calls))
            .collect();
        assert!(
            !calls.is_empty(),
            "expected CALLS edge to sym:helper; got {edges:?}"
        );
        assert!(
            (calls[0].confidence - 1.0).abs() < 1e-6,
            "expected confidence 1.0, got {}",
            calls[0].confidence
        );
    }

    #[test]
    fn rust_imported_call_confidence_0_6() {
        let src = r#"
use crate::store::Store;
fn caller(s: Store) {
    s.open("x");
}
"#;
        let edges = extract_edges(src, Lang::Rust, &["caller"]);
        let uses: Vec<&RawEdge> = edges
            .iter()
            .filter(|e| e.dst_sym == "sym:Store" && matches!(e.edge_kind, EdgeKind::UsesType))
            .collect();
        assert!(
            !uses.is_empty(),
            "expected USES_TYPE edge to sym:Store; got {edges:?}"
        );
        assert!(
            uses[0].confidence >= 0.59,
            "expected confidence ~0.6, got {}",
            uses[0].confidence
        );
    }

    #[test]
    fn rust_use_decl_imports_edge() {
        let src = "use crate::store::Store;\nuse std::collections::HashMap;\n";
        let edges = extract_edges(src, Lang::Rust, &[]);
        let imp_store = edges
            .iter()
            .any(|e| e.dst_sym == "sym:Store" && matches!(e.edge_kind, EdgeKind::Imports));
        let imp_map = edges
            .iter()
            .any(|e| e.dst_sym == "sym:HashMap" && matches!(e.edge_kind, EdgeKind::Imports));
        assert!(imp_store, "expected IMPORTS sym:Store; got {edges:?}");
        assert!(imp_map, "expected IMPORTS sym:HashMap; got {edges:?}");
    }

    #[test]
    fn cpp_call_and_uses_type() {
        let src = r#"
#include "Stitcher.h"
struct Pixel { int r; };
int blend(Pixel p) {
    return process(p.r);
}
"#;
        let edges = extract_edges(src, Lang::Cpp, &["blend", "Pixel"]);
        let calls_process = edges
            .iter()
            .any(|e| e.dst_sym == "sym:process" && matches!(e.edge_kind, EdgeKind::Calls));
        let uses_pixel = edges
            .iter()
            .any(|e| e.dst_sym == "sym:Pixel" && matches!(e.edge_kind, EdgeKind::UsesType));
        assert!(calls_process, "expected CALLS sym:process; got {edges:?}");
        assert!(uses_pixel, "expected USES_TYPE sym:Pixel; got {edges:?}");
        let pixel_edge = edges
            .iter()
            .find(|e| e.dst_sym == "sym:Pixel" && matches!(e.edge_kind, EdgeKind::UsesType))
            .unwrap();
        assert!(
            (pixel_edge.confidence - 1.0).abs() < 1e-6,
            "Pixel defined in file → confidence 1.0, got {}",
            pixel_edge.confidence
        );
    }

    #[test]
    fn cpp_include_imports_edge() {
        let src = r#"
#include <stdio.h>
#include "MyLib.hpp"
void run() {}
"#;
        let edges = extract_edges(src, Lang::Cpp, &["run"]);
        let has_stdio = edges
            .iter()
            .any(|e| e.dst_sym == "sym:stdio" && matches!(e.edge_kind, EdgeKind::Imports));
        let has_mylib = edges
            .iter()
            .any(|e| e.dst_sym == "sym:MyLib" && matches!(e.edge_kind, EdgeKind::Imports));
        assert!(has_stdio, "expected IMPORTS sym:stdio; got {edges:?}");
        assert!(has_mylib, "expected IMPORTS sym:MyLib; got {edges:?}");
    }

    #[test]
    fn unsupported_lang_returns_empty() {
        let edges = extract_edges("def foo(): pass", Lang::Python, &[]);
        assert!(edges.is_empty(), "expected empty for Python; got {edges:?}");
    }

    #[test]
    fn parse_failure_graceful() {
        let edges = extract_edges("", Lang::Rust, &[]);
        assert!(edges.is_empty());
    }
}
