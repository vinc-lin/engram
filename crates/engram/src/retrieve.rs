use crate::embed::Embedder;
use crate::error::Result;
use crate::model::MemoryDoc;
use crate::store::{cosine, Store, TreeNode};
use serde::Serialize;
use std::collections::HashMap;

/// A scored retrieval result. `doc` fields are flattened into the JSON object.
#[derive(Debug, Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub doc: MemoryDoc,
    pub score: f64,
    pub vector: f64,
    pub keyword: f64,
    pub graph: f64,
}

/// A drilled tree node, scored by cosine to the query.
#[derive(Debug, Serialize)]
pub struct TreeHit {
    pub node_id: String,
    pub tree_kind: String,
    pub tree_key: String,
    pub level: i64,
    pub label: Option<String>,
    pub body: String,
    pub doc_id: Option<String>,
    pub score: f64,
}

/// A chunk-level code search result: the matching chunk's file path, line range, and snippet.
#[derive(Debug, Serialize)]
pub struct CodeHit {
    pub path: String,
    pub document_id: String,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub snippet: String,
    pub score: f64,
    pub vector: f64,
    pub keyword: f64,
}

/// BFS a summary tree from its top nodes down to `max_depth`, rerank by cosine, and keep only
/// the latest leaf per `doc_id`. Defaults to the namespace Global tree (the cross-source digest).
#[allow(clippy::too_many_arguments)]
pub fn drill_down(
    store: &Store,
    embedder: &dyn Embedder,
    namespace: &str,
    query_text: &str,
    tree_kind: Option<&str>,
    tree_key: Option<&str>,
    max_depth: usize,
    limit: usize,
) -> Result<Vec<TreeHit>> {
    let qv = embedder.embed(query_text)?;
    let kind = tree_kind.unwrap_or("global");
    let key = tree_key.unwrap_or("global");

    let mut collected: Vec<TreeNode> = Vec::new();
    let mut frontier = store.tree_top_nodes(namespace, kind, key)?;
    let mut depth = 0usize;
    while !frontier.is_empty() {
        collected.extend(frontier.iter().cloned());
        if depth >= max_depth {
            break;
        }
        let mut next = Vec::new();
        for n in &frontier {
            next.extend(store.children_of(&n.node_id)?);
        }
        frontier = next;
        depth += 1;
    }

    // Surface fresh unsealed leaves (ingested since the last seal) once the tree is
    // consolidated — otherwise they're unreachable from the sealed roots until the next
    // seal. (When top level is 0 they were already collected as the frontier.)
    let top_level = collected.iter().map(|n| n.level).max().unwrap_or(0);
    if top_level > 0 {
        collected.extend(store.unsealed_nodes(namespace, kind, key, 0)?);
    }

    // Keep only the latest leaf per doc_id (summary nodes have doc_id = None, always kept).
    let mut latest: HashMap<String, usize> = HashMap::new();
    let mut keep = vec![true; collected.len()];
    for (i, n) in collected.iter().enumerate() {
        if let Some(doc) = n.doc_id.clone() {
            match latest.get(&doc) {
                Some(&j) if collected[j].created_at >= n.created_at => keep[i] = false,
                Some(&j) => {
                    keep[j] = false;
                    latest.insert(doc, i);
                }
                None => {
                    latest.insert(doc, i);
                }
            }
        }
    }

    let mut hits: Vec<TreeHit> = collected
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, n)| {
            let score = cosine(&qv, &n.embedding);
            TreeHit {
                node_id: n.node_id,
                tree_kind: n.tree_kind,
                tree_key: n.tree_key,
                level: n.level,
                label: n.label,
                body: n.body,
                doc_id: n.doc_id,
                score,
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

// OpenHuman weights (reference §3.1): graph-present vs no-graph fallback.
const GRAPH_W: f64 = 0.55;
const VEC_W_G: f64 = 0.30;
const KW_W_G: f64 = 0.15;
const VEC_W_FALLBACK: f64 = 0.65;
const KW_W_FALLBACK: f64 = 0.35;

fn keyword_overlap(query: &str, text: &str) -> f64 {
    let terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    if terms.is_empty() {
        return 0.0;
    }
    let hay = text.to_lowercase();
    let hits = terms.iter().filter(|t| hay.contains(t.as_str())).count();
    hits as f64 / terms.len() as f64
}

/// Hybrid search: vector cosine + keyword overlap + graph entity signal, blended, top-`limit`.
/// When the query contains entities that match stored docs, graph weights (OH §3.1) apply;
/// otherwise falls back to vector+keyword only.
pub fn query(
    store: &Store,
    embedder: &dyn Embedder,
    namespace: &str,
    query_text: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    let qv = embedder.embed(query_text)?;
    let sig = embedder.signature();
    let candidates = store.chunks_for_namespace(namespace, &sig)?;

    let q_entities = crate::ingest::extract_entities(query_text);
    let graph_counts = store.docs_with_entities(namespace, &q_entities)?;
    let max_graph = graph_counts.values().copied().max().unwrap_or(0) as f64;
    let has_graph = max_graph > 0.0;
    let (wg, wv, wk) = if has_graph {
        (GRAPH_W, VEC_W_G, KW_W_G)
    } else {
        (0.0, VEC_W_FALLBACK, KW_W_FALLBACK)
    };

    let mut best: HashMap<String, (f64, f64, f64, f64)> = HashMap::new(); // (score, v, k, g)
    for (doc_id, text, vec) in &candidates {
        let v = cosine(&qv, vec);
        let k = keyword_overlap(query_text, text);
        let g = if has_graph {
            *graph_counts.get(doc_id).unwrap_or(&0) as f64 / max_graph
        } else {
            0.0
        };
        let score = wg * g + wv * v + wk * k;
        let entry = best
            .entry(doc_id.clone())
            .or_insert((f64::MIN, 0.0, 0.0, 0.0));
        if score > entry.0 {
            *entry = (score, v, k, g);
        }
    }

    let mut scored: Vec<(String, f64, f64, f64, f64)> = best
        .into_iter()
        .map(|(d, (s, v, k, g))| (d, s, v, k, g))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    let mut hits = Vec::new();
    for (doc_id, score, vector, keyword, graph) in scored {
        if let Some(doc) = store.get_doc(namespace, &doc_id)? {
            hits.push(Hit {
                doc,
                score,
                vector,
                keyword,
                graph,
            });
        }
    }
    Ok(hits)
}

/// Chunk-level code search: rank individual code chunks by vector cosine + keyword overlap,
/// returning the matching chunk's `path:line` and snippet (not whole documents). The doc-level
/// `query` is for prose; this is the code path used by the MCP `search_code` tool.
pub fn search_code(
    store: &Store,
    embedder: &dyn Embedder,
    namespace: &str,
    query_text: &str,
    limit: usize,
) -> Result<Vec<CodeHit>> {
    let qv = embedder.embed(query_text)?;
    let sig = embedder.signature();
    let candidates = store.code_chunks_for_namespace(namespace, &sig)?;

    let mut hits: Vec<CodeHit> = candidates
        .into_iter()
        .map(|c| {
            let v = cosine(&qv, &c.embedding);
            let k = keyword_overlap(query_text, &c.text);
            let score = VEC_W_FALLBACK * v + KW_W_FALLBACK * k;
            CodeHit {
                path: c.key,
                document_id: c.document_id,
                line_start: c.line_start,
                line_end: c.line_end,
                snippet: c.text,
                score,
                vector: v,
                keyword: k,
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn freshness(updated_at: f64, now: f64) -> f64 {
    let age_hours = (now - updated_at).max(0.0) / 3600.0;
    (1.0 / (1.0 + age_hours / 24.0)).clamp(0.0, 1.0)
}

/// Query-less recall: most recent first (freshness decay). Graph/priority
/// signals arrive in later plans.
pub fn recall(store: &Store, namespace: &str, limit: usize) -> Result<Vec<Hit>> {
    let now = now_secs();
    let mut hits: Vec<Hit> = store
        .list_namespace(namespace, 1000)?
        .into_iter()
        .map(|doc| {
            let score = freshness(doc.updated_at, now);
            Hit {
                doc,
                score,
                vector: 0.0,
                keyword: 0.0,
                graph: 0.0,
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::model::NewDoc;

    fn seed() -> (Store, HashEmbedder, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(64);
        let mk = |k: &str, content: &str| NewDoc {
            key: k.into(),
            title: k.into(),
            content: content.into(),
            author: "a".into(),
            taint: crate::model::Taint::Internal,
            meta: None,
        };
        for (k, c) in [
            ("d1", "rust memory service with sqlite"),
            ("d2", "bananas grow on tropical trees"),
        ] {
            let doc = store.insert_doc("alice", &mk(k, c)).unwrap();
            let v = e.embed(c).unwrap();
            store
                .upsert_chunk(
                    "alice",
                    &doc.document_id,
                    &doc.document_id,
                    c,
                    &v,
                    &e.signature(),
                )
                .unwrap();
        }
        (store, e, dir)
    }

    #[test]
    fn query_ranks_relevant_first() {
        let (store, e, _d) = seed();
        let hits = query(&store, &e, "alice", "rust memory sqlite", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc.key, "d1");
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn recall_orders_by_recency() {
        let (store, _e, _d) = seed();
        let hits = recall(&store, "alice", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc.key, "d2"); // inserted last → freshest
    }

    #[test]
    fn drill_down_ranks_leaves_by_cosine() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(64);
        for (k, c) in [
            ("d1", "rust memory service"),
            ("d2", "bananas grow on trees"),
        ] {
            let new = crate::model::NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "a".into(),
                taint: crate::model::Taint::Internal,
                meta: None,
            };
            crate::ingest::ingest_document(&store, &e, "alice", &new).unwrap();
        }
        // Build the global tree (default gates → leaves only) by draining the queue.
        let chat = crate::llm::FakeChatClient::ok("S");
        let audit = crate::llm::NullAuditSink;
        let cfg = crate::config::Config::from_vars(|_| None);
        let ctx = crate::tree::TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        while let Some(job) = store.claim_job().unwrap() {
            crate::tree::process_doc(&store, &ctx, &job.namespace, &job.document_id).unwrap();
            store.complete_job(&job.job_id).unwrap();
        }
        let hits = drill_down(&store, &e, "alice", "rust memory", None, None, 3, 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].body.contains("rust"));
        assert!(hits[0].score >= hits.last().unwrap().score);
    }

    #[test]
    fn drill_down_surfaces_fresh_leaves_after_consolidation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let chat = crate::llm::FakeChatClient::ok("summary");
        let audit = crate::llm::NullAuditSink;
        // Tiny gates → consolidate the global tree into a sealed summary.
        let mut cfg = crate::config::Config::from_vars(|_| None);
        cfg.seal_input_token_budget = 1;
        cfg.seal_fanout = 2;
        cfg.seal_flush_age_secs = 1e12;
        let ctx = crate::tree::TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        let emb = e.embed("alpha consolidated").unwrap();
        crate::tree::append_leaf(
            &store,
            &ctx,
            "alice",
            "global",
            "global",
            "alpha consolidated",
            "d1",
            &emb,
        )
        .unwrap();
        crate::tree::append_leaf(
            &store,
            &ctx,
            "alice",
            "global",
            "global",
            "alpha again here",
            "d2",
            &emb,
        )
        .unwrap();
        assert!(store.tree_top_nodes("alice", "global", "global").unwrap()[0].level >= 1); // sealed

        // A fresh leaf added with default gates stays unsealed at L0.
        let cfg2 = crate::config::Config::from_vars(|_| None);
        let ctx2 = crate::tree::TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg2,
            vault: None,
        };
        let emb2 = e.embed("fresh zebra leaf").unwrap();
        crate::tree::append_leaf(
            &store,
            &ctx2,
            "alice",
            "global",
            "global",
            "fresh zebra leaf",
            "d3",
            &emb2,
        )
        .unwrap();

        // drill-down must surface the fresh leaf despite the consolidated summary.
        let hits = drill_down(&store, &e, "alice", "fresh zebra", None, None, 5, 10).unwrap();
        assert!(hits.iter().any(|h| h.body.contains("fresh zebra")));
    }

    #[test]
    fn search_code_returns_chunk_level_hits_with_paths_and_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(64);
        let mk = |k: &str, c: &str| crate::model::NewDoc {
            key: k.into(),
            title: k.into(),
            content: c.into(),
            author: "code".into(),
            taint: crate::model::Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        crate::ingest::ingest_document(
            &store,
            &e,
            "repo:x",
            &mk("src/auth.rs", "fn login() {}\nfn logout() {}"),
        )
        .unwrap();
        crate::ingest::ingest_document(
            &store,
            &e,
            "repo:x",
            &mk("src/math.rs", "fn add() {}\nfn mul() {}"),
        )
        .unwrap();

        let hits = search_code(&store, &e, "repo:x", "login", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].path, "src/auth.rs");
        assert!(hits[0].snippet.contains("login"));
        assert_eq!(hits[0].line_start, Some(1));
        assert!(hits[0].score >= hits.last().unwrap().score);
    }

    #[test]
    fn query_uses_graph_signal_for_entities() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = crate::embed::HashEmbedder::new(64);
        for (k, c) in [
            ("d1", "release notes ping @bob"),
            ("d2", "release notes only"),
        ] {
            let new = crate::model::NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "a".into(),
                taint: crate::model::Taint::Internal,
                meta: None,
            };
            crate::ingest::ingest_document(&store, &e, "alice", &new).unwrap();
        }
        let hits = query(&store, &e, "alice", "release from @bob", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc.key, "d1");
        assert!(hits[0].graph > 0.0);
    }
}
