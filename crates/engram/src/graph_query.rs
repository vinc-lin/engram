//! graph_query.rs — BFS callers / callees over the code_edges table.
//!
//! Both queries do a bounded-depth breadth-first search. Cross-file edge resolution uses
//! `Store::resolve_dst_doc` which hits `chunk_entities` via its index
//! (`idx_chunk_entities_entity`) — no extra join table needed. Upward caller expansion uses
//! `Store::sym_entities_for_doc`.
//!
//! These functions read only SQLite (no embed / LLM calls), so the axum handlers that call
//! them do NOT need `spawn_blocking`.
//!
//! retrieve.rs / search_code / query NEVER call into this module — code_edges is a structural
//! index only, kept off the recall-tuned path to avoid the 0.55 graph-signal recall crater.

use crate::error::Result;
use crate::model::EdgeKind;
use crate::store::Store;
use std::collections::{HashSet, VecDeque};

/// One hop in a caller or callee traversal result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceHop {
    /// File path (the document's `key`).
    pub path: String,
    pub document_id: String,
    /// The symbol targeted by this edge (in `sym:<Name>` form).
    pub sym: String,
    /// The edge kind string ("CALLS", "USES_TYPE", "IMPORTS").
    pub edge_kind: String,
    pub confidence: f32,
    pub depth: usize,
}

/// BFS over inbound `CALLS` edges to find files that call `sym`.
///
/// Starting from `sym`, each BFS level walks inbound `code_edges` rows whose `dst_sym =
/// current_sym`. For each `src_doc_id`, the path is resolved from `memory_docs`. The BFS then
/// expands by resolving the caller's document to its own `sym:` definitions
/// (`sym_entities_for_doc`) and walking their inbound edges in turn, up to `max_depth`.
pub fn callers(
    store: &Store,
    ns: &str,
    sym: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Vec<TraceHop>> {
    let mut results: Vec<TraceHop> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((sym.to_string(), 1));

    while let Some((target_sym, depth)) = queue.pop_front() {
        if depth > max_depth || results.len() >= limit {
            break;
        }
        let inbound = store.edges_to_sym(ns, &target_sym)?;
        for (src_doc_id, edge) in inbound {
            if !matches!(edge.edge_kind, EdgeKind::Calls) {
                continue;
            }
            if visited.contains(&src_doc_id) {
                continue;
            }
            visited.insert(src_doc_id.clone());
            let path = store
                .get_doc(ns, &src_doc_id)?
                .map(|d| d.key)
                .unwrap_or_else(|| src_doc_id.clone());
            results.push(TraceHop {
                path,
                document_id: src_doc_id.clone(),
                sym: target_sym.clone(),
                edge_kind: edge.edge_kind.as_str().to_string(),
                confidence: edge.confidence,
                depth,
            });
            if results.len() >= limit {
                break;
            }
            if depth < max_depth {
                // Expand upward: queue this caller's own sym: definitions so the next level
                // finds callers-of-callers.
                for s in store.sym_entities_for_doc(ns, &src_doc_id)? {
                    queue.push_back((s, depth + 1));
                }
            }
        }
    }
    Ok(results)
}

/// BFS over outbound `CALLS` edges from the file at `path` to find what it calls (directly and
/// transitively, up to `max_depth`).
pub fn callees(
    store: &Store,
    ns: &str,
    path: &str,
    max_depth: usize,
    limit: usize,
) -> Result<Vec<TraceHop>> {
    let src_doc = match store.get_by_key(ns, path)? {
        Some(d) => d,
        None => return Ok(vec![]),
    };
    let mut results: Vec<TraceHop> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((src_doc.document_id.clone(), 1));
    visited.insert(src_doc.document_id.clone());

    while let Some((doc_id, depth)) = queue.pop_front() {
        if depth > max_depth || results.len() >= limit {
            break;
        }
        let outbound = store.edges_from(ns, &doc_id)?;
        for edge in outbound {
            if !matches!(edge.edge_kind, EdgeKind::Calls) {
                continue;
            }
            let dst_sym = edge.dst_sym.clone();
            let dst_doc_id = match store.resolve_dst_doc(ns, &dst_sym)? {
                Some(id) => id,
                None => {
                    // Unresolved: record the hop without recursing.
                    results.push(TraceHop {
                        path: dst_sym.clone(),
                        document_id: String::new(),
                        sym: dst_sym.clone(),
                        edge_kind: edge.edge_kind.as_str().to_string(),
                        confidence: edge.confidence,
                        depth,
                    });
                    continue;
                }
            };
            let dst_path = store
                .get_doc(ns, &dst_doc_id)?
                .map(|d| d.key)
                .unwrap_or_else(|| dst_doc_id.clone());
            if !visited.contains(&dst_doc_id) {
                visited.insert(dst_doc_id.clone());
                results.push(TraceHop {
                    path: dst_path,
                    document_id: dst_doc_id.clone(),
                    sym: dst_sym,
                    edge_kind: edge.edge_kind.as_str().to_string(),
                    confidence: edge.confidence,
                    depth,
                });
                if results.len() >= limit {
                    break;
                }
                if depth < max_depth {
                    queue.push_back((dst_doc_id, depth + 1));
                }
            }
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callers_bfs_single_hop() {
        use crate::embed::{Embedder, HashEmbedder};
        use crate::model::{EdgeKind, NewDoc, RawEdge, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let emb = HashEmbedder::new(4);

        let caller_doc = {
            let new = NewDoc {
                key: "src/caller.rs".into(),
                title: "caller".into(),
                content: "fn caller() { target(); }".into(),
                author: "a".into(),
                taint: Taint::Internal,
                meta: Some(serde_json::json!({"kind": "file"})),
            };
            let emb_vec = emb.embed("fn caller() { target(); }").unwrap();
            let edges = vec![RawEdge {
                dst_sym: "sym:target".into(),
                edge_kind: EdgeKind::Calls,
                src_line: Some(1),
                confidence: 0.3,
            }];
            store
                .commit_ingest(
                    "ns",
                    &new,
                    &["fn caller() { target(); }".to_string()],
                    &[emb_vec],
                    &[vec![]],
                    &[Some((1, 1))],
                    &edges,
                    "hash:4",
                )
                .unwrap()
        };

        let target_doc = {
            let new = NewDoc {
                key: "src/target.rs".into(),
                title: "target".into(),
                content: "fn target() {}".into(),
                author: "a".into(),
                taint: Taint::Internal,
                meta: Some(serde_json::json!({"kind": "file"})),
            };
            let emb_vec = emb.embed("fn target() {}").unwrap();
            store
                .commit_ingest(
                    "ns",
                    &new,
                    &["fn target() {}".to_string()],
                    &[emb_vec],
                    &[vec!["sym:target".to_string()]],
                    &[Some((1, 1))],
                    &[],
                    "hash:4",
                )
                .unwrap()
        };

        let hops = callers(&store, "ns", "sym:target", 2, 10).unwrap();
        assert!(!hops.is_empty(), "expected at least one caller hop");
        let hop = &hops[0];
        assert_eq!(hop.document_id, caller_doc.document_id);
        assert_eq!(hop.sym, "sym:target");
        assert!(hop.depth <= 2);
        let _ = target_doc;
    }

    #[test]
    fn callees_bfs_single_hop() {
        use crate::embed::{Embedder, HashEmbedder};
        use crate::model::{EdgeKind, NewDoc, RawEdge, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let emb = HashEmbedder::new(4);

        let new = NewDoc {
            key: "src/foo.rs".into(),
            title: "foo".into(),
            content: "fn foo() { bar(); baz(); }".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        let emb_vec = emb.embed("fn foo() { bar(); baz(); }").unwrap();
        let edges = vec![
            RawEdge {
                dst_sym: "sym:bar".into(),
                edge_kind: EdgeKind::Calls,
                src_line: Some(1),
                confidence: 0.3,
            },
            RawEdge {
                dst_sym: "sym:baz".into(),
                edge_kind: EdgeKind::Calls,
                src_line: Some(1),
                confidence: 0.3,
            },
        ];
        let doc = store
            .commit_ingest(
                "ns",
                &new,
                &["fn foo() { bar(); baz(); }".to_string()],
                &[emb_vec],
                &[vec![]],
                &[Some((1, 1))],
                &edges,
                "hash:4",
            )
            .unwrap();

        let hops = callees(&store, "ns", "src/foo.rs", 2, 10).unwrap();
        assert!(
            hops.iter().any(|h| h.sym == "sym:bar"),
            "expected sym:bar; got {hops:?}"
        );
        assert!(
            hops.iter().any(|h| h.sym == "sym:baz"),
            "expected sym:baz; got {hops:?}"
        );
        let _ = doc;
    }
}
