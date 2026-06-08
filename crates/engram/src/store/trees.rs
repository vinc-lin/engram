use super::{bytes_to_vec, now_secs, vec_to_bytes, Store};
use crate::error::Result;
use rusqlite::{params, Connection};

/// A tree node row (leaf at level 0, summary at level ≥1).
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub node_id: String,
    pub tree_kind: String,
    pub tree_key: String,
    pub level: i64,
    pub seq: i64,
    pub sealed: bool,
    pub label: Option<String>,
    pub body: String,
    pub doc_id: Option<String>,
    pub token_count: i64,
    pub embedding: Vec<f32>,
    pub created_at: f64,
}

/// Insert payload for a tree node (borrows; embedding stored as LE-f32).
pub struct NewTreeNode<'a> {
    pub namespace: &'a str,
    pub tree_kind: &'a str,
    pub tree_key: &'a str,
    pub level: i64,
    pub sealed: bool,
    pub label: Option<&'a str>,
    pub body: &'a str,
    pub doc_id: Option<&'a str>,
    pub token_count: i64,
    pub embedding: &'a [f32],
    pub model_signature: &'a str,
}

const TREE_COLS: &str =
    "node_id, tree_kind, tree_key, level, seq, sealed, label, body, doc_id, token_count, embedding, created_at";

impl Store {
    pub fn append_leaf_node(&self, n: &NewTreeNode) -> Result<String> {
        let conn = self.write.lock().unwrap();
        Ok(insert_tree_node_sql(&conn, n, now_secs())?)
    }

    /// Insert a parent summary node, freeze edges to its children, and seal the children —
    /// atomically in one transaction (spec §7). Returns the parent node id.
    pub fn seal_buffer(&self, parent: &NewTreeNode, child_ids: &[String]) -> Result<String> {
        let now = now_secs();
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let parent_id = insert_tree_node_sql(&tx, parent, now)?;
        for cid in child_ids {
            tx.execute(
                "INSERT OR IGNORE INTO tree_edges (parent_id, child_id) VALUES (?1, ?2)",
                params![parent_id, cid],
            )?;
            tx.execute(
                "UPDATE tree_nodes SET sealed=1, sealed_at=?2 WHERE node_id=?1",
                params![cid, now],
            )?;
        }
        tx.commit()?;
        Ok(parent_id)
    }

    pub fn unsealed_nodes(
        &self,
        namespace: &str,
        tree_kind: &str,
        tree_key: &str,
        level: i64,
    ) -> Result<Vec<TreeNode>> {
        let conn = self.read.get()?;
        let sql = format!(
            "SELECT {TREE_COLS} FROM tree_nodes
             WHERE namespace=?1 AND tree_kind=?2 AND tree_key=?3 AND level=?4 AND sealed=0 ORDER BY seq");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![namespace, tree_kind, tree_key, level],
            map_tree_node,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn children_of(&self, parent_id: &str) -> Result<Vec<TreeNode>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT n.node_id, n.tree_kind, n.tree_key, n.level, n.seq, n.sealed, n.label, n.body, n.doc_id, n.token_count, n.embedding, n.created_at
             FROM tree_nodes n JOIN tree_edges e ON e.child_id = n.node_id
             WHERE e.parent_id = ?1 ORDER BY n.seq")?;
        let rows = stmt.query_map(params![parent_id], map_tree_node)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_unsealed_leaves_for_doc(
        &self,
        namespace: &str,
        document_id: &str,
    ) -> Result<usize> {
        let conn = self.write.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM tree_nodes WHERE namespace=?1 AND doc_id=?2 AND level=0 AND sealed=0",
            params![namespace, document_id],
        )?;
        Ok(n)
    }

    /// Highest-level nodes of a tree (drill-down seed). With no sealed summaries yet, this
    /// returns the level-0 leaves (MAX(level)=0), so drill-down works before any consolidation.
    pub fn tree_top_nodes(
        &self,
        namespace: &str,
        tree_kind: &str,
        tree_key: &str,
    ) -> Result<Vec<TreeNode>> {
        let conn = self.read.get()?;
        let sql = format!(
            "SELECT {TREE_COLS} FROM tree_nodes
             WHERE namespace=?1 AND tree_kind=?2 AND tree_key=?3
               AND level = (SELECT COALESCE(MAX(level), 0) FROM tree_nodes
                            WHERE namespace=?1 AND tree_kind=?2 AND tree_key=?3)
             ORDER BY seq"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![namespace, tree_kind, tree_key], map_tree_node)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Buffers (namespace, tree_kind, tree_key, level) whose oldest unsealed node is at or
    /// before `older_than` — the stale-flush candidates.
    pub fn due_stale_buffers(&self, older_than: f64) -> Result<Vec<(String, String, String, i64)>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT namespace, tree_kind, tree_key, level FROM tree_nodes
             WHERE sealed=0 GROUP BY namespace, tree_kind, tree_key, level
             HAVING MIN(created_at) <= ?1",
        )?;
        let rows = stmt.query_map(params![older_than], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Test-only: backdate all unsealed nodes in a namespace, to exercise stale-flush.
    #[cfg(test)]
    pub fn touch_unsealed_created_at(&self, namespace: &str, created_at: f64) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "UPDATE tree_nodes SET created_at=?2 WHERE namespace=?1 AND sealed=0",
            params![namespace, created_at],
        )?;
        Ok(())
    }
}

fn map_tree_node(row: &rusqlite::Row) -> rusqlite::Result<TreeNode> {
    let bytes: Vec<u8> = row.get(10)?;
    Ok(TreeNode {
        node_id: row.get(0)?,
        tree_kind: row.get(1)?,
        tree_key: row.get(2)?,
        level: row.get(3)?,
        seq: row.get(4)?,
        sealed: row.get::<_, i64>(5)? != 0,
        label: row.get(6)?,
        body: row.get(7)?,
        doc_id: row.get(8)?,
        token_count: row.get(9)?,
        embedding: bytes_to_vec(&bytes),
        created_at: row.get(11)?,
    })
}

fn insert_tree_node_sql(conn: &Connection, n: &NewTreeNode, now: f64) -> rusqlite::Result<String> {
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) + 1 FROM tree_nodes
         WHERE namespace=?1 AND tree_kind=?2 AND tree_key=?3 AND level=?4",
        params![n.namespace, n.tree_kind, n.tree_key, n.level],
        |r| r.get(0),
    )?;
    let node_id = uuid::Uuid::new_v4().to_string();
    let bytes = vec_to_bytes(n.embedding);
    conn.execute(
        "INSERT INTO tree_nodes
           (node_id, namespace, tree_kind, tree_key, level, seq, sealed, label, body, doc_id,
            token_count, embedding, model_signature, created_at, sealed_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14, NULL)",
        params![
            node_id,
            n.namespace,
            n.tree_kind,
            n.tree_key,
            n.level,
            seq,
            n.sealed as i64,
            n.label,
            n.body,
            n.doc_id,
            n.token_count,
            bytes,
            n.model_signature,
            now
        ],
    )?;
    Ok(node_id)
}
