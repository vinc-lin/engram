# engram — Architecture & Structure

> Snapshot of the codebase structure, intended as a base for further analysis
> (refactoring, performance, coverage, security, feature planning). Generated from a
> full read of `src/*.rs` + `Cargo.toml`. Pairs with `README.md` (user view) and
> `CLAUDE.md` (contributor guide). Line counts are approximate; everything else is
> extracted from source.

`engram` is a shared, single-writer memory service: an **axum HTTP API over SQLite**
implementing OpenHuman's `ingest → store → retrieve` design, plus an **autonomous
background consolidation pipeline** that folds documents into summary trees. ~3.2k
lines of Rust, one Cargo crate (`lib` + `bin`).

---

## 1. Snapshot

| Module | Total LOC | ~Test LOC | Pub items | Layer | Responsibility |
|--------|----------:|----------:|----------:|-------|----------------|
| `store.rs`    | 1031 | ~514 | 34 | infra      | SQLite: schema, single-writer, transactions, all queries |
| `tree.rs`     | 369 | ~123 |  8 | pipeline   | Cold consolidation: fan-out, seal cascade, sweeper |
| `api.rs`      | 400 | ~209 |  2 | http       | axum router, AppState, Bearer auth, handlers |
| `retrieve.rs` | 316 | ~114 |  5 | pipeline   | `query` (hybrid), `recall`, `drill_down` |
| `llm.rs`      | 288 | ~71  | 15 | io         | Chat + audit traits, gateway clients, `summarize_audited` |
| `ingest.rs`   | 204 | ~91  |  3 | pipeline   | Chunking, entity extraction, `ingest_document` |
| `embed.rs`    | 193 | ~51  |  7 | io         | `Embedder` trait + Hash/Ollama/Gateway impls |
| `vault.rs`    | 121 | ~31  |  5 | io         | Optional Obsidian markdown mirror |
| `jobs.rs`     | 115 | ~54  |  4 | infra      | `JobProcessor` trait, `worker_tick`, `spawn_workers` |
| `config.rs`   | 112 | ~53  |  3 | base       | Env → `Config` |
| `model.rs`    |  68 | ~30  |  3 | base       | `MemoryDoc`, `NewDoc` (incl. opaque `meta`), `Taint` |
| `main.rs`     |  50 |  0   |  0 | bin        | Composition root |
| `error.rs`    |  19 |  0   |  2 | base       | `Error` enum + `Result` alias |
| `lib.rs`      |  12 |  0   | 12 | —          | Module declarations only |

Tests are inline `#[cfg(test)] mod tests` per module — **no `tests/` dir**. Roughly a
third of the code is tests; `store.rs` and `api.rs` are ~47–52% test by line.

---

## 2. Crate layout

```
engram/  (single crate: [lib] engram = src/lib.rs, [[bin]] engram = src/main.rs)
├── Cargo.toml / Cargo.lock
├── README.md / CLAUDE.md / ARCHITECTURE.md
├── .env.example
├── deploy/run.sh          # screen-managed binary + reverse SSH tunnel (WSL2)
└── src/                   # flat modules — one .rs per module, no nested dirs
    ├── lib.rs  main.rs
    ├── api.rs  config.rs  embed.rs  error.rs  ingest.rs  jobs.rs
    └── llm.rs  model.rs   retrieve.rs  store.rs  tree.rs  vault.rs
```

`lib.rs` is pure declaration (no logic). `main.rs` is a 50-line composition root. The
library is fully usable and testable without the binary — which the inline tests exploit
(they build a `Store` + `HashEmbedder` directly).

---

## 3. Module dependency graph (production)

Intra-crate edges from `use crate::…` **and** fully-qualified `crate::mod::…` references,
restricted to non-test code. It is a clean **DAG (no cycles)**.

```
                         ┌──────────── main (bin) ────────────┐
                         │ wires hot path + cold path together │
            ┌────────────┴───────────────┬─────────────────────┘
            ▼                             ▼
          api ───────────► retrieve ───────────────┐
            │   │              │                    │
            │   └──► ingest ◄──┘  (query reuses     │
            │          │          ingest's entity   │
            ▼          ▼          extractor)        ▼
   ┌─────► store ◄─────┴──── jobs        tree ──► vault
   │        │                  │          │  │      │
   │        ▼                  ▼          │  └► llm  ▼
   │      model ◄──────────────┼──────────┼─────────┤
   └──────► error ◄────────────┴── embed ◄┴── config (leaf)
            (universal leaf)
```

**Production edge list:**

| Module | Depends on (intra-crate) |
|--------|--------------------------|
| `main` (bin) | api, config, embed, jobs, llm, store, tree, vault |
| `api`        | embed, ingest, model, retrieve, store |
| `tree`       | config, embed, error, jobs, llm, store, vault |
| `retrieve`   | embed, error, ingest, model, store |
| `ingest`     | embed, error, model, store |
| `jobs`       | error, store |
| `store`      | error, model |
| `embed`      | error |
| `llm`        | error |
| `vault`      | error, model |
| `config`, `model`, `error` | — (leaves) |

**Structural observations (analysis hooks):**
- **`store` is the hub** — depended on by `api`, `ingest`, `jobs`, `retrieve`, `tree` (5 of 10 non-trivial modules). It is also the largest file. Any storage change ripples widely.
- **`error` is a universal leaf** (no outgoing edges); 8 of 10 non-base modules import it directly — `api` and `main` are the exceptions (they surface `axum::Response` and `Box<dyn std::error::Error>` respectively). `model` is depended on by `api/ingest/retrieve/store/vault`; `config` is a near-leaf (only `tree` + `main`).
- **Two orchestrators:** `api` composes the *hot* (request) path; `tree` composes the *cold* (consolidation) path. The bin `main` has the largest fan-out (8) as the composition root; among **library** modules `tree` is the most coupled (7 deps).
- **Notable edge `retrieve → ingest`:** `query` calls `ingest::extract_entities` so query-side entity extraction is byte-for-byte the same as ingest-side. This coupling is intentional (consistency) but means the entity logic is shared infrastructure, not ingest-private.
- **Test-only edges** (present only under `#[cfg(test)]`, not in the prod graph): `api → {config, jobs, llm, tree}`, `retrieve → {config, llm, tree}`, `tree → {ingest, model}`. Tests drive the cold pipeline directly, hence the extra coupling.

---

## 4. Module reference cards

### `store.rs` — persistence (hub)
- **Type:** `Store { read: r2d2::Pool<SqliteConnectionManager>, write: Arc<Mutex<Connection>> }`, `Clone`.
- **Surface (34 pub items):** `open`; docs (`insert_doc`, `get_doc`, `get_by_key`, `list_namespace`, `delete_doc_by_key`); chunks/entities (`upsert_chunk`, `delete_chunks_for_doc`, `record_entities`, `docs_with_entities`, `chunks_for_namespace`, `chunks_for_doc`); the atomic hot-commit `commit_ingest`; job queue (`enqueue_job`, `claim_job`, `complete_job`, `fail_or_retry_job`, `requeue_running`, `job`, `pending_jobs`); trees (`append_leaf_node`, `seal_buffer`, `unsealed_nodes`, `children_of`, `tree_top_nodes`, `delete_unsealed_leaves_for_doc`, `due_stale_buffers`); types `Job`, `TreeNode`, `NewTreeNode`, `ChunkRow`; helpers `cosine`, `vec_to_bytes`/`bytes_to_vec` (`pub(crate)`).
- **Notes:** single-writer mutex; reads from pool (max 8); WAL. Multi-statement writes are transactions (see §8). Embeddings stored as little-endian f32 BLOBs.

### `api.rs` — HTTP surface
- **Surface:** `AppState { store, token, embedder: Arc<dyn Embedder> }`, `app(state) -> Router`.
- **Routes:** `GET /healthz` (open); under Bearer auth: `GET/POST /v1/:namespace/docs`, `GET /v1/:namespace/docs/:id`, `GET/DELETE /v1/:namespace/docs/by-key/:key` (`get_doc_by_key` → doc; `forget_doc_by_key` → 204/404), `POST /v1/:namespace/query`, `POST /v1/:namespace/code/search`, `GET /v1/:namespace/recall`, `POST /v1/:namespace/tree`.
- **Notes:** auth = exact Bearer string match. Handlers that embed (`ingest_doc`, `query_docs`, `tree_query`) wrap work in `tokio::task::spawn_blocking` (the embedder uses a blocking HTTP client). `list_docs` hard-caps at 100.

### `tree.rs` — cold consolidation (orchestrator)
- **Surface:** `TreeProcessor` (impl `JobProcessor`), `TreeCtx<'a>`, `append_leaf`, `seal_cascade`, `process_doc`, `sweep_stale`, `spawn_sweeper`.
- **Notes:** `process_doc` fans each chunk into Source/Global/Topic trees; `seal_cascade` recurses to `MAX_CASCADE_DEPTH=32`; LLM summary with deterministic `fallback_summary`; `gate_exceeded` encodes the three seal gates; labels differ per tree_kind.

### `retrieve.rs` — read paths
- **Surface:** `Hit`, `TreeHit`, `query`, `recall`, `drill_down`.
- **Notes:** weights `GRAPH_W=0.55 / VEC_W_G=0.30 / KW_W_G=0.15` vs fallback `VEC_W_FALLBACK=0.65 / KW_W_FALLBACK=0.35`; per-doc best-chunk aggregation; `drill_down` BFS + fresh-leaf surfacing + latest-leaf-per-doc + cosine rerank; `recall` uses a freshness decay `1/(1+age_hours/24)`.

### `ingest.rs` — write pipeline
- **Surface:** `chunk`, `extract_entities`, `ingest_document`.
- **Notes:** `MAX_CHUNK_CHARS=800` (char-based, CJK-safe); regex entities `email:/url:/handle:/hashtag:` (lowercased, sorted, deduped, URL-masked); embeds off-lock then `commit_ingest`.

### `embed.rs` — embedding abstraction
- **Surface:** `trait Embedder { embed, signature, dim }`; `HashEmbedder` (tests), `OllamaEmbedder` (`/api/embeddings`), `GatewayEmbedder` (`/v1/embeddings`, prod).
- **Notes:** signatures `hash:{dim}`, `ollama:{model}:{dim}`, `gateway:{model}:{dim}` — the cross-cutting key that gates all chunk reads. Blocking `reqwest`.

### `llm.rs` — summarization + audit
- **Surface:** `trait ChatClient { summarize }` (`GatewayChatClient` → `/v1/chat/completions`, `FakeChatClient`); `trait AuditSink { emit }` (`HttpAuditSink` → `/events`, `NullAuditSink`); `ChatResult`, `AuditEvent` (+`build`), `SummarizeCtx`, `summarize_audited`.
- **Notes:** `summarize_audited` measures latency and emits exactly one event (ok or error) per call. `AuditEvent` field set mirrors the deployed llm-audit's `extra="forbid"` payload; `op="chat"`, `app="engram"`, `persona=namespace`, `query_text="{kind}:{key}@L{level}"`. Audit failures are swallowed.

### `vault.rs` — Obsidian mirror (optional)
- **Surface:** `Vault { new, write_doc, write_node }`, `NodeMeta<'a>`.
- **Notes:** write-only; SQLite remains source of truth. SHA256 `content_sha` in YAML frontmatter dedups unchanged writes. Paths `{root}/{ns}/docs/{slug(id)}.md` and `{root}/{ns}/{tree_kind}/{slug(tree_key)}/L{level}-{slug(node_id)}.md`. `slug` maps non-alphanumerics to `_`.

### `jobs.rs` — worker runtime
- **Surface:** `trait JobProcessor { process }`, `NoopProcessor`, `worker_tick`, `spawn_workers`.
- **Notes:** `spawn_workers` launches N `std::thread`s looping `worker_tick` until `stop`. `worker_tick` is the synchronous claim-one-and-process unit tests use for determinism.

### `config.rs` / `model.rs` / `error.rs`
- `Config` + `from_env`/`from_vars(closure)` (closure form enables test injection; parsing is permissive — bad/missing values fall back to defaults). `MemoryDoc`, `NewDoc`, `Taint{Internal, ExternalSync}` (serde snake_case, default Internal); both doc types carry an opaque `meta: Option<serde_json::Value>` (stored as JSON text, round-tripped untouched, omitted from output when `None`). `Error` (thiserror): `Sqlite`/`Pool`/`Io` (`#[from]`), `NotFound`, `Embed(String)`, `Llm(String)`; `Result<T>` alias.

---

## 5. Data model (SQLite)

6 tables, 7 secondary indexes (created once in `Store::open`). All rows are
**namespace-scoped by column** — no per-namespace tables, no FK constraints (integrity
upheld by transactions in §8).

| Table | Key | Purpose / notable columns |
|-------|-----|---------------------------|
| `memory_docs` | PK `document_id`, `UNIQUE(namespace, key)` | Canonical docs: `document_id`, `namespace`, `key`, `title`, `content`, `author`, `taint`, `created_at`/`updated_at` (REAL secs), `meta` (TEXT, opaque JSON). Upsert on `(namespace,key)` keeps a stable `document_id`. |
| `vector_chunks` | PK `(namespace, chunk_id)` | One row/chunk: `text`, `embedding` BLOB (LE-f32), `model_signature`, `dim`. `chunk_id = "{doc_id}#{seq}"`. |
| `chunk_entities` | PK `(namespace, chunk_id, entity_id)` | Mechanical entity co-occurrence index → graph signal. |
| `post_acquire_jobs` | PK `job_id` (`"{ns}:{doc_id}"`) | Durable queue: `status` (pending/running/done/failed), `attempts`, `last_error`. |
| `tree_nodes` | PK `node_id` | Summary tree: `tree_kind` (source/global/topic), `tree_key`, `level`, `seq`, `sealed`, `body`, `doc_id` (leaf→doc, NULL for summaries), `token_count`, `embedding`, `sealed_at`. |
| `tree_edges` | PK `(parent_id, child_id)` | Parent→child links, frozen at seal time. |

Indexes: `idx_docs_ns_updated`, `idx_vchunks_ns_doc`, `idx_chunk_entities_entity`,
`idx_jobs_claim(status, created_at)`, `idx_tree_buffer(ns,kind,key,level,sealed,seq)`,
`idx_tree_doc`, `idx_tree_edges_parent`.

**Relationships (by convention):** `memory_docs 1—N vector_chunks N—N entities`;
`memory_docs 1—1 post_acquire_jobs` (idempotent re-enqueue); `tree_nodes` form per-`(kind,key)`
forests joined through `tree_edges`; leaves carry `doc_id` linking back to a document.

---

## 6. Trait abstractions (extension points)

| Trait | File | Prod impl | Test/null impl | Swap point |
|-------|------|-----------|----------------|-----------|
| `Embedder` | `embed.rs` | `GatewayEmbedder` | `HashEmbedder`; `OllamaEmbedder` (unwired) | `main.rs` builds it into `AppState` + `TreeProcessor` |
| `JobProcessor` | `jobs.rs` | `TreeProcessor` (in `tree.rs`) | `NoopProcessor` | `spawn_workers` arg |
| `ChatClient` | `llm.rs` | `GatewayChatClient` | `FakeChatClient` | `TreeProcessor.chat` |
| `AuditSink` | `llm.rs` | `HttpAuditSink` | `NullAuditSink` | `TreeProcessor.audit` |

All four are `Send + Sync` trait objects behind `Arc`, injected at composition time. This
is the seam for testing (network-free doubles) and for future backends (e.g. selecting
`OllamaEmbedder`, adding a real cost model to audit events).

---

## 7. Runtime & concurrency model

- **Async hot plane:** `#[tokio::main]` runs the axum server. Handlers are async; blocking
  work (embedding, gateway calls) is pushed to `spawn_blocking`.
- **Sync cold plane:** `spawn_workers` (N `std::thread`, default 2) drain the job queue;
  `spawn_sweeper` (1 `std::thread`) seals stale buffers every `stale_sweep_secs`. Both poll
  a shared `stop: Arc<AtomicBool>` (checked in ≤1s slices) — there is no graceful drain on
  process exit beyond that flag.
- **Concurrency control:** one `Arc<Mutex<Connection>>` serializes *all* writes (workers +
  HTTP writes + sweeper contend on it); reads use the r2d2 pool. WAL lets readers proceed
  during a write. `busy_timeout=15s` absorbs lock contention.
- **Crash recovery:** `requeue_running()` at startup resets orphaned `running` jobs to
  `pending`. Sealed tree history is immutable; re-ingest only drops a doc's *unsealed* L0 leaves.

---

## 8. Primary data flows

**Ingest (write) — `POST /v1/:ns/docs`:**
`ingest_doc` → `spawn_blocking` → `ingest::ingest_document`: token-aware CJK-safe `chunk()`
→ `embedder.embed_batch()` (batched, retry-on-transient, per-chunk tolerant — failed chunks
are skipped so one bad chunk doesn't abort the whole doc) → `extract_entities()` per chunk →
`store.commit_ingest()` **(one transaction:** upsert doc, delete old chunks+entities, insert
new chunks+entities, enqueue job**)** → returns `MemoryDoc` (201).

**Consolidate (cold) — background:** worker `claim_job` (atomic pending→running, attempts++)
→ `TreeProcessor.process` → `process_doc`: load doc+chunks, drop prior unsealed leaves, then
per chunk `append_leaf` into Source(author)/Global/Topic(entity) → each `append_leaf` runs
`seal_cascade`: if a gate trips, `summarize_audited` (off-lock) + embed summary →
`store.seal_buffer` **(one transaction:** insert parent, add edges, seal children**)** →
recurse upward → optional `vault.write_node` → `complete_job` / `fail_or_retry_job`.

**Seal gates (`gate_exceeded`):** L0 → Σ`token_count` ≥ `seal_input_token_budget`
(`approx_tokens = chars/4`); L≥1 → buffer len ≥ `seal_fanout`; OR oldest node older than
`seal_flush_age_secs`.

**Query (read) — `POST /v1/:ns/query`:** `spawn_blocking` → `retrieve::query`: embed query,
`chunks_for_namespace(sig)`, `extract_entities(query)` + `docs_with_entities` → blend
graph/vector/keyword (graph weights iff an entity matched) → best chunk per doc → top-N `Hit`.

**Code search (read) — `POST /v1/:ns/code/search`:** `spawn_blocking` → `retrieve::search_code`:
embed query, `code_chunks_for_namespace(sig)` (code chunks only — `line_start IS NOT NULL`),
score each chunk `vec+keyword` (no graph yet) → top-N `CodeHit` (`path:line` + snippet). This is
the chunk-level read side of code knowledge; the indexer (`engram-index`) and MCP server
(`engram-mcp`) consume it. See `docs/ROADMAP.md`.

**Drill-down — `POST /v1/:ns/tree`:** `spawn_blocking` → `retrieve::drill_down`: BFS from
`tree_top_nodes` (defaults to Global) to `max_depth`, surface fresh unsealed L0 leaves,
latest-leaf-per-doc, cosine rerank, top-N `TreeHit`.

**Recall — `GET /v1/:ns/recall`:** `retrieve::recall` (no embedding): `list_namespace` →
freshness-decay sort. *(The only read handler that does not use `spawn_blocking`.)*

**Forget — `DELETE /v1/:ns/docs/by-key/:key`:** `forget_doc_by_key` → `store.delete_doc_by_key`
**(one transaction:** resolve `document_id` by `(ns,key)`; if absent → `Ok(false)` → 404; else
delete its `vector_chunks`, `chunk_entities`, **unsealed L0** `tree_nodes`, `post_acquire_jobs`,
and the `memory_docs` row → `Ok(true)` → 204**)**. **Sealed summary nodes are deliberately
*not* deleted** (immutable history). The sibling `GET /v1/:ns/docs/by-key/:key` is a plain
read via `get_by_key`.

---

## 9. Configuration surface (`src/config.rs`, 22 vars)

| Var | Default | Var | Default |
|-----|---------|-----|---------|
| `ENGRAM_DB` | `engram.db` | `ENGRAM_JOBS_WORKERS` | `2` |
| `ENGRAM_BIND` | `127.0.0.1:8088` | `ENGRAM_JOBS_POLL_MS` | `500` |
| `ENGRAM_TOKEN` | `dev-token` | `ENGRAM_JOBS_MAX_ATTEMPTS` | `5` |
| `ENGRAM_OLLAMA_URL` | `http://127.0.0.1:11434` | `ENGRAM_SEAL_INPUT_TOKEN_BUDGET` | `50000` |
| `ENGRAM_EMBED_MODEL` | `bge-m3` | `ENGRAM_SEAL_FANOUT` | `10` |
| `ENGRAM_EMBED_DIM` | `1024` | `ENGRAM_SEAL_FLUSH_AGE_SECS` | `604800` |
| `ENGRAM_GATEWAY_URL` | `http://127.0.0.1:4000` | `ENGRAM_STALE_SWEEP_SECS` | `3600` |
| `ENGRAM_GATEWAY_KEY` | (empty) | `ENGRAM_MAX_SUMMARY_OUTPUT_TOKENS` | `5000` |
| `ENGRAM_LLM_MODEL` | `qwen3` | `ENGRAM_LLM_PROVIDER` | `ollama` |
| `ENGRAM_LLM_TIMEOUT_SECS` | `90` | `ENGRAM_VAULT_DIR` | (unset → vault off) |
| `ENGRAM_AUDIT_URL` | `http://127.0.0.1:8383` | `ENGRAM_EMBED_TIMEOUT_SECS` | `30` |

Parsing is permissive: missing/invalid values silently use the default (no validation, no
required vars; an empty `ENGRAM_GATEWAY_KEY` is accepted).

---

## 10. External dependencies (`Cargo.toml`)

| Crate | Role |
|-------|------|
| `rusqlite` (`bundled`) | Embedded SQLite (compiled in; needs a C compiler, no system lib) |
| `r2d2` + `r2d2_sqlite` | Read connection pool |
| `axum` 0.7 | HTTP server / routing / middleware |
| `tokio` (`full`) | Async runtime for the HTTP plane |
| `serde` + `serde_json` | (De)serialization of wire types + JSON bodies |
| `reqwest` 0.12 (`json`, `blocking`, `default-features=false`) | HTTP client for embed/LLM/audit |
| `regex` | Mechanical entity extraction |
| `uuid` (`v4`) | `document_id` / `node_id` / audit `event_id` |
| `thiserror` | `Error` derive |
| `tracing` + `tracing-subscriber` | Structured logging |
| `time` (`formatting`) | RFC3339 timestamps for audit events |
| `sha2` | Vault content hashing (dedup) |
| `tempfile` (dev) | Ephemeral DBs/dirs in tests |

Note: `reqwest` is built with `default-features=false` + only `json`/`blocking`, so **no TLS
feature is compiled** — all configured endpoints are `http://` (LAN). External `https://`
would require adding `rustls-tls`/`native-tls`.

---

## 11. Cross-cutting invariants & design decisions

1. **Single writer.** All mutations serialize on one `Mutex<Connection>`; reads scale via the pool. WAL keeps readers unblocked.
2. **Off-lock heavy work.** Embeddings and LLM summaries are computed before the write lock is taken; only fast DB writes happen under it (and inside one transaction).
3. **Atomic critical writes.** `commit_ingest`, `seal_buffer`, `delete_chunks_for_doc`, `claim_job` each wrap multiple statements in a transaction so crashes can't leave partial state.
4. **Embedder signature gates reads.** Changing embed model/dim orphans all existing chunks (filtered out by signature, not migrated) → requires full re-ingest.
5. **Query/ingest entity parity.** `retrieve::query` reuses `ingest::extract_entities` so the graph signal is consistent on both sides.
6. **Cascades never abort.** LLM/audit/gateway failures degrade to deterministic concat summaries; audit emit errors are swallowed.
7. **Immutable history.** Sealed tree nodes are never rewritten; re-ingest drops only unsealed L0 leaves.
8. **Auth/tenancy are coarse.** One shared Bearer token, exact match; namespace is an arbitrary request string with no per-namespace authorization.
9. **Everything is env-config'd**, parsed permissively, with `config.rs` as the single source of truth.

---

## 12. Analysis hooks / open questions

Starting points for deeper analysis, derived from the structure above:

- **`store.rs` size & centrality (1031 LOC, 34 pub items, 5 dependents).** Candidate for splitting into a `store/` submodule dir (docs / chunks / jobs / trees) — the hub status means it's the highest-leverage refactor and the biggest blast radius.
- **Write-lock contention.** Workers, sweeper, and HTTP writes all serialize on one mutex; `commit_ingest` and `seal_cascade` hold transactions. Worth profiling under concurrent ingest + consolidation; `seal_cascade` can do several sequential `seal_buffer` transactions per leaf.
- **Embedding on the hot path.** Each ingest/query embeds synchronously via a blocking gateway call inside `spawn_blocking` — latency is gateway-bound. README lists "move embedding off the hot path" as deferred.
- **No schema migrations.** Schema is `CREATE … IF NOT EXISTS` only; there's no versioning/migration path for column or embedding-format changes.
- **Orphaned chunks on model change.** Signature gating means stale chunks linger silently after an embed-model swap — no GC.
- **Coarse multi-tenancy.** Single token, no namespace authz, no rate limiting — relevant for any shared/external exposure (currently mitigated by loopback bind + gateway pass-through).
- **`OllamaEmbedder` is dead-ish code** (implemented, tested, but not selected at runtime) — decide whether to wire it (provider switch) or drop it.
- **Audit `cost_usd` is hardcoded 0.0** and `raw_json` is `"{}"` — audit events carry latency/usage but not cost or raw payloads yet.
- **No TLS compiled** into `reqwest` — fine for LAN, a gap if any endpoint moves to `https://`.
- **Test coverage shape.** Heavy unit/integration coverage inline, but the live-gateway paths (`GatewayEmbedder`, `GatewayChatClient`) are only exercised by `#[ignore]`d tests — the real network adapters are untested in CI.
- **Background-thread observability.** Worker/sweeper errors are `tracing::warn!`-logged and otherwise invisible; `failed` jobs accumulate with no surfaced metric beyond `pending_jobs()`.
- **"Forget" is incomplete by design.** `delete_doc_by_key` removes the doc, chunks, entities, pending job, and *unsealed* leaves, but a forgotten doc's text can persist inside already-**sealed summary bodies** (immutable history) and in the optional vault mirror (write-only, never deleted). Relevant for any true-deletion / right-to-erasure requirement.
- **`meta` is opaque & unindexed.** The new `meta` JSON is stored as TEXT, round-tripped untouched, and not queried/filtered on — it's payload, not a retrieval signal (yet).
```
