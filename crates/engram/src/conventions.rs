//! Phase 3b: extract a repo's coding conventions from its config files + architecture digests
//! into a single document in the `<ns>:meta` namespace, read back by `get_conventions`.

use crate::embed::Embedder;
use crate::error::Result;
use crate::llm::ChatClient;
use crate::model::{MemoryDoc, NewDoc, Taint};
use crate::retrieve::drill_down;
use crate::store::Store;

/// The meta namespace for a repo namespace (`repo:<id>` → `repo:<id>:meta`).
pub fn meta_namespace(namespace: &str) -> String {
    format!("{namespace}:meta")
}

/// True if a doc key looks like a convention-source config file.
pub fn is_config_file(key: &str) -> bool {
    let base = key.rsplit('/').next().unwrap_or(key).to_ascii_lowercase();
    matches!(
        base.as_str(),
        ".editorconfig"
            | "rustfmt.toml"
            | ".rustfmt.toml"
            | "clippy.toml"
            | "contributing.md"
            | "pyproject.toml"
            | "setup.cfg"
            | ".pylintrc"
            | "tsconfig.json"
            | ".gitattributes"
    ) || base.starts_with(".eslintrc")
        || base.starts_with(".prettierrc")
}

/// Rebuild the conventions document for `namespace` from its config files + the global
/// architecture digest. Stores it at `<ns>:meta` key `conventions` and returns the stored doc.
pub fn rebuild_conventions(
    store: &Store,
    chat: &dyn ChatClient,
    embedder: &dyn Embedder,
    namespace: &str,
    max_tokens: usize,
) -> Result<MemoryDoc> {
    // 1. Gather config files already ingested in the code namespace.
    let docs = store.list_namespace(namespace, 1000)?;
    let config_keys: Vec<&MemoryDoc> = docs.iter().filter(|d| is_config_file(&d.key)).collect();
    let mut configs = String::new();
    for d in &config_keys {
        let snippet: String = d.content.chars().take(1500).collect();
        configs.push_str(&format!("### {}\n{}\n\n", d.key, snippet));
    }

    // 2. Gather the global architecture digest (query-less drill — no embed call).
    let digests = drill_down(
        store,
        embedder,
        namespace,
        "",
        Some("global"),
        Some("global"),
        3,
        12,
    )
    .unwrap_or_default();
    let mut digest_text = String::new();
    for h in &digests {
        let line = h
            .label
            .clone()
            .unwrap_or_else(|| h.body.chars().take(120).collect());
        digest_text.push_str(&format!("- {line}\n"));
    }

    // 3. LLM extraction with a deterministic fallback so the doc is never empty.
    let prompt = format!(
        "You are documenting the coding conventions of a software project. Using ONLY the evidence \
         below (configuration files and architecture digests), produce a markdown bullet list of the \
         project's concrete conventions. Each bullet states one rule, then '(evidence: <file/area>)'. \
         Do not invent rules unsupported by the evidence. Aim for at least 10 distinct conventions.\n\n\
         ## Config files ({})\n{}\n## Architecture digests\n{}",
        config_keys.len(),
        configs,
        digest_text
    );
    let body = match chat.summarize(&prompt, max_tokens) {
        Ok(r) if !r.text.trim().is_empty() => r.text,
        _ => {
            let mut s = String::from("Conventions (mechanical fallback — LLM unavailable):\n");
            for d in &config_keys {
                s.push_str(&format!(
                    "- config present: {} (evidence: {})\n",
                    d.key, d.key
                ));
            }
            s
        }
    };

    // 4. Upsert into the meta namespace.
    let new = NewDoc {
        key: "conventions".into(),
        title: "conventions".into(),
        content: body,
        author: "engram".into(),
        taint: Taint::Internal,
        meta: Some(serde_json::json!({ "kind": "conventions" })),
    };
    crate::ingest::ingest_document(store, embedder, &meta_namespace(namespace), &new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::llm::FakeChatClient;
    use crate::model::{NewDoc, Taint};

    #[test]
    fn is_config_file_recognizes_common_configs() {
        assert!(is_config_file("rustfmt.toml"));
        assert!(is_config_file("crates/x/.editorconfig"));
        assert!(is_config_file(".eslintrc.json"));
        assert!(is_config_file("CONTRIBUTING.md"));
        assert!(!is_config_file("src/main.rs"));
        assert!(!is_config_file("README.md"));
    }

    #[test]
    fn rebuild_writes_conventions_doc_to_meta_ns() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        // Ingest a config file + a source file into the code namespace.
        for (k, c) in [
            ("rustfmt.toml", "max_width = 100\n"),
            ("src/main.rs", "fn main() {}"),
        ] {
            let new = NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "code".into(),
                taint: Taint::Internal,
                meta: Some(serde_json::json!({ "kind": "file" })),
            };
            crate::ingest::ingest_document(&store, &e, "repo:x", &new).unwrap();
        }
        let chat =
            FakeChatClient::ok("- format with rustfmt, max_width 100 (evidence: rustfmt.toml)");
        let doc = rebuild_conventions(&store, &chat, &e, "repo:x", 1000).unwrap();
        assert_eq!(doc.namespace, "repo:x:meta");
        assert_eq!(doc.key, "conventions");
        assert!(doc.content.contains("rustfmt"));
        // Readable back by key from the meta namespace.
        let got = store
            .get_by_key("repo:x:meta", "conventions")
            .unwrap()
            .unwrap();
        assert!(got.content.contains("rustfmt"));
    }

    #[test]
    fn rebuild_falls_back_when_llm_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let new = NewDoc {
            key: ".editorconfig".into(),
            title: ".editorconfig".into(),
            content: "indent_style = space\n".into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({ "kind": "file" })),
        };
        crate::ingest::ingest_document(&store, &e, "repo:y", &new).unwrap();
        let chat = FakeChatClient::failing();
        let doc = rebuild_conventions(&store, &chat, &e, "repo:y", 1000).unwrap();
        assert!(doc.content.contains(".editorconfig"));
    }
}
