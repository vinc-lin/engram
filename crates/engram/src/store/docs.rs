use super::{now_secs, taint_str, Store};
use crate::error::Result;
use crate::model::{MemoryDoc, NewDoc, Taint};
use rusqlite::{params, OptionalExtension};

impl Store {
    pub fn insert_doc(&self, namespace: &str, new: &NewDoc) -> Result<MemoryDoc> {
        let now = now_secs();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let conn = self.write.lock().unwrap();
            conn.execute(
                "INSERT INTO memory_docs
                   (document_id, namespace, key, title, content, author, taint, created_at, updated_at, meta)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
                 ON CONFLICT(namespace, key) DO UPDATE SET
                   title=excluded.title, content=excluded.content, author=excluded.author,
                   taint=excluded.taint, updated_at=excluded.updated_at, meta=excluded.meta",
                params![id, namespace, new.key, new.title, new.content, new.author,
                        taint_str(new.taint), now, new.meta.as_ref().map(|v| v.to_string())],
            )?;
        }
        self.get_by_key(namespace, &new.key)?
            .ok_or(crate::error::Error::NotFound)
    }

    pub fn get_doc(&self, namespace: &str, document_id: &str) -> Result<Option<MemoryDoc>> {
        let conn = self.read.get()?;
        Ok(conn
            .query_row(
                "SELECT document_id,namespace,key,title,content,author,taint,created_at,updated_at,meta
                 FROM memory_docs WHERE namespace=?1 AND document_id=?2",
                params![namespace, document_id],
                map_doc,
            )
            .optional()?)
    }

    pub fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryDoc>> {
        let conn = self.read.get()?;
        Ok(conn
            .query_row(
                "SELECT document_id,namespace,key,title,content,author,taint,created_at,updated_at,meta
                 FROM memory_docs WHERE namespace=?1 AND key=?2",
                params![namespace, key],
                map_doc,
            )
            .optional()?)
    }

    pub fn list_namespace(&self, namespace: &str, limit: i64) -> Result<Vec<MemoryDoc>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT document_id,namespace,key,title,content,author,taint,created_at,updated_at,meta
             FROM memory_docs WHERE namespace=?1 ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![namespace, limit], map_doc)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Forget a memory by its `key`: delete the doc row + its chunks + entities + unsealed
    /// tree leaves, atomically. Sealed tree summaries that already absorbed it stay (immutable
    /// history). Returns false if no doc with that key exists.
    pub fn delete_doc_by_key(&self, namespace: &str, key: &str) -> Result<bool> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let doc_id: Option<String> = tx
            .query_row(
                "SELECT document_id FROM memory_docs WHERE namespace=?1 AND key=?2",
                params![namespace, key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(doc_id) = doc_id else {
            tx.commit()?;
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM vector_chunks WHERE namespace=?1 AND document_id=?2",
            params![namespace, doc_id],
        )?;
        tx.execute(
            "DELETE FROM chunk_entities WHERE namespace=?1 AND document_id=?2",
            params![namespace, doc_id],
        )?;
        // Remove all code edges where this doc is the source (outbound references from the doc).
        tx.execute(
            "DELETE FROM code_edges WHERE namespace=?1 AND src_doc_id=?2",
            params![namespace, doc_id],
        )?;
        // Future-proofing for an eager-resolution schema: remove edges whose resolved
        // destination is this doc. TODAY dst_doc_id is ALWAYS NULL — cross-file resolution is
        // lazy at query time and never written back — so this DELETE removes nothing in
        // practice. Inbound references to a forgotten doc's symbols therefore survive as
        // unresolvable "ghost" edges (callers/callees show the bare sym): a documented
        // trade-off, not over-deleted via a dst_sym subquery (symbol-name collisions would
        // over-delete).
        tx.execute(
            "DELETE FROM code_edges WHERE namespace=?1 AND dst_doc_id=?2",
            params![namespace, doc_id],
        )?;
        tx.execute(
            "DELETE FROM tree_nodes WHERE namespace=?1 AND doc_id=?2 AND level=0 AND sealed=0",
            params![namespace, doc_id],
        )?;
        tx.execute(
            "DELETE FROM post_acquire_jobs WHERE namespace=?1 AND document_id=?2",
            params![namespace, doc_id],
        )?;
        tx.execute(
            "DELETE FROM memory_docs WHERE namespace=?1 AND document_id=?2",
            params![namespace, doc_id],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

fn taint_from(s: &str) -> Taint {
    match s {
        "external_sync" => Taint::ExternalSync,
        _ => Taint::Internal,
    }
}

fn map_doc(row: &rusqlite::Row) -> rusqlite::Result<MemoryDoc> {
    let meta: Option<String> = row.get(9)?;
    Ok(MemoryDoc {
        document_id: row.get(0)?,
        namespace: row.get(1)?,
        key: row.get(2)?,
        title: row.get(3)?,
        content: row.get(4)?,
        author: row.get(5)?,
        taint: taint_from(&row.get::<_, String>(6)?),
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        meta: meta.and_then(|s| serde_json::from_str(&s).ok()),
    })
}
