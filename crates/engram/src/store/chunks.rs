use super::{bytes_to_vec, now_secs, vec_to_bytes, Store};
use crate::error::Result;
use rusqlite::params;
use std::collections::HashMap;

type ChunkTuple = (String, String, Vec<f32>, Option<i64>, Option<i64>);

/// One chunk of a document, with its mechanical entities, for the cold tree pipeline.
#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub chunk_id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub entities: Vec<String>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}

/// A code chunk joined to its document's key (file path), for chunk-level code search.
#[derive(Debug, Clone)]
pub struct CodeChunkRow {
    pub key: String,
    pub document_id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}

impl Store {
    pub fn upsert_chunk(
        &self,
        namespace: &str,
        document_id: &str,
        chunk_id: &str,
        text: &str,
        embedding: &[f32],
        signature: &str,
    ) -> Result<()> {
        let now = now_secs();
        let bytes = vec_to_bytes(embedding);
        let dim = embedding.len() as i64;
        let conn = self.write.lock().unwrap();
        conn.execute(
            "INSERT INTO vector_chunks
               (namespace, document_id, chunk_id, text, embedding, model_signature, dim, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(namespace, chunk_id) DO UPDATE SET
               document_id=excluded.document_id, text=excluded.text, embedding=excluded.embedding,
               model_signature=excluded.model_signature, dim=excluded.dim, created_at=excluded.created_at",
            params![namespace, document_id, chunk_id, text, bytes, signature, dim, now],
        )?;
        Ok(())
    }

    pub fn delete_chunks_for_doc(&self, namespace: &str, document_id: &str) -> Result<()> {
        // Atomic: chunks and their entity rows must clear together, else a crash
        // between the two DELETEs could orphan entity rows (skewing graph scores).
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM vector_chunks WHERE namespace=?1 AND document_id=?2",
            params![namespace, document_id],
        )?;
        tx.execute(
            "DELETE FROM chunk_entities WHERE namespace=?1 AND document_id=?2",
            params![namespace, document_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn chunks_for_namespace(
        &self,
        namespace: &str,
        signature: &str,
    ) -> Result<Vec<(String, String, Vec<f32>)>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT document_id, text, embedding FROM vector_chunks
             WHERE namespace=?1 AND model_signature=?2",
        )?;
        let rows = stmt.query_map(params![namespace, signature], |row| {
            let doc_id: String = row.get(0)?;
            let text: String = row.get(1)?;
            let bytes: Vec<u8> = row.get(2)?;
            Ok((doc_id, text, bytes_to_vec(&bytes)))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// All chunks for a namespace+signature, joined to their document key (file path) and
    /// carrying line ranges — the candidate set for chunk-level code search.
    pub fn code_chunks_for_namespace(
        &self,
        namespace: &str,
        signature: &str,
    ) -> Result<Vec<CodeChunkRow>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            // line_start IS NOT NULL ⟺ a code-mode chunk (prose chunks store NULL), so this
            // returns only code chunks even if the namespace also holds prose docs.
            "SELECT d.key, c.document_id, c.text, c.embedding, c.line_start, c.line_end
             FROM vector_chunks c
             JOIN memory_docs d ON d.namespace = c.namespace AND d.document_id = c.document_id
             WHERE c.namespace = ?1 AND c.model_signature = ?2 AND c.line_start IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![namespace, signature], |r| {
            let bytes: Vec<u8> = r.get(3)?;
            Ok(CodeChunkRow {
                key: r.get(0)?,
                document_id: r.get(1)?,
                text: r.get(2)?,
                embedding: bytes_to_vec(&bytes),
                line_start: r.get(4)?,
                line_end: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn chunks_for_doc(&self, namespace: &str, document_id: &str) -> Result<Vec<ChunkRow>> {
        let conn = self.read.get()?;
        let mut cstmt = conn.prepare(
            "SELECT chunk_id, text, embedding, line_start, line_end FROM vector_chunks
             WHERE namespace=?1 AND document_id=?2 ORDER BY chunk_id",
        )?;
        let crows = cstmt.query_map(params![namespace, document_id], |r| {
            let bytes: Vec<u8> = r.get(2)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                bytes_to_vec(&bytes),
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut chunks: Vec<ChunkTuple> = Vec::new();
        for r in crows {
            chunks.push(r?);
        }

        let mut estmt = conn.prepare(
            "SELECT chunk_id, entity_id FROM chunk_entities WHERE namespace=?1 AND document_id=?2",
        )?;
        let erows = estmt.query_map(params![namespace, document_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut emap: HashMap<String, Vec<String>> = HashMap::new();
        for r in erows {
            let (cid, eid) = r?;
            emap.entry(cid).or_default().push(eid);
        }

        Ok(chunks
            .into_iter()
            .map(|(chunk_id, text, embedding, line_start, line_end)| {
                let entities = emap.remove(&chunk_id).unwrap_or_default();
                ChunkRow {
                    chunk_id,
                    text,
                    embedding,
                    entities,
                    line_start,
                    line_end,
                }
            })
            .collect())
    }
}
