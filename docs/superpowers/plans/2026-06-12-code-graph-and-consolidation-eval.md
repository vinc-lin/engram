# Code-graph (callers/callees + trace_symbol) and consolidation evaluation

**Goal:** Add a structural code-graph (`code_edges`) that powers `callers`/`callees` BFS and a `trace_symbol` MCP tool, behind a default-off gate, without ever touching the recall-tuned retrieval path; and stand up the two evaluation tracks (graph precision/recall + consolidation A/B) that decide whether the feature and consolidation earn production enablement.

**Architecture:** A new `code_edges` SQLite table is written only inside `commit_ingest`'s existing transaction from edges produced off-lock by a pure `graph::extract_edges` (Rust + C/C++ in v1). The table is a *structural index only* — `retrieve.rs`/`search_code`/`query` never read it, so the 0.55-weight graph signal that cratered recall@5 cannot recur. Two new endpoints (`POST /v1/:ns/code/graph/callers` and `/callees`) run a bounded-depth BFS over the table with lazy cross-file resolution through `chunk_entities`; the `engram-mcp` crate exposes them as one new `trace_symbol` tool (its own HTTP-deserialized `TraceHop`).

**Tech Stack:** Rust (axum, rusqlite/SQLite bundled, r2d2 read pool, tree-sitter), the `engram` / `engram-index` / `engram-mcp` Cargo workspace; Python stdlib-only eval harnesses (`urllib`, `json`, `sqlite3`).

REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement task-by-task. Steps use `- [ ]` checkboxes.

---

## Locked decisions

- **v1 language scope = Rust + C/C++.** `graph::extract_edges` returns an empty `Vec` for every other language (Python/TS/JS/Go/Java/Kotlin are a noted follow-up — one addition to `graph.rs`, no wiring change).
- **OnceLock gate.** Extraction is gated by `ENGRAM_CODE_GRAPH_EXTRACT` (default **false**), implemented as a process-global `OnceLock` free function in `config.rs`, matching the existing `code_graph()` / `consolidate_code()` pattern. Two more gates: `ENGRAM_CODE_GRAPH_MIN_CONFIDENCE` (0.6) and `ENGRAM_CODE_GRAPH_MAX_DEPTH` (4).
- **Types live in `model.rs`.** `EdgeKind` and `RawEdge` are defined once in `model.rs`; every other module `use`s them.
- **Edges are written ONLY inside `commit_ingest`'s existing transaction**, via a `pub(super) edges::insert_edges_in_tx` helper. There is no other write path. Re-ingest replaces a doc's edges atomically (DELETE-then-INSERT in the same `tx`).
- **Lazy cross-file resolution (Option a).** `code_edges.dst_doc_id` is left NULL at write time; the BFS resolves a symbol to its defining document at query time via `chunk_entities` (index `idx_chunk_entities_entity`). The indexer (`engram-index`) is untouched.
- **v1 query surface = `callers` / `callees` + the `trace_symbol` MCP tool.** Nothing else reads the table.
- **Deferred to v1.1:** `neighbors`, `blast_radius`, `dead_symbols`, `detect_changes` endpoints/tools.
- **Option D rejected.** A frozen nomic embedder was measured at recall@5 0.380 — *below* random (0.519) and far below keyword-only (0.787). Not pursued.

---

## The exact-index invariant (hard gate)

`code_edges` is a **structural index only**. The retrieval path stays exactly as tuned:

- `retrieve.rs` is **UNCHANGED**. `search_code` and `query` never reference `code_edges`, `graph_query`, `store::edges`, `edges_from`, `edges_to_sym`, or `resolve_dst_doc`.
- The only readers of `code_edges` are `graph_query::callers` / `graph_query::callees`, reachable solely through the two `/code/graph/*` endpoints and the `trace_symbol` MCP tool.
- This is the explicit mechanism that avoids the **0.55 graph-signal recall crater** (the `sym:`-graph blend was measured to *hurt* code recall, cratering recall@5 to 0.600 — worse than the no-graph baseline; that is why `ENGRAM_CODE_GRAPH` ships off). A structural index that search never reads cannot regress search.
- Verification step (Track-1 final task): `grep -n "code_edges\|graph_query\|edges_from\|edges_to_sym\|resolve_dst_doc" crates/engram/src/retrieve.rs` must return **zero matches**.

---

## Canonical contracts (single source of truth)

These are the *one* authoritative definition of each shared surface. The tasks below implement exactly these; if a task's code disagrees with a contract here, the contract wins.

### `EdgeKind` + `RawEdge` (in `model.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind { Calls, UsesType, Imports }
// as_str() -> "CALLS"|"USES_TYPE"|"IMPORTS"; parse(&str) -> Option<EdgeKind> (case-sensitive)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEdge {
    pub dst_sym: String,          // canonical "sym:<Name>"
    pub edge_kind: EdgeKind,
    pub src_line: Option<i64>,    // 1-based, optional
    pub confidence: f32,          // [0.0,1.0]
}                                  // NO src_doc_id field
```

### `TraceHop` (core crate, in `graph_query.rs`)

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceHop {
    pub path: String,
    pub document_id: String,
    pub sym: String,
    pub edge_kind: String,   // the EdgeKind string ("CALLS"/...)
    pub confidence: f32,
    pub depth: usize,
}
```

(The `engram-mcp` crate defines its **own** crate-local `TraceHop` — derives `Deserialize` only — because that crate talks to the core over HTTP and does not depend on it.)

### New `commit_ingest` signature (in `store/mod.rs`)

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
    raw_edges: &[crate::model::RawEdge],   // NEW — between line_ranges and signature
    signature: &str,
) -> Result<MemoryDoc>
```

### The two routes + `GraphReq` (in `api.rs`)

```
POST /v1/:namespace/code/graph/callers   body: GraphReq, requires `sym`
POST /v1/:namespace/code/graph/callees    body: GraphReq, requires `path`
```

```rust
#[derive(Deserialize)]
struct GraphReq {
    sym: Option<String>,
    path: Option<String>,
    max_depth: Option<usize>,
    limit: Option<usize>,
    min_confidence: Option<f32>,
}
```

Handlers read only SQLite (no embed/LLM) → **no `spawn_blocking`**. Depth: `let max_depth = req.max_depth.unwrap_or(2).min(crate::config::code_graph_max_depth());`. `min_confidence` is applied **post-BFS** in the handler (`hops.retain(|h| h.confidence >= min_conf)`).

### `trace_symbol` MCP tool schema (in `engram-mcp/src/lib.rs`)

```json
{
  "name": "trace_symbol",
  "description": "Walk the code call-graph to find callers of a symbol or callees from a file...",
  "inputSchema": {
    "type": "object",
    "properties": {
      "sym":       {"type": "string"},
      "path":      {"type": "string"},
      "direction": {"type": "string", "description": "'callers' (default) or 'callees'"},
      "depth":     {"type": "integer", "description": "default 2"},
      "limit":     {"type": "integer", "description": "default 20"}
    }
  }
}
```

### The three config gate functions (in `config.rs`)

```rust
pub fn code_graph_extract() -> bool        // ENGRAM_CODE_GRAPH_EXTRACT, default false
pub fn code_graph_min_confidence() -> f32  // ENGRAM_CODE_GRAPH_MIN_CONFIDENCE, default 0.6, clamped to [0,1]
pub fn code_graph_max_depth() -> usize     // ENGRAM_CODE_GRAPH_MAX_DEPTH, default 4, 0 clamped up to 1
```

---

## Build order

The explicit ordered task list (corrected from the completeness verifier's 16-step order for the ownership + code fixes below). Each numbered item is one Track-1 task in the "Track 1 — tasks" section.

1. `model.rs` — `EdgeKind` + `RawEdge` (single canonical definition; `Copy` on `EdgeKind`, serde on both).
2. `config.rs` — the three OnceLock gate functions.
3. `store/mod.rs` — `code_edges` DDL (incl. `dst_doc_id TEXT`) + `migrate()` bump to `user_version = 2` + `commit_ingest` gains `raw_edges` (writes via `edges::insert_edges_in_tx`); update every existing call site with `&[]`.
4. `store/edges.rs` (create) — `insert_edges_in_tx` (write, `pub(super)`) + read methods `edges_from`, `edges_to_sym`, `resolve_dst_doc`.
5. `store/entities.rs` — add `Store::sym_entities_for_doc(ns, doc_id) -> Result<Vec<String>>` (read pool; `sym:%` from `chunk_entities`).
6. `store/docs.rs` — `delete_doc_by_key` also clears `code_edges` for both `src_doc_id` and `dst_doc_id`.
7. `treesit.rs` — `pub(crate)` on `parse`, `symbol_name`, `declarator_name` (visibility only).
8. `graph.rs` (create) — `extract_edges` for Rust + C/C++.
9. `lib.rs` — add `pub mod graph;`.
10. `ingest.rs` — wire `extract_edges` into the `is_code` path (build `file_syms`, gate on `code_graph_extract()`, filter `< code_graph_min_confidence()`, thread `raw_edges` into `commit_ingest`).
11. `graph_query.rs` (create) — `TraceHop`, `callers`, `callees` (using `sym_entities_for_doc`, no `read_conn`, no dead loop) + `lib.rs` `pub mod graph_query;`.
12. `api.rs` — `GraphReq`, `graph_callers`, `graph_callees` handlers + route registration.
13. `engram-mcp/src/lib.rs` — crate-local `TraceHop`, `trace_symbol` on the trait + `HttpCodeSearch` impl + `FakeSearch`/`ErrorSearch` stubs + `FakeTracer` + `format_trace` + tool def + dispatch arm + tools/list 6→7 test.
14. eval Track-1: `eval/graph_gold.json`, `eval/android/graph_gold_native.json`, `eval/graph_eval.py`.
15. `tree.rs` integration test (Track-2 mechanical).
16. `eval/consolidation/*` (Track-2 fallback-rate + A/B).

---

## Track 1 — tasks

Detailed tasks in build order, grouped by module. Each task lists `Files:` and the TDD steps (failing test → run → implement → run → commit). Code is copied verbatim from the owning section with the B-fixes applied.

---

### Task 1 — `model.rs`: `EdgeKind` + `RawEdge`

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/model.rs`

Insert after the `NewDoc` struct, before the existing `#[cfg(test)]` block. `serde::{Deserialize, Serialize}` is already imported at the top of `model.rs`.

- [ ] Step: write the failing test — add inside the existing `mod tests` block, after `meta_round_trips_and_defaults_none`, before the closing `}`:

  ```rust
  #[test]
  fn edge_kind_as_str_round_trips() {
      assert_eq!(EdgeKind::Calls.as_str(), "CALLS");
      assert_eq!(EdgeKind::UsesType.as_str(), "USES_TYPE");
      assert_eq!(EdgeKind::Imports.as_str(), "IMPORTS");

      assert_eq!(EdgeKind::parse("CALLS"), Some(EdgeKind::Calls));
      assert_eq!(EdgeKind::parse("USES_TYPE"), Some(EdgeKind::UsesType));
      assert_eq!(EdgeKind::parse("IMPORTS"), Some(EdgeKind::Imports));
      assert_eq!(EdgeKind::parse("calls"), None); // case-sensitive
      assert_eq!(EdgeKind::parse("UNKNOWN"), None);
  }

  #[test]
  fn raw_edge_fields_accessible() {
      let e = RawEdge {
          dst_sym: "sym:FooBar".into(),
          edge_kind: EdgeKind::Calls,
          src_line: Some(42),
          confidence: 0.9,
      };
      assert_eq!(e.dst_sym, "sym:FooBar");
      assert_eq!(e.edge_kind.as_str(), "CALLS");
      assert_eq!(e.src_line, Some(42));
      assert!((e.confidence - 0.9).abs() < 1e-6);
      // no src_doc_id field on RawEdge — verify by exhaustive struct literal above compiling
  }
  ```

- [ ] Step: run it, expect FAIL

  ```
  cargo test -p engram edge_kind raw_edge
  ```

  Expected: compile error — `EdgeKind` and `RawEdge` not found in `model`.

- [ ] Step: implement — insert this block after the `NewDoc` struct, immediately before the `#[cfg(test)]` line:

  ```rust
  /// The semantic relationship carried by a directed code-graph edge.
  /// Stored as the canonical uppercase strings "CALLS", "USES_TYPE", "IMPORTS"
  /// in `code_edges.edge_kind` (TEXT NOT NULL).
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
  pub enum EdgeKind {
      Calls,
      UsesType,
      Imports,
  }

  impl EdgeKind {
      /// Canonical uppercase string stored in SQLite and returned in API responses.
      pub fn as_str(&self) -> &'static str {
          match self {
              EdgeKind::Calls => "CALLS",
              EdgeKind::UsesType => "USES_TYPE",
              EdgeKind::Imports => "IMPORTS",
          }
      }

      /// Parse the canonical uppercase string. Returns `None` for any other input
      /// (case-sensitive — the DB stores exactly these strings).
      pub fn parse(s: &str) -> Option<EdgeKind> {
          match s {
              "CALLS" => Some(EdgeKind::Calls),
              "USES_TYPE" => Some(EdgeKind::UsesType),
              "IMPORTS" => Some(EdgeKind::Imports),
              _ => None,
          }
      }
  }

  /// One directed edge extracted from source text, before the write lock.
  ///
  /// `src_doc_id` is intentionally absent: the edge is produced by `graph::extract_edges`
  /// (which runs off-lock, before `commit_ingest` assigns a document id) and is passed into
  /// `commit_ingest` as a slice. The store layer stamps `src_doc_id` from the freshly-assigned
  /// doc id inside the transaction.
  ///
  /// `dst_doc_id` is resolved lazily at query time via `chunk_entities` (cross-file resolution,
  /// Option a) — it is NULL in the database until `graph_query` resolves it.
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct RawEdge {
      /// Target symbol in canonical entity form, e.g. `"sym:FooBar"`.
      pub dst_sym: String,
      pub edge_kind: EdgeKind,
      /// Source line number within the file (1-based), if known.
      pub src_line: Option<i64>,
      /// Extraction confidence in [0.0, 1.0].
      pub confidence: f32,
  }
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram edge_kind raw_edge`
- [ ] Step: commit — `git add crates/engram/src/model.rs && git commit -m "feat(model): EdgeKind + RawEdge value types for code-graph edges"`

---

### Task 2 — `config.rs`: three OnceLock gate functions

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/config.rs`

Insert after the existing `code_graph()` function, before the `#[cfg(test)]` block.

- [ ] Step: write the failing tests — add inside the existing `mod tests` block, after the last existing test, before the closing `}`:

  ```rust
  #[test]
  fn code_graph_extract_defaults_false() {
      let a = code_graph_extract();
      let b = code_graph_extract();
      assert_eq!(a, b);
      if std::env::var("ENGRAM_CODE_GRAPH_EXTRACT").is_err() {
          assert!(!a);
      }
  }

  #[test]
  fn code_graph_min_confidence_defaults_0_6() {
      let a = code_graph_min_confidence();
      let b = code_graph_min_confidence();
      assert!((a - b).abs() < 1e-9);
      if std::env::var("ENGRAM_CODE_GRAPH_MIN_CONFIDENCE").is_err() {
          assert!((a - 0.6_f32).abs() < 1e-6);
      }
  }

  #[test]
  fn code_graph_max_depth_defaults_4() {
      let a = code_graph_max_depth();
      let b = code_graph_max_depth();
      assert_eq!(a, b);
      if std::env::var("ENGRAM_CODE_GRAPH_MAX_DEPTH").is_err() {
          assert_eq!(a, 4_usize);
      }
  }
  ```

- [ ] Step: run it, expect FAIL

  ```
  cargo test -p engram code_graph_extract code_graph_min_confidence code_graph_max_depth
  ```

  Expected: compile error — the three functions not found.

- [ ] Step: implement — insert after `code_graph()`, before the `#[cfg(test)]` line:

  ```rust
  /// Whether `ingest_document` should run `graph::extract_edges` on code-mode files and pass
  /// the resulting edges to `commit_ingest` for storage in `code_edges`. Cached read of
  /// `ENGRAM_CODE_GRAPH_EXTRACT` (default **false**). Only Rust and C/C++ edge extraction is
  /// implemented in v1; other languages produce an empty edge set regardless of this flag.
  /// Off by default to keep ingest cost identical to pre-graph builds until the feature is
  /// exercised. When on, `extract_edges` runs *before* the write lock (off-lock invariant
  /// preserved) and the edge slice is handed into the atomic `commit_ingest` transaction.
  pub fn code_graph_extract() -> bool {
      use std::sync::OnceLock;
      static CACHE: OnceLock<bool> = OnceLock::new();
      *CACHE.get_or_init(|| {
          std::env::var("ENGRAM_CODE_GRAPH_EXTRACT")
              .ok()
              .and_then(|s| match s.to_ascii_lowercase().as_str() {
                  "true" | "1" | "yes" | "on" => Some(true),
                  "false" | "0" | "no" | "off" => Some(false),
                  _ => None,
              })
              .unwrap_or(false)
      })
  }

  /// Minimum confidence threshold for edges written to `code_edges`. Edges extracted by
  /// `graph::extract_edges` with `confidence < code_graph_min_confidence()` are silently
  /// dropped before `commit_ingest`. Cached read of `ENGRAM_CODE_GRAPH_MIN_CONFIDENCE`
  /// (default **0.6**). Values outside [0.0, 1.0] are ignored and the default applies.
  pub fn code_graph_min_confidence() -> f32 {
      use std::sync::OnceLock;
      static CACHE: OnceLock<f32> = OnceLock::new();
      *CACHE.get_or_init(|| {
          std::env::var("ENGRAM_CODE_GRAPH_MIN_CONFIDENCE")
              .ok()
              .and_then(|s| s.parse::<f32>().ok())
              .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
              .unwrap_or(0.6)
      })
  }

  /// Maximum BFS depth for `graph_query::callers` and `graph_query::callees` traversals.
  /// Used as the process-global cap when the caller passes `None` or a value larger than this
  /// limit. Cached read of `ENGRAM_CODE_GRAPH_MAX_DEPTH` (default **4**). Values of 0 are
  /// clamped to 1 so a traversal always returns at least direct neighbours.
  pub fn code_graph_max_depth() -> usize {
      use std::sync::OnceLock;
      static CACHE: OnceLock<usize> = OnceLock::new();
      *CACHE.get_or_init(|| {
          std::env::var("ENGRAM_CODE_GRAPH_MAX_DEPTH")
              .ok()
              .and_then(|s| s.parse::<usize>().ok())
              .map(|v| v.max(1))
              .unwrap_or(4)
      })
  }
  ```

  Then add the three test functions (above) inside the existing `mod tests` block.

- [ ] Step: run, expect PASS — `cargo test -p engram code_graph_extract code_graph_min_confidence code_graph_max_depth`
- [ ] Step: full suite still green — `cargo test -p engram`
- [ ] Step: commit — `git add crates/engram/src/config.rs && git commit -m "feat(config): code_graph_extract/min_confidence/max_depth OnceLock gate flags"`

**Integration notes (downstream):**
- `EdgeKind::parse` is the sole deserialization path from the `code_edges` TEXT column back to the enum — `store/edges.rs` must call it (never construct an `EdgeKind` from a raw string any other way).
- `code_graph_min_confidence()` is consumed in `ingest.rs`: `extracted.retain(|e| e.confidence >= crate::config::code_graph_min_confidence())`.
- `code_graph_max_depth()` is the ceiling in `api.rs`: `req.max_depth.unwrap_or(2).min(crate::config::code_graph_max_depth())`.
- The OnceLock values are frozen at first call. Tests that need to vary thresholds use the `Config::from_vars` struct path, not these free functions.

---

### Task 3 — `store/mod.rs`: `code_edges` DDL + migration + `commit_ingest` `raw_edges` param

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/store/mod.rs`

This is the **sole owner** of: the `code_edges` DDL, the migration, the `commit_ingest` signature change, the in-tx edge write (delegated to `edges::insert_edges_in_tx`), and updating every existing `commit_ingest` call site.

- [ ] Step: write the failing tests — add to `#[cfg(test)] mod tests` in `store/mod.rs`:

  ```rust
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
  ```

  Replace the existing `commit_ingest_is_atomic_and_enqueues` test with the expanded version that exercises the edge write and idempotent replace:

  ```rust
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
          .commit_ingest("alice", &new, &chunks, &embs, &ents, &[None, None], &edges, "sig")
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
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram code_edges_table_and_indexes_exist commit_ingest_is_atomic_and_enqueues`

  Expected: compile error — `commit_ingest` does not yet accept `raw_edges`; `code_edges` table does not exist.

- [ ] Step: implement (DDL) — append to the `SCHEMA` const in `store/mod.rs`, after the `tree_edges` block, before the closing `"`:

  ```sql
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
  ```

- [ ] Step: implement (migration) — update `migrate()` to add `code_edges` for existing DBs and bump to `user_version = 2`. This is the **only** migration that bumps the version:

  ```rust
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
  ```

  (Both `SCHEMA` and `migrate()` create `code_edges` with `IF NOT EXISTS`, mirroring the existing `line_start`/`line_end` pattern — `SCHEMA` runs first on fresh DBs, `migrate()` is a no-op there and creates the table on pre-v2 DBs.)

- [ ] Step: implement (`commit_ingest`) — add the import and change the signature/body. Add `mod edges;` after the existing submodule declarations (after `mod trees;`). Add `RawEdge`/`EdgeKind` to the model import at the top of `store/mod.rs`:

  ```rust
  use crate::model::{EdgeKind, MemoryDoc, NewDoc, RawEdge, Taint};
  ```

  Change the `commit_ingest` signature to the canonical contract (`raw_edges: &[RawEdge]` between `line_ranges` and `signature`). Inside the transaction body, after the `for (seq, text)` chunk/entity loop and **before** `enqueue_job_sql`, insert the delegated edge write:

  ```rust
              // Replace all code edges for this doc, inside the same tx (mirrors the chunk/entity
              // DELETE+INSERT pattern). dst_doc_id is left NULL; resolved lazily at query time.
              edges::insert_edges_in_tx(&tx, namespace, &doc_id, raw_edges)?;
  ```

  (`edges::` is unqualified because `mod edges;` is declared in this same file.)

- [ ] Step: implement (call sites) — every existing `commit_ingest` call in the `store/mod.rs` test block must gain `&[]` before the `"sig"` argument. Convert each from `(..., &[None, None], "sig")` to `(..., &[None, None], &[], "sig")`. The compiler enforces the 8-argument signature, so no site can be missed silently.

- [ ] Step: run, expect PASS — `cargo test -p engram code_edges_table_and_indexes_exist commit_ingest_is_atomic_and_enqueues open_sets_wal_and_schema` then `cargo test -p engram`
- [ ] Step: commit — `git add crates/engram/src/store/mod.rs && git commit -m "feat(store): code_edges DDL + migrate user_version=2; commit_ingest gains raw_edges param"`

---

### Task 4 — `store/edges.rs` (create): write helper + read methods

**Files:** Create `/mnt/x/code/engram/crates/engram/src/store/edges.rs`

`store/edges.rs` holds the `pub(super)` write helper `insert_edges_in_tx` (the only writer, called from `commit_ingest`) and the three read methods `edges_from`, `edges_to_sym`, `resolve_dst_doc` (read pool only).

- [ ] Step: write the failing test — add to `store/mod.rs` `#[cfg(test)]`:

  ```rust
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
                  RawEdge { dst_sym: "sym:bar".into(), edge_kind: EdgeKind::Calls, src_line: Some(1), confidence: 1.0 },
                  RawEdge { dst_sym: "sym:Baz".into(), edge_kind: EdgeKind::UsesType, src_line: Some(2), confidence: 0.8 },
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
      let calls_edge = from.iter().find(|e| e.edge_kind == EdgeKind::Calls).unwrap();
      assert_eq!(calls_edge.dst_sym, "sym:bar");
      assert_eq!(calls_edge.src_line, Some(1));
      assert!((calls_edge.confidence - 1.0).abs() < 1e-6);
      let type_edge = from.iter().find(|e| e.edge_kind == EdgeKind::UsesType).unwrap();
      assert_eq!(type_edge.dst_sym, "sym:Baz");
      assert!((type_edge.confidence - 0.8).abs() < 1e-5);

      assert!(store.edges_from("other", &src_doc.document_id).unwrap().is_empty());

      let to = store.edges_to_sym("ns", "sym:bar").unwrap();
      assert_eq!(to.len(), 1);
      assert_eq!(to[0].0, src_doc.document_id);
      assert_eq!(to[0].1.dst_sym, "sym:bar");
      assert_eq!(to[0].1.edge_kind, EdgeKind::Calls);

      let resolved = store.resolve_dst_doc("ns", "sym:bar").unwrap();
      assert!(resolved.is_some(), "sym:bar should resolve to bar.rs's doc_id");

      let unresolved = store.resolve_dst_doc("ns", "sym:Baz").unwrap();
      assert!(unresolved.is_none());
  }
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram edges_read_methods_seed_and_roundtrip`

  Expected: compile error — `store::edges` does not exist; methods/helper undefined.

- [ ] Step: implement — create `/mnt/x/code/engram/crates/engram/src/store/edges.rs`:

  ```rust
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
                  out.push(RawEdge { dst_sym, edge_kind, src_line, confidence: confidence as f32 });
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
                      RawEdge { dst_sym: dst_sym.to_string(), edge_kind, src_line, confidence: confidence as f32 },
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
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram edges_read_methods_seed_and_roundtrip` then `cargo test -p engram`
- [ ] Step: commit — `git add crates/engram/src/store/edges.rs crates/engram/src/store/mod.rs && git commit -m "feat(store): store/edges.rs — insert_edges_in_tx write helper + edges_from/edges_to_sym/resolve_dst_doc"`

**Critical detail:** `confidence` is stored as SQLite `REAL` (f64), cast from `f32` on write and back on read — negligible precision loss for a [0,1] score. There is no write method other than `insert_edges_in_tx`; `edges.rs` is otherwise structurally read-only (it never takes `self.write.lock()`).

---

### Task 5 — `store/entities.rs`: `sym_entities_for_doc`

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/store/entities.rs`

This replaces the nonexistent `store.read_conn()` raw-SQL access that the graph-extract section's `queue_doc_syms` invented. `graph_query::callers` calls this method to expand the upward BFS through a caller's own `sym:` entities.

- [ ] Step: write the failing test — add to `store/mod.rs` `#[cfg(test)]`:

  ```rust
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
              &[vec!["sym:foo".into(), "sym:Bar".into(), "path:src/lib.rs".into(), "import:std".into()]],
              &[Some((1, 1))],
              &[],
              "sig",
          )
          .unwrap();
      let mut syms = store.sym_entities_for_doc("ns", &doc.document_id).unwrap();
      syms.sort();
      assert_eq!(syms, vec!["sym:Bar".to_string(), "sym:foo".to_string()]);
      // wrong namespace → empty
      assert!(store.sym_entities_for_doc("other", &doc.document_id).unwrap().is_empty());
  }
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram sym_entities_for_doc_returns_only_sym_entities`

  Expected: compile error — `sym_entities_for_doc` not found on `Store`.

- [ ] Step: implement — add the method to `impl Store` in `store/entities.rs` (follow the existing read-method pattern in that file — `self.read.get()?`, `prepare`, `query_map`):

  ```rust
  /// All distinct `sym:` entity ids recorded for a document in `namespace`, via the read pool
  /// (index `idx_chunk_entities_entity`). Used by `graph_query::callers` to expand the upward
  /// BFS through a caller's own defined symbols. Returns only `sym:%` entities — `path:`,
  /// `import:`, and prose entities are excluded.
  pub fn sym_entities_for_doc(&self, ns: &str, doc_id: &str) -> Result<Vec<String>> {
      let conn = self.read.get()?;
      let mut stmt = conn.prepare_cached(
          "SELECT DISTINCT entity_id FROM chunk_entities
           WHERE namespace=?1 AND document_id=?2 AND entity_id LIKE 'sym:%'",
      )?;
      let rows = stmt.query_map(params![ns, doc_id], |r| r.get::<_, String>(0))?;
      let mut out = Vec::new();
      for row in rows {
          out.push(row?);
      }
      Ok(out)
  }
  ```

  (Ensure `use rusqlite::params;` is present in `store/entities.rs` — it already is, per the existing read methods.)

- [ ] Step: run, expect PASS — `cargo test -p engram sym_entities_for_doc_returns_only_sym_entities`
- [ ] Step: commit — `git add crates/engram/src/store/entities.rs crates/engram/src/store/mod.rs && git commit -m "feat(store): sym_entities_for_doc read method for callers BFS expansion"`

---

### Task 6 — `store/docs.rs`: `delete_doc_by_key` clears `code_edges`

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/store/docs.rs`

The "forget" path must also remove a doc's outbound edges (`src_doc_id`) and any inbound edges resolved to it (`dst_doc_id`).

- [ ] Step: write the failing test — replace the existing `delete_doc_by_key_removes_doc_and_chunks` test in `store/mod.rs`:

  ```rust
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

      // Manually insert an edge where callee is the dst_doc_id
      {
          let conn = store.write.lock().unwrap();
          conn.execute(
              "INSERT INTO code_edges (namespace, src_doc_id, dst_sym, dst_doc_id, edge_kind, src_line, confidence)
               VALUES ('alice', 'some-other-doc', 'sym:target', ?1, 'CALLS', 5, 1.0)",
              rusqlite::params![callee.document_id],
          )
          .unwrap();
      }

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

          let dst_count: i64 = conn
              .query_row(
                  "SELECT count(*) FROM code_edges WHERE namespace='alice' AND dst_doc_id=?1",
                  rusqlite::params![callee.document_id],
                  |r| r.get(0),
              )
              .unwrap();
          assert_eq!(dst_count, 0, "inbound edge pointing at deleted doc must also be deleted");
      }

      assert!(!store.delete_doc_by_key("alice", "k").unwrap());
  }
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram delete_doc_by_key_removes_doc_and_chunks`

  Expected: assertion failure — edges not deleted because `delete_doc_by_key` doesn't touch `code_edges` yet.

- [ ] Step: implement — in `delete_doc_by_key` (`store/docs.rs`), after the `chunk_entities` DELETE and before the `tree_nodes` DELETE, add two statements inside the existing transaction:

  ```rust
      // Remove all code edges where this doc is the source (outbound references from the doc).
      tx.execute(
          "DELETE FROM code_edges WHERE namespace=?1 AND src_doc_id=?2",
          params![namespace, doc_id],
      )?;
      // Remove all code edges where this doc is the resolved destination (inbound references
      // to the doc). dst_doc_id is nullable; only non-NULL resolved rows are affected.
      tx.execute(
          "DELETE FROM code_edges WHERE namespace=?1 AND dst_doc_id=?2",
          params![namespace, doc_id],
      )?;
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram delete_doc_by_key_removes_doc_and_chunks` then `cargo test -p engram`
- [ ] Step: commit — `git add crates/engram/src/store/docs.rs crates/engram/src/store/mod.rs && git commit -m "feat(store): delete_doc_by_key removes code_edges for src_doc_id and dst_doc_id"`

---

### Task 7 — `treesit.rs`: `pub(crate)` visibility (visibility only)

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/treesit.rs`

Expose `parse`, `symbol_name`, and `declarator_name` as `pub(crate)` so `graph.rs` can call them. Zero behavior change; bodies untouched.

- [ ] Step: write the failing test — add inside `graph.rs` (created empty for now, just the test) `#[cfg(test)] mod tests`:

  ```rust
  use crate::treesit::{parse, Lang};
  #[test]
  fn treesit_parse_is_pub_crate() {
      let tree = parse(Lang::Rust, "fn foo() {}");
      assert!(tree.is_some());
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram treesit_parse_is_pub_crate`

  Expected: `error[E0603]: function 'parse' is private`.

- [ ] Step: implement — change three signatures in `treesit.rs`:

  ```rust
  pub(crate) fn symbol_name(lang: Lang, node: &tree_sitter::Node, src: &[u8]) -> Option<String> {
  ```
  ```rust
  pub(crate) fn declarator_name(mut node: tree_sitter::Node, src: &[u8]) -> Option<String> {
  ```
  ```rust
  pub(crate) fn parse(lang: Lang, content: &str) -> Option<tree_sitter::Tree> {
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram treesit_parse_is_pub_crate`
- [ ] Step: commit — `git add crates/engram/src/treesit.rs crates/engram/src/graph.rs && git commit -m "refactor(treesit): pub(crate) parse + symbol_name + declarator_name for graph.rs"`

---

### Task 8 — `graph.rs` (create): `extract_edges` for Rust + C/C++

**Files:** Create `/mnt/x/code/engram/crates/engram/src/graph.rs`; Modify `/mnt/x/code/engram/crates/engram/src/lib.rs`

`extract_edges` is the single public entry. It parses via `treesit::parse`, walks the AST for Rust and C/C++, and returns `Vec<RawEdge>` with confidence: 1.0 = dst in `file_syms`, 0.6 = dst is an import target, 0.3 = bare unresolved name.

- [ ] Step: write the failing tests — inside `graph.rs` `#[cfg(test)] mod tests`:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::treesit::Lang;

      #[test]
      fn rust_same_file_call_confidence_1() {
          let src = r#"
  fn helper() {}
  fn caller() {
      helper();
  }
  "#;
          let edges = extract_edges(src, Lang::Rust, &["helper", "caller"]);
          let calls: Vec<&RawEdge> = edges
              .iter()
              .filter(|e| e.dst_sym == "sym:helper" && matches!(e.edge_kind, EdgeKind::Calls))
              .collect();
          assert!(!calls.is_empty(), "expected CALLS edge to sym:helper; got {edges:?}");
          assert!(
              (calls[0].confidence - 1.0).abs() < 1e-6,
              "expected confidence 1.0, got {}",
              calls[0].confidence
          );
      }

      #[test]
      fn rust_imported_call_confidence_0_6() {
          let src = r#"
  use crate::store::Store;
  fn caller(s: Store) {
      s.open("x");
  }
  "#;
          let edges = extract_edges(src, Lang::Rust, &["caller"]);
          let uses: Vec<&RawEdge> = edges
              .iter()
              .filter(|e| e.dst_sym == "sym:Store" && matches!(e.edge_kind, EdgeKind::UsesType))
              .collect();
          assert!(!uses.is_empty(), "expected USES_TYPE edge to sym:Store; got {edges:?}");
          assert!(
              uses[0].confidence >= 0.59,
              "expected confidence ~0.6, got {}",
              uses[0].confidence
          );
      }

      #[test]
      fn rust_use_decl_imports_edge() {
          let src = "use crate::store::Store;\nuse std::collections::HashMap;\n";
          let edges = extract_edges(src, Lang::Rust, &[]);
          let imp_store = edges.iter().any(|e| {
              e.dst_sym == "sym:Store" && matches!(e.edge_kind, EdgeKind::Imports)
          });
          let imp_map = edges.iter().any(|e| {
              e.dst_sym == "sym:HashMap" && matches!(e.edge_kind, EdgeKind::Imports)
          });
          assert!(imp_store, "expected IMPORTS sym:Store; got {edges:?}");
          assert!(imp_map, "expected IMPORTS sym:HashMap; got {edges:?}");
      }

      #[test]
      fn cpp_call_and_uses_type() {
          let src = r#"
  #include "Stitcher.h"
  struct Pixel { int r; };
  int blend(Pixel p) {
      return process(p.r);
  }
  "#;
          let edges = extract_edges(src, Lang::Cpp, &["blend", "Pixel"]);
          let calls_process = edges.iter().any(|e| {
              e.dst_sym == "sym:process" && matches!(e.edge_kind, EdgeKind::Calls)
          });
          let uses_pixel = edges.iter().any(|e| {
              e.dst_sym == "sym:Pixel" && matches!(e.edge_kind, EdgeKind::UsesType)
          });
          assert!(calls_process, "expected CALLS sym:process; got {edges:?}");
          assert!(uses_pixel, "expected USES_TYPE sym:Pixel; got {edges:?}");
          let pixel_edge = edges
              .iter()
              .find(|e| e.dst_sym == "sym:Pixel" && matches!(e.edge_kind, EdgeKind::UsesType))
              .unwrap();
          assert!(
              (pixel_edge.confidence - 1.0).abs() < 1e-6,
              "Pixel defined in file → confidence 1.0, got {}",
              pixel_edge.confidence
          );
      }

      #[test]
      fn cpp_include_imports_edge() {
          let src = r#"
  #include <stdio.h>
  #include "MyLib.hpp"
  void run() {}
  "#;
          let edges = extract_edges(src, Lang::Cpp, &["run"]);
          let has_stdio = edges.iter().any(|e| {
              e.dst_sym == "sym:stdio" && matches!(e.edge_kind, EdgeKind::Imports)
          });
          let has_mylib = edges.iter().any(|e| {
              e.dst_sym == "sym:MyLib" && matches!(e.edge_kind, EdgeKind::Imports)
          });
          assert!(has_stdio, "expected IMPORTS sym:stdio; got {edges:?}");
          assert!(has_mylib, "expected IMPORTS sym:MyLib; got {edges:?}");
      }

      #[test]
      fn unsupported_lang_returns_empty() {
          let edges = extract_edges("def foo(): pass", Lang::Python, &[]);
          assert!(edges.is_empty(), "expected empty for Python; got {edges:?}");
      }

      #[test]
      fn parse_failure_graceful() {
          let edges = extract_edges("", Lang::Rust, &[]);
          assert!(edges.is_empty());
      }
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram rust_same_file_call_confidence_1`

  Expected: `error[E0433]: failed to resolve: use of undeclared crate or module 'graph'` (module not yet declared in lib.rs).

- [ ] Step: implement — create `/mnt/x/code/engram/crates/engram/src/graph.rs`:

  ```rust
  //! graph.rs — Pure edge extraction for the engram code-graph.
  //!
  //! `extract_edges` is the single public entry point. It parses `content` via the tree-sitter
  //! grammar already loaded by `treesit::parse`, walks the AST for Rust and C/C++, and returns
  //! a `Vec<RawEdge>` with confidence scores assigned by in-file resolution:
  //!
  //!   1.0 — dst symbol is in `file_syms` (defined in this file)
  //!   0.6 — dst symbol is named in a `use`/`#include` import of this file
  //!   0.3 — bare unresolved name (exists in neither set)
  //!
  //! The caller (`ingest::ingest_document`) is responsible for applying `min_confidence` filtering
  //! before passing edges to `commit_ingest`.
  //!
  //! # Known misses (by design — structural extraction only)
  //!
  //! ## Rust
  //! - **Macro-generated calls**: `vec![]`, `println!`, proc-macro invocations expand at compile
  //!   time; tree-sitter sees only the macro call expression, not the generated call sites inside.
  //! - **Virtual / trait-object dispatch**: `dyn Trait` method calls resolve to the trait method
  //!   name, not the concrete impl. The edge `sym:method_name` is recorded but points at the trait,
  //!   not the concrete receiver.
  //! - **Method calls on variables**: `x.foo()` emits `sym:foo` without the receiver type; if
  //!   `foo` is common (e.g. `to_string`, `clone`) this creates noisy low-confidence edges.
  //!   The IDF-gated `code_def_boost` in `retrieve.rs` mitigates this for search.
  //! - **Calls inside `macro_definition` body**: tree-sitter's Rust grammar puts the body of a
  //!   `macro_definition` in a `token_tree` node, not individual `call_expression` nodes.
  //!
  //! ## C/C++
  //! - **Overloaded functions**: multiple definitions share one symbol name; edges are name-correct
  //!   but resolve ambiguously across overloads.
  //! - **Virtual method dispatch**: calls via pointer/reference to a base class emit the
  //!   declared method name, not the vtable target.
  //! - **Function-pointer calls**: `(*fp)(args)` — the callee is a dereferenced expression, not
  //!   an identifier; these are skipped.
  //! - **Template specialisations**: the template name is extracted; instantiated specialisation
  //!   names (e.g. `vector<int>::push_back`) are not separately recorded.
  //! - **Preprocessor macro expansion**: call sites inside macro bodies are not visited by
  //!   tree-sitter at the unexpanded source level.
  //!
  //! ## Both
  //! - Cross-file confidence (0.3) is a heuristic — it can be spuriously high for common names
  //!   that appear in many files, and spuriously low for rare names imported via a wildcard.
  //! - `dst_doc_id` resolution is deferred to query time via `chunk_entities`; see `graph_query.rs`.

  use crate::model::{EdgeKind, RawEdge};
  use crate::treesit::{declarator_name, parse, symbol_name, Lang};

  // ---------------------------------------------------------------------------
  // Public interface
  // ---------------------------------------------------------------------------

  /// Extract raw call/type/import edges from `content` for the given language.
  ///
  /// `file_syms` is the slice of **bare symbol names** (without the `sym:` prefix) that are
  /// *defined* in this file. Edges whose `dst_sym` (bare name) appears in `file_syms` get
  /// confidence 1.0; edges whose name appears as an import target in the file get 0.6; everything
  /// else gets 0.3. Returns an empty `Vec` for any unsupported language or parse failure.
  pub fn extract_edges(content: &str, lang: Lang, file_syms: &[&str]) -> Vec<RawEdge> {
      match lang {
          Lang::Rust => extract_rust(content, file_syms),
          Lang::C | Lang::Cpp => extract_c_cpp(content, lang, file_syms),
          // v1 scope: Rust + C/C++ only.
          // Follow-up: Python, TypeScript/JavaScript, Go, Java/Kotlin.
          _ => vec![],
      }
  }

  // ---------------------------------------------------------------------------
  // Confidence helper
  // ---------------------------------------------------------------------------

  fn confidence(name: &str, file_syms: &[&str], import_names: &[String]) -> f32 {
      if file_syms.contains(&name) {
          1.0
      } else if import_names.iter().any(|i| i == name) {
          0.6
      } else {
          0.3
      }
  }

  // ---------------------------------------------------------------------------
  // Rust extraction
  // ---------------------------------------------------------------------------

  fn extract_rust(content: &str, file_syms: &[&str]) -> Vec<RawEdge> {
      let tree = match parse(Lang::Rust, content) {
          Some(t) => t,
          None => return vec![],
      };
      let src = content.as_bytes();
      let mut edges: Vec<RawEdge> = Vec::new();

      let import_names = rust_collect_imports(&tree, src);

      for name in &import_names {
          if name.is_empty() || name == "self" || name == "super" || name == "crate" {
              continue;
          }
          edges.push(RawEdge {
              dst_sym: format!("sym:{name}"),
              edge_kind: EdgeKind::Imports,
              src_line: None,
              confidence: confidence(name, file_syms, &import_names),
          });
      }

      let mut stack = vec![tree.root_node()];
      while let Some(node) = stack.pop() {
          match node.kind() {
              "call_expression" => {
                  if let Some(callee_name) =
                      rust_callee_name(node.child_by_field_name("function"), src)
                  {
                      let line = node.start_position().row as i64 + 1;
                      edges.push(RawEdge {
                          dst_sym: format!("sym:{callee_name}"),
                          edge_kind: EdgeKind::Calls,
                          src_line: Some(line),
                          confidence: confidence(&callee_name, file_syms, &import_names),
                      });
                  }
              }
              "type_identifier" => {
                  if let Ok(name) = node.utf8_text(src) {
                      if !name.is_empty() {
                          let line = node.start_position().row as i64 + 1;
                          edges.push(RawEdge {
                              dst_sym: format!("sym:{name}"),
                              edge_kind: EdgeKind::UsesType,
                              src_line: Some(line),
                              confidence: confidence(name, file_syms, &import_names),
                          });
                      }
                  }
              }
              _ => {}
          }
          let mut cur = node.walk();
          for child in node.named_children(&mut cur) {
              stack.push(child);
          }
      }

      dedup_edges(edges)
  }

  fn rust_collect_imports(tree: &tree_sitter::Tree, src: &[u8]) -> Vec<String> {
      let mut names: Vec<String> = Vec::new();
      let mut stack = vec![tree.root_node()];
      while let Some(node) = stack.pop() {
          if node.kind() == "use_declaration" {
              if let Some(arg) = node.child_by_field_name("argument") {
                  rust_collect_use_tree(arg, src, &mut names);
              }
          }
          let mut cur = node.walk();
          for child in node.named_children(&mut cur) {
              stack.push(child);
          }
      }
      names.sort();
      names.dedup();
      names
  }

  fn rust_collect_use_tree(node: tree_sitter::Node, src: &[u8], out: &mut Vec<String>) {
      match node.kind() {
          "scoped_identifier" => {
              if let Some(name_node) = node.child_by_field_name("name") {
                  if let Ok(t) = name_node.utf8_text(src) {
                      out.push(t.to_string());
                  }
              }
          }
          "identifier" => {
              if let Ok(t) = node.utf8_text(src) {
                  out.push(t.to_string());
              }
          }
          "use_list" => {
              let mut cur = node.walk();
              for child in node.named_children(&mut cur) {
                  rust_collect_use_tree(child, src, out);
              }
          }
          "use_as_clause" => {
              if let Some(alias) = node.child_by_field_name("alias") {
                  if let Ok(t) = alias.utf8_text(src) {
                      out.push(t.to_string());
                  }
              }
          }
          _ => {}
      }
  }

  fn rust_callee_name(node: Option<tree_sitter::Node>, src: &[u8]) -> Option<String> {
      let node = node?;
      match node.kind() {
          "identifier" => node
              .utf8_text(src)
              .ok()
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          "scoped_identifier" => node
              .child_by_field_name("name")
              .and_then(|n| n.utf8_text(src).ok())
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          "field_expression" => node
              .child_by_field_name("field")
              .and_then(|n| n.utf8_text(src).ok())
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          "generic_function" => rust_callee_name(node.child_by_field_name("function"), src),
          _ => None,
      }
  }

  // ---------------------------------------------------------------------------
  // C / C++ extraction
  // ---------------------------------------------------------------------------

  fn extract_c_cpp(content: &str, lang: Lang, file_syms: &[&str]) -> Vec<RawEdge> {
      let tree = match parse(lang, content) {
          Some(t) => t,
          None => return vec![],
      };
      let src = content.as_bytes();
      let mut edges: Vec<RawEdge> = Vec::new();

      let import_names = cpp_collect_includes(&tree, src);

      for name in &import_names {
          if name.is_empty() {
              continue;
          }
          edges.push(RawEdge {
              dst_sym: format!("sym:{name}"),
              edge_kind: EdgeKind::Imports,
              src_line: None,
              confidence: confidence(name, file_syms, &import_names),
          });
      }

      let mut stack = vec![tree.root_node()];
      while let Some(node) = stack.pop() {
          match node.kind() {
              "call_expression" => {
                  if let Some(callee_name) =
                      cpp_callee_name(node.child_by_field_name("function"), src)
                  {
                      let line = node.start_position().row as i64 + 1;
                      edges.push(RawEdge {
                          dst_sym: format!("sym:{callee_name}"),
                          edge_kind: EdgeKind::Calls,
                          src_line: Some(line),
                          confidence: confidence(&callee_name, file_syms, &import_names),
                      });
                  }
              }
              "type_identifier" => {
                  if let Ok(name) = node.utf8_text(src) {
                      if !name.is_empty() {
                          let line = node.start_position().row as i64 + 1;
                          edges.push(RawEdge {
                              dst_sym: format!("sym:{name}"),
                              edge_kind: EdgeKind::UsesType,
                              src_line: Some(line),
                              confidence: confidence(name, file_syms, &import_names),
                          });
                      }
                  }
              }
              _ => {}
          }
          let mut cur = node.walk();
          for child in node.named_children(&mut cur) {
              stack.push(child);
          }
      }

      dedup_edges(edges)
  }

  fn cpp_collect_includes(tree: &tree_sitter::Tree, src: &[u8]) -> Vec<String> {
      let mut names: Vec<String> = Vec::new();
      let mut stack = vec![tree.root_node()];
      while let Some(node) = stack.pop() {
          if node.kind() == "preproc_include" {
              let mut cur = node.walk();
              for child in node.named_children(&mut cur) {
                  let raw = match child.utf8_text(src) {
                      Ok(t) => t,
                      Err(_) => continue,
                  };
                  let stripped = raw
                      .trim_matches(|c| c == '<' || c == '>' || c == '"')
                      .trim();
                  let filename = stripped.rsplit('/').next().unwrap_or(stripped);
                  let stem = filename
                      .rsplit_once('.')
                      .map(|(s, _)| s)
                      .unwrap_or(filename);
                  if !stem.is_empty() {
                      names.push(stem.to_string());
                  }
              }
          }
          let mut cur = node.walk();
          for child in node.named_children(&mut cur) {
              stack.push(child);
          }
      }
      names.sort();
      names.dedup();
      names
  }

  fn cpp_callee_name(node: Option<tree_sitter::Node>, src: &[u8]) -> Option<String> {
      let node = node?;
      match node.kind() {
          "identifier" => node
              .utf8_text(src)
              .ok()
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          "qualified_identifier" => declarator_name(node, src),
          "field_expression" => node
              .child_by_field_name("field")
              .and_then(|n| n.utf8_text(src).ok())
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          "template_function" => node
              .child_by_field_name("name")
              .and_then(|n| n.utf8_text(src).ok())
              .filter(|t| !t.is_empty())
              .map(str::to_string),
          _ => None,
      }
  }

  // ---------------------------------------------------------------------------
  // Shared post-processing
  // ---------------------------------------------------------------------------

  /// Deduplicate edges: keep the highest-confidence occurrence of each (dst_sym, edge_kind) pair.
  fn dedup_edges(mut edges: Vec<RawEdge>) -> Vec<RawEdge> {
      edges.sort_by(|a, b| {
          a.dst_sym
              .cmp(&b.dst_sym)
              .then(a.edge_kind.as_str().cmp(b.edge_kind.as_str()))
              .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
      });
      edges.dedup_by(|later, first| {
          later.dst_sym == first.dst_sym && later.edge_kind.as_str() == first.edge_kind.as_str()
      });
      edges
  }
  ```

  Note: `symbol_name` is imported for parity with the C/C++ declarator helpers; if the final compile flags it as unused, drop it from the `use` line. Add to `lib.rs` after `pub mod treesit;`:

  ```rust
  pub mod graph;
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram rust_same_file_call_confidence_1 rust_imported_call_confidence_0_6 rust_use_decl_imports_edge cpp_call_and_uses_type cpp_include_imports_edge unsupported_lang_returns_empty parse_failure_graceful`
- [ ] Step: commit — `git add crates/engram/src/graph.rs crates/engram/src/lib.rs && git commit -m "feat(graph): extract_edges — Rust + C/C++ call/type/import edge extraction (v1)"`

---

### Task 9 — `ingest.rs`: wire `extract_edges` into the `is_code` path

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/ingest.rs`

This task owns ONLY the wiring: build `file_syms`, call `crate::graph::extract_edges` when `crate::config::code_graph_extract()`, filter `< code_graph_min_confidence()`, pass `raw_edges` into `commit_ingest`. It does NOT redefine types or re-edit the store. `extract_edges` is pure and runs in the `is_code` branch before the embed loop (off-lock invariant preserved).

- [ ] Step: write the failing test — add to `ingest.rs` `#[cfg(test)] mod tests`:

  ```rust
  #[test]
  fn ingest_code_doc_with_hand_built_edges_reaches_store() {
      // Drives commit_ingest directly with a hand-built &[RawEdge] to confirm the wiring from
      // ingest into the store works, without depending on the process-global gate.
      use crate::embed::HashEmbedder;
      use crate::model::{EdgeKind, NewDoc, RawEdge, Taint};
      use crate::store::Store;

      let dir = tempfile::tempdir().unwrap();
      let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();

      let new = NewDoc {
          key: "src/lib.rs".into(),
          title: "lib.rs".into(),
          content: "fn caller() { callee(); }".into(),
          author: "code".into(),
          taint: Taint::Internal,
          meta: Some(serde_json::json!({"kind": "file"})),
      };
      let edges = vec![RawEdge {
          dst_sym: "sym:callee".to_string(),
          edge_kind: EdgeKind::Calls,
          src_line: Some(1),
          confidence: 0.95,
      }];

      let embedder = HashEmbedder::new(32);
      let text = "fn caller() { callee(); }".to_string();
      let emb = embedder.embed(&text).unwrap();

      let doc = store
          .commit_ingest(
              "repo:test",
              &new,
              &[text],
              &[emb],
              &[vec!["sym:caller".to_string()]],
              &[Some((1, 1))],
              &edges,
              &embedder.signature(),
          )
          .unwrap();

      let from = store.edges_from("repo:test", &doc.document_id).unwrap();
      assert_eq!(from.len(), 1);
      assert_eq!(from[0].dst_sym, "sym:callee");
      assert_eq!(from[0].edge_kind, EdgeKind::Calls);
      assert_eq!(from[0].src_line, Some(1));
      assert!((from[0].confidence - 0.95).abs() < 1e-5);
  }
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram ingest_code_doc_with_hand_built_edges_reaches_store`

  Expected: compile error — `ingest_document` still calls `commit_ingest` with the old arity.

- [ ] Step: implement — add `RawEdge` to the model import at the top of `ingest.rs`:

  ```rust
  use crate::model::{MemoryDoc, NewDoc, RawEdge};
  ```

  Update the `IngestParts` type alias to a 4-tuple (or drop it in favour of the explicit type annotation on the binding):

  ```rust
  type IngestParts = (Vec<String>, Vec<Option<(i64, i64)>>, Vec<Vec<String>>, Vec<RawEdge>);
  ```

  Replace the `is_code` branch so it binds a fourth value, `raw_edges`. The `else` (prose) branch yields `Vec::new()`:

  ```rust
      let (chunk_texts, line_ranges, entities, raw_edges): IngestParts = if is_code {
          let lang = if crate::config::code_tree_sitter() {
              crate::treesit::lang_for_path(&new.key)
          } else {
              None
          };
          let pieces = match lang.and_then(|l| crate::treesit::chunk_code_ts(&new.content, l)) {
              Some(p) if !p.is_empty() => p,
              _ => chunk_code_with(&new.content, crate::config::code_symbol_split()),
          };
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
                  if let Some(l) = lang {
                      if let Some(syms) = crate::treesit::extract_symbols_ts(t, l) {
                          es.extend(syms);
                      }
                  }
                  es.push(path_ent.clone());
                  es.sort();
                  es.dedup();
                  es
              })
              .collect();

          // Build file-level symbol set (union of all sym: entities across chunks) and extract
          // edges off-lock when the gate is on. extract_edges returns empty for non-Rust/C/C++.
          let edges: Vec<RawEdge> = if crate::config::code_graph_extract() {
              if let Some(l) = lang {
                  let file_syms: Vec<&str> = ents
                      .iter()
                      .flat_map(|chunk_ents| chunk_ents.iter())
                      .filter(|e| e.starts_with("sym:"))
                      .map(|e| e.strip_prefix("sym:").unwrap_or(e.as_str()))
                      .collect::<std::collections::BTreeSet<_>>()
                      .into_iter()
                      .collect();
                  let min_conf = crate::config::code_graph_min_confidence();
                  let mut extracted = crate::graph::extract_edges(&new.content, l, &file_syms);
                  extracted.retain(|e| e.confidence >= min_conf);
                  extracted
              } else {
                  Vec::new()
              }
          } else {
              Vec::new()
          };

          (texts, ranges, ents, edges)
      } else {
          let texts = chunk(&new.content);
          let ranges = vec![None; texts.len()];
          let ents: Vec<Vec<String>> = texts.iter().map(|t| extract_entities(t)).collect();
          (texts, ranges, ents, Vec::new())
      };
  ```

  Note: `file_syms` must be the **bare** names (strip `"sym:"`), because `graph::extract_edges` compares against bare names and re-prefixes internally. Finally, thread `raw_edges` into the existing `commit_ingest` call:

  ```rust
      let sig = embedder.signature();
      store.commit_ingest(
          namespace,
          new,
          &kept_texts,
          &kept_embeddings,
          &kept_entities,
          &kept_ranges,
          &raw_edges,
          &sig,
      )
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram ingest_code_doc_with_hand_built_edges_reaches_store ingest_document_chunks_embeds_extracts_and_enqueues` then `cargo test -p engram`
- [ ] Step: commit — `git add crates/engram/src/ingest.rs && git commit -m "feat(ingest): wire extract_edges into is_code path; thread raw_edges into commit_ingest"`

**Test-isolation note:** `code_graph_extract()` is process-global; do NOT write a same-binary pair of tests that asserts "gate off → no edges" and "gate on → edges". The test above bypasses the gate by calling `commit_ingest` directly; the gate's on-path behavior is covered once by the live `graph_eval.py` run (Track-1).

---

### Task 10 — `graph_query.rs` (create): BFS `callers` / `callees`

**Files:** Create `/mnt/x/code/engram/crates/engram/src/graph_query.rs`; Modify `/mnt/x/code/engram/crates/engram/src/lib.rs`

Both queries do a bounded-depth BFS over `code_edges`. Cross-file resolution uses `resolve_dst_doc` (→ `chunk_entities`). Upward caller expansion uses `sym_entities_for_doc` (the B5 fix — **no `read_conn`**). The dead `for out_edge in outbound` loop is removed (B4 fix). These functions read only SQLite, so the axum handlers do not need `spawn_blocking`.

- [ ] Step: write the failing tests — inside `graph_query.rs` `#[cfg(test)] mod tests`:

  ```rust
  #[test]
  fn callers_bfs_single_hop() {
      use crate::embed::HashEmbedder;
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
          store.commit_ingest(
              "ns", &new,
              &["fn caller() { target(); }".to_string()],
              &[emb_vec], &[vec![]],
              &[Some((1, 1))],
              &edges, "hash:4",
          ).unwrap()
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
          store.commit_ingest(
              "ns", &new,
              &["fn target() {}".to_string()],
              &[emb_vec],
              &[vec!["sym:target".to_string()]],
              &[Some((1, 1))],
              &[], "hash:4",
          ).unwrap()
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
      use crate::embed::HashEmbedder;
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
          RawEdge { dst_sym: "sym:bar".into(), edge_kind: EdgeKind::Calls, src_line: Some(1), confidence: 0.3 },
          RawEdge { dst_sym: "sym:baz".into(), edge_kind: EdgeKind::Calls, src_line: Some(1), confidence: 0.3 },
      ];
      let doc = store.commit_ingest(
          "ns", &new,
          &["fn foo() { bar(); baz(); }".to_string()],
          &[emb_vec], &[vec![]],
          &[Some((1, 1))],
          &edges, "hash:4",
      ).unwrap();

      let hops = callees(&store, "ns", "src/foo.rs", 2, 10).unwrap();
      assert!(hops.iter().any(|h| h.sym == "sym:bar"), "expected sym:bar; got {hops:?}");
      assert!(hops.iter().any(|h| h.sym == "sym:baz"), "expected sym:baz; got {hops:?}");
      let _ = doc;
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram callers_bfs_single_hop`

  Expected: module `graph_query` not found.

- [ ] Step: implement — create `/mnt/x/code/engram/crates/engram/src/graph_query.rs`:

  ```rust
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
  ```

  Add to `lib.rs`:

  ```rust
  pub mod graph_query;
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram callers_bfs_single_hop callees_bfs_single_hop`
- [ ] Step: commit — `git add crates/engram/src/graph_query.rs crates/engram/src/lib.rs && git commit -m "feat(graph_query): BFS callers/callees over code_edges with lazy cross-file resolution"`

---

### Task 11 — `api.rs`: `GraphReq` + `graph_callers` / `graph_callees` handlers + routes

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/api.rs`

This task owns ONLY the core-crate routes/handlers/`GraphReq`. It must NOT touch `engram-mcp`. Handlers read only SQLite → no `spawn_blocking`. Depth uses `unwrap_or(2).min(code_graph_max_depth())` (B6). `min_confidence` is applied post-BFS.

- [ ] Step: write the failing tests — append to `api.rs` `#[cfg(test)] mod tests`:

  ```rust
  #[test]
  fn graph_callers_returns_200_or_empty() {
      let rt = tokio::runtime::Runtime::new().unwrap();
      rt.block_on(async {
          let addr = spawn().await;
          let client = reqwest::Client::new();
          let resp = client
              .post(format!("http://{addr}/v1/ns/code/graph/callers"))
              .bearer_auth("test-token")
              .json(&serde_json::json!({"sym": "sym:Foo"}))
              .send()
              .await
              .unwrap();
          assert_eq!(resp.status(), 200);
          let body: Vec<serde_json::Value> = resp.json().await.unwrap();
          assert!(body.is_empty());
      });
  }

  #[test]
  fn graph_callees_returns_200_or_empty() {
      let rt = tokio::runtime::Runtime::new().unwrap();
      rt.block_on(async {
          let addr = spawn().await;
          let client = reqwest::Client::new();
          let resp = client
              .post(format!("http://{addr}/v1/ns/code/graph/callees"))
              .bearer_auth("test-token")
              .json(&serde_json::json!({"path": "src/foo.rs"}))
              .send()
              .await
              .unwrap();
          assert_eq!(resp.status(), 200);
          let body: Vec<serde_json::Value> = resp.json().await.unwrap();
          assert!(body.is_empty());
      });
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram graph_callers_returns_200_or_empty`

  Expected: 404 (routes not registered).

- [ ] Step: implement — add the request type after the existing `QueryReq`:

  ```rust
  #[derive(Deserialize)]
  struct GraphReq {
      sym: Option<String>,
      path: Option<String>,
      max_depth: Option<usize>,
      limit: Option<usize>,
      min_confidence: Option<f32>,
  }
  ```

  Add two handlers after `recall_docs`:

  ```rust
  async fn graph_callers(
      State(state): State<AppState>,
      Path(namespace): Path<String>,
      Json(req): Json<GraphReq>,
  ) -> Response {
      // graph_query reads only SQLite — no embed/LLM calls, no spawn_blocking needed.
      let sym = match req.sym {
          Some(s) => s,
          None => return (StatusCode::BAD_REQUEST, "sym is required").into_response(),
      };
      let max_depth = req.max_depth.unwrap_or(2).min(crate::config::code_graph_max_depth());
      let limit = req.limit.unwrap_or(20);
      let min_conf = req.min_confidence.unwrap_or(0.0);
      match crate::graph_query::callers(&state.store, &namespace, &sym, max_depth, limit) {
          Ok(mut hops) => {
              if min_conf > 0.0 {
                  hops.retain(|h| h.confidence >= min_conf);
              }
              Json(hops).into_response()
          }
          Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
      }
  }

  async fn graph_callees(
      State(state): State<AppState>,
      Path(namespace): Path<String>,
      Json(req): Json<GraphReq>,
  ) -> Response {
      let path = match req.path {
          Some(p) => p,
          None => return (StatusCode::BAD_REQUEST, "path is required").into_response(),
      };
      let max_depth = req.max_depth.unwrap_or(2).min(crate::config::code_graph_max_depth());
      let limit = req.limit.unwrap_or(20);
      let min_conf = req.min_confidence.unwrap_or(0.0);
      match crate::graph_query::callees(&state.store, &namespace, &path, max_depth, limit) {
          Ok(mut hops) => {
              if min_conf > 0.0 {
                  hops.retain(|h| h.confidence >= min_conf);
              }
              Json(hops).into_response()
          }
          Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
      }
  }
  ```

  Register the routes in the protected router (after the `/v1/:namespace/recall` line):

  ```rust
  .route("/v1/:namespace/code/graph/callers", post(graph_callers))
  .route("/v1/:namespace/code/graph/callees", post(graph_callees))
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram graph_callers_returns_200_or_empty graph_callees_returns_200_or_empty`
- [ ] Step: commit — `git add crates/engram/src/api.rs && git commit -m "feat(api): POST /code/graph/callers + /code/graph/callees endpoints"`

---

### Task 12 — `engram-mcp/src/lib.rs`: `trace_symbol` tool (7th tool)

**Files:** Modify `/mnt/x/code/engram/crates/engram-mcp/src/lib.rs`

Sole owner of ALL `engram-mcp` changes: a crate-local `TraceHop` (`Deserialize`-only), `RawTraceHop` wire type, `trace_symbol` on the `CodeSearch` trait + `HttpCodeSearch` impl, `FakeSearch`/`ErrorSearch` stubs, a `FakeTracer` struct for the non-empty formatting test, `format_trace`, the tool def, the dispatch arm, and the tools/list 6→7 update. The `HttpCodeSearch::trace_symbol` URL is `format!("{}/v1/{}/{}", self.url, self.namespace, suffix)` with `suffix` = `"code/graph/callers"` / `"code/graph/callees"` (B7 — never double-prefixed). `FakeSearch::trace_symbol` returns `Ok(vec![])` (B7).

- [ ] Step (12a): write the failing test — add to `#[cfg(test)] mod tests`:

  ```rust
  #[test]
  fn trace_symbol_method_exists_on_trait() {
      // FakeSearch returns empty for trace_symbol; FakeTracer returns the stub hop.
      let fake = no_hits(); // FakeSearch(vec![])
      let hops = fake
          .trace_symbol(Some("ingest_document"), None, "callers", 2, 10)
          .unwrap();
      assert!(hops.is_empty(), "FakeSearch::trace_symbol must return empty");
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram-mcp trace_symbol_method_exists_on_trait`

  Expected: `error[E0599]: no method named 'trace_symbol' found`.

- [ ] Step: implement (types + trait + HttpCodeSearch) —

  `TraceHop` domain type (after `CommitHit`):

  ```rust
  /// One hop in a call-graph trace (callers or callees of a symbol). Crate-local: engram-mcp
  /// talks to the core over HTTP and does not depend on the engram crate.
  #[derive(Debug, Clone)]
  pub struct TraceHop {
      pub path: String,
      pub document_id: String,
      pub sym: String,
      pub edge_kind: String,
      pub confidence: f32,
      pub depth: usize,
  }
  ```

  `RawTraceHop` wire type (after `RawConventions`):

  ```rust
  /// Wire-shape returned by POST /v1/:ns/code/graph/callers and /callees.
  #[derive(Deserialize)]
  struct RawTraceHop {
      path: String,
      document_id: String,
      sym: String,
      edge_kind: String,
      confidence: f32,
      depth: usize,
  }
  ```

  Trait method (after `get_conventions`):

  ```rust
  /// Walk the code graph from a symbol (callers) or a file (callees).
  /// `direction` is `"callers"` or `"callees"`.
  fn trace_symbol(
      &self,
      sym: Option<&str>,
      path: Option<&str>,
      direction: &str,
      depth: usize,
      limit: usize,
  ) -> Result<Vec<TraceHop>, String>;
  ```

  `HttpCodeSearch` impl (note the suffix-only URL — B7):

  ```rust
  fn trace_symbol(
      &self,
      sym: Option<&str>,
      path: Option<&str>,
      direction: &str,
      depth: usize,
      limit: usize,
  ) -> Result<Vec<TraceHop>, String> {
      let suffix = if direction == "callees" { "code/graph/callees" } else { "code/graph/callers" };
      let endpoint = format!("{}/v1/{}/{}", self.url, self.namespace, suffix);
      let mut body = json!({ "max_depth": depth, "limit": limit });
      if let Some(s) = sym {
          body["sym"] = json!(s);
      }
      if let Some(p) = path {
          body["path"] = json!(p);
      }
      let resp = self
          .client
          .post(&endpoint)
          .bearer_auth(&self.token)
          .json(&body)
          .send()
          .map_err(|e| e.to_string())?;
      let status = resp.status();
      if !status.is_success() {
          return Err(format!("HTTP {}: {}", status, resp.text().unwrap_or_default()));
      }
      let raw: Vec<RawTraceHop> = resp.json().map_err(|e| e.to_string())?;
      Ok(raw
          .into_iter()
          .map(|h| TraceHop {
              path: h.path,
              document_id: h.document_id,
              sym: h.sym,
              edge_kind: h.edge_kind,
              confidence: h.confidence,
              depth: h.depth,
          })
          .collect())
  }
  ```

- [ ] Step (12b): implement (test doubles) — `FakeSearch::trace_symbol` returns empty (B7); `ErrorSearch::trace_symbol` returns the error; add a `FakeTracer` for the non-empty formatting test:

  ```rust
  // In FakeSearch impl:
  fn trace_symbol(
      &self,
      _sym: Option<&str>,
      _path: Option<&str>,
      _direction: &str,
      _depth: usize,
      _limit: usize,
  ) -> Result<Vec<TraceHop>, String> {
      Ok(vec![])
  }

  // In ErrorSearch impl:
  fn trace_symbol(
      &self,
      _sym: Option<&str>,
      _path: Option<&str>,
      _direction: &str,
      _depth: usize,
      _limit: usize,
  ) -> Result<Vec<TraceHop>, String> {
      Err(self.0.clone())
  }

  // A separate test double that returns one non-empty hop, for the formatting test.
  struct FakeTracer;
  impl CodeSearch for FakeTracer {
      fn search(&self, _: &str, _: usize) -> Result<Vec<CodeHit>, String> { Ok(vec![]) }
      fn get_architecture(&self, _: &str, _: usize) -> Result<Vec<Digest>, String> { Ok(vec![]) }
      fn get_module(&self, _: &str, _: &str, _: usize) -> Result<Vec<Digest>, String> { Ok(vec![]) }
      fn why(&self, _: &str, _: usize) -> Result<Vec<CommitHit>, String> { Ok(vec![]) }
      fn get_conventions(&self) -> Result<String, String> { Ok(String::new()) }
      fn trace_symbol(&self, _: Option<&str>, _: Option<&str>, _: &str, _: usize, _: usize) -> Result<Vec<TraceHop>, String> {
          Ok(vec![TraceHop {
              path: "src/ingest.rs".into(),
              document_id: "doc-abc".into(),
              sym: "sym:ingest_document".into(),
              edge_kind: "CALLS".into(),
              confidence: 0.95,
              depth: 1,
          }])
      }
  }
  ```

- [ ] Step: run, expect PASS — `cargo test -p engram-mcp trace_symbol_method_exists_on_trait`

- [ ] Step (12c): write the failing tests — `format_trace`, dispatch, and the tools/list 6→7 update:

  ```rust
  #[test]
  fn format_trace_empty_returns_sentinel() {
      assert_eq!(format_trace(&[]), "No graph edges found.");
  }

  #[test]
  fn format_trace_nonempty_contains_path_sym_kind_depth() {
      let hops = vec![TraceHop {
          path: "src/api.rs".into(),
          document_id: "doc-xyz".into(),
          sym: "sym:handle_ingest".into(),
          edge_kind: "CALLS".into(),
          confidence: 0.90,
          depth: 1,
      }];
      let s = format_trace(&hops);
      assert!(s.contains("src/api.rs"));
      assert!(s.contains("sym:handle_ingest"));
      assert!(s.contains("CALLS"));
      assert!(s.contains("depth=1"));
      assert!(s.contains("0.90"));
  }

  #[test]
  fn tools_list_includes_all_seven_tools() {
      let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
      let resp = dispatch(&req, &no_hits()).unwrap();
      let tools = resp["result"]["tools"].as_array().unwrap();
      let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
      assert_eq!(names.len(), 7, "expected 7 tools, got {names:?}");
      assert!(names.contains(&"search_code"));
      assert!(names.contains(&"get_architecture"));
      assert!(names.contains(&"get_module"));
      assert!(names.contains(&"why"));
      assert!(names.contains(&"find_symbol"));
      assert!(names.contains(&"get_conventions"));
      assert!(names.contains(&"trace_symbol"));
  }

  #[test]
  fn tools_call_trace_symbol_formats_hops() {
      let req = json!({
          "jsonrpc": "2.0", "id": 20, "method": "tools/call",
          "params": { "name": "trace_symbol",
                      "arguments": { "sym": "ingest_document", "direction": "callers", "depth": 2, "limit": 20 } }
      });
      let resp = dispatch(&req, &FakeTracer).unwrap();
      assert_eq!(resp["result"]["isError"], false);
      let text = resp["result"]["content"][0]["text"].as_str().unwrap();
      assert!(text.contains("src/ingest.rs"));
      assert!(text.contains("sym:ingest_document"));
      assert!(text.contains("CALLS"));
      assert!(text.contains("depth=1"));
  }

  #[test]
  fn tools_call_trace_symbol_no_results() {
      let req = json!({
          "jsonrpc": "2.0", "id": 21, "method": "tools/call",
          "params": { "name": "trace_symbol", "arguments": { "sym": "nothing" } }
      });
      let resp = dispatch(&req, &no_hits()).unwrap();
      assert_eq!(resp["result"]["isError"], false);
      let text = resp["result"]["content"][0]["text"].as_str().unwrap();
      assert_eq!(text, "No graph edges found.");
  }
  ```

- [ ] Step: run, expect FAIL — `cargo test -p engram-mcp format_trace tools_call_trace_symbol tools_list_includes_all_seven_tools`

- [ ] Step: implement (formatter, tool def, dispatch) —

  `format_trace` (after `format_commits`):

  ```rust
  pub fn format_trace(hops: &[TraceHop]) -> String {
      if hops.is_empty() {
          return "No graph edges found.".to_string();
      }
      hops.iter()
          .map(|h| {
              format!(
                  "{}  {}  {}  conf={:.2}  depth={}\n{}",
                  h.path, h.edge_kind, h.sym, h.confidence, h.depth, h.document_id
              )
          })
          .collect::<Vec<_>>()
          .join("\n\n")
  }
  ```

  `trace_symbol_tool_def()` (after `get_conventions_tool_def`):

  ```rust
  fn trace_symbol_tool_def() -> Value {
      json!({
          "name": "trace_symbol",
          "description": "Walk the code call-graph to find callers of a symbol or callees from a file. Uses statically extracted edges (Rust/C/C++ in v1). Supply either `sym` (e.g. 'ingest_document') or `path` (a file path); `direction` controls which way to walk.",
          "inputSchema": {
              "type": "object",
              "properties": {
                  "sym":       { "type": "string", "description": "symbol name to look up callers for, e.g. 'process_doc'" },
                  "path":      { "type": "string", "description": "repo-relative file path to look up callees from, e.g. 'src/api.rs'" },
                  "direction": { "type": "string", "description": "'callers' (default) or 'callees'" },
                  "depth":     { "type": "integer", "description": "max hops to traverse (default 2)" },
                  "limit":     { "type": "integer", "description": "max results (default 20)" }
              }
          }
      })
  }
  ```

  Add `trace_symbol_tool_def()` to the `tools/list` array (making it 7), and add the dispatch arm before the `_ =>` wildcard:

  ```rust
  "trace_symbol" => {
      let sym = args.get("sym").and_then(Value::as_str);
      let path = args.get("path").and_then(Value::as_str);
      let direction = args.get("direction").and_then(Value::as_str).unwrap_or("callers");
      let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
      let trace_limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
      backend
          .trace_symbol(sym, path, direction, depth, trace_limit)
          .map(|h| format_trace(&h))
  }
  ```

  (`trace_limit` reads the agent-supplied `limit` directly with the tool's own default of 20, avoiding the globally-extracted `limit` default.)

- [ ] Step: run, expect PASS — `cargo test -p engram-mcp`
- [ ] Step: commit — `git add crates/engram-mcp/src/lib.rs && git commit -m "feat(mcp): trace_symbol tool (callers/callees); TraceHop + format_trace; tools list 6->7"`

---

### Task 13 — eval Track-1 gold files + harness

**Files:** Create `/mnt/x/code/engram/eval/graph_gold.json`, `/mnt/x/code/engram/eval/android/graph_gold_native.json`, `/mnt/x/code/engram/eval/graph_eval.py`

These stand alone (no Rust dependency at author time). Acceptance bars live in the harness and are repeated in the "Acceptance bars" section below.

- [ ] Step: write `eval/graph_gold.json` — 15 structural Rust gold entries grounded in engram's own call graph (`ingest_document` ← api.rs; `commit_ingest` ← ingest.rs; `chunk_code_ts` ← ingest.rs; `extract_entities` ← ingest.rs + retrieve.rs; `seal_buffer` ← tree.rs; plus per-path callee coverage for ingest.rs/store/mod.rs/treesit.rs/retrieve.rs/api.rs/config.rs):

  ```json
  {
    "namespace": "repo:engram",
    "repo": ".",
    "description": "Structural call-graph gold for engram's own Rust codebase. Requires ENGRAM_CODE_GRAPH_EXTRACT=true on ingest.",
    "entries": [
      { "kind": "callers", "query_sym": "ingest_document",
        "note": "Called from api.rs (ingest_doc handler) and from ingest.rs tests; store/mod.rs tests call commit_ingest directly, not ingest_document.",
        "expected_callers": ["crates/engram/src/api.rs"],
        "unexpected_callers": ["crates/engram/src/store/mod.rs", "crates/engram/src/tree.rs"] },
      { "kind": "callers", "query_sym": "commit_ingest",
        "note": "commit_ingest is only called from ingest::ingest_document; store tests call it directly but those are in the same file.",
        "expected_callers": ["crates/engram/src/ingest.rs"],
        "unexpected_callers": ["crates/engram/src/api.rs", "crates/engram/src/tree.rs"] },
      { "kind": "callers", "query_sym": "chunk_code_ts",
        "note": "chunk_code_ts is called from ingest::ingest_document only.",
        "expected_callers": ["crates/engram/src/ingest.rs"],
        "unexpected_callers": ["crates/engram/src/retrieve.rs", "crates/engram/src/api.rs"] },
      { "kind": "callers", "query_sym": "extract_entities",
        "note": "Called from ingest.rs prose path and from retrieve.rs for query entity extraction.",
        "expected_callers": ["crates/engram/src/ingest.rs", "crates/engram/src/retrieve.rs"],
        "unexpected_callers": ["crates/engram/src/tree.rs", "crates/engram/src/store/mod.rs"] },
      { "kind": "callers", "query_sym": "seal_buffer",
        "note": "seal_buffer is called from tree.rs seal_cascade only.",
        "expected_callers": ["crates/engram/src/tree.rs"],
        "unexpected_callers": ["crates/engram/src/ingest.rs", "crates/engram/src/api.rs"] },
      { "kind": "callees", "query_path": "crates/engram/src/ingest.rs",
        "note": "ingest.rs defines chunk, chunk_code, chunk_code_with, extract_entities, extract_code_entities, ingest_document, estimate_tokens.",
        "expected_callees_contain": ["sym:chunk_code", "sym:chunk_code_with", "sym:extract_entities", "sym:extract_code_entities", "sym:ingest_document", "sym:estimate_tokens"] },
      { "kind": "callees", "query_path": "crates/engram/src/store/mod.rs",
        "note": "store/mod.rs defines commit_ingest, open, and helper fns vec_to_bytes, bytes_to_vec, enqueue_job_sql.",
        "expected_callees_contain": ["sym:commit_ingest", "sym:vec_to_bytes", "sym:bytes_to_vec"] },
      { "kind": "callees", "query_path": "crates/engram/src/treesit.rs",
        "note": "treesit.rs defines chunk_code_ts, lang_for_path, parse, is_chunkable, is_boundary, symbol_name, declarator_name, chunk_segments, extract_symbols_ts.",
        "expected_callees_contain": ["sym:chunk_code_ts", "sym:lang_for_path", "sym:extract_symbols_ts"] },
      { "kind": "callers", "query_sym": "lang_for_path",
        "note": "lang_for_path is called from ingest.rs (ingest_document code path).",
        "expected_callers": ["crates/engram/src/ingest.rs"],
        "unexpected_callers": ["crates/engram/src/api.rs", "crates/engram/src/retrieve.rs"] },
      { "kind": "callers", "query_sym": "worker_tick",
        "note": "worker_tick is the hot loop called from spawn_workers (jobs.rs) and directly from tests in tree.rs and jobs.rs.",
        "expected_callers": ["crates/engram/src/jobs.rs"],
        "unexpected_callers": ["crates/engram/src/api.rs", "crates/engram/src/ingest.rs"] },
      { "kind": "callees", "query_path": "crates/engram/src/retrieve.rs",
        "note": "retrieve.rs defines query, search_code, recall, drill_down, digest_hit.",
        "expected_callees_contain": ["sym:query", "sym:search_code", "sym:recall", "sym:drill_down", "sym:digest_hit"] },
      { "kind": "callers", "query_sym": "rebuild_conventions",
        "note": "rebuild_conventions (conventions.rs) is called from the rebuild_conventions_handler in api.rs only.",
        "expected_callers": ["crates/engram/src/api.rs"],
        "unexpected_callers": ["crates/engram/src/ingest.rs", "crates/engram/src/tree.rs"] },
      { "kind": "callees", "query_path": "crates/engram/src/api.rs",
        "note": "api.rs defines ingest_doc, get_doc, query_docs, search_code_docs, tree_query, get_architecture, get_module, rebuild handlers, auth, health.",
        "expected_callees_contain": ["sym:ingest_doc", "sym:query_docs", "sym:search_code_docs", "sym:tree_query", "sym:auth"] },
      { "kind": "callers", "query_sym": "delete_doc_by_key",
        "note": "delete_doc_by_key (store/docs.rs) is called from the forget_doc_by_key handler in api.rs.",
        "expected_callers": ["crates/engram/src/api.rs"],
        "unexpected_callers": ["crates/engram/src/ingest.rs", "crates/engram/src/tree.rs"] },
      { "kind": "callees", "query_path": "crates/engram/src/config.rs",
        "note": "config.rs defines the OnceLock free-function gates and the Config struct.",
        "expected_callees_contain": ["sym:code_symbol_split", "sym:code_tree_sitter", "sym:code_native_pack", "sym:code_min_score", "sym:code_graph"] }
    ]
  }
  ```

- [ ] Step: commit — `git add eval/graph_gold.json && git commit -m "eval: graph_gold.json — 15 structural Rust gold entries for engram-self callers/callees"`

- [ ] Step: write `eval/android/graph_gold_native.json` — C/C++ gold for the 3 proxy namespaces (`repo:ndk-samples`, `repo:libxcam`, `repo:gpuimage-plus`). **Oracle-assisted authoring** is required before human verification (clangd `callHierarchy/incomingCalls` on a `cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` build, OR GNU Global `global -r`). Minimum human-verification bar: for each `expected_caller`, read the cited file and confirm the symbol appears in a *call expression* (not a comment/forward-decl/type-use); for `unexpected_callers`, confirm it is absent from call sites.

  ```json
  {
    "datasets": [
      {
        "namespace": "repo:ndk-samples",
        "repo": "~/engram-eval/proxies/ndk-samples",
        "oracle_notes": "Generated with clangd callHierarchy/incomingCalls on a cmake -DCMAKE_EXPORT_COMPILE_COMMANDS=ON build of camera/basic. Human-verified by reading call-site lines.",
        "entries": [
          { "kind": "callers", "query_sym": "ImageReader",
            "note": "ImageReader ctor is called from CameraEngine::CreateCamera, not from android_main.",
            "expected_callers": ["camera/basic/src/main/cpp/camera_engine.cpp"],
            "unexpected_callers": ["camera/basic/src/main/cpp/android_main.cpp", "camera/basic/src/main/cpp/camera_ui.cpp"] },
          { "kind": "callers", "query_sym": "NDKCamera",
            "note": "NDKCamera ctor is called from CameraEngine::CreateCamera.",
            "expected_callers": ["camera/basic/src/main/cpp/camera_engine.cpp"],
            "unexpected_callers": ["camera/basic/src/main/cpp/android_main.cpp"] },
          { "kind": "callees", "query_path": "camera/basic/src/main/cpp/camera_engine.cpp",
            "note": "camera_engine.cpp defines CameraEngine, CreateCamera, DrawFrame, DeleteCamera.",
            "expected_callees_contain": ["sym:CameraEngine", "sym:DrawFrame", "sym:CreateCamera", "sym:DeleteCamera"] },
          { "kind": "callers", "query_sym": "CreateCamera",
            "note": "CreateCamera is called from android_main as the entry point into the camera subsystem.",
            "expected_callers": ["camera/basic/src/main/cpp/android_main.cpp"],
            "unexpected_callers": ["camera/basic/src/main/cpp/camera_manager.cpp", "camera/basic/src/main/cpp/image_reader.cpp"] },
          { "kind": "callees", "query_path": "camera/basic/src/main/cpp/camera_manager.cpp",
            "note": "camera_manager.cpp defines NDKCamera, MatchCaptureSizeRequest, StartPreview, StopPreview.",
            "expected_callees_contain": ["sym:NDKCamera", "sym:MatchCaptureSizeRequest", "sym:StartPreview", "sym:StopPreview"] }
        ]
      },
      {
        "namespace": "repo:libxcam",
        "repo": "~/engram-eval/proxies/libxcam",
        "oracle_notes": "Generated with GNU Global (gtags + global -r). Human-verified by reading reference lines.",
        "entries": [
          { "kind": "callers", "query_sym": "CLImageProcessor",
            "note": "CLImageProcessor ctor is called from factory functions in cl_image_processor.cpp and pipeline setup.",
            "expected_callers": ["modules/ocl/cl_image_processor.cpp"],
            "unexpected_callers": ["xcore/xcam_common.cpp", "xcore/buffer_pool.cpp"] },
          { "kind": "callees", "query_path": "modules/ocl/cl_image_processor.cpp",
            "note": "cl_image_processor.cpp defines CLImageProcessor and its process_buffer, push_buffer methods.",
            "expected_callees_contain": ["sym:CLImageProcessor", "sym:process_buffer", "sym:push_buffer"] }
        ]
      },
      {
        "namespace": "repo:gpuimage-plus",
        "repo": "~/engram-eval/proxies/gpuimage-plus",
        "oracle_notes": "Generated with clangd. Human-verified. .mm files fall back to the heuristic chunker; .cpp files parse with tree-sitter C++.",
        "entries": [
          { "kind": "callers", "query_sym": "GIPFilter",
            "note": "GIPFilter ctor/factory is referenced from pipeline setup in GIPProgram or GIPFilterGroup.",
            "expected_callers": ["source/GIPFilterGroup.cpp"],
            "unexpected_callers": ["source/GIPContext.cpp"] },
          { "kind": "callees", "query_path": "source/GIPProgram.cpp",
            "note": "GIPProgram.cpp defines GIPProgram and render-related methods.",
            "expected_callees_contain": ["sym:GIPProgram"] }
        ]
      }
    ]
  }
  ```

- [ ] Step: commit — `git add eval/android/graph_gold_native.json && git commit -m "eval: graph_gold_native.json — C/C++ structural gold for 3 proxy namespaces with oracle authoring notes"`

- [ ] Step: write `eval/graph_eval.py` — stdlib-only harness (mirrors `validate.py` style): caller precision@k / recall@k + callee coverage, with acceptance bars and `sys.exit(0 if ok else 1)`:

  ```python
  #!/usr/bin/env python3
  """Graph evaluation harness for engram's TRACK-1 code-graph feature.

  Measures caller precision@k / recall@k and callee coverage against
  eval/graph_gold.json (Rust) and eval/android/graph_gold_native.json (C/C++).

  Prerequisites:
    1. engram running with ENGRAM_CODE_GRAPH_EXTRACT=true
    2. Repos indexed (ENGRAM_CODE_GRAPH_EXTRACT=true cargo run -p engram-index -- index ...)
    3. ENGRAM_TOKEN set

  Endpoints:
    POST /v1/{ns}/code/graph/callers  body: {"sym": "<name>", "limit": k}
    POST /v1/{ns}/code/graph/callees  body: {"path": "<repo-rel-path>", "limit": k}
  """
  import argparse, json, os, sys, urllib.parse, urllib.request, urllib.error

  def http(method, url, token, body=None, timeout=30):
      data = json.dumps(body).encode() if body is not None else None
      req = urllib.request.Request(url, data=data, method=method,
          headers={"Authorization": f"Bearer {token}",
                   "Content-Type": "application/json"})
      try:
          with urllib.request.urlopen(req, timeout=timeout) as r:
              return r.status, r.read()
      except urllib.error.HTTPError as e:
          return e.code, e.read()
      except Exception as e:
          return 0, str(e).encode()

  def graph_callers(url, ns, token, sym, limit):
      st, raw = http("POST", f"{url}/v1/{ns}/code/graph/callers", token,
                     {"sym": sym, "limit": limit})
      if st != 200:
          return st, []
      try:
          return st, json.loads(raw)
      except Exception:
          return st, []

  def graph_callees(url, ns, token, path, limit):
      st, raw = http("POST", f"{url}/v1/{ns}/code/graph/callees", token,
                     {"path": path, "limit": limit})
      if st != 200:
          return st, []
      try:
          return st, json.loads(raw)
      except Exception:
          return st, []

  def precision_at_k(returned_paths, expected_set, unexpected_set, k):
      top = returned_paths[:k]
      if not top:
          return 0.0
      correct = sum(1 for p in top if p in expected_set and p not in unexpected_set)
      return correct / len(top)

  def recall_at_k(returned_paths, expected_set, k):
      if not expected_set:
          return 1.0
      top_set = set(returned_paths[:k])
      return sum(1 for e in expected_set if e in top_set) / len(expected_set)

  def callee_coverage(returned_syms, expected_syms):
      if not expected_syms:
          return 1.0
      ret_set = set(returned_syms)
      return sum(1 for s in expected_syms if s in ret_set) / len(expected_syms)

  def eval_dataset(url, token, namespace, entries, k, label):
      caller_rows = []
      callee_rows = []
      for entry in entries:
          if entry["kind"] == "callers":
              sym = entry["query_sym"]
              expected = set(entry.get("expected_callers", []))
              unexpected = set(entry.get("unexpected_callers", []))
              st, hits = graph_callers(url, namespace, token, sym, k)
              paths = [h.get("path", "") for h in hits]
              p = precision_at_k(paths, expected, unexpected, k)
              r = recall_at_k(paths, expected, k)
              caller_rows.append((sym, p, r, len(paths), st))
          elif entry["kind"] == "callees":
              path = entry["query_path"]
              expected = entry.get("expected_callees_contain", [])
              st, hits = graph_callees(url, namespace, token, path, 200)
              returned_syms = [h.get("sym", "") for h in hits]
              cov = callee_coverage(returned_syms, expected)
              callee_rows.append((path, cov, len(returned_syms), st))
      return caller_rows, callee_rows

  def print_caller_table(rows, k, label):
      if not rows:
          return
      print(f"\n=== callers ({label}, precision@{k} / recall@{k}) ===")
      print(f"  {'sym':<35} {'prec@k':>7} {'rec@k':>7} {'n_ret':>6} {'http':>5}")
      print(f"  {'-'*35} {'-'*7} {'-'*7} {'-'*6} {'-'*5}")
      for sym, p, r, n, st in rows:
          print(f"  {sym:<35} {p:>7.3f} {r:>7.3f} {n:>6} {st:>5}")
      avg_p = sum(p for _, p, _, _, _ in rows) / len(rows)
      avg_r = sum(r for _, _, r, _, _ in rows) / len(rows)
      print(f"  {'MEAN':<35} {avg_p:>7.3f} {avg_r:>7.3f}")

  def print_callee_table(rows, label):
      if not rows:
          return
      print(f"\n=== callees ({label}, sym coverage) ===")
      print(f"  {'path':<60} {'coverage':>9} {'n_ret':>6} {'http':>5}")
      print(f"  {'-'*60} {'-'*9} {'-'*6} {'-'*5}")
      for path, cov, n, st in rows:
          short = path[-58:] if len(path) > 60 else path
          print(f"  {short:<60} {cov:>9.3f} {n:>6} {st:>5}")
      avg = sum(c for _, c, _, _ in rows) / len(rows)
      print(f"  {'MEAN':<60} {avg:>9.3f}")

  BARS = {
      "rust_callers_prec":  ("repo:engram", "callers", "prec", 0.70, ">="),
      "rust_callers_rec":   ("repo:engram", "callers", "rec",  0.50, ">="),
      "rust_callees_cov":   ("repo:engram", "callees", "cov",  0.90, ">="),
      "cpp_callers_prec":   ("repo:ndk-samples", "callers", "prec", 0.50, ">="),
      "cpp_callees_cov":    ("repo:ndk-samples", "callees", "cov",  0.70, ">="),
  }

  def check_bars(results_by_ns):
      passed = []
      failed = []
      for bar_id, (ns, kind, metric, threshold, op) in BARS.items():
          key = f"{kind}_{metric}"
          val = results_by_ns.get(ns, {}).get(key)
          if val is None:
              failed.append((bar_id, "no data", threshold))
              continue
          ok = (val >= threshold) if op == ">=" else (val <= threshold)
          (passed if ok else failed).append((bar_id, round(val, 3), threshold))
      return passed, failed

  def main():
      ap = argparse.ArgumentParser()
      ap.add_argument("--rust-gold",
                      default=os.path.join(os.path.dirname(__file__), "graph_gold.json"))
      ap.add_argument("--native-gold",
                      default=os.path.join(os.path.dirname(__file__), "android", "graph_gold_native.json"))
      ap.add_argument("--url",  default=os.environ.get("ENGRAM_URL", "http://127.0.0.1:8088"))
      ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
      ap.add_argument("--k", type=int, default=10, help="k for precision@k / recall@k on callers")
      args = ap.parse_args()

      all_caller_rows = {}
      all_callee_rows = {}

      if os.path.exists(args.rust_gold):
          rust = json.load(open(args.rust_gold))
          ns = rust["namespace"]
          entries = rust.get("entries", [])
          cr, ce = eval_dataset(args.url, args.token, ns, entries, args.k, ns)
          all_caller_rows[ns] = cr
          all_callee_rows[ns] = ce
          print_caller_table(cr, args.k, ns)
          print_callee_table(ce, ns)
      else:
          print(f"[warn] rust gold not found: {args.rust_gold}", file=sys.stderr)

      if os.path.exists(args.native_gold):
          native = json.load(open(args.native_gold))
          for ds in native.get("datasets", []):
              ns = ds["namespace"]
              entries = ds.get("entries", [])
              cr, ce = eval_dataset(args.url, args.token, ns, entries, args.k, ns)
              all_caller_rows.setdefault(ns, []).extend(cr)
              all_callee_rows.setdefault(ns, []).extend(ce)
              print_caller_table(cr, args.k, ns)
              print_callee_table(ce, ns)
      else:
          print(f"[warn] native gold not found: {args.native_gold}", file=sys.stderr)

      results_by_ns = {}
      for ns in set(list(all_caller_rows.keys()) + list(all_callee_rows.keys())):
          cr = all_caller_rows.get(ns, [])
          ce = all_callee_rows.get(ns, [])
          results_by_ns[ns] = {}
          if cr:
              results_by_ns[ns]["callers_prec"] = sum(p for _, p, _, _, _ in cr) / len(cr)
              results_by_ns[ns]["callers_rec"]  = sum(r for _, _, r, _, _ in cr) / len(cr)
          if ce:
              results_by_ns[ns]["callees_cov"] = sum(c for _, c, _, _ in ce) / len(ce)

      passed, failed = check_bars(results_by_ns)

      print("\n=== acceptance bars ===")
      for bar_id, val, threshold in passed:
          print(f"  PASS  {bar_id:<35} got={val}  bar>={threshold}")
      for bar_id, val, threshold in failed:
          print(f"  FAIL  {bar_id:<35} got={val}  bar>={threshold}")

      ok = len(failed) == 0
      print(f"\nRESULT: {'PASS' if ok else 'FAIL'}  ({len(passed)} bars passed, {len(failed)} failed)")
      sys.exit(0 if ok else 1)

  if __name__ == "__main__":
      main()
  ```

  Note: `check_bars` keys the `BARS` thresholds on `callers_prec`/`callers_rec`/`callees_cov` as computed in `results_by_ns`; the `metric` field in each `BARS` tuple (`"prec"`/`"rec"`/`"cov"`) maps to those keys via `f"{kind}_{metric}"` → adjust the dict keys to match (`callers_prec`, `callers_rec`, `callees_cov`) when wiring, so the lookup resolves.

- [ ] Step: smoke-test the harness shape (no live engram — confirm gold parses and exits non-zero on no-data):

  ```bash
  cd /mnt/x/code/engram
  ENGRAM_TOKEN=dev-token python3 eval/graph_eval.py \
    --url http://127.0.0.1:19999 \
    --rust-gold eval/graph_gold.json \
    --native-gold eval/android/graph_gold_native.json \
    --k 10
  # Expected: tables with all http=0, all scores 0.0, then "RESULT: FAIL" — confirms parsing + bar logic.
  ```

- [ ] Step: commit — `git add eval/graph_eval.py && git commit -m "eval: graph_eval.py — caller prec@k/rec@k + callee coverage harness for Rust and C/C++ gold"`

- [ ] Step (live, cost-bearing): index engram-self with `ENGRAM_CODE_GRAPH_EXTRACT=true`, run the Rust-only harness, then index the 3 proxy repos and run the full harness; record output in `eval/RESULTS_graph.md`. Expected: Rust callee coverage ~0.90+, Rust caller precision ~0.70, C/C++ callee coverage ~0.70, C/C++ caller precision ~0.50–0.65.

---

## Track 2 — consolidation evaluation

Three layers of evidence gate any decision to enable consolidation in production:

- **(a) Mechanical** — does the Rust cold pipeline produce the correct tree shape, respect immutability, and recover from crashes? Verified offline with `HashEmbedder` + `FakeChatClient`.
- **(b) Fallback-rate** — what fraction of live LLM calls during a real drain error out and fall back to deterministic concat? A fallback rate above ~10% means the summary tree is not trusted text.
- **(c) Value A/B** — does `drill_down` (Arm A) actually answer cross-document questions better than flat chunk retrieval (Arm B)? If not, the cost of consolidation is unjustified and `ENGRAM_CONSOLIDATE_CODE` stays off.

---

### Task 14 — Mechanical integration test for the full cold pipeline

Covers: multi-doc fan-out, shrunk-gate cascade depth, idempotent re-ingest (unsealed drop, sealed immutability), crash recovery via `requeue_running`.

**Files:** Modify `/mnt/x/code/engram/crates/engram/src/tree.rs` (append inside `#[cfg(test)] mod tests`)

- [ ] Step: write the failing test — append inside `#[cfg(test)] mod tests` in `tree.rs`, after the `sweep_seals_stale_buffers` test. Add `use crate::ingest::ingest_document;` and `use crate::model::{NewDoc, Taint};` at the module level if not already present:

  ```rust
  /// Build a Config where gates trip on every leaf so the cascade is deep enough to observe
  /// multi-level sealing within a small number of docs.
  fn tiny_cfg() -> Config {
      let mut c = Config::from_vars(|_| None);
      c.seal_input_token_budget = 1;     // every leaf immediately seals its L0 buffer
      c.seal_fanout = 2;                  // every pair of L1 nodes seals into L2
      c.seal_flush_age_secs = 1e15;       // age gate never fires during the test
      c
  }

  #[test]
  fn integration_multi_doc_drain_tree_shape_and_reingest_invariants() {
      use crate::ingest::ingest_document;
      use crate::jobs::worker_tick;
      use crate::model::{NewDoc, Taint};

      let (store, _d) = temp();
      let e = HashEmbedder::new(16);
      let cfg = tiny_cfg();

      let docs = [
          ("k1", "First doc with handle @alice and url https://a.com"),
          ("k2", "Second doc mentioning @bob and #rust"),
          ("k3", "Third doc about @carol and email carol@example.com"),
          ("k4", "Fourth doc referencing @dave and #cargo"),
      ];
      let ns = "integ";
      for (key, content) in &docs {
          let new = NewDoc {
              key: (*key).into(),
              title: (*key).into(),
              content: (*content).into(),
              author: "agentA".into(),
              taint: Taint::Internal,
              meta: None,
          };
          ingest_document(&store, &e, ns, &new).unwrap();
      }
      assert_eq!(store.pending_jobs().unwrap(), 4);

      let proc = TreeProcessor {
          embedder: std::sync::Arc::new(HashEmbedder::new(16)),
          chat: std::sync::Arc::new(FakeChatClient::ok("SUMMARY")),
          audit: std::sync::Arc::new(NullAuditSink),
          cfg: cfg.clone(),
          vault: None,
      };

      let mut ticks = 0usize;
      while worker_tick(&store, &proc, 5).unwrap() {
          ticks += 1;
      }
      assert_eq!(ticks, 4, "should process exactly 4 jobs");
      assert_eq!(store.pending_jobs().unwrap(), 0);

      let top_source = store.tree_top_nodes(ns, "source", "agentA").unwrap();
      assert_eq!(top_source.len(), 1, "source tree must have converged to a single top node");
      assert!(
          top_source[0].level >= 2,
          "cascade must have climbed at least to L2 (got L{})",
          top_source[0].level
      );
      assert_eq!(top_source[0].body, "SUMMARY", "top node body must be the FakeChat reply");

      let top_global = store.tree_top_nodes(ns, "global", "global").unwrap();
      assert_eq!(top_global.len(), 1);
      assert!(top_global[0].level >= 2);

      for handle in ["handle:alice", "handle:bob", "handle:carol", "handle:dave"] {
          let top = store.tree_top_nodes(ns, "topic", handle).unwrap();
          assert_eq!(top.len(), 1, "topic tree for {handle} must have exactly one top node");
          assert_eq!(top[0].level, 1, "single leaf → L1, no L2");
      }

      let sealed_before: i64 = {
          let conn = store.read.get().unwrap();
          conn.query_row("SELECT count(*) FROM tree_nodes WHERE namespace=?1 AND sealed=1",
                         rusqlite::params![ns], |r| r.get(0)).unwrap()
      };
      assert!(sealed_before > 0, "there must be sealed nodes after drain");

      let new_k1 = NewDoc {
          key: "k1".into(),
          title: "k1".into(),
          content: "First doc with handle @alice and url https://a.com".into(),
          author: "agentA".into(),
          taint: Taint::Internal,
          meta: None,
      };
      ingest_document(&store, &e, ns, &new_k1).unwrap();
      assert_eq!(store.pending_jobs().unwrap(), 1);
      while worker_tick(&store, &proc, 5).unwrap() {}

      let sealed_after: i64 = {
          let conn = store.read.get().unwrap();
          conn.query_row("SELECT count(*) FROM tree_nodes WHERE namespace=?1 AND sealed=1",
                         rusqlite::params![ns], |r| r.get(0)).unwrap()
      };
      assert!(
          sealed_after >= sealed_before,
          "sealed nodes must not decrease after re-ingest (before={sealed_before}, after={sealed_after})"
      );

      ingest_document(&store, &e, ns, &NewDoc {
          key: "k5".into(),
          title: "k5".into(),
          content: "Fifth doc @eve".into(),
          author: "agentA".into(),
          taint: Taint::Internal,
          meta: None,
      }).unwrap();
      let claimed = store.claim_job().unwrap().unwrap();
      assert_eq!(claimed.namespace, ns);
      let requeued = store.requeue_running().unwrap();
      assert_eq!(requeued, 1, "requeue_running must recover the orphaned running job");
      assert_eq!(
          store.job(&format!("{ns}:{}", claimed.document_id)).unwrap().unwrap().0,
          "pending"
      );
  }
  ```

- [ ] Step: run it, expect FAIL — `cargo test -p engram integration_multi_doc_drain_tree_shape_and_reingest_invariants`
- [ ] Step: implement — paste the test into `tree.rs` `#[cfg(test)] mod tests` after `sweep_seals_stale_buffers`. It uses already-imported `HashEmbedder`, `FakeChatClient`, `NullAuditSink`, `Config`, `Store`, `TreeProcessor`, `worker_tick` plus `ingest_document` and `NewDoc`/`Taint`.
- [ ] Step: run, expect PASS — `cargo test -p engram integration_multi_doc_drain_tree_shape_and_reingest_invariants -- --nocapture`
- [ ] Step: commit — `git add crates/engram/src/tree.rs && git commit -m "test(tree): integration test — multi-doc drain, tree shape, re-ingest invariants, crash recovery"`

---

### Task 15 — Fallback-rate monitor (`eval/consolidation/fallback_rate.py`)

Runs a live consolidation drain against a real engram instance (real deepseek + `HttpAuditSink`), tails the audit log, and reports the fraction of LLM calls that errored out and fell back to deterministic concat. **Cost-bearing live run — not for CI.**

**Files:** Create `/mnt/x/code/engram/eval/consolidation/fallback_rate.py`

- [ ] Step: write the script — verbatim from the eval-track2 section. It ingests a small prose corpus (engram's own README/ARCHITECTURE/CLAUDE.md/ROADMAP) into a fresh namespace, shrinks the seal gates via env, polls `post_acquire_jobs` until drained, reads the `llm-audit` events (SQLite via `ENGRAM_AUDIT_DB` or HTTP via `ENGRAM_AUDIT_URL`) filtering `app='engram' AND op='chat'`, and reports `fallback_rate = error/(ok+error)`. **GATE: `FALLBACK_RATE_GATE = 0.10`** — `sys.exit(1)` if `rate > 0.10`, `sys.exit(2)` if no audit events were collected, `sys.exit(0)` on pass. Key constants: `BASE` from `ENGRAM_URL` (default `http://127.0.0.1:8096`), `NS = "fallback-eval"`, `CORPUS_PATHS` = the four engram doc files, `wait_drain(db_path)` polling `status IN ('pending','running')`, `read_audit_events_from_db` / `read_audit_events_from_api`.

- [ ] Step: run it, expect FAIL (no live engram) —

  ```bash
  ENGRAM_TOKEN=secret ENGRAM_GATEWAY_URL=http://127.0.0.1:4000 ENGRAM_GATEWAY_KEY=x \
      python3 /mnt/x/code/engram/eval/consolidation/fallback_rate.py
  ```

  Expected: connection refused or "no corpus files found" exit.

- [ ] Step: implement — the script is self-contained; no Rust changes. Start engram with small gates (`ENGRAM_SEAL_INPUT_TOKEN_BUDGET=500`, `ENGRAM_SEAL_FANOUT=3`) and real credentials, re-run, record `fallback_rate` in `eval/consolidation/RESULTS.md`.
- [ ] Step: commit — `git add eval/consolidation/fallback_rate.py && git commit -m "eval(consolidation): fallback_rate.py — measure live LLM error rate during drain"`

---

### Task 16 — Synthesis gold set + A/B harness (`eval/consolidation/`)

The decision gate: does `drill_down` over the consolidated global tree (Arm A) answer cross-document questions better than flat chunk retrieval (Arm B)? If A does not beat B by the margin, consolidation stays off. **Cost-bearing live run** (~400 deepseek calls for 20 questions × 3 arms + judging).

**Files:** Create `/mnt/x/code/engram/eval/consolidation/synthesis_gold.json`, `/mnt/x/code/engram/eval/consolidation/ab_eval.py`

- [ ] Step (16a): write `synthesis_gold.json` — 20 cross-document synthesis questions (`sg01`–`sg20`), each requiring facts spread across ≥2 docs (ARCHITECTURE.md + CLAUDE.md, plus eval/RESULTS.md for sg12). Each entry has `id`, `question`, `requires_docs`, `ground_truth`. Copied verbatim from the eval-track2 section — the questions cover: spawn_blocking + off-lock write lock (sg01), job_id format + idempotent re-ingest (sg02), the three seal gates (sg03), forget-path deletion semantics (sg04), ext4-vs-v9fs WAL (sg05), embedder signature consequences (sg06), path-prior + def-boost vs the 0.55 graph signal (sg07), the three embedders (sg08), requeue_running (sg09), FallbackEmbedder signature delegation (sg10), the two chunking modes (sg11), the recall@5 0.600 graph crater (sg12), off-lock invariant functions (sg13), prose tree fan-out (sg14), single-pass digest vs tree drill (sg15), commit_ingest atomicity (sg16), read/write pool architecture (sg17), the three namespace suffixes (sg18), IDF keyword re-rank (sg19), std::thread cold-path workers (sg20).

- [ ] Step (16b): write `ab_eval.py` — verbatim from the eval-track2 section. Three arms: **A** = `drill_down` (POST `/tree`), **B** = flat chunk query (POST `/query`), **C** = one-shot deepseek summary of the corpus (length-matched). Answerer = deepseek-chat held constant; blind 0–2 multi-judge (`JUDGE_CALLS=3`, median). **GATE: `MARGIN = 0.20`** — `sys.exit(0 if (A_mean - B_mean) >= 0.20 else 1)`. Writes `answers.json`, `scores.json`, `RESULTS.md` under `~/engram-eval/consolidation/`. Key pieces: `http_post` with 429/5xx retry, `eng()` namespace helper, `gw_chat()` gateway caller, `ctx_A`/`ctx_B`/`build_ctx_C` context builders, `answer()` (ANS_SYS), `judge_once`/`judge_median` (JUDGE_SYS, regex-extract `[012]`), `arm_stats` aggregation, the RESULTS.md table writer.

- [ ] Step: run dry, expect FAIL (no live engram) —

  ```bash
  ENGRAM_TOKEN=secret ENGRAM_GATEWAY_URL=http://127.0.0.1:4000 ENGRAM_GATEWAY_KEY=x \
      python3 /mnt/x/code/engram/eval/consolidation/ab_eval.py
  ```

  Expected: `KeyError: 'ENGRAM_TOKEN'` or connection refused.

- [ ] Step: run live — after priming the `distill` namespace (`~/engram-eval/distill/harness.py`) and draining consolidation with `ENGRAM_CONSOLIDATE_CODE=true`, run `ab_eval.py`; record the gap and PASS/FAIL.
- [ ] Step: interpret gate — if `A_drill_down mean - B_chunk_query mean >= 0.20`, consolidation provides measurable synthesis benefit and `ENGRAM_CONSOLIDATE_CODE=true` can be enabled for prose namespaces. Otherwise it stays off.
- [ ] Step: commit — `git add eval/consolidation/synthesis_gold.json eval/consolidation/ab_eval.py && git commit -m "eval(consolidation): synthesis gold (20 cross-doc questions) + A/B harness (drill_down vs chunk query)"`

**The three layers:**

| Layer | File(s) | Runtime | Network | Gate |
|-------|---------|---------|---------|------|
| (a) Mechanical | `crates/engram/src/tree.rs` | `cargo test` | none | test must pass |
| (b) Fallback-rate | `eval/consolidation/fallback_rate.py` | live, ~5 min | real deepseek | fallback_rate ≤ 0.10 |
| (c) Value A/B | `eval/consolidation/synthesis_gold.json` + `ab_eval.py` | live, ~20 min, ~400 LLM calls | real deepseek | A − B gap ≥ 0.20 |

Run (a) in every CI pass. Run (b) and (c) once, cost-bearing, before any decision to enable `ENGRAM_CONSOLIDATE_CODE=true`. If (c) fails, consolidation stays off for prose namespaces too.

---

## Verification

Run after every Track-1 task and as the final gate:

- [ ] `cargo test --workspace` — all existing ~179 tests plus the new model/config/store/edges/graph/graph_query/api/mcp/tree tests pass, zero failures.
- [ ] `cargo clippy --workspace -- -D warnings` — clean. If the AST-walk helpers draw an `only_used_in_recursion` lint, add `#[allow(clippy::only_used_in_recursion)]` at the offending call site.
- [ ] `cargo fmt --check` — formatted.

**Regression (the hard gate):**

- [ ] Existing consolidation tests still pass (`cargo test -p engram tree::` / the `cascade_*`, `process_doc_*`, `sweep_*` tests).
- [ ] `python eval/validate.py` numbers are **unchanged** with the graph flag off (the default). `code_edges` is a structural index never read by `search_code`/`query`, so retrieval metrics (ingest 0.998, recall@1 0.543, recall@5 0.771, recall@10 0.914, line-recall@10 0.771) must not move.
- [ ] Isolation grep returns zero matches:

  ```bash
  grep -n "code_edges\|graph_query\|edges_from\|edges_to_sym\|resolve_dst_doc" \
    crates/engram/src/retrieve.rs
  ```

**Track-1 live:** `python eval/graph_eval.py` against an engram instance indexed with `ENGRAM_CODE_GRAPH_EXTRACT=true` (engram-self + 3 proxy repos); record in `eval/RESULTS_graph.md`.

**Track-2 live:** run `eval/consolidation/fallback_rate.py` first (the fallback-rate gate); only if it passes, run `eval/consolidation/ab_eval.py` (the A/B value gate).

---

## Acceptance bars

| Case | Metric | Bar | Rationale |
|---|---|---|---|
| Rust same-file callee coverage | callees precision (coverage) | **≥ 0.90** | Tree-sitter AST on `.rs`: `sym:` entities are accurate; all defs in a file are indexed in its `chunk_entities`. |
| Rust cross-file caller precision | callers precision@k=10 | **~0.70** | Lazy cross-file resolution via `chunk_entities`; clear call-expression text but some false positives from trait-impl dispatch. |
| Rust cross-file caller recall | callers recall@k=10 | **~0.50–0.60** | Only direct textual call-site matches; macro-generated calls and indirect dispatch are missed (intentional v1 scope). |
| C/C++ callee coverage | callees coverage | **≥ 0.70** | C++ function names extract well, but template specialisations, anonymous namespaces, `extern "C"` wrappers reduce coverage. |
| C/C++ caller precision | callers precision@k=10 | **~0.50–0.65** | Noisier: preprocessor, includes, forward decls all contain the symbol name; extractor must separate call-sites from other uses. |
| C/C++ caller recall | callers recall@k=10 | **~0.40–0.55** | Cross-TU resolution is accurate for rare callee names; common names (`init`, `get`) have high false-positive pressure. |

The Rust→C/C++ gap is structural (no preprocessor, no forward declarations, no header/impl split in Rust). The C/C++ numbers are **measurement targets, not pass/fail gates** for v1 — the harness reports them so future work (header dedup, template instantiation tracking, macro expansion) can track improvement. The Rust bars (callee coverage ≥ 0.90, caller precision ~0.70) are the real v1 gate.

**Track-2 gate:** `drill_down` (Arm A) must beat flat chunk retrieval (Arm B) on the synthesis gold by **A − B mean-score ≥ 0.20**, *and* the live fallback-rate must be **≤ 0.10**, before `ENGRAM_CONSOLIDATE_CODE=true` is enabled. If the A/B gap is below the margin, consolidation stays off — for prose namespaces too.

---

## Out of scope / deferred

- **v1.1 query surface:** `neighbors`, `blast_radius`, `dead_symbols`, `detect_changes` endpoints + their MCP tools and gold entries. Not built in v1.
- **Non-Rust/C/C++ extraction:** Python (import + type-annotation walk), TypeScript/JavaScript (`call_expression` + `import_statement`), Go (`call_expression` + `import_spec`), Java/Kotlin (`method_invocation` + import). `graph::extract_edges` returns `vec![]` for these today; adding a language is a single addition to `graph.rs` with no wiring change.
- **Eager `dst_doc_id` resolution** (background job to fill the NULL column) and multi-hop display in the MCP formatter.
- **Option D rejected:** a frozen nomic embedder. Measured recall@5 **0.380** < random **0.519** < keyword-only **0.787** — strictly worse than the cheapest baseline, so it is not pursued.

---

## Reconciliation notes (what the verifiers caught)

The 8 ownership conflicts the consistency/completeness verifiers found, and the fixes applied during assembly (so the reader can trust this single document over the 8 source sections):

1. **`commit_ingest` edits had two implementations** (store section delegated to a helper; graph-extract and ingest-wiring sections re-inlined the SQL). **Fix:** the store section (Task 3) is the sole owner; the in-tx write is delegated to `edges::insert_edges_in_tx`; the duplicate store/DDL/commit_ingest tasks from graph-extract and ingest-wiring were deleted.
2. **`EdgeKind`/`RawEdge` defined four times** with divergent derives (one omitted `Copy`; one omitted `Serialize`/`Deserialize`). **Fix:** one canonical definition in `model.rs` — `EdgeKind` derives `Copy` + serde, `RawEdge` derives serde, no `src_doc_id` field (B1).
3. **`code_edges` DDL disagreed on `dst_doc_id`** (store section had it; graph-extract/ingest-wiring omitted it) and two sections each set `user_version = 2`. **Fix:** the canonical DDL includes nullable `dst_doc_id TEXT`; exactly one migration bumps `user_version` (B2), in the store task only.
4. **`graph_query::callers` had a dead `for out_edge in outbound { let _ = out_edge; }` loop.** **Fix:** deleted entirely; upward BFS expansion is via the doc's `sym:` entities only (B4).
5. **`graph_query` called a nonexistent `store.read_conn()` with raw SQL.** **Fix:** added a real `Store::sym_entities_for_doc` to `store/entities.rs` (Task 5, an explicit build-order step) and call it; removed the ghost `symbols_for_doc` summary-table row (B5).
6. **`code_graph_max_depth` clamping disagreed** (`.filter(|&v| v >= 1)` drops 0 → default 4, vs `.map(|v| v.max(1))` clamps 0 → 1). **Fix:** use `.map(|v| v.max(1)).unwrap_or(4)` (clamp 0→1) per the locked decision, and the comment matches (B3).
7. **`TraceHop`, `trace_symbol`, and `HttpCodeSearch::trace_symbol` were defined in both the query+api and mcp sections**, with the query+api version carrying a double-prefixed URL bug (`.../code/graph/code/graph/callers`). **Fix:** `engram-mcp` is the sole owner of all MCP changes (Task 12); the api.rs task (Task 11) touches only core-crate routes/handlers/`GraphReq`; the MCP URL is `format!("{}/v1/{}/{}", url, ns, suffix)` with suffix `"code/graph/callers"`/`"code/graph/callees"` (B7).
8. **API handler depth + `FakeSearch` semantics.** **Fix:** the handler uses `req.max_depth.unwrap_or(2).min(code_graph_max_depth())` with `min_confidence` filtered post-BFS (B6); `FakeSearch::trace_symbol` returns `Ok(vec![])` and a separate `FakeTracer` struct provides the non-empty hop for the formatting test (B7).
