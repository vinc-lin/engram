# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`engram` is a shared, single-writer memory service for an agent-ops cluster: an axum
HTTP API over SQLite that runs OpenHuman's ingest → store → retrieve design, plus an
autonomous background consolidation pipeline that folds documents into summary trees. It is
**also a code knowledge base for coding agents** — the same store/ingest/retrieve machinery,
extended with code-mode chunking, AST symbol extraction, git-history ingest, and a conventions
digest. The design spec lives in the `anima` repo
(`docs/superpowers/specs/2026-06-06-shared-memory-service-design.md`); `README.md` and
`ARCHITECTURE.md` are the user-facing overviews. The whole project is a Cargo workspace of three
crates totalling ~8.8k lines of Rust:

| Crate | Role |
|-------|------|
| `crates/engram` | Core service: HTTP API, store, ingest, retrieve, embed, tree, llm, conventions, treesit, vault |
| `crates/engram-index` | Repo indexer CLI (`index` / `reindex` / `index-history` / `install-hook`) |
| `crates/engram-mcp` | MCP server exposing the 6 agent tools over stdio and (optionally) HTTP JSON-RPC |

## Commands

```bash
cargo build                 # debug (whole workspace)
cargo build --release       # release (deploy uses target/release/engram)
cargo test                  # all tests (inline #[cfg(test)] in every module, ~175)
cargo test commit_ingest_is_atomic_and_enqueues   # run a single test by name (substring match)
cargo test -- --ignored     # network tests, off by default: need a live Ollama / litellm gateway
cargo clippy                # lints (clippy + fmt are clean; code carries #[allow(clippy::...)] in a few spots)
cargo fmt                   # format

# Run locally — env vars are the only config surface (see crates/engram/src/config.rs for the full list):
ENGRAM_DB=engram.db ENGRAM_BIND=127.0.0.1:8088 ENGRAM_TOKEN=secret cargo run -p engram

# Index a repo into engram (namespace repo:<id>) and ingest its git history:
cargo run -p engram-index -- index . --namespace repo:engram --url http://127.0.0.1:8088
cargo run -p engram-index -- index-history . --namespace repo:engram:history --url ...

# MCP server (stdio by default; set ENGRAM_MCP_HTTP=addr for HTTP JSON-RPC):
cargo run -p engram-mcp

# Eval: ingest-rate + recall@k + line-recall + hard negatives (see eval/RESULTS.md):
python eval/validate.py

# Deploy (WSL2): screen-managed binary + reverse SSH tunnel to gateway-host.
deploy/run.sh {start|stop|status|logs}
```

Build note: `rusqlite` uses the `bundled` feature, so SQLite is compiled into the binary
(no system libsqlite needed, but a C compiler is required). Tree-sitter grammars
(Rust/Python/JS/TS/TSX/Go/Kotlin/Java/C/C++) are also compiled in, so a C compiler is required too.

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

### Storage: single-writer + read pool (`store/`)
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

### Module map (`crates/engram/src/`)
| File | Role |
|------|------|
| `main.rs` | Bootstrap: load `Config` → `Store::open` → `requeue_running` (crash recovery) → build embedder (`GatewayEmbedder`, optionally wrapped in `FallbackEmbedder`) + `GatewayChatClient` + `TreeProcessor` → spawn workers + sweeper → serve axum |
| `api.rs` | Router, `AppState` (`store`, `token`, `Arc<dyn Embedder>`, `Arc<dyn ChatClient>`), Bearer-auth middleware, handlers |
| `store/` | SQLite layer split into a module dir: `mod.rs` (schema + open + write lock), `docs.rs`, `chunks.rs`, `entities.rs`, `jobs.rs`, `trees.rs` (single-writer, transactions, vector + entity + tree + job queries) |
| `ingest.rs` | Chunking + mechanical entity extraction + `ingest_document` (hot path); **code-mode dispatch** on `meta.kind=="file"` |
| `treesit.rs` | Tree-sitter boundary chunking + AST symbols for 10 langs (Rust/Python/JS/TS/TSX/Go/Kotlin/Java/C/C++); C/C++ also support a definition-packing mode (`is_boundary`/`chunk_segments`) |
| `conventions.rs` | `rebuild_conventions` — derives the per-repo conventions/architecture digest into `<ns>:meta` |
| `retrieve.rs` | `query` (hybrid scoring), `search_code` (chunk-level code search), `recall` (recency), `drill_down` (tree BFS) |
| `embed.rs` | `Embedder` trait + `HashEmbedder` (tests), `OllamaEmbedder`, `GatewayEmbedder` (prod), `FallbackEmbedder` (primary+fallback wrapper) |
| `jobs.rs` | `JobProcessor` trait, `worker_tick`, `spawn_workers`, `NoopProcessor` |
| `tree.rs` | Cold pipeline: `process_doc` fanout, `seal_cascade`, `TreeProcessor`, `spawn_sweeper` |
| `llm.rs` | `ChatClient`/`AuditSink` traits, `GatewayChatClient`/`HttpAuditSink`, `summarize_audited`, fallback, test doubles |
| `vault.rs` | Optional Obsidian markdown mirror of docs + sealed summaries |
| `config.rs` / `model.rs` / `error.rs` / `lib.rs` | Env config, request/response types, error enum, module decls |

Sibling crates: `engram-index/src/` (`walk.rs`, `commits.rs`, `diff.rs`, `hook.rs`, `client.rs`)
is the indexer CLI; `engram-mcp/src/lib.rs` is the MCP tool surface.

### Namespaces (the code-KB mapping)
A repo maps onto three namespaces: a **file** is a document (`meta.kind="file"` → code mode), an
**identifier** is an entity (`sym:` / `import:` / `path:`).
- `repo:<id>` — the file documents + their chunks/symbols (the searchable code).
- `repo:<id>:history` — git commits (`meta.kind="commit"`), backing `why`.
- `repo:<id>:meta` — the single conventions/architecture digest doc (key `conventions`).

Prose memory (the original use case) keeps using arbitrary author-scoped namespaces; the entity
ids there are the regex-extracted `email:`/`url:`/`handle:`/`hashtag:` form.

### Ingest pipeline (`ingest.rs` + `treesit.rs`)
`POST /docs` → `ingest_document`: chunk content → embed each chunk off-lock → `extract_entities`
per chunk → `commit_ingest` (atomic, also enqueues the consolidation job). Two chunking modes:
- **Prose mode** (default): paragraph split on `\n\n`, packed to 800 **chars** — CJK-safe, not
  bytes; oversized paragraphs hard-split. Entities = mechanical regex → canonical ids `email:` /
  `url:` / `handle:` / `hashtag:` (lowercased, sorted, deduped; URL spans masked before the
  handle/hashtag passes so `@x`/`#y` inside a URL aren't mis-extracted).
- **Code mode** (dispatched on `meta.kind=="file"`): **tree-sitter function/type-boundary
  chunking** (`treesit.rs`; Rust/Python/JS/TS/TSX/Go/Kotlin/Java/C/C++; `.h`→C++ — falls back to the
  heuristic line/symbol chunker `chunk_code` on unsupported langs or parse failure) + **AST symbol
  extraction** yielding accurate `sym:` entities, plus an `import:` and `path:` entity. Token-aware
  and CJK-safe; **per-chunk-tolerant** — a bad chunk is skipped, the rest still ingest. C/C++ can
  pack consecutive small definitions into wider, boundary-aligned chunks (`ENGRAM_CODE_NATIVE_PACK`
  + `ENGRAM_CODE_NATIVE_BUDGET`) — only useful with a long-context embedder (mxbai truncates at 512
  tok); validated to lift native line-recall 0.59→0.89 (see `eval/RESULTS_android.md`).

### Hybrid retrieval (`retrieve.rs`)
`query` (prose) scores each candidate doc by its best chunk, blending three signals. **The same
`extract_entities` runs on the query**; if any query entity matches a stored doc, graph weights
apply (graph 0.55 / vector 0.30 / keyword 0.15, graph normalized by the max entity-overlap
count); otherwise it falls back to vector 0.65 / keyword 0.35. Each `Hit` exposes its
`vector`/`keyword`/`graph` sub-scores. `recall` is query-less, ordered by a freshness decay.

`search_code` is **chunk-level** (not doc-level): vector cosine + keyword, plus a **path-type
ranking prior** (gated by `ENGRAM_CODE_PATH_PRIOR`) that down-weights docs/tests/config so source
ranks first. It returns `path:line` + a code snippet. It reads **chunks, not trees** — so code
consolidation is unread until the digest tools (`get_architecture`/`get_module`) are used. An
optional abstention floor (`ENGRAM_CODE_MIN_SCORE`, default off) drops hits below a score so
absent-topic queries return nothing instead of a confident-but-wrong hit.

### Autonomous consolidation (`tree.rs`)
A worker claims a job and runs `process_doc`, which fans chunks out as level-0 leaves into a set
of trees that depends on the doc's mode:
- **Prose**: a **Source** tree (`tree_key` = author), the **Global** tree (`tree_key` =
  `"global"`), and one **Topic** tree per entity (`tree_key` = the entity id).
- **Code**: a directory-keyed **module** tree + the **global** tree + **topic** trees on `sym:` /
  `import:` entities only. **Gated by `ENGRAM_CONSOLIDATE_CODE` (DEFAULT OFF)** — since
  `search_code` reads chunks, code consolidation only matters once the digest tools are used.

Each `append_leaf` triggers `seal_cascade`, which seals a buffer when a gate trips and recurses
upward (`MAX_CASCADE_DEPTH` = 32): **L0** gate = summed approx-tokens (`chars/4`) ≥
`seal_input_token_budget`; **L≥1** gate = sibling count ≥ `seal_fanout`; plus a stale-flush age
gate. Sealing summarizes the buffer via the LLM gateway (`summarize_audited`, every call
self-audits to llm-audit); on any LLM error it falls back to a deterministic concat
(`fallback_summary`) **so a cascade never aborts**. `drill_down` (`POST /tree`, defaults to the
Global tree) BFSes from the top nodes, also surfaces fresh unsealed L0 leaves once a tree is
consolidated, keeps the latest leaf per `doc_id`, and cosine-reranks against the query.

### API surface & MCP tools (`api.rs` + `engram-mcp`)
HTTP endpoints (all under `/v1/:namespace`, Bearer-auth; plus `GET /healthz`): `GET|POST /docs`,
`GET /docs/:id`, `DELETE /docs/by-key/:key`, `POST /query`, `POST /code/search`, `GET /recall`,
`POST /tree`, `POST /code/architecture`, `POST /code/architecture/rebuild`, `POST /code/module`,
`GET /conventions`, `POST /conventions/rebuild`.

**Digest path is single-pass, not the tree.** `get_architecture`/`get_module` serve a cached
**single-pass** digest from `<ns>:meta` (key `architecture` / `module:<dir>`), built by
`POST /code/architecture/rebuild` → `conventions::rebuild_architecture_digest` (one LLM summary over
the repo's source files; the consolidation-tree drill is only a fallback when no digest is cached).
A single compression pass preserves specifics that the deep summary-of-summaries fold loses —
measured: one-shot 1.58 vs tree 1.08 of 2, see `eval/RESULTS_distillation.md`.

`engram-mcp` exposes **6 tools** over stdio (and HTTP JSON-RPC when `ENGRAM_MCP_HTTP` is set),
each backed by an endpoint above: `search_code`, `get_architecture` (global single-pass digest),
`get_module(path)` (a directory's single-pass digest), `why(query)` (git-history commits, via
`:history`), `find_symbol(name)`, `get_conventions()` (the `:meta` digest).

### Job queue semantics (`store/jobs.rs` + `jobs.rs`)
`job_id` = `"{namespace}:{document_id}"`, so re-ingesting a doc idempotently re-enqueues (the
`ON CONFLICT` resets it to `pending`). `claim_job` atomically flips `pending`→`running` and
increments `attempts`; `fail_or_retry_job` returns it to `pending` until `attempts` hits
`max_attempts`, then `failed`; `requeue_running` (run at startup) recovers jobs orphaned by a
crash. `worker_tick` is the synchronous "claim-one-and-process" unit — tests drive the whole
cold pipeline deterministically with `while worker_tick(...) {}`.

## Gotchas & invariants

- **Embedder signature gates everything.** Each embedder's `signature()` (e.g.
  `gateway:mxbai-embed-large:1024`, `hash:64`) is stored on every chunk/tree node, and reads
  filter by it (`chunks_for_namespace(ns, sig)`). Changing `ENGRAM_EMBED_MODEL` or
  `ENGRAM_EMBED_DIM` makes all existing chunks invisible (orphaned, not migrated) — a full
  re-ingest is required. **Model decision matrix (prose→mxbai, native code→bge-m3) + serving
  gotchas (broken gateway bge-m3 route, single-URL embed/LLM coupling, 500-storm fix):
  `docs/EMBEDDINGS.md`.**
- **`GatewayEmbedder` is the prod embedder** (model `mxbai-embed-large`, dim 1024), routed
  through the litellm gateway, and so are LLM summaries (model `deepseek-chat`). When
  `ENGRAM_EMBED_FALLBACK` is set, `main.rs` wraps it in a `FallbackEmbedder` that adds a local
  `OllamaEmbedder` failover — but its `signature()` **delegates to the primary**, so a failover
  never orphans chunks under a different signature. (`HashEmbedder` is tests-only.)
- **Sealed tree nodes are immutable.** Re-ingest drops only the doc's *unsealed* L0 leaves
  (`delete_unsealed_leaves_for_doc`); sealed summary history is never rewritten. Likewise
  `delete_doc_by_key` (`DELETE /v1/:ns/docs/by-key/:key`, the "forget" path) atomically removes
  the doc + chunks + entities + pending job + unsealed leaves, but **not** sealed summaries — a
  forgotten doc's text can still live inside sealed summary bodies (and the write-only vault).
- **Auth is a single shared Bearer token** (exact string match, no scopes/expiry). The namespace
  is just a request-path string — any caller with the token can read/write any namespace.
- **Config parsing is permissive.** Missing or unparseable env vars silently fall back to
  defaults (a typo'd `ENGRAM_EMBED_DIM=abc` yields 1024, not an error). `crates/engram/src/config.rs`
  is the single source of truth for all env vars (several beyond the README table). The
  code-KB / fallback ones (defaults in parens): `ENGRAM_EMBED_MODEL` (`mxbai-embed-large`),
  `ENGRAM_EMBED_DIM` (1024), `ENGRAM_EMBED_TIMEOUT_SECS` (30), `ENGRAM_EMBED_FALLBACK` (false),
  `ENGRAM_CONSOLIDATE_CODE` (false), `ENGRAM_CODE_SYMBOL_SPLIT` (true), `ENGRAM_CODE_PATH_PRIOR`
  (true), `ENGRAM_CODE_TREE_SITTER` (true), `ENGRAM_CODE_MIN_SCORE` (0.0 = off; search_code
  abstention floor), `ENGRAM_CODE_NATIVE_PACK` (false), `ENGRAM_CODE_NATIVE_BUDGET` (480),
  `ENGRAM_MCP_HTTP` (unset), `ENGRAM_INDEX_TIMEOUT_SECS` (120, read by `engram-index`, not the core
  `Config`). Plus the gateway/LLM ones:
  `ENGRAM_GATEWAY_URL`, `ENGRAM_GATEWAY_KEY`, `ENGRAM_LLM_MODEL`, `ENGRAM_LLM_PROVIDER`.
- **SQLite must live on native ext4, not the v9fs repo mount.** This repo lives on a Windows
  v9fs mount where WAL is flaky and `chmod` doesn't stick. Deploy keeps the DB, vault, logs, and
  `.env` (chmod 600) under `$HOME/engram` on ext4. Point `ENGRAM_DB` at an ext4 path for local
  runs too. (`cargo test` is fine — it uses system tempdirs.)
- **Code consolidation is OFF by default** (`ENGRAM_CONSOLIDATE_CODE=false`). `search_code` works
  without it (it reads chunks). Phases 2/3a/3b are code-complete but their *live quality bars*
  (non-fallback module/architecture digests; `why` relevance ≥ 0.7) require a cost-bearing
  consolidation + history-ingest run and remain **pending-live**.

## Testing conventions

Tests are inline `#[cfg(test)] mod tests` in each module (across all three crates) — there is no
`tests/` dir; ~175 tests total, clippy + fmt clean. They build an ephemeral DB via
`tempfile::tempdir()` + `Store::open`, and use **`HashEmbedder`** (a deterministic, network-free
bag-of-words embedder) so nothing touches the network. LLM doubles live in `llm.rs`
(`FakeChatClient`, `NullAuditSink`). API tests use the `spawn()` / `spawn_full()` helpers in
`api.rs` (bind `127.0.0.1:0`, drive with `reqwest`). To make consolidation deterministic, tests
either call `worker_tick`/`process_doc` directly or shrink the seal gates
(`seal_input_token_budget`, `seal_fanout`) via `Config::from_vars`. The two `#[ignore]`d tests in
`embed.rs` require a live Ollama / gateway and only run under `cargo test -- --ignored`.

**Retrieval eval** lives in `eval/`: `validate.py` measures ingest-rate + recall@1/5/10 +
line-recall + hard negatives over `eval/agentmemory_gold.json` (35 labeled NL→file queries,
≤2/file). Live production numbers under the tree-sitter config: ingest 0.998, recall@1 0.543,
recall@5 0.771, recall@10 0.914, line-recall@10 0.771 — see `eval/RESULTS.md` for the recall
journey (0.533 baseline → 0.800 Phase F → tree-sitter, which trades a little recall@5 for higher
precision/coverage/line-accuracy).

There is also a **second eval suite for Android feature-migration** (`eval/android/avm_gold.json`,
108 probes + 15 cross-layer feature footprints): `eval/agent/` is a litellm tool-calling coding
agent (DeepSeek/Qwen) and `eval/harness/` runs Lens-2 (retrieval vs ripgrep/native) + Lens-1
(agent A/B). Findings in `eval/RESULTS_android.md`.
