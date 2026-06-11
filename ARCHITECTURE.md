# engram — Architecture & Structure

> Snapshot of the codebase structure, intended as a base for further analysis
> (refactoring, performance, coverage, security, feature planning). Pairs with
> `README.md` (user view) and `CLAUDE.md` (contributor guide). Line counts are
> approximate; everything else is extracted from source.

`engram` is a shared, single-writer memory service: an **axum HTTP API over SQLite**
implementing OpenHuman's `ingest → store → retrieve` design, plus an **autonomous
background consolidation pipeline** that folds documents into summary trees. It has
been **extended into a code knowledge base for coding agents**: a Cargo **workspace** of
three crates —

- **`crates/engram`** — the core service (HTTP API, store, ingest, retrieve, embed, tree,
  llm, conventions, treesit, vault).
- **`crates/engram-index`** — a repo indexer CLI (`index` / `reindex` / `index-history` /
  `install-hook`) that walks a repo and POSTs each file as a code-mode document.
- **`crates/engram-mcp`** — an MCP server exposing six agent tools over stdio **and** HTTP
  JSON-RPC, backed by the core service's HTTP endpoints.

**Memory model → code.** A repo is a namespace `repo:<id>`; a file is a document
(`meta.kind == "file"` selects *code mode*); an identifier is an entity (`sym:` /
`import:` / `path:`). Two derived namespaces hang off a repo: `repo:<id>:history` (git
commits, `meta.kind == "commit"`) and `repo:<id>:meta` (the conventions doc). Prose docs
(the original memory use-case) still work unchanged in any namespace.

---

## 1. Snapshot

`crates/engram` modules (the core service):

| Module | Layer | Responsibility |
|--------|-------|----------------|
| `store/` (`mod.rs` + `docs`/`chunks`/`entities`/`jobs`/`trees`) | infra | SQLite: schema, single-writer, transactions, all queries (split into a submodule dir) |
| `tree.rs`       | pipeline | Cold consolidation: fan-out, seal cascade, sweeper (prose **and** code modes) |
| `api.rs`        | http     | axum router, AppState, Bearer auth, handlers (incl. code-search / digest / conventions) |
| `retrieve.rs`   | pipeline | `query` (hybrid), `search_code` (chunk-level + path prior), `recall`, `drill_down` |
| `llm.rs`        | io       | Chat + audit traits, gateway clients, `summarize_audited` |
| `ingest.rs`     | pipeline | Prose + code chunking, entity extraction, `ingest_document` (code-mode dispatch) |
| `treesit.rs`    | pipeline | tree-sitter function/type-boundary chunking + AST symbol extraction (10 langs; code mode) |
| `conventions.rs`| pipeline | `meta` namespace: conventions digest + single-pass architecture/module digests (rebuild/read) |
| `embed.rs`      | io       | `Embedder` trait + Hash/Ollama/Gateway/**Fallback** impls |
| `vault.rs`      | io       | Optional Obsidian markdown mirror |
| `jobs.rs`       | infra    | `JobProcessor` trait, `worker_tick`, `spawn_workers` |
| `config.rs`     | base     | Env → `Config` (+ cached free-fn reads of the code toggles) |
| `model.rs`      | base     | `MemoryDoc`, `NewDoc` (incl. opaque `meta`), `Taint` |
| `main.rs`       | bin      | Composition root |
| `error.rs`      | base     | `Error` enum + `Result` alias |
| `lib.rs`        | —        | Module declarations only |

Sibling crates: `crates/engram-index` (`client` / `walk` / `commits` / `diff` / `hook` +
`main` — the indexer CLI) and `crates/engram-mcp` (`lib` + `main` — the JSON-RPC server).

Tests are inline `#[cfg(test)] mod tests` per module — **no `tests/` dir**. The full suite
is **168 tests**; `clippy` and `fmt` are clean.

---

## 2. Workspace layout

```
engram/  (Cargo workspace, resolver "2")
├── Cargo.toml / Cargo.lock
├── README.md / CLAUDE.md / ARCHITECTURE.md
├── .env.example
├── deploy/run.sh          # screen-managed binary + reverse SSH tunnel (WSL2)
├── docs/                  # ROADMAP.md, STRUCTURE.md, CODE-AGENT-LOOP.md, ...
├── eval/                  # validate.py + agentmemory_gold.json + RESULTS.md
└── crates/
    ├── engram/            # [lib] engram + [[bin]] engram — the core service
    │   └── src/
    │       ├── lib.rs  main.rs
    │       ├── api.rs  config.rs  conventions.rs  embed.rs  error.rs
    │       ├── ingest.rs  jobs.rs  llm.rs  model.rs  retrieve.rs
    │       ├── treesit.rs  tree.rs  vault.rs
    │       └── store/  (mod.rs + docs.rs chunks.rs entities.rs jobs.rs trees.rs)
    ├── engram-index/      # repo indexer CLI (index / reindex / index-history / install-hook)
    └── engram-mcp/        # MCP server (stdio + HTTP JSON-RPC), 6 agent tools
```

`lib.rs` is pure declaration (no logic). `main.rs` is the composition root. The library is
fully usable and testable without the binary — which the inline tests exploit (they build a
`Store` + `HashEmbedder` directly). `store.rs` has been split into a `store/` submodule dir
(docs / chunks / entities / jobs / trees), with `mod.rs` holding the schema, single-writer
plumbing, and the cross-table transactions.

---

## 3. Module dependency graph (production)

Intra-crate edges (within `crates/engram`) from `use crate::…` **and** fully-qualified
`crate::mod::…` references, restricted to non-test code. It is a clean **DAG (no cycles)**.

**Production edge list:**

| Module | Depends on (intra-crate) |
|--------|--------------------------|
| `main` (bin) | api, config, embed, jobs, llm, store, tree, vault |
| `api`        | conventions, embed, ingest, model, retrieve, store |
| `conventions`| embed, error, ingest, llm, model, retrieve, store |
| `tree`       | config, embed, error, ingest, jobs, llm, store, vault |
| `retrieve`   | config, embed, error, ingest, model, store |
| `ingest`     | config, embed, error, model, store, treesit |
| `treesit`    | ingest |
| `jobs`       | error, store |
| `store`      | error, model |
| `embed`      | error |
| `llm`        | error |
| `vault`      | error, model |
| `config`, `model`, `error` | — (leaves) |

**Structural observations (analysis hooks):**
- **`store` is the hub** — depended on by `api`, `conventions`, `ingest`, `jobs`, `retrieve`,
  `tree`. Any storage change ripples widely; it is now a submodule dir (`store/`).
- **`ingest` ↔ `treesit` is a deliberate two-way pair:** `ingest` dispatches code-mode docs to
  `treesit` (`chunk_code_ts` / `extract_symbols_ts`), and `treesit` reuses `ingest`'s
  `chunk_code_with` / `estimate_tokens` for its fallback and token budget. Both are non-cyclic
  in the strict sense (each item used is distinct), but they form the code-ingest seam.
- **Two orchestrators:** `api` composes the *hot* (request) path; `tree` composes the *cold*
  (consolidation) path. `conventions` is a third small orchestrator — it `drill_down`s the meta
  namespace and `ingest_document`s the rebuilt conventions doc.
- **Notable edge `retrieve → ingest`:** `query` calls `ingest::extract_entities`, and
  `search_code` consults `config::code_path_prior`. Query-side entity extraction is byte-for-byte
  the same as ingest-side (intentional consistency).
- **Test-only edges** (present only under `#[cfg(test)]`): tests drive the cold pipeline directly
  and inject `llm`/`config` doubles, so the test graph couples more than the prod graph.

---

## 4. Module reference cards

### `store/` — persistence (hub)
- **Type:** `Store { read: r2d2::Pool<SqliteConnectionManager>, write: Arc<Mutex<Connection>> }`, `Clone`. Split into `mod.rs` (schema, single-writer plumbing, cross-table transactions) + `docs.rs` / `chunks.rs` / `entities.rs` / `jobs.rs` / `trees.rs`.
- **Surface:** `open`; docs (`insert_doc`, `get_doc`, `get_by_key`, `list_namespace`, `delete_doc_by_key`); chunks/entities (`upsert_chunk`, `delete_chunks_for_doc`, `record_entities`, `docs_with_entities`, `chunks_for_namespace`, **`code_chunks_for_namespace`** (code chunks only — `line_start IS NOT NULL`), `chunks_for_doc`); the atomic hot-commit `commit_ingest`; job queue (`enqueue_job`, `claim_job`, `complete_job`, `fail_or_retry_job`, `requeue_running`, `job`, `pending_jobs`); trees (`append_leaf_node`, `seal_buffer`, `unsealed_nodes`, `children_of`, `tree_top_nodes`, `delete_unsealed_leaves_for_doc`, `due_stale_buffers`); types `Job`, `TreeNode`, `NewTreeNode`, `ChunkRow` (now carries `line_start`); helpers `cosine`, `vec_to_bytes`/`bytes_to_vec` (`pub(crate)`).
- **Notes:** single-writer mutex; reads from pool (max 8); WAL. Multi-statement writes are transactions (see §8). Embeddings stored as little-endian f32 BLOBs. `vector_chunks` gained `line_start`/`line_end` for code chunks (NULL for prose).

### `api.rs` — HTTP surface
- **Surface:** `AppState { store, token, embedder: Arc<dyn Embedder> }` (+ a `ChatClient` for conventions rebuild), `app(state) -> Router`.
- **Routes:** `GET /healthz` (open); under Bearer auth:
  - `GET/POST /v1/:namespace/docs`, `GET /v1/:namespace/docs/:id`, `GET/DELETE /v1/:namespace/docs/by-key/:key`
  - `POST /v1/:namespace/query`, `GET /v1/:namespace/recall`, `POST /v1/:namespace/tree`
  - **Code KB:** `POST /v1/:namespace/code/search`, `POST /v1/:namespace/code/architecture` (global digest), `POST /v1/:namespace/code/architecture/rebuild` (single-pass rebuild → `:meta`), `POST /v1/:namespace/code/module` (a directory's digest)
  - **Conventions:** `POST /v1/:namespace/conventions/rebuild`, `GET /v1/:namespace/conventions`
- **Notes:** auth = exact Bearer string match. Handlers that embed or summarize (`ingest_doc`, `query_docs`, `tree_query`, `search_code_docs`, `get_architecture`, `get_module`, the conventions handlers) wrap work in `tokio::task::spawn_blocking` (the embedder/LLM use blocking HTTP clients). `list_docs` hard-caps at 100. The MCP server's six tools map onto `/code/search`, `/code/architecture`, `/code/module`, `/query`, `GET /conventions` (and `POST /conventions/rebuild`), and `/tree` (see §6a).

### `tree.rs` — cold consolidation (orchestrator)
- **Surface:** `TreeProcessor` (impl `JobProcessor`), `TreeCtx<'a>`, `append_leaf`, `seal_cascade`, `process_doc`, `sweep_stale`, `spawn_sweeper`.
- **Notes:** `process_doc` is **mode-aware** — prose fans each chunk into **source** (author-keyed) / **global** / **topic** (entity) trees; code fans into a directory-keyed **module** tree + **global** + **topic** trees on `sym:`/`import:` entities only (not `path:`). Code consolidation is **gated by `consolidate_code`** (default off — `search_code` reads chunks, not trees, so code trees stay unread until the digest tools are used; when off, code docs skip the fan-out). `seal_cascade` recurses to `MAX_CASCADE_DEPTH=32`; LLM summary with deterministic `fallback_summary`; `gate_exceeded` encodes the three seal gates; `label_for` shares labels for `source`/`module`.

### `retrieve.rs` — read paths
- **Surface:** `Hit`, `CodeHit`, `TreeHit`, `query`, `search_code`, `recall`, `drill_down`.
- **Notes:** `query` weights `GRAPH_W=0.55 / VEC_W_G=0.30 / KW_W_G=0.15` vs fallback `VEC_W_FALLBACK=0.65 / KW_W_FALLBACK=0.35`; per-doc best-chunk aggregation. `search_code` is **chunk-level** (vector cosine + keyword, no graph yet) over `code_chunks_for_namespace`, multiplied by a **path-type ranking prior** (`path_prior`, toggled by `code_path_prior`) that down-weights docs/tests/config so source ranks first; each `CodeHit` carries `path` + `line_start` + snippet. `drill_down` BFS + fresh-leaf surfacing + latest-leaf-per-doc + cosine rerank (query-less ⇒ top digests); `recall` uses a freshness decay `1/(1+age_hours/24)`.

### `ingest.rs` — write pipeline
- **Surface:** `chunk`, `chunk_code` / `chunk_code_with`, `estimate_tokens`, `extract_entities`, `extract_code_entities`, `ingest_document`.
- **Notes:** prose path: paragraph chunking `MAX_CHUNK_CHARS=800` (char-based, CJK-safe), regex entities `email:/url:/handle:/hashtag:` (lowercased, sorted, deduped, URL-masked). **Code mode** (dispatched on `meta.kind == "file"`): tree-sitter boundary chunking via `treesit` when `code_tree_sitter` is on and the language is supported, else the heuristic line/symbol chunker `chunk_code_with`; AST symbols (`sym:`) merged with the regex `extract_code_entities` (`sym:` / `import:`), plus a `path:` entity. Token-aware (`CODE_CHUNK_TOKEN_BUDGET`, kept under the embed model's 512-token ceiling) and **per-chunk tolerant** (a bad chunk is skipped, the rest ingest). Both paths embed off-lock then `commit_ingest`.

### `treesit.rs` — tree-sitter code chunking + symbols
- **Surface:** `Lang` (Rust/Python/JavaScript/TypeScript/Tsx/Go/Kotlin/Java/C/C++), `lang_for_path`, `chunk_code_ts`, `extract_symbols_ts`.
- **Notes:** maps a file extension to a grammar (10 languages; `.h` routes to the C++ grammar), walks the parse tree splitting at definition boundaries (functions/types/classes/impls/etc.), and extracts accurate `sym:` identifiers from declaration nodes. Returns `None` on unsupported language or parse failure, so `ingest` cleanly falls back to `chunk_code_with` + regex symbols. C/C++ additionally support a definition-packing mode (`is_boundary` / `chunk_segments`) that packs consecutive small defs into wider, boundary-aligned chunks (`ENGRAM_CODE_NATIVE_PACK` + `ENGRAM_CODE_NATIVE_BUDGET`). Grammars are linked in via the `tree-sitter-*` crates.

### `conventions.rs` — meta namespace + single-pass digests
- **Surface:** `meta_namespace`, `is_config_file`, `rebuild_conventions`, `rebuild_architecture_digest`, `get_conventions` read path.
- **Notes:** the `repo:<id>:meta` namespace holds rolled-up digest documents keyed by purpose: `conventions`, `architecture` (global), and `module:<dir>` (per-directory). `rebuild_conventions` `drill_down`s the meta namespace (config-file digests) and `ingest_document`s the result; `GET /conventions` returns it. **`rebuild_architecture_digest`** builds the architecture / `module:<dir>` digests in a **single LLM pass over the repo's whole source files** (with extractive overflow when the source overruns the context), then stores the result in `:meta` — so `get_architecture` / `get_module` serve a cached one-shot digest rather than folding the consolidation tree. One compression pass preserves the specifics a deep summary-of-summaries fold loses (measured: one-shot 1.58 vs tree 1.08 of 2 — see `eval/RESULTS_distillation.md`). `is_config_file` classifies config-ish keys.

### `embed.rs` — embedding abstraction
- **Surface:** `trait Embedder { embed, signature, dim }`; `HashEmbedder` (tests), `OllamaEmbedder` (`/api/embeddings`), `GatewayEmbedder` (`/v1/embeddings`, prod), **`FallbackEmbedder`** (wraps a primary + a local Ollama fallback).
- **Notes:** signatures `hash:{dim}`, `ollama:{model}:{dim}`, `gateway:{model}:{dim}` — the cross-cutting key that gates all chunk reads. `FallbackEmbedder::signature()` **delegates to the primary**, so a failover never changes the signature and thus never orphans chunks. Wiring is opt-in via `embed_fallback` (`ENGRAM_EMBED_FALLBACK`). Blocking `reqwest`.

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
| `vector_chunks` | PK `(namespace, chunk_id)` | One row/chunk: `text`, `embedding` BLOB (LE-f32), `model_signature`, `dim`, `line_start`/`line_end` (set for code chunks, NULL for prose). `chunk_id = "{doc_id}#{seq}"`. |
| `chunk_entities` | PK `(namespace, chunk_id, entity_id)` | Mechanical/AST entity co-occurrence index → graph signal (`sym:`/`import:`/`path:` for code). |
| `post_acquire_jobs` | PK `job_id` (`"{ns}:{doc_id}"`) | Durable queue: `status` (pending/running/done/failed), `attempts`, `last_error`. |
| `tree_nodes` | PK `node_id` | Summary tree: `tree_kind` (source/module/global/topic), `tree_key`, `level`, `seq`, `sealed`, `body`, `doc_id` (leaf→doc, NULL for summaries), `token_count`, `embedding`, `sealed_at`. |
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
| `Embedder` | `embed.rs` | `GatewayEmbedder`, optionally wrapped by `FallbackEmbedder` | `HashEmbedder`; `OllamaEmbedder` (also the fallback leg) | `main.rs` builds it into `AppState` + `TreeProcessor` |
| `JobProcessor` | `jobs.rs` | `TreeProcessor` (in `tree.rs`) | `NoopProcessor` | `spawn_workers` arg |
| `ChatClient` | `llm.rs` | `GatewayChatClient` | `FakeChatClient` | `TreeProcessor.chat`, conventions rebuild |
| `AuditSink` | `llm.rs` | `HttpAuditSink` | `NullAuditSink` | `TreeProcessor.audit` |

All four are `Send + Sync` trait objects behind `Arc`, injected at composition time. This
is the seam for testing (network-free doubles) and for backends. `main.rs` selects
`GatewayEmbedder` (prod), and when `embed_fallback` is set wraps it in a `FallbackEmbedder`
backed by `OllamaEmbedder`.

---

## 6a. Sibling crates & the agent tool surface

**`crates/engram-mcp`** — an MCP (Model Context Protocol) server speaking JSON-RPC 2.0 over
**stdio** by default, or over **HTTP** when `ENGRAM_MCP_HTTP=addr` is set (e.g.
`127.0.0.1:8765`). It exposes **six tools**, each a thin call to a core-service HTTP endpoint:

| Tool | What it answers | Backing endpoint |
|------|-----------------|------------------|
| `search_code(query)` | chunk-level code search (`path:line` + snippet) | `POST /code/search` |
| `get_architecture()` | the global digest of the repo (cached single-pass digest from `:meta`) | `POST /code/architecture` |
| `get_module(path)` | a directory's digest (cached single-pass digest from `:meta`) | `POST /code/module` |
| `why(query)` | relevant git-history commits | `POST /query` (history namespace) |
| `find_symbol(name)` | where a symbol is defined | `POST /code/search` (symbol query) |
| `get_conventions()` | the repo's conventions doc | `GET /conventions` |

**`crates/engram-index`** — a CLI that turns a repo into documents: `index` / `reindex` walk
the tree (`walk`) and POST each file as a code-mode doc via the HTTP `client`; `index-history`
parses `git log` (`commits`, `diff`) into `meta.kind == "commit"` docs in `repo:<id>:history`;
`install-hook` drops a post-commit git hook (`hook`) so history stays fresh.

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
`ingest_doc` → `spawn_blocking` → `ingest::ingest_document`. **Dispatch on `meta.kind`:**
prose → token-aware CJK-safe `chunk()` + regex `extract_entities()`; **code** (`kind == "file"`)
→ tree-sitter boundary chunking (`treesit`, falls back to `chunk_code_with`) + AST/regex
`sym:`/`import:`/`path:` entities. Then `embedder.embed_batch()` (batched, retry-on-transient,
per-chunk tolerant — failed chunks are skipped so one bad chunk doesn't abort the whole doc) →
`store.commit_ingest()` **(one transaction:** upsert doc, delete old chunks+entities, insert
new chunks+entities incl. `line_start`/`line_end`, enqueue job**)** → returns `MemoryDoc` (201).

**Consolidate (cold) — background:** worker `claim_job` (atomic pending→running, attempts++)
→ `TreeProcessor.process` → `process_doc`: load doc+chunks, drop prior unsealed leaves, then
per chunk `append_leaf` — **prose** into source(author)/global/topic(entity); **code** (only
when `consolidate_code` is on) into module(directory)/global/topic(`sym:`/`import:` only). Each
`append_leaf` runs `seal_cascade`: if a gate trips, `summarize_audited` (off-lock) + embed
summary → `store.seal_buffer` **(one transaction:** insert parent, add edges, seal children**)**
→ recurse upward → optional `vault.write_node` → `complete_job` / `fail_or_retry_job`.

**Seal gates (`gate_exceeded`):** L0 → Σ`token_count` ≥ `seal_input_token_budget`
(`approx_tokens = chars/4`); L≥1 → buffer len ≥ `seal_fanout`; OR oldest node older than
`seal_flush_age_secs`.

**Query (read) — `POST /v1/:ns/query`:** `spawn_blocking` → `retrieve::query`: embed query,
`chunks_for_namespace(sig)`, `extract_entities(query)` + `docs_with_entities` → blend
graph/vector/keyword (graph weights iff an entity matched) → best chunk per doc → top-N `Hit`.

**Code search (read) — `POST /v1/:ns/code/search`:** `spawn_blocking` → `retrieve::search_code`:
embed query, `code_chunks_for_namespace(sig)` (code chunks only — `line_start IS NOT NULL`),
score each chunk `vec+keyword` × `path_prior` (down-weights docs/tests/config) → top-N `CodeHit`
(`path:line` + snippet). The chunk-level read side; the indexer (`engram-index`) feeds it and the
MCP server (`engram-mcp`) consumes it (also as `find_symbol`).

**Architecture / module digests — `POST /v1/:ns/code/{architecture,module}`:** `spawn_blocking`
→ serve the **cached single-pass digest** from `repo:<id>:meta` (key `architecture` or
`module:<dir>`), built by `POST .../code/architecture/rebuild` → `conventions::rebuild_architecture_digest`
(one LLM summary over the repo's whole source files, with extractive overflow). If no single-pass
digest is cached, fall back to `retrieve::drill_down` over the **global** tree (architecture) or a
directory-keyed **module** tree (module, body = directory `path`) — which itself needs a
consolidation run with `consolidate_code` enabled to return non-fallback summaries. The single-pass
digest is preferred because one compression pass keeps the specifics a summary-of-summaries fold
drops (see `eval/RESULTS_distillation.md`).

**Conventions — `GET /v1/:ns/conventions` & `POST .../conventions/rebuild`:** the read returns
the `repo:<id>:meta` conventions doc; the rebuild (`spawn_blocking`) digests config-file
knowledge via `conventions::rebuild_conventions` and re-`ingest`s it.

**Why (history) — MCP `why(query)`:** `POST /v1/repo:<id>:history/query` — ordinary hybrid
`query` over commit docs (`meta.kind == "commit"`), returning relevant commits.

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

## 9. Configuration surface (`crates/engram/src/config.rs`)

| Var | Default | Var | Default |
|-----|---------|-----|---------|
| `ENGRAM_DB` | `engram.db` | `ENGRAM_JOBS_WORKERS` | `2` |
| `ENGRAM_BIND` | `127.0.0.1:8088` | `ENGRAM_JOBS_POLL_MS` | `500` |
| `ENGRAM_TOKEN` | `dev-token` | `ENGRAM_JOBS_MAX_ATTEMPTS` | `5` |
| `ENGRAM_OLLAMA_URL` | `http://127.0.0.1:11434` | `ENGRAM_SEAL_INPUT_TOKEN_BUDGET` | `50000` |
| `ENGRAM_EMBED_MODEL` | `bge-m3` | `ENGRAM_SEAL_FANOUT` | `10` |
| `ENGRAM_EMBED_DIM` | `1024` | `ENGRAM_SEAL_FLUSH_AGE_SECS` | `604800` |
| `ENGRAM_EMBED_TIMEOUT_SECS` | `30` | `ENGRAM_STALE_SWEEP_SECS` | `3600` |
| `ENGRAM_GATEWAY_URL` | `http://127.0.0.1:4000` | `ENGRAM_MAX_SUMMARY_OUTPUT_TOKENS` | `5000` |
| `ENGRAM_GATEWAY_KEY` | (empty) | `ENGRAM_VAULT_DIR` | (unset → vault off) |
| `ENGRAM_LLM_MODEL` | `qwen3` | `ENGRAM_EMBED_FALLBACK` | `false` |
| `ENGRAM_LLM_PROVIDER` | `ollama` | `ENGRAM_CONSOLIDATE_CODE` | `false` |
| `ENGRAM_LLM_TIMEOUT_SECS` | `90` | `ENGRAM_CODE_SYMBOL_SPLIT` | `true` |
| `ENGRAM_AUDIT_URL` | `http://127.0.0.1:8383` | `ENGRAM_CODE_PATH_PRIOR` | `true` |
| `ENGRAM_EMBED_URL` | = `ENGRAM_GATEWAY_URL` | `ENGRAM_CODE_TREE_SITTER` | `true` |
| `ENGRAM_EMBED_FALLBACK` | `false` | `ENGRAM_CODE_MIN_SCORE` | `0.0` (off) |
| | | `ENGRAM_CODE_NATIVE_PACK` | `false` |
| | | `ENGRAM_CODE_NATIVE_BUDGET` | `480` |

`ENGRAM_EMBED_URL` defaults to `ENGRAM_GATEWAY_URL`; set it to route **embeddings** to a separate
backend (e.g. a local bge-m3 Ollama) while LLM/chat calls stay on the gateway — decoupling the
embedder from the LLM (they used to share one URL). Full guide: `docs/EMBEDDINGS.md`.

`ENGRAM_MCP_HTTP` (unset → stdio) is read by **`crates/engram-mcp`**, and `ENGRAM_INDEX_TIMEOUT_SECS`
(`120`) by **`crates/engram-index`**, neither by the core `Config`. The `ENGRAM_CODE_*` toggles
(`code_symbol_split` / `code_path_prior` / `code_tree_sitter`) and the native-pack / min-score knobs
(`code_native_pack` / `code_native_budget` / `code_min_score`) are read via cached `OnceLock` free
functions rather than the `Config` struct.

Parsing is permissive: missing/invalid values silently use the default (no validation, no
required vars; an empty `ENGRAM_GATEWAY_KEY` is accepted; an unrecognized boolean falls back to
its default). **Deployed values differ from the code defaults:** embeddings route through the
litellm gateway as `mxbai-embed-large` (dim 1024) and LLM summaries as `deepseek-chat`.

**Embedding-model guidance (don't swap casually).** Production embeds with `mxbai-embed-large`
(dim 1024). `bge-m3` is **not** a better prose embedder (measured ~equal or worse as a drop-in);
its value is **enabling wide C/C++ chunks** (`ENGRAM_CODE_NATIVE_PACK` + `ENGRAM_CODE_NATIVE_BUDGET`
≈1500), which lifts native line-recall 0.59→0.89. Changing `ENGRAM_EMBED_MODEL`/`_DIM` orphans all
existing chunks (signature change ⇒ full re-index). The decision matrix + serving gotchas (the
gateway's broken bge-m3 route, the `ENGRAM_EMBED_URL` split) live in `docs/EMBEDDINGS.md`.

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
| `tree-sitter` 0.23 + `tree-sitter-{rust,python,javascript,typescript,go,kotlin,java,c,cpp}` | Code-mode boundary chunking + AST symbols (`treesit.rs`); 10 languages (`.h` → C++) |
| `uuid` (`v4`) | `document_id` / `node_id` / audit `event_id` |
| `thiserror` | `Error` derive |
| `tracing` + `tracing-subscriber` | Structured logging |
| `time` (`formatting`) | RFC3339 timestamps for audit events |
| `sha2` | Vault content hashing (dedup) |
| `tempfile` (dev) | Ephemeral DBs/dirs in tests |

Sibling crates add: `clap` (`derive`, `env`) + `percent-encoding` (the `engram-index` CLI) and
`tiny_http` (the `engram-mcp` HTTP JSON-RPC transport); both reuse `reqwest`/`serde`.

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
10. **Mode dispatch on `meta.kind`.** `"file"` ⇒ code mode (tree-sitter chunking, AST symbols, path-prior search, module trees); `"commit"` marks history docs; everything else is prose. The same store/queue/cascade machinery serves both.
11. **Failover never orphans.** `FallbackEmbedder.signature()` delegates to the primary, so an Ollama failover writes chunks under the gateway signature and they stay readable.
12. **Code consolidation is opt-in.** `consolidate_code` gates the code fan-out; with it off, code is searchable (chunks) but the architecture/module digests fall back rather than reflect a real consolidation run.

---

## 12. Analysis hooks / open questions

Starting points for deeper analysis, derived from the structure above:

- **Write-lock contention.** Workers, sweeper, and HTTP writes all serialize on one mutex; `commit_ingest` and `seal_cascade` hold transactions. Worth profiling under concurrent ingest + consolidation; `seal_cascade` can do several sequential `seal_buffer` transactions per leaf.
- **Embedding on the hot path.** Each ingest/query embeds synchronously via a blocking gateway call inside `spawn_blocking` — latency is gateway-bound.
- **No schema migrations.** Schema is `CREATE … IF NOT EXISTS` only (plus additive code columns like `line_start`); there's no versioning/migration path for column or embedding-format changes.
- **Orphaned chunks on model change.** Signature gating means stale chunks linger silently after an embed-model swap — no GC.
- **Coarse multi-tenancy.** Single token, no namespace authz, no rate limiting — relevant for any shared/external exposure (currently mitigated by loopback bind + gateway pass-through). The MCP HTTP transport (`ENGRAM_MCP_HTTP`) is a second loopback surface to keep in mind.
- **Audit `cost_usd` is hardcoded 0.0** and `raw_json` is `"{}"` — audit events carry latency/usage but not cost or raw payloads yet.
- **No TLS compiled** into `reqwest` — fine for LAN, a gap if any endpoint moves to `https://`.
- **Test coverage shape.** 168 tests, inline; but the live-gateway paths (`GatewayEmbedder`, `GatewayChatClient`) are only exercised by `#[ignore]`d tests — the real network adapters are untested in CI.
- **Background-thread observability.** Worker/sweeper errors are `tracing::warn!`-logged and otherwise invisible; `failed` jobs accumulate with no surfaced metric beyond `pending_jobs()`.
- **"Forget" is incomplete by design.** `delete_doc_by_key` removes the doc, chunks, entities, pending job, and *unsealed* leaves, but a forgotten doc's text can persist inside already-**sealed summary bodies** (immutable history) and in the optional vault mirror (write-only, never deleted). Relevant for any true-deletion / right-to-erasure requirement.
- **`meta` is semi-opaque.** Stored as TEXT and round-tripped untouched, but `kind` is now load-bearing (dispatches code/commit modes). It is still not a filterable retrieval signal beyond that dispatch.
- **Digest path is single-pass, tree is fallback.** `get_architecture`/`get_module` now serve a cached **single-pass** digest from `:meta` (built by `conventions::rebuild_architecture_digest`); the consolidation-tree drill is only a fallback when none is cached. One compression pass beat the summary-of-summaries fold in measurement (`eval/RESULTS_distillation.md`). `search_code` (chunks) is live; the `why` history path still needs a history-ingest run to clear its relevance bar.

---

## 13. Validation & roadmap status

`eval/validate.py` scores ingest-rate + recall@{1,5,10} + line-recall + hard negatives over
`eval/agentmemory_gold.json` (35 labeled NL→file queries, ≤2 per file). Live production numbers
(tree-sitter config): **ingest 0.998, recall@1 0.543, recall@5 0.771, recall@10 0.914,
line-recall@10 0.771**. Recall journey: 0.533 baseline → 0.800 (heuristic Phase F) → tree-sitter
(precision/coverage/line-accuracy up; recall@5 0.771 an accepted trade — see `eval/RESULTS.md`).

Two further eval suites back the code-KB work: **`eval/RESULTS_android.md`** (AVM Android
feature-migration eval — the 10-language tree-sitter grammars, the native line-recall fix via wide
C/C++ chunks, and an agent A/B) and **`eval/RESULTS_distillation.md`** (distillation vs simple
summary — the consolidation tree digest *loses* to a single-pass summary, motivating the
single-pass `rebuild_architecture_digest` path).

The roadmap (phases 0, 1a, 1a.1, A, B, 1b, 1c, E, F, R, 2, 3a, 3b, 4) is **complete**; phases 2 /
3a / 3b are code-complete with their live quality bars (non-fallback digests; `why` quality)
noted as pending a cost-bearing consolidation/history-ingest run. See `docs/ROADMAP.md`.
