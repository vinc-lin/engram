# Phase 1a — Core Code Ingest & Search — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `engram` core crate able to ingest code files (with per-chunk line ranges + code entities) and answer chunk-level code search (`path:line` + snippet), all behind the existing HTTP/library API — the engine half of "searchable code".

**Architecture:** Phase 1a of `docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`, building on the merged Phase 0 workspace + `store/` split. `ingest_document` gains a branch on the incoming doc's `meta.kind` (`"file"` → code-mode; else today's prose path). Code-mode uses a new line-aware `chunk_code` + `extract_code_entities`; chunks persist nullable `line_start`/`line_end` (the one core schema change). A new chunk-level `search_code` returns matching chunk text + `path:line`, distinct from the existing doc-level `query`. The `engram-index` CLI (Phase 1b) and `engram-mcp` server (Phase 1c) are deferred to their own plans.

**Tech Stack:** Rust 2021, rusqlite (bundled SQLite), regex, serde. Tests use the in-crate `HashEmbedder` + `tempfile`.

**Baseline:** master HEAD `28e3c0e`; `cargo test -p engram` = 59 passed, 3 ignored. This plan ADDS tests, so the count only grows — after each task, **all** tests (existing + new) must pass and `RUSTFLAGS="-D warnings" cargo build -p engram` must be clean.

---

## File structure

All changes are inside `crates/engram/`:

| File | Change | Responsibility added |
|------|--------|----------------------|
| `src/store/mod.rs` | modify | `vector_chunks` gains `line_start`/`line_end`; `commit_ingest` threads line ranges; 3 store tests updated for the new signature |
| `src/store/chunks.rs` | modify | `ChunkRow` gains line fields; `chunks_for_doc` reads them; new `CodeChunkRow` + `code_chunks_for_namespace` (joins `memory_docs` for the file path) |
| `src/ingest.rs` | modify | new `chunk_code` (line-aware), new `extract_code_entities` (`sym:`/`import:`), `ingest_document` dispatch on `meta.kind` |
| `src/retrieve.rs` | modify | new `CodeHit` + `search_code` (chunk-level, `path:line`) |

No new files; no new crates (the clients are later phases). No new tables (two nullable columns only).

---

## Task 1: Persist chunk line ranges (schema + ChunkRow + commit_ingest)

Add nullable `line_start`/`line_end` to `vector_chunks` and thread them through `commit_ingest` and `chunks_for_doc`. Existing prose chunks store `NULL` (no backfill, no forced re-index).

**Files:**
- Modify: `crates/engram/src/store/mod.rs` (SCHEMA, `commit_ingest`, tests)
- Modify: `crates/engram/src/store/chunks.rs` (`ChunkRow`, `chunks_for_doc`)
- Modify: `crates/engram/src/ingest.rs` (`ingest_document` passes `None` line ranges for now)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/engram/src/store/mod.rs`:

```rust
    #[test]
    fn commit_ingest_persists_line_ranges() {
        let (store, _d) = temp_store();
        let new = NewDoc {
            key: "f".into(), title: "t".into(), content: "x".into(),
            author: "code".into(), taint: Taint::Internal, meta: None,
        };
        // one chunk with a line range, one without (None)
        let doc = store
            .commit_ingest(
                "ns",
                &new,
                &["line one\nline two".to_string(), "tail".to_string()],
                &[vec![1.0f32, 0.0], vec![0.0f32, 1.0]],
                &[vec![], vec![]],
                &[Some((1, 2)), None],
                "sig",
            )
            .unwrap();
        let rows = store.chunks_for_doc("ns", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 2);
        let first = rows.iter().find(|r| r.text == "line one\nline two").unwrap();
        assert_eq!((first.line_start, first.line_end), (Some(1), Some(2)));
        let second = rows.iter().find(|r| r.text == "tail").unwrap();
        assert_eq!((second.line_start, second.line_end), (None, None));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engram commit_ingest_persists_line_ranges 2>&1 | tail -20`
Expected: FAIL to compile — `commit_ingest` takes 6 args not 7, and `ChunkRow` has no `line_start`/`line_end`.

- [ ] **Step 3: Add the schema columns**

In `crates/engram/src/store/mod.rs`, in the `SCHEMA` constant, change the `vector_chunks` table so the two columns are added before the PRIMARY KEY line:

```sql
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
```

(Nullable, no default. Because `CREATE TABLE IF NOT EXISTS` runs on a fresh DB in tests, no migration code is needed there; for an existing on-disk DB these columns won't appear until that note is handled in Phase 1b deployment — out of scope here.)

- [ ] **Step 4: Add line fields to `ChunkRow` and read them in `chunks_for_doc`**

In `crates/engram/src/store/chunks.rs`, extend `ChunkRow`:

```rust
#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub chunk_id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub entities: Vec<String>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}
```

Update `chunks_for_doc` to select and carry the columns. Replace the chunk-loading query + row map + final construction:

```rust
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
        let mut chunks: Vec<(String, String, Vec<f32>, Option<i64>, Option<i64>)> = Vec::new();
        for r in crows {
            chunks.push(r?);
        }
```

and the final mapping:

```rust
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
```

- [ ] **Step 5: Thread line ranges through `commit_ingest`**

In `crates/engram/src/store/mod.rs`, change the `commit_ingest` signature to add a `line_ranges` slice (parallel to `chunk_texts`) before `signature`:

```rust
    #[allow(clippy::too_many_arguments)]
    pub fn commit_ingest(
        &self,
        namespace: &str,
        new: &NewDoc,
        chunk_texts: &[String],
        embeddings: &[Vec<f32>],
        entities: &[Vec<String>],
        line_ranges: &[Option<(i64, i64)>],
        signature: &str,
    ) -> Result<MemoryDoc> {
```

(Add `#[allow(clippy::too_many_arguments)]` above it if not already present — this method now takes 8 args.)

Inside the chunk loop, replace the `vector_chunks` INSERT with one that includes the line columns:

```rust
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
```

- [ ] **Step 6: Fix existing callers so the crate compiles**

(a) In `crates/engram/src/ingest.rs`, update `ingest_document` to pass an all-`None` line-range slice (the code-mode branch comes in Task 4):

```rust
    let entities: Vec<Vec<String>> = chunk_texts.iter().map(|t| extract_entities(t)).collect();
    let line_ranges: Vec<Option<(i64, i64)>> = vec![None; chunk_texts.len()];
    let sig = embedder.signature();
    store.commit_ingest(namespace, new, &chunk_texts, &embeddings, &entities, &line_ranges, &sig)
```

(b) In `crates/engram/src/store/mod.rs` tests, the two existing tests that call `commit_ingest` (`commit_ingest_is_atomic_and_enqueues` and `chunks_for_doc_returns_text_emb_entities` and `meta_persists_through_commit_ingest_and_reads`) must pass the new arg. For each call, insert an all-`None` slice of the right length before the `"sig"` argument. For a 2-chunk call: `&[None, None],`; for a 1-chunk call: `&[None],`. Example for `commit_ingest_is_atomic_and_enqueues` (2 chunks then 1 chunk on re-ingest):

```rust
        let doc = store
            .commit_ingest("alice", &new, &chunks, &embs, &ents, &[None, None], "sig")
            .unwrap();
        // ...
        let doc2 = store
            .commit_ingest("alice", &new, &["only".to_string()], &[vec![1.0f32, 1.0]], &[vec![]], &[None], "sig")
            .unwrap();
```

Apply the same `&[None; N]`-shaped argument to every other `commit_ingest` call in the test module (count the chunk slice length per call and match it). In `chunks_for_doc_returns_text_emb_entities` the existing assertions on `rows[0].text/embedding/entities` stay; add nothing there beyond the new arg.

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test -p engram commit_ingest_persists_line_ranges 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 8: Full suite + warning-clean**

Run: `cargo test -p engram 2>&1 | grep "test result"` (expect 60 passed, 3 ignored — the original 59 + this 1 new test)
Run: `RUSTFLAGS="-D warnings" cargo build -p engram 2>&1 | tail -2` (expect clean)

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(store): nullable line_start/line_end on chunks; commit_ingest threads ranges

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Code-aware chunker `chunk_code`

A line-aware chunker that packs consecutive lines up to a char cap without ever splitting a line, returning 1-based inclusive line ranges. Leaves the existing prose `chunk` untouched.

**Files:**
- Modify: `crates/engram/src/ingest.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/engram/src/ingest.rs`:

```rust
    #[test]
    fn chunk_code_one_chunk_for_small_input() {
        let out = chunk_code("fn main() {\n    let x = 1;\n}\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, 1); // start line
        assert_eq!(out[0].2, 3); // end line
        assert!(out[0].0.contains("fn main()"));
        assert!(out[0].0.contains("let x = 1;"));
    }

    #[test]
    fn chunk_code_splits_on_char_cap_at_line_boundary() {
        // Two ~800-char lines exceed the 1500 cap together → two chunks, split at the line boundary.
        let a = "a".repeat(800);
        let b = "b".repeat(800);
        let out = chunk_code(&format!("{a}\n{b}"));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].1, out[0].2), (1, 1));
        assert_eq!((out[1].1, out[1].2), (2, 2));
        assert!(out[0].0.contains(&a));
        assert!(out[1].0.contains(&b));
    }

    #[test]
    fn chunk_code_oversized_single_line_is_its_own_chunk() {
        let big = "z".repeat(2000); // one line over the cap
        let out = chunk_code(&format!("head\n{big}\ntail"));
        // "head" packs with nothing else once the giant line forces a flush; the giant line is
        // its own chunk; "tail" follows.
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].1, out[0].2), (1, 1)); // head
        assert_eq!((out[1].1, out[1].2), (2, 2)); // big line alone
        assert_eq!((out[2].1, out[2].2), (3, 3)); // tail
        assert!(out[1].0.contains(&big));
    }

    #[test]
    fn chunk_code_empty_is_no_chunks() {
        assert!(chunk_code("").is_empty());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p engram chunk_code 2>&1 | tail -10`
Expected: FAIL to compile — `chunk_code` not defined.

- [ ] **Step 3: Implement `chunk_code`**

Add to `crates/engram/src/ingest.rs` (near the top, after `MAX_CHUNK_CHARS`):

```rust
/// Max characters per code chunk (code packs by line, not paragraph).
const MAX_CODE_CHUNK_CHARS: usize = 1500;

/// Code-aware chunking: pack consecutive lines up to a char cap, never splitting a line.
/// Returns `(chunk_text, start_line, end_line)` with 1-based inclusive line numbers. A single
/// line longer than the cap becomes its own chunk (line boundaries are always respected).
pub fn chunk_code(content: &str) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 1usize;
    let mut cur_lines = 0usize;
    let mut line_no = 0usize;

    for line in content.lines() {
        line_no += 1;
        let add_len = line.chars().count() + 1; // +1 for the newline we append
        if cur_lines > 0 && cur.chars().count() + add_len > MAX_CODE_CHUNK_CHARS {
            out.push((std::mem::take(&mut cur), cur_start, line_no - 1));
            cur_lines = 0;
        }
        if cur_lines == 0 {
            cur_start = line_no;
        }
        cur.push_str(line);
        cur.push('\n');
        cur_lines += 1;
    }
    if cur_lines > 0 {
        out.push((cur, cur_start, line_no));
    }
    out
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p engram chunk_code 2>&1 | tail -6`
Expected: PASS (4 tests).

- [ ] **Step 5: Full suite + warning-clean**

Run: `cargo test -p engram 2>&1 | grep "test result"` (expect 64 passed, 3 ignored)
Run: `RUSTFLAGS="-D warnings" cargo build -p engram 2>&1 | tail -2` (clean)

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(ingest): chunk_code — line-aware code chunker with 1-based line ranges

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `extract_code_entities`

Mechanical code entity extraction → `sym:` (definitions) and `import:` (dependency targets), case-preserving (code is case-sensitive), sorted + deduped. (`path:` for the file's own path is added by the ingest dispatch in Task 4, since it needs the doc key, not the chunk text. Regex extraction of *referenced* paths is deferred — too noisy without ASTs.)

**Files:**
- Modify: `crates/engram/src/ingest.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/engram/src/ingest.rs`:

```rust
    #[test]
    fn extract_code_entities_finds_defs_and_imports_case_preserving() {
        let text = "use crate::store::Store;\npub fn DoThing() {}\nstruct MyType;\n";
        let got = extract_code_entities(text);
        assert!(got.contains(&"import:crate::store::Store".to_string()));
        assert!(got.contains(&"sym:DoThing".to_string())); // case preserved (not lowercased)
        assert!(got.contains(&"sym:MyType".to_string()));
        // sorted + deduped
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(got, sorted);
    }

    #[test]
    fn extract_code_entities_handles_multiple_languages() {
        let py = "import os\ndef handler():\n    pass\nclass Worker:\n    pass\n";
        let got = extract_code_entities(py);
        assert!(got.contains(&"import:os".to_string()));
        assert!(got.contains(&"sym:handler".to_string()));
        assert!(got.contains(&"sym:Worker".to_string()));
    }

    #[test]
    fn extract_code_entities_empty_for_prose() {
        assert!(extract_code_entities("just some words, nothing to define").is_empty());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p engram extract_code_entities 2>&1 | tail -8`
Expected: FAIL to compile — `extract_code_entities` not defined.

- [ ] **Step 3: Implement `extract_code_entities`**

Add to `crates/engram/src/ingest.rs` (after `extract_entities`):

```rust
/// Mechanical (regex) code entities → canonical ids: `sym:<name>` (definitions) and
/// `import:<target>` (import/use/require/include/from targets). Case-preserving (code is
/// case-sensitive). Sorted + deduped. Language-agnostic baseline; AST-precise symbols are a
/// later (tree-sitter) phase.
pub fn extract_code_entities(text: &str) -> Vec<String> {
    static DEF: OnceLock<Regex> = OnceLock::new();
    static IMPORT: OnceLock<Regex> = OnceLock::new();

    let def = DEF.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:pub\s+)?(?:async\s+)?(?:fn|def|class|struct|enum|interface|trait|type|func)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .unwrap()
    });
    let import = IMPORT.get_or_init(|| {
        Regex::new(r#"(?m)^\s*(?:use|import|from|require|include|using)\s+([^\s;(){}'"]+)"#).unwrap()
    });

    let mut out: Vec<String> = Vec::new();
    for c in def.captures_iter(text) {
        out.push(format!("sym:{}", &c[1]));
    }
    for c in import.captures_iter(text) {
        out.push(format!("import:{}", &c[1]));
    }
    out.sort();
    out.dedup();
    out
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p engram extract_code_entities 2>&1 | tail -6`
Expected: PASS (3 tests).

- [ ] **Step 5: Full suite + warning-clean**

Run: `cargo test -p engram 2>&1 | grep "test result"` (expect 67 passed, 3 ignored)
Run: `RUSTFLAGS="-D warnings" cargo build -p engram 2>&1 | tail -2` (clean)

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(ingest): extract_code_entities — sym:/import: regex extraction

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: `meta.kind` dispatch in `ingest_document` (wire code-mode)

Branch ingest on the incoming doc's `meta.kind`: `"file"` → code-mode (`chunk_code` + `extract_code_entities` + a `path:<key>` entity + real line ranges); anything else → today's prose path (unchanged behavior).

**Files:**
- Modify: `crates/engram/src/ingest.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/engram/src/ingest.rs`:

```rust
    #[test]
    fn ingest_code_mode_sets_line_ranges_and_code_entities() {
        use crate::embed::{Embedder, HashEmbedder};
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let new = NewDoc {
            key: "src/lib.rs".into(),
            title: "src/lib.rs".into(),
            content: "use crate::store::Store;\npub fn run() {}\n".into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file", "lang": "rust"})),
        };
        let doc = ingest_document(&store, &e, "repo:x", &new).unwrap();
        let rows = store.chunks_for_doc("repo:x", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].line_start, rows[0].line_end), (Some(1), Some(2)));
        assert!(rows[0].entities.contains(&"sym:run".to_string()));
        assert!(rows[0].entities.contains(&"import:crate::store::Store".to_string()));
        assert!(rows[0].entities.contains(&"path:src/lib.rs".to_string()));
    }

    #[test]
    fn ingest_prose_mode_unchanged_when_no_file_kind() {
        use crate::embed::{Embedder, HashEmbedder};
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let new = NewDoc {
            key: "note".into(), title: "note".into(),
            content: "ping @bob about the launch".into(),
            author: "a".into(), taint: Taint::Internal, meta: None,
        };
        let doc = ingest_document(&store, &e, "alice", &new).unwrap();
        let rows = store.chunks_for_doc("alice", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].line_start, rows[0].line_end), (None, None)); // prose → no lines
        assert!(rows[0].entities.contains(&"handle:bob".to_string()));     // mechanical entities
        assert!(rows[0].entities.iter().all(|e| !e.starts_with("sym:")));  // no code entities
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p engram ingest_code_mode_sets_line_ranges_and_code_entities ingest_prose_mode_unchanged_when_no_file_kind 2>&1 | tail -12`
Expected: FAIL — code-mode test fails (line ranges are `None`, no `sym:`/`path:` entities) because dispatch doesn't exist yet.

- [ ] **Step 3: Implement the dispatch**

In `crates/engram/src/ingest.rs`, replace the entire body of `ingest_document` with the branching version:

```rust
pub fn ingest_document(
    store: &Store,
    embedder: &dyn Embedder,
    namespace: &str,
    new: &NewDoc,
) -> Result<MemoryDoc> {
    let is_code = new
        .meta
        .as_ref()
        .and_then(|m| m.get("kind"))
        .and_then(|k| k.as_str())
        == Some("file");

    let (chunk_texts, line_ranges, entities): (
        Vec<String>,
        Vec<Option<(i64, i64)>>,
        Vec<Vec<String>>,
    ) = if is_code {
        let pieces = chunk_code(&new.content);
        let texts: Vec<String> = pieces.iter().map(|(t, _, _)| t.clone()).collect();
        let ranges: Vec<Option<(i64, i64)>> = pieces
            .iter()
            .map(|(_, s, e)| Some((*s as i64, *e as i64)))
            .collect();
        let path_ent = format!("path:{}", new.key);
        let ents: Vec<Vec<String>> = texts
            .iter()
            .map(|t| {
                let mut es = extract_code_entities(t);
                es.push(path_ent.clone());
                es.sort();
                es.dedup();
                es
            })
            .collect();
        (texts, ranges, ents)
    } else {
        let texts = chunk(&new.content);
        let ranges = vec![None; texts.len()];
        let ents: Vec<Vec<String>> = texts.iter().map(|t| extract_entities(t)).collect();
        (texts, ranges, ents)
    };

    let mut embeddings = Vec::with_capacity(chunk_texts.len());
    for t in &chunk_texts {
        embeddings.push(embedder.embed(t)?); // off-lock — the slow part
    }
    let sig = embedder.signature();
    store.commit_ingest(namespace, new, &chunk_texts, &embeddings, &entities, &line_ranges, &sig)
}
```

(This supersedes the `line_ranges = vec![None; ...]` shim added in Task 1.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p engram ingest_code_mode_sets_line_ranges_and_code_entities ingest_prose_mode_unchanged_when_no_file_kind 2>&1 | tail -6`
Expected: PASS (2 tests).

- [ ] **Step 5: Full suite + warning-clean**

Run: `cargo test -p engram 2>&1 | grep "test result"` (expect 69 passed, 3 ignored)
Run: `RUSTFLAGS="-D warnings" cargo build -p engram 2>&1 | tail -2` (clean)

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(ingest): code-mode dispatch on meta.kind (line ranges + code entities + path)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Chunk-level `search_code`

A store query that returns code chunks joined to their file path + line ranges, and a `retrieve::search_code` that ranks chunks (vector + keyword) and returns `CodeHit { path, line_start, line_end, snippet, score, ... }`. Distinct from the doc-level `query`.

**Files:**
- Modify: `crates/engram/src/store/chunks.rs` (`CodeChunkRow` + `code_chunks_for_namespace`)
- Modify: `crates/engram/src/retrieve.rs` (`CodeHit` + `search_code`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/engram/src/retrieve.rs`:

```rust
    #[test]
    fn search_code_returns_chunk_level_hits_with_paths_and_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(64);
        let mk = |k: &str, c: &str| crate::model::NewDoc {
            key: k.into(), title: k.into(), content: c.into(), author: "code".into(),
            taint: crate::model::Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        crate::ingest::ingest_document(&store, &e, "repo:x", &mk("src/auth.rs", "fn login() {}\nfn logout() {}")).unwrap();
        crate::ingest::ingest_document(&store, &e, "repo:x", &mk("src/math.rs", "fn add() {}\nfn mul() {}")).unwrap();

        let hits = search_code(&store, &e, "repo:x", "login", 10).unwrap();
        assert!(!hits.is_empty());
        // the auth file ranks first for a login query
        assert_eq!(hits[0].path, "src/auth.rs");
        assert!(hits[0].snippet.contains("login"));
        assert_eq!(hits[0].line_start, Some(1));
        assert!(hits[0].score >= hits.last().unwrap().score);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p engram search_code_returns_chunk_level_hits_with_paths_and_lines 2>&1 | tail -10`
Expected: FAIL to compile — `search_code` and `CodeChunkRow`/`code_chunks_for_namespace` not defined.

- [ ] **Step 3: Add `CodeChunkRow` + `code_chunks_for_namespace` to the store**

In `crates/engram/src/store/chunks.rs`, add the struct (below `ChunkRow`) and the method (inside `impl Store`):

```rust
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
```

```rust
    /// All chunks for a namespace+signature, joined to their document key (file path) and
    /// carrying line ranges — the candidate set for chunk-level code search.
    pub fn code_chunks_for_namespace(
        &self,
        namespace: &str,
        signature: &str,
    ) -> Result<Vec<CodeChunkRow>> {
        let conn = self.read.get()?;
        let mut stmt = conn.prepare(
            "SELECT d.key, c.document_id, c.text, c.embedding, c.line_start, c.line_end
             FROM vector_chunks c
             JOIN memory_docs d ON d.namespace = c.namespace AND d.document_id = c.document_id
             WHERE c.namespace = ?1 AND c.model_signature = ?2",
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
```

- [ ] **Step 4: Re-export `CodeChunkRow` for crate-path use**

In `crates/engram/src/store/mod.rs`, beside the existing `pub use jobs::Job;` line, the store already re-exports types referenced by path. `CodeChunkRow` is only used inside `retrieve` via the method return type (not named by path), so **no re-export is required** — but `search_code` will name `crate::store::CodeChunkRow`? It does not (it consumes the `Vec<CodeChunkRow>` by value and reads fields). Confirm `retrieve.rs` does NOT write `crate::store::CodeChunkRow` anywhere; it only calls `store.code_chunks_for_namespace(...)` and iterates. No re-export needed. (If a future caller names the type by path, add `pub use chunks::CodeChunkRow;` then.)

- [ ] **Step 5: Add `CodeHit` + `search_code` to retrieve**

In `crates/engram/src/retrieve.rs`, add the struct (after `TreeHit`) and the function (after `query`):

```rust
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
```

```rust
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
```

(Reuses the existing `cosine`, `keyword_overlap`, `VEC_W_FALLBACK`, `KW_W_FALLBACK` already in this file. Chunk-level — no per-doc dedup, no graph term in Phase 1a; entity-graph boosting for code arrives with Phase 2 topic trees.)

- [ ] **Step 6: Run to verify pass**

Run: `cargo test -p engram search_code_returns_chunk_level_hits_with_paths_and_lines 2>&1 | tail -6`
Expected: PASS.

- [ ] **Step 7: Full suite + warning-clean + clippy**

Run: `cargo test -p engram 2>&1 | grep "test result"` (expect 70 passed, 3 ignored)
Run: `cargo clippy -p engram --all-targets -- -D warnings 2>&1 | tail -2` (clean)
Run: `cargo fmt -p engram` then `cargo fmt --all -- --check` (clean)

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat(retrieve): chunk-level search_code (path:line + snippet) over code chunks

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-review (completed)

**Spec coverage (Phase 1a slice of the spec):**
- "add nullable line_start/line_end to vector_chunks + ChunkRow + commit_ingest threading" → Task 1. ✅
- "code-aware chunker that emits line ranges (~1500 chars, language-agnostic, line-boundary respecting)" → Task 2 (`chunk_code`, `MAX_CODE_CHUNK_CHARS = 1500`). ✅
- "extract_code_entities (sym:/import:/path:)" → Task 3 (`sym:`/`import:`) + Task 4 (`path:<own key>` added at dispatch; referenced-path regex deferred, matching the spec's "language-agnostic baseline, tree-sitter later"). ✅
- "meta.kind dispatch in ingest_document (file → code-mode, else prose-mode)" → Task 4. ✅
- "chunk-level code search in retrieve (return matching chunk text + path:line, not whole-doc)" → Task 5 (`CodeHit` + `search_code`). ✅
- Out of scope (own plans): `engram-index` (1b), `engram-mcp` (1c), entity-graph boosting for code search, tree-sitter.

**Placeholder scan:** No "TBD/handle errors/etc." — every code step has complete code. The only intentional notes are explicit "this supersedes the Task 1 shim" / "no re-export needed" guidance, not placeholders.

**Type/name consistency:** `commit_ingest(..., line_ranges: &[Option<(i64,i64)>], signature)` is defined in Task 1 and called identically in Task 1's caller fix and Task 4's dispatch. `ChunkRow.line_start/line_end: Option<i64>` (Task 1) match `chunk_code`'s `usize` line numbers cast to `i64` at the dispatch (Task 4). `CodeChunkRow` fields (Task 5 store) match `search_code`'s field reads (Task 5 retrieve). `chunk_code -> Vec<(String, usize, usize)>` (Task 2) is consumed with `.map(|(t,_,_)|...)` / `.map(|(_,s,e)|...)` in Task 4. Weight consts `VEC_W_FALLBACK`/`KW_W_FALLBACK` already exist in `retrieve.rs`.

**Test-count trail (sanity):** 59 → 60 (T1) → 64 (T2, +4) → 67 (T3, +3) → 69 (T4, +2) → 70 (T5, +1), 3 ignored throughout. Each task verifies the running total.

**Note for the implementer:** Task 1 Step 6(b) requires finding *every* `commit_ingest(` call in `store/mod.rs`'s test module and adding the `&[None; N]` argument (N = that call's chunk-slice length). Don't miss one, or the test module won't compile.
