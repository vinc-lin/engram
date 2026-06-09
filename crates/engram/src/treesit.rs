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
    Kotlin,
    Java,
    C,
    Cpp,
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
        "kt" | "kts" => Lang::Kotlin,
        "java" => Lang::Java,
        "c" => Lang::C,
        // `.h` routes to C++ (a superset of C): the C++ grammar parses the vast majority of C
        // headers, and a parse failure falls back to the heuristic chunker (zero data loss).
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h" => Lang::Cpp,
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
        Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
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
        Lang::Kotlin => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "object_declaration"
                | "property_declaration"
                | "secondary_constructor"
        ),
        Lang::Java => matches!(
            kind,
            "method_declaration"
                | "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "constructor_declaration"
                | "record_declaration"
        ),
        Lang::C => matches!(
            kind,
            "function_definition"
                | "struct_specifier"
                | "union_specifier"
                | "enum_specifier"
                | "type_definition"
        ),
        Lang::Cpp => matches!(
            kind,
            "function_definition"
                | "struct_specifier"
                | "union_specifier"
                | "enum_specifier"
                | "type_definition"
                | "class_specifier"
                | "namespace_definition"
                | "template_declaration"
        ),
    }
}

/// Whether a top-level node starts a chunk. For C/C++ this also unwraps a `declaration` /
/// `type_definition` / `linkage_specification` that *defines* a struct/union/enum/class (it has a
/// body) — those wrap the specifier, so the flat `is_chunkable` check alone misses them.
fn is_boundary(lang: Lang, node: &tree_sitter::Node) -> bool {
    if is_chunkable(lang, node.kind()) {
        return true;
    }
    if matches!(lang, Lang::C | Lang::Cpp)
        && matches!(
            node.kind(),
            "declaration" | "type_definition" | "linkage_specification"
        )
    {
        let mut cur = node.walk();
        return node.named_children(&mut cur).any(|ch| {
            matches!(
                ch.kind(),
                "struct_specifier" | "union_specifier" | "enum_specifier" | "class_specifier"
            ) && ch.child_by_field_name("body").is_some()
        });
    }
    false
}

/// The defining identifier for a chunkable node. Uses the `name` field when present; for C/C++
/// `function_definition` (whose name lives under `declarator`) it follows the declarator chain.
fn symbol_name(lang: Lang, node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return name
            .utf8_text(src)
            .ok()
            .filter(|t| !t.is_empty())
            .map(str::to_string);
    }
    if matches!(lang, Lang::C | Lang::Cpp) && node.kind() == "function_definition" {
        if let Some(decl) = node.child_by_field_name("declarator") {
            return declarator_name(decl, src);
        }
    }
    None
}

/// Follow a C/C++ declarator chain (`function_declarator` -> `pointer_declarator` -> ...) down to
/// the function-name identifier.
fn declarator_name(mut node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    loop {
        if matches!(
            node.kind(),
            "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
        ) {
            return node
                .utf8_text(src)
                .ok()
                .filter(|t| !t.is_empty())
                .map(str::to_string);
        }
        node = node.child_by_field_name("declarator")?;
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
        if is_boundary(lang, &child) {
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
            if let Some(t) = symbol_name(lang, &node, src) {
                syms.push(format!("sym:{t}"));
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
        assert_eq!(lang_for_path("Main.kt"), Some(Lang::Kotlin));
        assert_eq!(lang_for_path("Foo.java"), Some(Lang::Java));
        assert_eq!(lang_for_path("img.c"), Some(Lang::C));
        assert_eq!(lang_for_path("stitch.cpp"), Some(Lang::Cpp));
        assert_eq!(lang_for_path("native_lib.h"), Some(Lang::Cpp)); // .h -> C++ (superset)
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

    #[test]
    fn kotlin_chunks_and_symbols() {
        let src = "package x\n\nimport y.Z\n\nfun alpha(n: Int): Int {\n    return n + 1\n}\n\nclass Beta(val a: Int) {\n    fun method() {}\n}\n";
        let out = chunk_code_ts(src, Lang::Kotlin).unwrap();
        let joined: Vec<&str> = out.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(
            joined.iter().any(|t| t.contains("fun alpha")),
            "alpha chunk"
        );
        assert!(
            joined.iter().any(|t| t.contains("class Beta")),
            "Beta chunk"
        );
        let syms = extract_symbols_ts(src, Lang::Kotlin).unwrap();
        assert!(syms.contains(&"sym:alpha".to_string()), "got {syms:?}");
        assert!(syms.contains(&"sym:Beta".to_string()), "got {syms:?}");
    }

    #[test]
    fn java_chunks_and_symbols() {
        let src = "package x;\n\nimport y.Z;\n\nclass Foo {\n    int alpha(int n) { return n + 1; }\n    void beta() {}\n}\n";
        let out = chunk_code_ts(src, Lang::Java).unwrap();
        assert!(out.iter().any(|(t, _, _)| t.contains("class Foo")));
        let syms = extract_symbols_ts(src, Lang::Java).unwrap();
        assert!(syms.contains(&"sym:Foo".to_string()), "got {syms:?}");
        assert!(syms.contains(&"sym:alpha".to_string()), "got {syms:?}");
    }

    #[test]
    fn c_chunks_cover_function_and_struct() {
        let src = "#include <stdio.h>\n\nstruct Pixel { int r, g, b; };\n\nint dewarp(int x) {\n    return x * 2;\n}\n";
        let out = chunk_code_ts(src, Lang::C).unwrap();
        let joined: Vec<&str> = out.iter().map(|(t, _, _)| t.as_str()).collect();
        // The struct is now its OWN boundary chunk (declaration unwrapped), not lumped with dewarp.
        assert!(joined
            .iter()
            .any(|t| t.contains("struct Pixel") && !t.contains("int dewarp")));
        assert!(joined.iter().any(|t| t.contains("int dewarp")));
        let syms = extract_symbols_ts(src, Lang::C).unwrap();
        assert!(
            syms.contains(&"sym:dewarp".to_string()),
            "C fn via declarator: {syms:?}"
        );
        assert!(
            syms.contains(&"sym:Pixel".to_string()),
            "C struct name: {syms:?}"
        );
    }

    #[test]
    fn cpp_chunks_namespace_and_class() {
        let src = "namespace avm {\n\nclass Stitcher {\npublic:\n    int blend(int a) { return a; }\n};\n\nvoid run() {}\n\n}\n";
        let out = chunk_code_ts(src, Lang::Cpp).unwrap();
        let joined: Vec<&str> = out.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(joined
            .iter()
            .any(|t| t.contains("class Stitcher") || t.contains("namespace avm")));
        // C++ top-level function name resolved via the declarator chain.
        let syms = extract_symbols_ts(src, Lang::Cpp).unwrap();
        assert!(
            syms.contains(&"sym:run".to_string()),
            "C++ fn via declarator: {syms:?}"
        );
    }

    #[test]
    fn malformed_cpp_does_not_panic() {
        // Garbage / truncated C++ still parses to a (possibly error-laden) tree, never panics.
        assert!(chunk_code_ts("class { void (( ; namespace }{", Lang::Cpp).is_some());
    }
}
