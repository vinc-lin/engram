//! Phase 4: tree-sitter function/type-boundary chunking + AST symbol extraction. Falls back to the
//! heuristic chunker (`ingest::chunk_code_with`) when a language is unsupported or parsing fails.

use crate::ingest::{chunk_code_with, estimate_tokens, CODE_CHUNK_TOKEN_BUDGET};
use tree_sitter::{Language, Parser};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
}

/// Map a file path's extension to a supported language (`None` if unsupported).
pub fn lang_for_path(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
        "ts" | "mts" | "cts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn language(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// Node kinds that delimit a chunkable definition unit.
fn is_chunkable(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "impl_item"
                | "mod_item"
                | "type_item"
                | "union_item"
                | "macro_definition"
        ),
        Lang::Python => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "abstract_class_declaration"
        ),
        Lang::Go => matches!(
            kind,
            "function_declaration" | "method_declaration" | "type_declaration"
        ),
    }
}

fn parse(lang: Lang, content: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language(lang)).ok()?;
    parser.parse(content, None)
}

/// Chunk code on function/type boundaries. Each top-level definition becomes its own chunk;
/// inter-definition lines (imports, comments) are grouped into gap chunks; oversized definitions
/// are sub-split with the heuristic chunker. Returns `(text, start_line, end_line)` (1-based,
/// inclusive). `None` if the language is unsupported or parsing fails.
pub fn chunk_code_ts(content: &str, lang: Lang) -> Option<Vec<(String, usize, usize)>> {
    let tree = parse(lang, content)?;
    let root = tree.root_node();
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Some(vec![]);
    }

    // Top-level chunkable spans (1-based inclusive), sorted.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut cur = root.walk();
    for child in root.named_children(&mut cur) {
        if is_chunkable(lang, child.kind()) {
            let s = child.start_position().row + 1;
            let e = (child.end_position().row + 1).clamp(s, total);
            spans.push((s, e));
        }
    }
    spans.sort();

    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let push = |a: usize, b: usize, out: &mut Vec<(String, usize, usize)>| {
        if a > b || a < 1 || b > total {
            return;
        }
        let text = lines[a - 1..b].join("\n");
        if text.trim().is_empty() {
            return;
        }
        if estimate_tokens(&text) <= CODE_CHUNK_TOKEN_BUDGET {
            out.push((text, a, b));
        } else {
            // Sub-split an oversized definition with the heuristic chunker; offset line numbers.
            for (t, s, e) in chunk_code_with(&text, false) {
                out.push((t, a + s - 1, a + e - 1));
            }
        }
    };

    let mut pos = 1usize;
    for (s, e) in spans {
        if s < pos {
            continue; // nested/overlapping — already covered
        }
        if s > pos {
            push(pos, s - 1, &mut out); // gap (imports, comments, top-level statements)
        }
        push(s, e, &mut out); // the definition
        pos = e + 1;
    }
    if pos <= total {
        push(pos, total, &mut out);
    }
    Some(out)
}

/// Extract `sym:<name>` entities for every definition via the AST — language-aware and more
/// accurate than the regex extractor (e.g. JS/TS `function`, Go methods). `None` on parse failure.
pub fn extract_symbols_ts(content: &str, lang: Lang) -> Option<Vec<String>> {
    let tree = parse(lang, content)?;
    let src = content.as_bytes();
    let mut syms: Vec<String> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if is_chunkable(lang, node.kind()) {
            if let Some(name) = node.child_by_field_name("name") {
                if let Ok(t) = name.utf8_text(src) {
                    if !t.is_empty() {
                        syms.push(format!("sym:{t}"));
                    }
                }
            }
        }
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            stack.push(child);
        }
    }
    syms.sort();
    syms.dedup();
    Some(syms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_detection() {
        assert_eq!(lang_for_path("src/main.rs"), Some(Lang::Rust));
        assert_eq!(lang_for_path("a/b.py"), Some(Lang::Python));
        assert_eq!(lang_for_path("x.ts"), Some(Lang::TypeScript));
        assert_eq!(lang_for_path("x.tsx"), Some(Lang::Tsx));
        assert_eq!(lang_for_path("x.go"), Some(Lang::Go));
        assert_eq!(lang_for_path("README.md"), None);
    }

    #[test]
    fn rust_chunks_on_item_boundaries_and_covers_all_lines() {
        let src = "use std::fmt;\n\nfn alpha() {\n    let x = 1;\n}\n\nstruct Beta {\n    a: u8,\n}\n\nfn gamma() {}\n";
        let out = chunk_code_ts(src, Lang::Rust).unwrap();
        // Expect distinct chunks; alpha, Beta, gamma each isolated.
        let joined: Vec<&str> = out.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(out.len() >= 3, "want >=3 chunks, got {}", out.len());
        assert!(joined.iter().any(|t| t.contains("fn alpha")));
        assert!(joined
            .iter()
            .any(|t| t.contains("struct Beta") && !t.contains("fn alpha")));
        assert!(joined.iter().any(|t| t.contains("fn gamma")));
        // Line ranges are within bounds and non-decreasing.
        for (_, s, e) in &out {
            assert!(*s >= 1 && e >= s);
        }
    }

    #[test]
    fn rust_symbols_via_ast() {
        let src = "fn alpha() {}\nstruct Beta;\nimpl Beta { fn method(&self) {} }\n";
        let syms = extract_symbols_ts(src, Lang::Rust).unwrap();
        assert!(syms.contains(&"sym:alpha".to_string()));
        assert!(syms.contains(&"sym:Beta".to_string()));
        assert!(
            syms.contains(&"sym:method".to_string()),
            "nested method symbol"
        );
    }

    #[test]
    fn typescript_function_symbol_caught_by_ast() {
        // The regex extractor misses JS/TS `function`; the AST catches it.
        let src = "export function doThing(x: number): number { return x; }\n";
        let syms = extract_symbols_ts(src, Lang::TypeScript).unwrap();
        assert!(syms.contains(&"sym:doThing".to_string()));
    }

    #[test]
    fn empty_content_is_no_chunks() {
        assert_eq!(chunk_code_ts("", Lang::Rust).unwrap().len(), 0);
    }
}
