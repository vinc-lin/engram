# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`engram` is a shared, single-writer memory service for an agent-ops cluster: an axum
HTTP API over SQLite that runs OpenHuman's ingest → store → retrieve design, plus an
autonomous background consolidation pipeline that folds documents into summary trees.
The design spec lives in the `anima` repo
(`docs/superpowers/specs/2026-06-06-shared-memory-service-design.md`); `README.md` is the
user-facing overview. The whole crate is ~3.2k lines of Rust across `src/*.rs`.

## Commands

```bash
cargo build                 # debug
cargo build --release       # release (deploy uses target/release/engram)
cargo test                  # all tests (inline #[cfg(test)] in every module)
cargo test commit_ingest_is_atomic_and_enqueues   # run a single test by name (substring match)
cargo test -- --ignored     # network tests, off by default: need a live Ollama / litellm gateway
cargo clippy                # lints (code already carries #[allow(clippy::...)] in a few spots)
cargo fmt                   # format

# Run locally — env vars are the only config surface (see src/config.rs for the full list):
ENGRAM_DB=engram.db ENGRAM_BIND=127.0.0.1:8088 ENGRAM_TOKEN=secret cargo run

# Deploy (WSL2): screen-managed binary + reverse SSH tunnel to gateway-host.
deploy/run.sh {start|stop|status|logs}
```

Build note: `rusqlite` uses the `bundled` feature, so SQLite is compiled into the binary
(no system libsqlite needed, but a C compiler is required).

## Architecture

### Two execution planes (the central design choice)
- **Hot path** — async axum handlers (`api.rs`). The embedder and all LLM/audit calls use
  `reqwest::blocking`, so every handler that embeds (`ingest_doc`, `query_docs`, `tree_query`)
  wraps the work in `tokio::task::spawn_blocking`. **Adding a handler that embeds or calls the
  gateway must do the same**, or it will stall the runtime.
- **Cold path** — `jobs::spawn_workers` and `tree::spawn_sweeper` run on dedicated
  **`std::thread` OS threads** (not tokio tasks), driven from `main.rs`. Workers drain the
  `post_acquire_jobs` queue; the sweeper periodically seals stale buffers. Both share a
  `stop: Arc<AtomicBool>`. They call blocking HTTP directly (no spawn_blocking needed there).

### Storage: single-writer + read pool (`store.rs`)
`Store` holds `read: r2d2::Pool` (up to 8 connections) and `write: Arc<Mutex<Connection>>`.
**All writes go through the one mutexed connection; reads come from the pool.** WAL mode
(`journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=15s`) lets readers run concurrently
with the writer. `Store` is `Clone` (it just clones the `Arc`s), so workers get cheap handles.

Any multi-statement write is wrapped in a transaction so a crash can't leave a partial state:
`commit_ingest` (doc + chunks + entities + job enqueue), `seal_buffer` (parent node + edges +
seal children), `delete_chunks_for_doc` (chunks then entities), `claim_job` (read oldest +
mark running). Schema is 6 tables (+7 indexes) created once on `Store::open`: `memory_docs`,
`vector_chunks`, `chunk_entities`, `post_acquire_jobs`, `tree_nodes`, `tree_edges`. There are no
FK constraints —
referential integrity is upheld by the transactions above. Everything is namespace-scoped by a
column; there is **no table-per-namespace and no per-namespace access control**.

**Off-lock invariant:** embeddings and LLM summaries are computed *before* taking the write
lock, then handed to an atomic commit (`ingest::ingest_document` embeds, then `commit_ingest`;
`tree::seal_cascade` summarizes + embeds, then `seal_buffer`). Never hold the write mutex across
network I/O.

### Module map
| File | Role |
|------|------|
| `main.rs` | Bootstrap: load `Config` → `Store::open` → `requeue_running` (crash recovery) → build `GatewayEmbedder` + `TreeProcessor` → spawn workers + sweeper → serve axum |
| `api.rs` | Router, `AppState` (`store`, `token`, `Arc<dyn Embedder>`), Bearer-auth middleware, handlers |
| `store.rs` | SQLite layer (schema, single-writer, transactions, vector + entity + tree + job queries) |
| `ingest.rs` | Chunking, mechanical entity extraction, `ingest_document` (hot path) |
| `retrieve.rs` | `query` (hybrid scoring), `recall` (recency), `drill_down` (tree BFS) |
| `embed.rs` | `Embedder` trait + `HashEmbedder` (tests), `OllamaEmbedder`, `GatewayEmbedder` (prod) |
| `jobs.rs` | `JobProcessor` trait, `worker_tick`, `spawn_workers`, `NoopProcessor` |
| `tree.rs` | Cold pipeline: `process_doc` fanout, `seal_cascade`, `TreeProcessor`, `spawn_sweeper` |
| `llm.rs` | `ChatClient`/`AuditSink` traits, gateway clients, `summarize_audited`, fallback, test doubles |
| `vault.rs` | Optional Obsidian markdown mirror of docs + sealed summaries |
| `config.rs` / `model.rs` / `error.rs` / `lib.rs` | Env config, request/response types, error enum, module decls |

### Ingest pipeline (`ingest.rs`)
`POST /docs` → `ingest_document`: chunk content (paragraph split on `\n\n`, packed to 800
**chars** — CJK-safe, not bytes; oversized paragraphs hard-split) → embed each chunk off-lock →
`extract_entities` per chunk → `commit_ingest` (atomic, also enqueues the consolidation job).
`extract_entities` is purely mechanical regex → canonical ids `email:` / `url:` / `handle:` /
`hashtag:` (lowercased, sorted, deduped; URL spans are masked before the handle/hashtag passes
so `@x`/`#y` inside a URL aren't mis-extracted).

### Hybrid retrieval (`retrieve.rs`)
`query` scores each candidate doc by its best chunk, blending three signals. **The same
`extract_entities` runs on the query**; if any query entity matches a stored doc, graph weights
apply (graph 0.55 / vector 0.30 / keyword 0.15, graph normalized by the max entity-overlap
count); otherwise it falls back to vector 0.65 / keyword 0.35. Each `Hit` exposes its
`vector`/`keyword`/`graph` sub-scores. `recall` is query-less, ordered by a freshness decay.

### Autonomous consolidation (`tree.rs`)
A worker claims a job and runs `process_doc`: it fans every chunk out as a level-0 leaf into
three trees — **Source** (`tree_key` = author), **Global** (`tree_key` = `"global"`), and one
**Topic** tree per entity (`tree_key` = the entity id). Each `append_leaf` triggers
`seal_cascade`, which seals a buffer when a gate trips and recurses upward (`MAX_CASCADE_DEPTH`
= 32): **L0** gate = summed approx-tokens (`chars/4`) ≥ `seal_input_token_budget`; **L≥1** gate
= sibling count ≥ `seal_fanout`; plus a stale-flush age gate. Sealing summarizes the buffer via
the LLM gateway (`summarize_audited`, every call self-audits to llm-audit); on any LLM error it
falls back to a deterministic concat (`fallback_summary`) **so a cascade never aborts**.
`drill_down` (`POST /tree`, defaults to the Global tree) BFSes from the top nodes, also surfaces
fresh unsealed L0 leaves once a tree is consolidated, keeps the latest leaf per `doc_id`, and
cosine-reranks against the query.

### Job queue semantics (`store.rs` + `jobs.rs`)
`job_id` = `"{namespace}:{document_id}"`, so re-ingesting a doc idempotently re-enqueues (the
`ON CONFLICT` resets it to `pending`). `claim_job` atomically flips `pending`→`running` and
increments `attempts`; `fail_or_retry_job` returns it to `pending` until `attempts` hits
`max_attempts`, then `failed`; `requeue_running` (run at startup) recovers jobs orphaned by a
crash. `worker_tick` is the synchronous "claim-one-and-process" unit — tests drive the whole
cold pipeline deterministically with `while worker_tick(...) {}`.

## Gotchas & invariants

- **Embedder signature gates everything.** Each embedder's `signature()` (e.g.
  `gateway:bge-m3:1024`, `hash:64`) is stored on every chunk/tree node, and reads filter by it
  (`chunks_for_namespace(ns, sig)`). Changing `ENGRAM_EMBED_MODEL` or `ENGRAM_EMBED_DIM` makes
  all existing chunks invisible (orphaned, not migrated) — a full re-ingest is required.
- **`GatewayEmbedder` is the only embedder wired in `main.rs`.** `OllamaEmbedder` exists but is
  not selected at runtime; it's a same-host/fallback option. Embeddings *and* LLM summaries both
  route through the litellm gateway.
- **Sealed tree nodes are immutable.** Re-ingest drops only the doc's *unsealed* L0 leaves
  (`delete_unsealed_leaves_for_doc`); sealed summary history is never rewritten. Likewise
  `delete_doc_by_key` (`DELETE /v1/:ns/docs/by-key/:key`, the "forget" path) atomically removes
  the doc + chunks + entities + pending job + unsealed leaves, but **not** sealed summaries — a
  forgotten doc's text can still live inside sealed summary bodies (and the write-only vault).
- **Auth is a single shared Bearer token** (exact string match, no scopes/expiry). The namespace
  is just a request-path string — any caller with the token can read/write any namespace.
- **Config parsing is permissive.** Missing or unparseable env vars silently fall back to
  defaults (a typo'd `ENGRAM_EMBED_DIM=abc` yields 1024, not an error). `src/config.rs` is the
  single source of truth for the ~21 env vars (several beyond the README table).
- **SQLite must live on native ext4, not the v9fs repo mount.** This repo (`/mnt/x/code/engram`)
  is a Windows v9fs mount where WAL is flaky and `chmod` doesn't stick. Deploy keeps the DB,
  vault, logs, and `.env` (chmod 600) under `~/engram` on ext4. Point `ENGRAM_DB` at an ext4
  path for local runs too. (`cargo test` is fine — it uses system tempdirs.)

## Testing conventions

Tests are inline `#[cfg(test)] mod tests` in each module — there is no `tests/` dir. They build
an ephemeral DB via `tempfile::tempdir()` + `Store::open`, and use **`HashEmbedder`** (a
deterministic, network-free bag-of-words embedder) so nothing touches the network. LLM doubles
live in `llm.rs` (`FakeChatClient`, `NullAuditSink`). API tests use the `spawn()` / `spawn_full()`
helpers in `api.rs` (bind `127.0.0.1:0`, drive with `reqwest`). To make consolidation
deterministic, tests either call `worker_tick`/`process_doc` directly or shrink the seal gates
(`seal_input_token_budget`, `seal_fanout`) via `Config::from_vars`. The two `#[ignore]`d tests in
`embed.rs` require a live Ollama / gateway and only run under `cargo test -- --ignored`.
