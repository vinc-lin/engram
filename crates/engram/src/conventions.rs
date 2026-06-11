//! Single-pass digests written into the `<ns>:meta` namespace: the coding-conventions doc (Phase
//! 3b, `rebuild_conventions`) and the architecture/module digest (`rebuild_architecture_digest`).
//! The architecture digest **replaces the deep consolidation-tree fold for the digest path** — one
//! LLM compression pass over the raw source preserves the specifics that a summary-of-summaries
//! loses (measured: `eval/RESULTS_distillation.md`).

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

/// True if a doc is an ingested source file (code mode) — the substance of an architecture digest.
fn is_source_file(d: &MemoryDoc) -> bool {
    d.meta
        .as_ref()
        .and_then(|m| m.get("kind"))
        .and_then(|k| k.as_str())
        == Some("file")
}

/// Build a **single-pass** architecture digest for `namespace` and cache it at `<ns>:meta` (key
/// `architecture`, or `module:<dir>` when `module` is set). One LLM summary over the repo's source
/// files (a bounded snippet per file, extractive when the corpus exceeds the budget) — the digest
/// path's replacement for the deep consolidation-tree fold. A single compression pass over the raw
/// source preserves concrete specifics that a multi-level summary-of-summaries averages away (see
/// `eval/RESULTS_distillation.md`: one-shot 1.58 vs tree 1.08 vs map-reduce 0.58 of 2). Falls back
/// to a deterministic file list when the LLM is unavailable, so the digest is never empty.
pub fn rebuild_architecture_digest(
    store: &Store,
    chat: &dyn ChatClient,
    embedder: &dyn Embedder,
    namespace: &str,
    module: Option<&str>,
    max_tokens: usize,
) -> Result<MemoryDoc> {
    const PER_FILE_CHARS: usize = 1200;
    const TOTAL_BUDGET_CHARS: usize = 60_000;

    let docs = store.list_namespace(namespace, 5000)?;
    let prefix = module.map(|p| format!("{}/", p.trim_end_matches('/')));
    let mut selected: Vec<&MemoryDoc> = docs
        .iter()
        .filter(|d| is_source_file(d))
        .filter(|d| match &prefix {
            Some(pfx) => d.key.starts_with(pfx.as_str()),
            None => true,
        })
        .collect();
    selected.sort_by(|a, b| a.key.cmp(&b.key));

    // Single-pass corpus: a bounded snippet per file, capped by a total budget (extractive overflow
    // — never an abstractive fold, which is what loses specifics).
    let mut corpus = String::new();
    for d in &selected {
        if corpus.len() >= TOTAL_BUDGET_CHARS {
            break;
        }
        let snippet: String = d.content.chars().take(PER_FILE_CHARS).collect();
        corpus.push_str(&format!("### {}\n{}\n\n", d.key, snippet));
    }

    let scope = match module {
        Some(p) => format!(" (module: {p})"),
        None => String::new(),
    };
    let prompt = format!(
        "You are writing an architecture digest of a software project{scope} for an engineer who \
         will work on it. Using ONLY the source files below, produce a dense, specific overview: the \
         key modules/files and their responsibilities, the main data and control flow, and the \
         load-bearing invariants. PRESERVE concrete specifics — identifier names, file names, and \
         mechanisms — never replace a specific with vague prose.\n\n## Source files ({})\n{}",
        selected.len(),
        corpus
    );
    let body = match chat.summarize(&prompt, max_tokens) {
        Ok(r) if !r.text.trim().is_empty() => r.text,
        _ => {
            let mut s =
                String::from("Architecture digest (mechanical fallback — LLM unavailable):\n");
            for d in &selected {
                s.push_str(&format!("- {}\n", d.key));
            }
            s
        }
    };

    let key = match module {
        Some(p) => format!("module:{}", p.trim_end_matches('/')),
        None => "architecture".to_string(),
    };
    let new = NewDoc {
        key: key.clone(),
        title: key,
        content: body,
        author: "engram".into(),
        taint: Taint::Internal,
        meta: Some(serde_json::json!({ "kind": "digest", "module": module })),
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

    fn ingest_file(store: &Store, e: &HashEmbedder, ns: &str, key: &str, content: &str) {
        let new = NewDoc {
            key: key.into(),
            title: key.into(),
            content: content.into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({ "kind": "file" })),
        };
        crate::ingest::ingest_document(store, e, ns, &new).unwrap();
    }

    #[test]
    fn architecture_digest_is_single_pass_and_module_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        ingest_file(&store, &e, "repo:z", "src/api.rs", "fn route() {}");
        ingest_file(&store, &e, "repo:z", "src/store/mod.rs", "struct Store;");
        ingest_file(&store, &e, "repo:z", "README.md", "the readme");

        let chat = FakeChatClient::ok("engram: axum API over SQLite; src/api.rs routes requests");
        let doc = rebuild_architecture_digest(&store, &chat, &e, "repo:z", None, 1000).unwrap();
        assert_eq!(doc.namespace, "repo:z:meta");
        assert_eq!(doc.key, "architecture");
        assert!(doc.content.contains("axum"));
        // Readable back by key (what get_architecture serves).
        let got = store
            .get_by_key("repo:z:meta", "architecture")
            .unwrap()
            .unwrap();
        assert!(got.content.contains("axum"));

        // A module digest is keyed by its directory.
        let m = rebuild_architecture_digest(&store, &chat, &e, "repo:z", Some("src/store"), 1000)
            .unwrap();
        assert_eq!(m.key, "module:src/store");
    }

    #[test]
    fn architecture_digest_falls_back_without_llm() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        ingest_file(&store, &e, "repo:w", "src/main.rs", "fn main() {}");
        let chat = FakeChatClient::failing();
        let doc = rebuild_architecture_digest(&store, &chat, &e, "repo:w", None, 1000).unwrap();
        assert!(doc.content.contains("src/main.rs")); // mechanical file-list fallback
    }
}
