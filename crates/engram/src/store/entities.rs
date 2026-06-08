use super::Store;
use crate::error::Result;
use rusqlite::params;
use std::collections::HashMap;

impl Store {
    pub fn record_entities(
        &self,
        namespace: &str,
        chunk_id: &str,
        document_id: &str,
        entity_ids: &[String],
    ) -> Result<()> {
        let conn = self.write.lock().unwrap();
        for eid in entity_ids {
            conn.execute(
                "INSERT OR IGNORE INTO chunk_entities (namespace, chunk_id, document_id, entity_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![namespace, chunk_id, document_id, eid],
            )?;
        }
        Ok(())
    }

    /// For each document in `namespace`, how many of `entity_ids` it mentions
    /// (across its chunks). Documents with zero overlap are omitted.
    pub fn docs_with_entities(
        &self,
        namespace: &str,
        entity_ids: &[String],
    ) -> Result<HashMap<String, i64>> {
        let mut out = HashMap::new();
        if entity_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = (0..entity_ids.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT document_id, COUNT(DISTINCT entity_id) AS n FROM chunk_entities
             WHERE namespace=?1 AND entity_id IN ({placeholders})
             GROUP BY document_id"
        );
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(&sql)?;
        let ns = namespace;
        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(entity_ids.len() + 1);
        binds.push(&ns);
        for e in entity_ids {
            binds.push(e);
        }
        let rows = stmt.query_map(binds.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (doc, n) = r?;
            out.insert(doc, n);
        }
        Ok(out)
    }
}
