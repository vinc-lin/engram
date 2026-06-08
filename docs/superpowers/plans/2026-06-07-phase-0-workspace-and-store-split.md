# Phase 0 — Workspace + store.rs Split — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the single `engram` crate into a Cargo workspace (core + two client-crate stubs) and split the 1031-line `store.rs` into a focused `store/` module directory — with **zero behavior change** and all tests green throughout.

**Architecture:** This is Phase 0 of the spec `docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`. It is pure structural cleanup that lands *before* any feature work: a virtual workspace manifest at the repo root with `crates/engram` (the existing lib+server, moved wholesale) plus empty `crates/engram-index` and `crates/engram-mcp` stubs; then `store.rs` becomes `store/mod.rs` + five submodules (`docs`, `chunks`, `entities`, `jobs`, `trees`) split by responsibility. No SQL, no logic, no public API changes.

**Tech Stack:** Rust 2021, Cargo workspaces, rusqlite (bundled), r2d2, axum, tokio. Existing inline `#[cfg(test)]` tests are the safety net.

**Refactor discipline (read first):** There is no new behavior, so we do not write new tests. Instead, **every task ends by running the full suite and confirming the same tests pass** (`cargo test` — currently 18 tests in the `store` module plus the rest of the crate's tests). If a move changes test results, the move was wrong — revert and retry. Commit only on green.

---

## File structure

**Before:**
```
engram/
  Cargo.toml            # [package] engram, [lib] src/lib.rs, [[bin]] engram src/main.rs
  Cargo.lock
  src/*.rs              # 14 modules incl. store.rs (1031 LOC)
  deploy/run.sh         # BIN = $REPO/target/release/engram
```

**After:**
```
engram/
  Cargo.toml            # [workspace] virtual manifest (members + resolver)
  Cargo.lock            # workspace lockfile (stays at root)
  crates/
    engram/
      Cargo.toml        # the moved [package] manifest (unchanged contents)
      src/
        *.rs            # all existing modules, store.rs now a dir:
        store/
          mod.rs        # Store struct, open, SCHEMA, commit_ingest, shared free fns, tests, mod decls
          docs.rs       # doc CRUD methods + map_doc + taint_from
          chunks.rs     # ChunkRow + chunk methods
          entities.rs   # entity methods
          jobs.rs       # Job + job-queue methods
          trees.rs      # TreeNode/NewTreeNode + tree methods + tree helpers
    engram-index/       # stub (Phase 1 fills it)
      Cargo.toml
      src/main.rs
    engram-mcp/         # stub (Phase 1 fills it)
      Cargo.toml
      src/main.rs
  deploy/run.sh         # unchanged path: $REPO/target/release/engram still valid
```

**`store.rs` partition (what moves where):**

| Destination | Items |
|---|---|
| `store/mod.rs` (keeps) | `struct Store`; `const SCHEMA`; `impl Store { open, commit_ingest }`; free fns `now_secs`, `enqueue_job_sql`, `taint_str`, `vec_to_bytes`, `bytes_to_vec`, `cosine`; the whole `#[cfg(test)] mod tests`; `mod docs/chunks/entities/jobs/trees;` declarations |
| `store/docs.rs` | `impl Store { insert_doc, get_doc, get_by_key, list_namespace, delete_doc_by_key }`; free fns `taint_from`, `map_doc` |
| `store/chunks.rs` | `struct ChunkRow`; `impl Store { upsert_chunk, delete_chunks_for_doc, chunks_for_namespace, chunks_for_doc }` |
| `store/entities.rs` | `impl Store { record_entities, docs_with_entities }` |
| `store/jobs.rs` | `struct Job`; `impl Store { enqueue_job, claim_job, complete_job, fail_or_retry_job, requeue_running, job, pending_jobs }` |
| `store/trees.rs` | `struct TreeNode`, `struct NewTreeNode`; `const TREE_COLS`; `impl Store { append_leaf_node, seal_buffer, unsealed_nodes, children_of, delete_unsealed_leaves_for_doc, chunks_for_doc?, tree_top_nodes, due_stale_buffers, touch_unsealed_created_at }`; free fns `map_tree_node`, `insert_tree_node_sql` |

> Note: `chunks_for_doc` returns `ChunkRow` and is chunk-oriented → put it in `chunks.rs` (not trees). `commit_ingest` is cross-cutting (touches docs+chunks+entities+jobs) → it stays in `mod.rs`. `enqueue_job_sql` is used by both `enqueue_job` (jobs.rs) and `commit_ingest` (mod.rs) → it **stays in `mod.rs`**; jobs.rs calls `super::enqueue_job_sql`. `now_secs`, `taint_str`, `vec_to_bytes`, `bytes_to_vec` stay in `mod.rs`; submodules reference them via `super::` (child modules may access ancestor-private items).

---

## Task 1: Convert to a Cargo workspace

**Files:**
- Move: `Cargo.toml` → `crates/engram/Cargo.toml`
- Move: `src/` → `crates/engram/src/`
- Create: `Cargo.toml` (new root workspace manifest)
- Create: `crates/engram-index/Cargo.toml`, `crates/engram-index/src/main.rs`
- Create: `crates/engram-mcp/Cargo.toml`, `crates/engram-mcp/src/main.rs`

- [ ] **Step 1: Confirm a clean baseline (tests green before touching anything)**

Run: `cargo test`
Expected: builds and all tests pass. Note the count (e.g. `test result: ok. N passed`). This N is the invariant for the rest of the plan.

- [ ] **Step 2: Move the existing crate under `crates/engram/`**

```bash
mkdir -p crates/engram
git mv src crates/engram/src
git mv Cargo.toml crates/engram/Cargo.toml
```

Leave `Cargo.lock` at the repo root (it becomes the workspace lock). Do not edit `crates/engram/Cargo.toml` — its `[lib] path = "src/lib.rs"` and `[[bin]] path = "src/main.rs"` are relative to the crate dir and remain correct.

- [ ] **Step 3: Create the root workspace manifest**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = ["crates/engram", "crates/engram-index", "crates/engram-mcp"]
```

- [ ] **Step 4: Create the `engram-index` stub crate**

Create `crates/engram-index/Cargo.toml`:

```toml
[package]
name = "engram-index"
version = "0.1.0"
edition = "2021"

[dependencies]
```

Create `crates/engram-index/src/main.rs`:

```rust
fn main() {
    eprintln!("engram-index: not yet implemented (Phase 1)");
}
```

- [ ] **Step 5: Create the `engram-mcp` stub crate**

Create `crates/engram-mcp/Cargo.toml`:

```toml
[package]
name = "engram-mcp"
version = "0.1.0"
edition = "2021"

[dependencies]
```

Create `crates/engram-mcp/src/main.rs`:

```rust
fn main() {
    eprintln!("engram-mcp: not yet implemented (Phase 1)");
}
```

- [ ] **Step 6: Build the whole workspace**

Run: `cargo build`
Expected: all three crates compile. `engram-index` and `engram-mcp` produce trivial binaries.

- [ ] **Step 7: Run the full test suite**

Run: `cargo test`
Expected: same N tests pass as in Step 1 (the stub crates add zero tests). If the count or result changed, stop and investigate.

- [ ] **Step 8: Verify the deploy binary path is unchanged**

Run: `cargo build --release -p engram && ls -l target/release/engram`
Expected: the `engram` server binary exists at `target/release/engram` (the path `deploy/run.sh` uses via `$REPO/target/release/engram`). No change to `deploy/run.sh` needed.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "refactor: convert engram to a Cargo workspace (core + index/mcp stubs)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Turn `store.rs` into a `store/` module (no split yet)

This isolates the rename from the content moves, so a problem is easy to bisect.

**Files:**
- Move: `crates/engram/src/store.rs` → `crates/engram/src/store/mod.rs`

- [ ] **Step 1: Move the file into a module directory**

```bash
cd crates/engram
mkdir -p src/store
git mv src/store.rs src/store/mod.rs
cd ../..
```

No content changes. `mod store;` in `crates/engram/src/lib.rs` still resolves (`store/mod.rs` is an equivalent module path).

- [ ] **Step 2: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass. (A bare file→dir module move must not change anything.)

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "refactor(store): move store.rs to store/mod.rs (no content change)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Extract `store/docs.rs`

Move document CRUD out of `mod.rs`. This is a cut-from-`mod.rs` / paste-into-`docs.rs` move — copy the method bodies verbatim; only the `use` header and module wiring are new.

**Files:**
- Create: `crates/engram/src/store/docs.rs`
- Modify: `crates/engram/src/store/mod.rs` (remove moved items; add `mod docs;`)

- [ ] **Step 1: Create `store/docs.rs` with the doc methods and helpers**

Create `crates/engram/src/store/docs.rs` with this header, then paste the **verbatim** bodies of `insert_doc`, `get_doc`, `get_by_key`, `list_namespace`, `delete_doc_by_key` inside one `impl Store` block, followed by the free fns `taint_from` and `map_doc` (cut from `mod.rs`):

```rust
use super::{now_secs, taint_str, Store};
use crate::error::Result;
use crate::model::{MemoryDoc, NewDoc, Taint};
use rusqlite::{params, OptionalExtension};

impl Store {
    // <-- paste insert_doc, get_doc, get_by_key, list_namespace, delete_doc_by_key here, unchanged
}

// <-- paste taint_from and map_doc here, unchanged
fn taint_from(s: &str) -> Taint { /* moved verbatim */ unreachable!() }
fn map_doc(row: &rusqlite::Row) -> rusqlite::Result<MemoryDoc> { /* moved verbatim */ unreachable!() }
```

(The two `unreachable!()` placeholders above are just to show where the verbatim bodies land — replace each stub with the real moved body. Do not leave `unreachable!()` in the file.)

- [ ] **Step 2: Remove the moved items from `mod.rs` and declare the submodule**

In `crates/engram/src/store/mod.rs`: delete the five doc methods from the main `impl Store` block, delete the `taint_from` and `map_doc` free fns, and add the declaration near the other module decls:

```rust
mod docs;
```

Keep `taint_str`, `now_secs`, `enqueue_job_sql`, `vec_to_bytes`, `bytes_to_vec`, `cosine`, `SCHEMA`, `Store`, `open`, `commit_ingest`, and the `#[cfg(test)] mod tests` in `mod.rs`.

- [ ] **Step 3: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass.
If you see `unused import` warnings in `mod.rs` (e.g. `OptionalExtension` no longer used there), remove the now-unused `use` lines from `mod.rs` until it builds warning-clean.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(store): extract docs.rs (doc CRUD)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Extract `store/chunks.rs`

**Files:**
- Create: `crates/engram/src/store/chunks.rs`
- Modify: `crates/engram/src/store/mod.rs` (remove `ChunkRow` + chunk methods; add `mod chunks;`)

- [ ] **Step 1: Create `store/chunks.rs`**

Create `crates/engram/src/store/chunks.rs` with this header, then move (verbatim) the `ChunkRow` struct and an `impl Store` block containing `upsert_chunk`, `delete_chunks_for_doc`, `chunks_for_namespace`, `chunks_for_doc`:

```rust
use super::{now_secs, vec_to_bytes, bytes_to_vec, Store};
use crate::error::Result;
use rusqlite::params;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ChunkRow {
    // <-- paste ChunkRow fields verbatim
}

impl Store {
    // <-- paste upsert_chunk, delete_chunks_for_doc, chunks_for_namespace, chunks_for_doc verbatim
}
```

(`chunks_for_doc` builds a `HashMap` of entities per chunk — hence the `std::collections::HashMap` import. If after building you find an import is unused, delete it.)

- [ ] **Step 2: Remove moved items from `mod.rs`, add `mod chunks;`**

Delete `ChunkRow` and the four chunk methods from `mod.rs`; add `mod chunks;`. If `mod.rs` no longer uses `HashMap`, remove that import there.

- [ ] **Step 3: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass; no warnings.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(store): extract chunks.rs (vector_chunks ops)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Extract `store/entities.rs`

**Files:**
- Create: `crates/engram/src/store/entities.rs`
- Modify: `crates/engram/src/store/mod.rs`

- [ ] **Step 1: Create `store/entities.rs`**

```rust
use super::Store;
use crate::error::Result;
use rusqlite::params;
use std::collections::HashMap;

impl Store {
    // <-- paste record_entities and docs_with_entities verbatim
}
```

(`docs_with_entities` returns a `HashMap<String, i64>` and builds a dynamic `IN (...)` placeholder list — move it verbatim including its `binds` logic.)

- [ ] **Step 2: Remove moved items from `mod.rs`, add `mod entities;`**

- [ ] **Step 3: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass; no warnings.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(store): extract entities.rs (chunk_entities graph index)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Extract `store/jobs.rs`

**Files:**
- Create: `crates/engram/src/store/jobs.rs`
- Modify: `crates/engram/src/store/mod.rs`

- [ ] **Step 1: Create `store/jobs.rs`**

Move the `Job` struct and the seven queue methods. `enqueue_job` calls `enqueue_job_sql`, which **stays in `mod.rs`** — reference it as `super::enqueue_job_sql`.

```rust
use super::{enqueue_job_sql, now_secs, Store};
use crate::error::Result;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct Job {
    // <-- paste Job fields verbatim
}

impl Store {
    // <-- paste enqueue_job, claim_job, complete_job, fail_or_retry_job,
    //     requeue_running, job, pending_jobs verbatim.
    //     Inside enqueue_job, the call becomes: enqueue_job_sql(&conn, ...) -> super::enqueue_job_sql is in scope via the use above.
}
```

- [ ] **Step 2: Remove moved items from `mod.rs`, add `mod jobs;`**

Delete `Job` and the seven methods from `mod.rs`. **Keep** `enqueue_job_sql` in `mod.rs` (still used by `commit_ingest`). Ensure `mod.rs` still imports what `enqueue_job_sql`/`commit_ingest` need (`params`, `Connection`).

- [ ] **Step 3: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass; no warnings.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(store): extract jobs.rs (post_acquire_jobs queue)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: Extract `store/trees.rs`

**Files:**
- Create: `crates/engram/src/store/trees.rs`
- Modify: `crates/engram/src/store/mod.rs`

- [ ] **Step 1: Create `store/trees.rs`**

Move the `TreeNode` and `NewTreeNode` structs, the `TREE_COLS` const, the tree methods, and the tree helpers `map_tree_node` and `insert_tree_node_sql`.

```rust
use super::{now_secs, vec_to_bytes, bytes_to_vec, Store};
use crate::error::Result;
use rusqlite::params;

#[derive(Debug, Clone)]
pub struct TreeNode {
    // <-- paste TreeNode fields verbatim
}

pub struct NewTreeNode<'a> {
    // <-- paste NewTreeNode fields verbatim
}

const TREE_COLS: &str =
    // <-- paste TREE_COLS value verbatim

impl Store {
    // <-- paste append_leaf_node, seal_buffer, unsealed_nodes, children_of,
    //     delete_unsealed_leaves_for_doc, tree_top_nodes, due_stale_buffers,
    //     touch_unsealed_created_at (keep its #[cfg(test)] attribute) verbatim
}

fn map_tree_node(row: &rusqlite::Row) -> rusqlite::Result<TreeNode> { /* moved verbatim */ }
fn insert_tree_node_sql(conn: &rusqlite::Connection, n: &NewTreeNode, now: f64) -> rusqlite::Result<String> { /* moved verbatim */ }
```

(Replace each `/* moved verbatim */` with the real body. `touch_unsealed_created_at` keeps its `#[cfg(test)]` attribute so it stays test-only.)

- [ ] **Step 2: Remove moved items from `mod.rs`, add `mod trees;`**

Delete the two structs, `TREE_COLS`, the tree methods, `map_tree_node`, and `insert_tree_node_sql` from `mod.rs`. Add `mod trees;`.

`commit_ingest` (still in `mod.rs`) does **not** call the tree helpers, so no cross-reference is needed. If `mod.rs` now has unused imports, remove them.

- [ ] **Step 3: Run the suite**

Run: `cargo test -p engram`
Expected: same N tests pass; no warnings.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(store): extract trees.rs (tree_nodes/tree_edges ops)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Final verification and tidy

**Files:**
- Modify (if needed): `crates/engram/src/store/mod.rs` (final import cleanup)

- [ ] **Step 1: Confirm `mod.rs` is now lean and declares all submodules**

`crates/engram/src/store/mod.rs` should contain only: the `use` header, `struct Store`, `const SCHEMA`, `impl Store { open, commit_ingest }`, the free fns `now_secs`, `enqueue_job_sql`, `taint_str`, `vec_to_bytes`, `bytes_to_vec`, `cosine`, the five `mod docs; mod chunks; mod entities; mod jobs; mod trees;` declarations, and the `#[cfg(test)] mod tests` block.

Run: `wc -l crates/engram/src/store/*.rs`
Expected: `mod.rs` is dramatically smaller than 1031 lines; each submodule is focused.

- [ ] **Step 2: Lint and format**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings/errors. Fix any unused-import / dead-code issues introduced by the moves.

Run: `cargo fmt --all`
Expected: formatting applied; re-run `cargo fmt --all -- --check` → clean.

- [ ] **Step 3: Full workspace test + build**

Run: `cargo test --workspace`
Expected: the same N store tests + all other crate tests pass. No test was added or removed.

Run: `cargo build --release -p engram && ls -l target/release/engram`
Expected: server binary present at the deploy path.

- [ ] **Step 4: Commit any final tidy**

```bash
git add -A
git commit -m "refactor(store): final import/clippy cleanup after split

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

(If Steps 2–3 produced no changes, skip this commit.)

---

## Self-review (completed)

**Spec coverage (Phase 0 only):**
- "Convert to a workspace" → Task 1 (root virtual manifest + `crates/engram` + `engram-index`/`engram-mcp` stubs). ✅
- "Split `store.rs`" → Tasks 2–7 (`store/mod.rs` + `docs`/`chunks`/`entities`/`jobs`/`trees`). ✅
- "No behavior change; everything still green" → every task ends with `cargo test` at the same N; Task 8 adds clippy/fmt/full-workspace gates. ✅
- Deploy path preserved → Task 1 Step 8 + Task 8 Step 3 verify `target/release/engram`. ✅
- Later phases (1, 2, 3a, 3b, 4) are intentionally **out of scope** here; the stub crates reserve their place.

**Placeholder scan:** The only `/* moved verbatim */` / `unreachable!()` markers are explicit "paste the existing body here" guides for a cut/paste move, each with an instruction to replace them — not left as real code. No "TODO/handle errors/add validation" placeholders.

**Type/name consistency:** Method, struct, const, and free-fn names match the current `store.rs` exactly (`commit_ingest`, `enqueue_job_sql`, `delete_doc_by_key`, `ChunkRow`, `TreeNode`, `NewTreeNode`, `TREE_COLS`, `vec_to_bytes`/`bytes_to_vec`, `cosine`). The cross-module references (`super::enqueue_job_sql`, `super::now_secs`, `super::{vec_to_bytes, bytes_to_vec}`, `super::taint_str`) are consistent with what stays in `mod.rs`.

**Risk note:** Splitting an inherent `impl Store` across submodules is valid Rust (multiple `impl Store { … }` blocks in the same crate). Child modules may reference ancestor-private items via `super::`. If a `use super::…` line names an item the engineer chose to keep elsewhere, the compiler error is immediate and local — fix the path and re-run.
