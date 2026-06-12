//! Read-only (from outside the store) access to `code_edges`, plus the single write helper.
//! Write path: `insert_edges_in_tx`, called exclusively from `commit_ingest` inside an open
//! `rusqlite::Transaction`, enforcing the "never hold write lock across I/O" invariant.
//!
//! retrieve.rs / search_code / query NEVER call into this module — code_edges is a structural
//! index only, kept off the recall-tuned path to avoid the 0.55 graph-signal recall crater.

use super::Store;
use crate::error::Result;
use crate::model::{EdgeKind, RawEdge};
use rusqlite::{params, OptionalExtension, Transaction};

impl Store {
    /// All outbound code edges from a given source document. Uses the read pool.
    pub fn edges_from(&self, ns: &str, src_doc_id: &str) -> Result<Vec<RawEdge>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare_cached(
            "SELECT dst_sym, edge_kind, src_line, confidence
             FROM code_edges
             WHERE namespace=?1 AND src_doc_id=?2",
        )?;
        let rows = stmt.query_map(params![ns, src_doc_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (dst_sym, kind_str, src_line, confidence) = row?;
            if let Some(edge_kind) = EdgeKind::parse(&kind_str) {
                out.push(RawEdge {
                    dst_sym,
                    edge_kind,
                    src_line,
                    confidence: confidence as f32,
                });
            }
        }
        Ok(out)
    }

    /// All code edges whose `dst_sym` matches the given symbol, with their source document id.
    /// Used by the callers BFS in `graph_query::callers`.
    pub fn edges_to_sym(&self, ns: &str, dst_sym: &str) -> Result<Vec<(String, RawEdge)>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare_cached(
            "SELECT src_doc_id, edge_kind, src_line, confidence
             FROM code_edges
             WHERE namespace=?1 AND dst_sym=?2",
        )?;
        let rows = stmt.query_map(params![ns, dst_sym], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (src_doc_id, kind_str, src_line, confidence) = row?;
            if let Some(edge_kind) = EdgeKind::parse(&kind_str) {
                out.push((
                    src_doc_id,
                    RawEdge {
                        dst_sym: dst_sym.to_string(),
                        edge_kind,
                        src_line,
                        confidence: confidence as f32,
                    },
                ));
            }
        }
        Ok(out)
    }

    /// Lazy cross-file resolution: find the document_id that defines `dst_sym` by looking it
    /// up in chunk_entities (index `idx_chunk_entities_entity`). Returns `None` if the symbol
    /// is not yet indexed in this namespace. dst_doc_id in code_edges is NOT updated here —
    /// callers use this for BFS traversal at query time without mutating the store.
    pub fn resolve_dst_doc(&self, ns: &str, dst_sym: &str) -> Result<Option<String>> {
        let conn = self.read.get()?;
        Ok(conn
            .query_row(
                "SELECT DISTINCT document_id FROM chunk_entities
                 WHERE namespace=?1 AND entity_id=?2
                 LIMIT 1",
                params![ns, dst_sym],
                |r| r.get(0),
            )
            .optional()?)
    }
}

/// Insert all edges for `src_doc_id` inside an already-open transaction.
/// Called exclusively from `commit_ingest`; `pub(super)` so `mod.rs` is the only caller.
/// Re-ingest replaces a doc's edges atomically (DELETE before the inserts, same tx).
pub(super) fn insert_edges_in_tx(
    tx: &Transaction<'_>,
    ns: &str,
    src_doc_id: &str,
    edges: &[RawEdge],
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM code_edges WHERE namespace=?1 AND src_doc_id=?2",
        params![ns, src_doc_id],
    )?;
    for e in edges {
        tx.execute(
            "INSERT OR REPLACE INTO code_edges
               (namespace, src_doc_id, dst_sym, dst_doc_id, edge_kind, src_line, confidence)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6)",
            params![
                ns,
                src_doc_id,
                e.dst_sym,
                e.edge_kind.as_str(),
                e.src_line,
                e.confidence as f64
            ],
        )?;
    }
    Ok(())
}
