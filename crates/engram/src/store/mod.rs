use crate::error::Result;
use crate::model::{MemoryDoc, NewDoc, RawEdge, Taint};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

mod chunks;
mod docs;
mod edges;
mod entities;
mod jobs;
mod trees;

pub use jobs::Job;
pub use trees::{NewTreeNode, TreeNode};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_docs (
    document_id TEXT PRIMARY KEY,
    namespace   TEXT NOT NULL,
    key         TEXT NOT NULL,
    title       TEXT NOT NULL,
    content     TEXT NOT NULL,
    author      TEXT NOT NULL,
    taint       TEXT NOT NULL DEFAULT 'internal',
    created_at  REAL NOT NULL,
    updated_at  REAL NOT NULL,
    meta        TEXT,
    UNIQUE(namespace, key)
);
CREATE INDEX IF NOT EXISTS idx_docs_ns_updated
    ON memory_docs(namespace, updated_at DESC);
CREATE TABLE IF NOT EXISTS vector_chunks (
    namespace       TEXT NOT NULL,
    document_id     TEXT NOT NULL,
    chunk_id        TEXT NOT NULL,
    text            TEXT NOT NULL,
    embedding       BLOB NOT NULL,
    model_signature TEXT NOT NULL,
    dim             INTEGER NOT NULL,
    created_at      REAL NOT NULL,
    line_start      INTEGER,
    line_end        INTEGER,
    PRIMARY KEY(namespace, chunk_id)
);
CREATE INDEX IF NOT EXISTS idx_vchunks_ns_doc ON vector_chunks(namespace, document_id);
CREATE TABLE IF NOT EXISTS chunk_entities (
    namespace   TEXT NOT NULL,
    chunk_id    TEXT NOT NULL,
    document_id TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    PRIMARY KEY(namespace, chunk_id, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_chunk_entities_entity ON chunk_entities(namespace, entity_id);
CREATE TABLE IF NOT EXISTS post_acquire_jobs (
    job_id       TEXT PRIMARY KEY,
    namespace    TEXT NOT NULL,
    document_id  TEXT NOT NULL,
    status       TEXT NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    created_at   REAL NOT NULL,
    updated_at   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_jobs_claim ON post_acquire_jobs(status, created_at);
CREATE TABLE IF NOT EXISTS tree_nodes (
    node_id         TEXT PRIMARY KEY,
    namespace       TEXT NOT NULL,
    tree_kind       TEXT NOT NULL,
    tree_key        TEXT NOT NULL,
    level           INTEGER NOT NULL,
    seq             INTEGER NOT NULL,
    sealed          INTEGER NOT NULL DEFAULT 0,
    label           TEXT,
    body            TEXT NOT NULL,
    doc_id          TEXT,
    token_count     INTEGER NOT NULL DEFAULT 0,
    embedding       BLOB,
    model_signature TEXT,
    created_at      REAL NOT NULL,
    sealed_at       REAL
);
CREATE INDEX IF NOT EXISTS idx_tree_buffer ON tree_nodes(namespace, tree_kind, tree_key, level, sealed, seq);
CREATE INDEX IF NOT EXISTS idx_tree_doc ON tree_nodes(namespace, tree_kind, tree_key, doc_id);
CREATE TABLE IF NOT EXISTS tree_edges (
    parent_id TEXT NOT NULL,
    child_id  TEXT NOT NULL,
    PRIMARY KEY (parent_id, child_id)
);
CREATE INDEX IF NOT EXISTS idx_tree_edges_parent ON tree_edges(parent_id);
CREATE TABLE IF NOT EXISTS code_edges (
    namespace   TEXT NOT NULL,
    src_doc_id  TEXT NOT NULL,
    dst_sym     TEXT NOT NULL,
    dst_doc_id  TEXT,
    edge_kind   TEXT NOT NULL,
    src_line    INTEGER,
    confidence  REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (namespace, src_doc_id, dst_sym, edge_kind)
);
CREATE INDEX IF NOT EXISTS idx_code_edges_src
    ON code_edges(namespace, src_doc_id);
CREATE INDEX IF NOT EXISTS idx_code_edges_dst_sym
    ON code_edges(namespace, dst_sym);
";

#[derive(Clone)]
pub struct Store {
    read: r2d2::Pool<SqliteConnectionManager>,
    write: Arc<Mutex<Connection>>,
}

fn migrate(conn: &Connection) -> Result<()> {
    // --- migration v1: add line columns to vector_chunks ---
    let mut stmt = conn.prepare("PRAGMA table_info(vector_chunks)")?;
    let col_names: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if !col_names.iter().any(|c| c == "line_start") {
        conn.execute_batch("ALTER TABLE vector_chunks ADD COLUMN line_start INTEGER;")?;
    }
    if !col_names.iter().any(|c| c == "line_end") {
        conn.execute_batch("ALTER TABLE vector_chunks ADD COLUMN line_end INTEGER;")?;
    }

    // --- migration v2: add code_edges table + indexes (IF NOT EXISTS = safe on fresh DBs) ---
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS code_edges (
            namespace   TEXT NOT NULL,
            src_doc_id  TEXT NOT NULL,
            dst_sym     TEXT NOT NULL,
            dst_doc_id  TEXT,
            edge_kind   TEXT NOT NULL,
            src_line    INTEGER,
            confidence  REAL NOT NULL DEFAULT 1.0,
            PRIMARY KEY (namespace, src_doc_id, dst_sym, edge_kind)
        );
        CREATE INDEX IF NOT EXISTS idx_code_edges_src
            ON code_edges(namespace, src_doc_id);
        CREATE INDEX IF NOT EXISTS idx_code_edges_dst_sym
            ON code_edges(namespace, dst_sym);",
    )?;

    conn.execute_batch("PRAGMA user_version = 2;")?;
    Ok(())
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let write = Connection::open(path)?;
        write.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=15000;",
        )?;
        write.execute_batch(SCHEMA)?;
        migrate(&write)?;

        let manager = SqliteConnectionManager::file(path)
            .with_init(|c| c.execute_batch("PRAGMA busy_timeout=15000;"));
        let read = r2d2::Pool::builder().max_size(8).build(manager)?;

        Ok(Store {
            read,
            write: Arc::new(Mutex::new(write)),
        })
    }

    /// Atomic hot-ingest: upsert the doc, replace its chunks + entities, and enqueue the
    /// post-acquire job — all in one transaction. Embeddings are computed off-lock by the
    /// caller (`ingest::ingest_document`). Fixes N-1 (partial chunks) and N-2 (orphan doc row).
    #[allow(clippy::too_many_arguments)]
    pub fn commit_ingest(
        &self,
        namespace: &str,
        new: &NewDoc,
        chunk_texts: &[String],
        embeddings: &[Vec<f32>],
        entities: &[Vec<String>],
        line_ranges: &[Option<(i64, i64)>],
        raw_edges: &[RawEdge],
        signature: &str,
    ) -> Result<MemoryDoc> {
        let now = now_secs();
        let new_id = uuid::Uuid::new_v4().to_string();
        {
            let mut conn = self.write.lock().unwrap();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO memory_docs
                   (document_id, namespace, key, title, content, author, taint, created_at, updated_at, meta)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
                 ON CONFLICT(namespace, key) DO UPDATE SET
                   title=excluded.title, content=excluded.content, author=excluded.author,
                   taint=excluded.taint, updated_at=excluded.updated_at, meta=excluded.meta",
                params![new_id, namespace, new.key, new.title, new.content, new.author,
                        taint_str(new.taint), now, new.meta.as_ref().map(|v| v.to_string())],
            )?;
            let doc_id: String = tx.query_row(
                "SELECT document_id FROM memory_docs WHERE namespace=?1 AND key=?2",
                params![namespace, new.key],
                |r| r.get(0),
            )?;
            tx.execute(
                "DELETE FROM vector_chunks WHERE namespace=?1 AND document_id=?2",
                params![namespace, doc_id],
            )?;
            tx.execute(
                "DELETE FROM chunk_entities WHERE namespace=?1 AND document_id=?2",
                params![namespace, doc_id],
            )?;
            for (seq, text) in chunk_texts.iter().enumerate() {
                let chunk_id = format!("{doc_id}#{seq}");
                let bytes = vec_to_bytes(&embeddings[seq]);
                let dim = embeddings[seq].len() as i64;
                let (ls, le) = match line_ranges[seq] {
                    Some((a, b)) => (Some(a), Some(b)),
                    None => (None, None),
                };
                tx.execute(
                    "INSERT INTO vector_chunks
                       (namespace, document_id, chunk_id, text, embedding, model_signature, dim, created_at, line_start, line_end)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![namespace, doc_id, chunk_id, text, bytes, signature, dim, now, ls, le],
                )?;
                for eid in &entities[seq] {
                    tx.execute(
                        "INSERT OR IGNORE INTO chunk_entities (namespace, chunk_id, document_id, entity_id)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![namespace, chunk_id, doc_id, eid],
                    )?;
                }
            }
            // Replace all code edges for this doc, inside the same tx (mirrors the chunk/entity
            // DELETE+INSERT pattern). dst_doc_id is left NULL; resolved lazily at query time.
            edges::insert_edges_in_tx(&tx, namespace, &doc_id, raw_edges)?;
            enqueue_job_sql(&tx, namespace, &doc_id, now)?;
            tx.commit()?;
        }
        self.get_by_key(namespace, &new.key)?
            .ok_or(crate::error::Error::NotFound)
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn enqueue_job_sql(
    conn: &Connection,
    namespace: &str,
    document_id: &str,
    now: f64,
) -> rusqlite::Result<()> {
    let job_id = format!("{namespace}:{document_id}");
    conn.execute(
        "INSERT INTO post_acquire_jobs (job_id, namespace, document_id, status, attempts, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4)
         ON CONFLICT(job_id) DO UPDATE SET status='pending', attempts=0, last_error=NULL, updated_at=?4",
        params![job_id, namespace, document_id, now],
    )?;
    Ok(())
}

fn taint_str(t: Taint) -> &'static str {
    match t {
        Taint::Internal => "internal",
        Taint::ExternalSync => "external_sync",
    }
}

pub(crate) fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for &f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

pub(crate) fn bytes_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..a.len() {
        dot += a[i] as f64 * b[i] as f64;
        na += (a[i] as f64) * (a[i] as f64);
        nb += (b[i] as f64) * (b[i] as f64);
    }
    if na <= f64::EPSILON || nb <= f64::EPSILON {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open(path.to_str().unwrap()).unwrap();
        (store, dir)
    }

    #[test]
    fn open_sets_wal_and_schema() {
        let (store, _d) = temp_store();
        let conn = store.read.get().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_docs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn code_edges_table_and_indexes_exist() {
        let (store, _d) = temp_store();
        let conn = store.read.get().unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='code_edges'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "code_edges table missing");

        let si: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_code_edges_src'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(si, 1, "idx_code_edges_src missing");

        let di: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_code_edges_dst_sym'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(di, 1, "idx_code_edges_dst_sym missing");

        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 2, "user_version should be 2 after migration");
    }

    #[test]
    fn insert_then_get() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "k1".into(),
            title: "Title".into(),
            content: "Hello".into(),
            author: "alice".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let saved = store.insert_doc("alice", &new).unwrap();
        assert_eq!(saved.namespace, "alice");
        assert_eq!(saved.key, "k1");
        assert!(saved.created_at > 0.0);

        let got = store.get_doc("alice", &saved.document_id).unwrap().unwrap();
        assert_eq!(got.content, "Hello");

        // upsert on (namespace,key): same row, content updated
        let mut n2 = new.clone();
        n2.content = "Updated".into();
        let saved2 = store.insert_doc("alice", &n2).unwrap();
        assert_eq!(saved2.document_id, saved.document_id);
        assert_eq!(saved2.content, "Updated");
    }

    #[test]
    fn list_is_namespace_scoped() {
        let (store, _d) = temp_store();
        let mk = |k: &str| NewDoc {
            key: k.into(),
            title: "t".into(),
            content: "c".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        store.insert_doc("alice", &mk("a1")).unwrap();
        store.insert_doc("alice", &mk("a2")).unwrap();
        store.insert_doc("bob", &mk("b1")).unwrap();

        let alice = store.list_namespace("alice", 100).unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().all(|d| d.namespace == "alice"));

        let bob = store.list_namespace("bob", 100).unwrap();
        assert_eq!(bob.len(), 1);
    }

    #[test]
    fn vec_bytes_roundtrip() {
        let v = vec![0.0f32, 1.5, -2.25, 3.0];
        assert_eq!(super::bytes_to_vec(&super::vec_to_bytes(&v)), v);
    }

    #[test]
    fn cosine_basic() {
        assert!((super::cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(super::cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert_eq!(super::cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn docs_with_entities_counts() {
        let (store, _d) = temp_store();
        store
            .record_entities(
                "alice",
                "d1#0",
                "d1",
                &["handle:bob".into(), "hashtag:x".into()],
            )
            .unwrap();
        store
            .record_entities("alice", "d2#0", "d2", &["handle:bob".into()])
            .unwrap();
        store
            .record_entities("alice", "d3#0", "d3", &["hashtag:other".into()])
            .unwrap();
        let counts = store
            .docs_with_entities("alice", &["handle:bob".into(), "hashtag:x".into()])
            .unwrap();
        assert_eq!(counts.get("d1"), Some(&2));
        assert_eq!(counts.get("d2"), Some(&1));
        assert_eq!(counts.get("d3"), None);
        assert!(store.docs_with_entities("alice", &[]).unwrap().is_empty());
    }

    #[test]
    fn delete_and_record_entities() {
        let (store, _d) = temp_store();
        store
            .upsert_chunk("alice", "doc1", "doc1#0", "hi", &[0.1, 0.2], "hash:2")
            .unwrap();
        store
            .record_entities(
                "alice",
                "doc1#0",
                "doc1",
                &["email:a@b.com".into(), "handle:bob".into()],
            )
            .unwrap();
        store.delete_chunks_for_doc("alice", "doc1").unwrap();
        assert_eq!(
            store.chunks_for_namespace("alice", "hash:2").unwrap().len(),
            0
        );
        store
            .upsert_chunk("alice", "doc1", "doc1#0", "hi2", &[0.3, 0.4], "hash:2")
            .unwrap();
        store
            .record_entities("alice", "doc1#0", "doc1", &["handle:bob".into()])
            .unwrap();
        store
            .record_entities("alice", "doc1#0", "doc1", &["handle:bob".into()])
            .unwrap(); // idempotent
    }

    #[test]
    fn chunk_upsert_and_fetch() {
        let (store, _d) = temp_store();
        store
            .upsert_chunk("alice", "doc1", "doc1", "hello", &[0.1, 0.2, 0.3], "hash:3")
            .unwrap();
        store
            .upsert_chunk("alice", "doc2", "doc2", "world", &[0.4, 0.5, 0.6], "hash:3")
            .unwrap();
        store
            .upsert_chunk("bob", "doc3", "doc3", "other", &[0.7, 0.8, 0.9], "hash:3")
            .unwrap();

        let alice = store.chunks_for_namespace("alice", "hash:3").unwrap();
        assert_eq!(alice.len(), 2);

        let none = store
            .chunks_for_namespace("alice", "ollama:bge-m3:1024")
            .unwrap();
        assert_eq!(none.len(), 0);

        store
            .upsert_chunk(
                "alice",
                "doc1",
                "doc1",
                "hello again",
                &[1.0, 0.0, 0.0],
                "hash:3",
            )
            .unwrap();
        let alice2 = store.chunks_for_namespace("alice", "hash:3").unwrap();
        assert_eq!(alice2.len(), 2);
        let d1 = alice2.iter().find(|(id, _, _)| id == "doc1").unwrap();
        assert_eq!(d1.1, "hello again");
        assert_eq!(d1.2, vec![1.0, 0.0, 0.0]);
    }

    #[test]
    fn commit_ingest_is_atomic_and_enqueues() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "ignored-by-store".into(),
            author: "alice".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let chunks = vec!["chunk one @bob".to_string(), "chunk two".to_string()];
        let embs = vec![vec![1.0f32, 0.0], vec![0.0f32, 1.0]];
        let ents = vec![vec!["handle:bob".to_string()], vec![]];
        let edges = vec![crate::model::RawEdge {
            dst_sym: "sym:Foo".to_string(),
            edge_kind: crate::model::EdgeKind::Calls,
            src_line: Some(10),
            confidence: 0.9,
        }];

        let doc = store
            .commit_ingest(
                "alice",
                &new,
                &chunks,
                &embs,
                &ents,
                &[None, None],
                &edges,
                "sig",
            )
            .unwrap();
        let got = store.chunks_for_namespace("alice", "sig").unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(d, _, _)| d == &doc.document_id));
        assert_eq!(
            store
                .docs_with_entities("alice", &["handle:bob".into()])
                .unwrap()
                .get(&doc.document_id)
                .copied(),
            Some(1)
        );
        assert_eq!(store.pending_jobs().unwrap(), 1);
        assert_eq!(
            store
                .job(&format!("alice:{}", doc.document_id))
                .unwrap()
                .unwrap()
                .0,
            "pending"
        );

        // Verify edges were written
        {
            let conn = store.read.get().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "expected 1 edge after first ingest");
            let (dst_sym, kind, src_line, conf): (String, String, Option<i64>, f64) = conn
                .query_row(
                    "SELECT dst_sym, edge_kind, src_line, confidence
                     FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(dst_sym, "sym:Foo");
            assert_eq!(kind, "CALLS");
            assert_eq!(src_line, Some(10));
            assert!((conf - 0.9).abs() < 1e-6);
        }

        // re-ingest same key with DIFFERENT edges: replaces chunks, replaces edges, re-enqueues
        let new_edges = vec![crate::model::RawEdge {
            dst_sym: "sym:Bar".to_string(),
            edge_kind: crate::model::EdgeKind::UsesType,
            src_line: None,
            confidence: 1.0,
        }];
        let doc2 = store
            .commit_ingest(
                "alice",
                &new,
                &["only".to_string()],
                &[vec![1.0f32, 1.0]],
                &[vec![]],
                &[None],
                &new_edges,
                "sig",
            )
            .unwrap();
        assert_eq!(doc2.document_id, doc.document_id);
        assert_eq!(store.chunks_for_namespace("alice", "sig").unwrap().len(), 1);
        assert!(store
            .docs_with_entities("alice", &["handle:bob".into()])
            .unwrap()
            .is_empty());

        // old edge (sym:Foo) gone, new edge (sym:Bar) present — idempotent replace
        {
            let conn = store.read.get().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "expected exactly 1 edge after re-ingest");
            let dst: String = conn
                .query_row(
                    "SELECT dst_sym FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(dst, "sym:Bar", "old edge must be replaced by new edge");
        }
    }

    #[test]
    fn edges_read_methods_seed_and_roundtrip() {
        use crate::model::{EdgeKind, RawEdge};

        let (store, _d) = temp_store();

        let mk = |key: &str| NewDoc {
            key: key.into(),
            title: key.into(),
            content: "x".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let src_doc = store
            .commit_ingest(
                "ns",
                &mk("src.rs"),
                &["fn foo() { bar(); }".into()],
                &[vec![1.0f32]],
                &[vec!["sym:foo".into()]],
                &[Some((1, 3))],
                &[
                    RawEdge {
                        dst_sym: "sym:bar".into(),
                        edge_kind: EdgeKind::Calls,
                        src_line: Some(1),
                        confidence: 1.0,
                    },
                    RawEdge {
                        dst_sym: "sym:Baz".into(),
                        edge_kind: EdgeKind::UsesType,
                        src_line: Some(2),
                        confidence: 0.8,
                    },
                ],
                "sig",
            )
            .unwrap();
        let _dst_doc = store
            .commit_ingest(
                "ns",
                &mk("bar.rs"),
                &["fn bar() {}".into()],
                &[vec![0.5f32]],
                &[vec!["sym:bar".into()]],
                &[Some((1, 1))],
                &[],
                "sig",
            )
            .unwrap();

        let from = store.edges_from("ns", &src_doc.document_id).unwrap();
        assert_eq!(from.len(), 2);
        let calls_edge = from
            .iter()
            .find(|e| e.edge_kind == EdgeKind::Calls)
            .unwrap();
        assert_eq!(calls_edge.dst_sym, "sym:bar");
        assert_eq!(calls_edge.src_line, Some(1));
        assert!((calls_edge.confidence - 1.0).abs() < 1e-6);
        let type_edge = from
            .iter()
            .find(|e| e.edge_kind == EdgeKind::UsesType)
            .unwrap();
        assert_eq!(type_edge.dst_sym, "sym:Baz");
        assert!((type_edge.confidence - 0.8).abs() < 1e-5);

        assert!(store
            .edges_from("other", &src_doc.document_id)
            .unwrap()
            .is_empty());

        let to = store.edges_to_sym("ns", "sym:bar").unwrap();
        assert_eq!(to.len(), 1);
        assert_eq!(to[0].0, src_doc.document_id);
        assert_eq!(to[0].1.dst_sym, "sym:bar");
        assert_eq!(to[0].1.edge_kind, EdgeKind::Calls);

        let resolved = store.resolve_dst_doc("ns", "sym:bar").unwrap();
        assert!(
            resolved.is_some(),
            "sym:bar should resolve to bar.rs's doc_id"
        );

        let unresolved = store.resolve_dst_doc("ns", "sym:Baz").unwrap();
        assert!(unresolved.is_none());
    }

    #[test]
    fn sym_entities_for_doc_returns_only_sym_entities() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "src/lib.rs".into(),
            title: "lib".into(),
            content: "fn foo() {}".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let doc = store
            .commit_ingest(
                "ns",
                &new,
                &["fn foo() {}".into()],
                &[vec![1.0f32]],
                &[vec![
                    "sym:foo".into(),
                    "sym:Bar".into(),
                    "path:src/lib.rs".into(),
                    "import:std".into(),
                ]],
                &[Some((1, 1))],
                &[],
                "sig",
            )
            .unwrap();
        let mut syms = store.sym_entities_for_doc("ns", &doc.document_id).unwrap();
        syms.sort();
        assert_eq!(syms, vec!["sym:Bar".to_string(), "sym:foo".to_string()]);
        // wrong namespace → empty
        assert!(store
            .sym_entities_for_doc("other", &doc.document_id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn job_lifecycle() {
        let (store, _d) = temp_store();
        store.enqueue_job("alice", "doc1").unwrap();
        store.enqueue_job("alice", "doc1").unwrap(); // idempotent
        assert_eq!(store.pending_jobs().unwrap(), 1);

        let j = store.claim_job().unwrap().unwrap();
        assert_eq!(j.document_id, "doc1");
        assert_eq!(j.attempts, 1); // incremented at claim
        assert_eq!(store.pending_jobs().unwrap(), 0);
        assert!(store.claim_job().unwrap().is_none()); // none left pending

        store.complete_job("alice:doc1").unwrap();
        assert_eq!(store.job("alice:doc1").unwrap().unwrap().0, "done");
    }

    #[test]
    fn top_nodes_and_stale_buffers() {
        let (store, _d) = temp_store();
        let emb = vec![1.0f32];
        let n = NewTreeNode {
            namespace: "alice",
            tree_kind: "topic",
            tree_key: "t",
            level: 0,
            sealed: false,
            label: None,
            body: "leaf",
            doc_id: Some("d"),
            token_count: 1,
            embedding: &emb,
            model_signature: "s",
        };
        store.append_leaf_node(&n).unwrap();

        let top = store.tree_top_nodes("alice", "topic", "t").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].level, 0); // only leaves → top level is 0

        assert!(store.due_stale_buffers(1000.0).unwrap().is_empty()); // created ~now, far newer than 1000
        store.touch_unsealed_created_at("alice", 0.0).unwrap(); // backdate to epoch
        let due = store.due_stale_buffers(1.0).unwrap();
        assert_eq!(due, vec![("alice".into(), "topic".into(), "t".into(), 0)]);
    }

    #[test]
    fn tree_leaf_seal_and_children() {
        let (store, _d) = temp_store();
        let emb = vec![1.0f32, 0.0];
        let n1 = NewTreeNode {
            namespace: "alice",
            tree_kind: "global",
            tree_key: "global",
            level: 0,
            sealed: false,
            label: None,
            body: "leaf one",
            doc_id: Some("d1"),
            token_count: 2,
            embedding: &emb,
            model_signature: "sig",
        };
        let id1 = store.append_leaf_node(&n1).unwrap();
        let n2 = NewTreeNode {
            namespace: "alice",
            tree_kind: "global",
            tree_key: "global",
            level: 0,
            sealed: false,
            label: None,
            body: "leaf two",
            doc_id: Some("d2"),
            token_count: 2,
            embedding: &emb,
            model_signature: "sig",
        };
        let id2 = store.append_leaf_node(&n2).unwrap();

        let buf = store
            .unsealed_nodes("alice", "global", "global", 0)
            .unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!((buf[0].seq, buf[1].seq), (0, 1));

        let pemb = vec![0.5f32, 0.5];
        let parent = NewTreeNode {
            namespace: "alice",
            tree_kind: "global",
            tree_key: "global",
            level: 1,
            sealed: false,
            label: Some("sum"),
            body: "summary",
            doc_id: None,
            token_count: 1,
            embedding: &pemb,
            model_signature: "sig",
        };
        let pid = store.seal_buffer(&parent, &[id1, id2]).unwrap();
        assert!(store
            .unsealed_nodes("alice", "global", "global", 0)
            .unwrap()
            .is_empty()); // children sealed
        let l1 = store
            .unsealed_nodes("alice", "global", "global", 1)
            .unwrap();
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0].node_id, pid);
        assert_eq!(store.children_of(&pid).unwrap().len(), 2);
    }

    #[test]
    fn delete_unsealed_leaves_only() {
        let (store, _d) = temp_store();
        let emb = vec![1.0f32];
        let n = NewTreeNode {
            namespace: "alice",
            tree_kind: "topic",
            tree_key: "t",
            level: 0,
            sealed: false,
            label: None,
            body: "x",
            doc_id: Some("d9"),
            token_count: 1,
            embedding: &emb,
            model_signature: "s",
        };
        store.append_leaf_node(&n).unwrap();
        assert_eq!(
            store.delete_unsealed_leaves_for_doc("alice", "d9").unwrap(),
            1
        );
        assert!(store
            .unsealed_nodes("alice", "topic", "t", 0)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn chunks_for_doc_returns_text_emb_entities() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "x".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let doc = store
            .commit_ingest(
                "alice",
                &new,
                &["chunk @bob".into()],
                &[vec![1.0f32, 2.0]],
                &[vec!["handle:bob".into()]],
                &[None],
                &[],
                "sig",
            )
            .unwrap();
        let rows = store.chunks_for_doc("alice", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "chunk @bob");
        assert_eq!(rows[0].embedding, vec![1.0f32, 2.0]);
        assert_eq!(rows[0].entities, vec!["handle:bob".to_string()]);
    }

    #[test]
    fn job_fail_then_retry_then_failed() {
        let (store, _d) = temp_store();
        store.enqueue_job("alice", "d").unwrap();
        store.claim_job().unwrap(); // attempts=1
        store.fail_or_retry_job("alice:d", "boom", 2).unwrap(); // 1<2 → pending
        assert_eq!(
            store.job("alice:d").unwrap().unwrap(),
            ("pending".into(), 1)
        );
        store.claim_job().unwrap(); // attempts=2
        store.fail_or_retry_job("alice:d", "boom", 2).unwrap(); // 2>=2 → failed
        assert_eq!(store.job("alice:d").unwrap().unwrap(), ("failed".into(), 2));
    }

    #[test]
    fn requeue_running_resets() {
        let (store, _d) = temp_store();
        store.enqueue_job("alice", "d").unwrap();
        store.claim_job().unwrap(); // → running
        assert_eq!(store.requeue_running().unwrap(), 1);
        assert_eq!(store.job("alice:d").unwrap().unwrap().0, "pending");
    }

    #[test]
    fn delete_doc_by_key_removes_doc_and_chunks() {
        use crate::model::{EdgeKind, RawEdge};

        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "ping @bob".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        // Ingest a "callee" doc first so we can test dst_doc_id-side deletion too
        let callee = store
            .commit_ingest(
                "alice",
                &NewDoc {
                    key: "callee.rs".into(),
                    title: "callee.rs".into(),
                    content: "fn target() {}".into(),
                    author: "a".into(),
                    taint: Taint::Internal,
                    meta: None,
                },
                &["fn target() {}".into()],
                &[vec![0.5f32]],
                &[vec!["sym:target".into()]],
                &[Some((1, 1))],
                &[],
                "sig",
            )
            .unwrap();

        let doc = store
            .commit_ingest(
                "alice",
                &new,
                &["ping @bob".into()],
                &[vec![1.0f32]],
                &[vec!["handle:bob".into()]],
                &[None],
                &[RawEdge {
                    dst_sym: "sym:target".into(),
                    edge_kind: EdgeKind::Calls,
                    src_line: Some(1),
                    confidence: 1.0,
                }],
                "sig",
            )
            .unwrap();

        // Manually insert an INBOUND ghost edge: callee.rs references the doc we are about to
        // delete (dst_sym names one of its symbols) but with dst_doc_id NULL — exactly the
        // state production creates (cross-file resolution is lazy and never writes dst_doc_id
        // back). This edge must SURVIVE the delete (the dst_doc_id DELETE matches nothing), so
        // it becomes an unresolvable ghost edge by design.
        {
            let conn = store.write.lock().unwrap();
            conn.execute(
                "INSERT INTO code_edges (namespace, src_doc_id, dst_sym, dst_doc_id, edge_kind, src_line, confidence)
                 VALUES ('alice', ?1, 'sym:ping', NULL, 'CALLS', 5, 1.0)",
                rusqlite::params![callee.document_id],
            )
            .unwrap();
        }

        assert!(store.get_by_key("alice", "k").unwrap().is_some());
        assert_eq!(store.chunks_for_namespace("alice", "sig").unwrap().len(), 2); // doc + callee

        {
            let conn = store.read.get().unwrap();
            let src_count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(src_count, 1, "should have 1 outbound edge before delete");
        }

        let removed = store.delete_doc_by_key("alice", "k").unwrap();
        assert!(removed);
        assert!(store.get_by_key("alice", "k").unwrap().is_none());
        assert_eq!(store.chunks_for_namespace("alice", "sig").unwrap().len(), 1); // only callee remains
        assert!(store
            .docs_with_entities("alice", &["handle:bob".into()])
            .unwrap()
            .is_empty());

        {
            let conn = store.read.get().unwrap();
            let src_count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM code_edges WHERE namespace='alice' AND src_doc_id=?1",
                    rusqlite::params![doc.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(src_count, 0, "outbound edges must be deleted with the doc");

            // The inbound NULL-dst_doc_id ghost edge from callee.rs SURVIVES: dst_doc_id is
            // never written in production, so the dst_doc_id DELETE matches nothing and inbound
            // references to a forgotten doc become unresolvable ghost edges by design.
            let ghost_count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM code_edges
                     WHERE namespace='alice' AND src_doc_id=?1 AND dst_sym='sym:ping'",
                    rusqlite::params![callee.document_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                ghost_count, 1,
                "inbound ghost edge (dst_doc_id NULL) survives deletion by design"
            );
        }

        assert!(!store.delete_doc_by_key("alice", "k").unwrap());
    }

    #[test]
    fn commit_ingest_persists_line_ranges() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "f".into(),
            title: "t".into(),
            content: "x".into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let doc = store
            .commit_ingest(
                "ns",
                &new,
                &["line one\nline two".to_string(), "tail".to_string()],
                &[vec![1.0f32, 0.0], vec![0.0f32, 1.0]],
                &[vec![], vec![]],
                &[Some((1, 2)), None],
                &[],
                "sig",
            )
            .unwrap();
        let rows = store.chunks_for_doc("ns", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 2);
        let first = rows
            .iter()
            .find(|r| r.text == "line one\nline two")
            .unwrap();
        assert_eq!((first.line_start, first.line_end), (Some(1), Some(2)));
        let second = rows.iter().find(|r| r.text == "tail").unwrap();
        assert_eq!((second.line_start, second.line_end), (None, None));
    }

    #[test]
    fn migrate_adds_line_columns_to_old_schema() {
        use crate::embed::HashEmbedder;
        use crate::ingest::ingest_document;
        // 1. Build an "old" DB with vector_chunks missing line_start / line_end.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE vector_chunks (
                    namespace       TEXT NOT NULL,
                    document_id     TEXT NOT NULL,
                    chunk_id        TEXT NOT NULL,
                    text            TEXT NOT NULL,
                    embedding       BLOB NOT NULL,
                    model_signature TEXT NOT NULL,
                    dim             INTEGER NOT NULL,
                    created_at      REAL NOT NULL,
                    PRIMARY KEY(namespace, chunk_id)
                );",
            )
            .unwrap();
            // conn drops here, closing the file
        }
        // 2. Open via Store::open — migrate() must add the missing columns.
        let store = Store::open(path.to_str().unwrap()).unwrap();
        // 3. Verify the columns now exist via PRAGMA table_info.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let mut stmt = conn.prepare("PRAGMA table_info(vector_chunks)").unwrap();
            let col_names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert!(
                col_names.contains(&"line_start".to_string()),
                "line_start missing; got: {col_names:?}"
            );
            assert!(
                col_names.contains(&"line_end".to_string()),
                "line_end missing; got: {col_names:?}"
            );
        }
        // 4. End-to-end ingest on the migrated DB must succeed.
        let embedder = HashEmbedder::new(64);
        let new = NewDoc {
            key: "file.rs".into(),
            title: "file.rs".into(),
            content: "fn first_line() {}\nfn second_line() {}\n".into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        ingest_document(&store, &embedder, "ns", &new).unwrap();
    }

    #[test]
    fn meta_persists_through_commit_ingest_and_reads() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "hello".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"category":"fact","importance":0.8})),
        };
        let doc = store
            .commit_ingest(
                "alice",
                &new,
                &["hello".into()],
                &[vec![1.0f32, 0.0]],
                &[vec![]],
                &[None],
                &[],
                "sig",
            )
            .unwrap();
        assert_eq!(doc.meta.as_ref().unwrap()["category"], "fact");
        let got = store.get_doc("alice", &doc.document_id).unwrap().unwrap();
        assert_eq!(got.meta.as_ref().unwrap()["importance"], 0.8);
        let by_key = store.get_by_key("alice", "k").unwrap().unwrap();
        assert_eq!(by_key.meta.as_ref().unwrap()["category"], "fact");
    }
}
