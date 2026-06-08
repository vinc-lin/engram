# engram

Shared, single-writer memory service for the agent-ops cluster. Ports
OpenHuman's memory design (ingest → store → retrieve) onto SQLite (WAL, a read
pool + a single writer). See the design spec in the `anima` repo:
`docs/superpowers/specs/2026-06-06-shared-memory-service-design.md`.

Built so far: persistence + token-authed HTTP doc CRUD (Plan 1), embeddings +
hybrid vector/keyword retrieval (Plan 2), a structured ingest pipeline with a
mechanical entity co-occurrence graph + graph-aware query (Plan 3), and
**autonomous post-acquire consolidation** — bucket-seal Source/Topic/Global
summary trees built by background workers, with tree drill-down retrieval, an
opt-in Obsidian vault mirror, and self-audited LLM summaries (Plan 4).

## Run

```bash
ENGRAM_DB=engram.db ENGRAM_BIND=127.0.0.1:8088 ENGRAM_TOKEN=secret cargo run
```

## API

- GET  /healthz
- POST /v1/{namespace}/docs        (Bearer auth) — body: {key,title,content,author,taint?,meta?}; `meta` is opaque JSON round-tripped on reads; runs the ingest pipeline
- GET  /v1/{namespace}/docs        (Bearer auth)
- GET  /v1/{namespace}/docs/{id}   (Bearer auth) — by engram document_id
- GET    /v1/{namespace}/docs/by-key/{key}  (Bearer auth) — fetch by caller key
- DELETE /v1/{namespace}/docs/by-key/{key}  (Bearer auth) — forget (doc + chunks + entities + unsealed leaves)
- POST /v1/{namespace}/query       (Bearer auth) — body: {query, limit?} → ranked hits (graph + vector + keyword)
- GET  /v1/{namespace}/recall      (Bearer auth) — ?limit= → most-recent first
- POST /v1/{namespace}/tree        (Bearer auth) — body: {query, tree_kind?, tree_key?, max_depth?, limit?} → drill-down hits (defaults to the Global tree)

## Ingest & retrieval

On `POST /docs`, ingest replaces the doc's prior chunks then, per chunk:
token-aware CJK-safe chunking (`\n\n`, packed to ~800 chars), batched embeddings with
retry-on-transient (skipping any chunk that permanently fails to embed), and mechanical
entity extraction (`email:`/`url:`/`handle:`/`hashtag:`) recorded into a
per-namespace co-occurrence index. `query` scores each candidate doc by the best of
its chunks: when the query shares entities with indexed docs it blends graph overlap
+ vector + keyword (weights 0.55/0.30/0.15); otherwise it falls back to vector +
keyword (0.65/0.35). Each `Hit` carries its `graph`/`vector`/`keyword` sub-scores.

## Autonomous consolidation (Plan 4)

`POST /docs` returns fast: it commits the doc + chunks + entities **and** enqueues a
post-acquire job in one transaction, then background worker threads drain a persisted
`post_acquire_jobs` queue. Each job fans the doc's chunks into **Source** (per author),
**Global** (per namespace), and **Topic** (per entity) bucket-seal trees. A buffer seals
when it exceeds the token budget (L0), the fanout (L≥1), or the stale-flush age — folding
its children into one immutable summary via the LLM gateway (`qwen3`/`deepseek-chat`),
with a deterministic concat fallback so cascades never abort. Every LLM call self-audits
to llm-audit. `POST /tree` does BFS drill-down over the summaries (cosine-reranked,
latest-leaf-per-doc). Set `ENGRAM_VAULT_DIR` to also mirror docs + sealed summaries as
Obsidian-browsable `.md`. LLM/embeddings route through the litellm gateway (gateway-host:4000).

## Env vars

| Variable             | Default                   | Description                        |
|----------------------|---------------------------|------------------------------------|
| ENGRAM_DB            | engram.db                 | SQLite database path               |
| ENGRAM_BIND          | 127.0.0.1:8088            | Listen address                     |
| ENGRAM_TOKEN         | dev-token                 | Bearer auth token                  |
| ENGRAM_GATEWAY_URL   | http://127.0.0.1:4000 | litellm gateway base URL (embeddings + LLM) |
| ENGRAM_GATEWAY_KEY   | (empty)                   | gateway Bearer master key          |
| ENGRAM_EMBED_MODEL   | bge-m3                    | embedding model name at the gateway |
| ENGRAM_EMBED_DIM     | 1024                      | Embedding vector dimension         |
| ENGRAM_EMBED_TIMEOUT_SECS | 30                   | embed HTTP timeout (seconds)       |
| ENGRAM_LLM_MODEL     | qwen3                     | summarizer model at the gateway    |
| ENGRAM_OLLAMA_URL    | http://127.0.0.1:11434    | direct-Ollama base URL (only the fallback `OllamaEmbedder`, not the default path) |
| ENGRAM_AUDIT_URL     | http://127.0.0.1:8383 | llm-audit sink                    |
| ENGRAM_JOBS_WORKERS  | 2                         | post-acquire worker threads        |
| ENGRAM_SEAL_INPUT_TOKEN_BUDGET | 50000           | L0 seal gate (≈chars/4)            |
| ENGRAM_SEAL_FANOUT   | 10                        | L≥1 seal gate (sibling count)      |
| ENGRAM_SEAL_FLUSH_AGE_SECS | 604800              | stale-flush age (7 days)           |
| ENGRAM_VAULT_DIR     | (unset)                   | if set, write the Obsidian mirror  |

Both embeddings and LLM summaries route through the litellm gateway, so they need a
reachable `ENGRAM_GATEWAY_URL` + `ENGRAM_GATEWAY_KEY` and a model the gateway serves
(`mxbai-embed-large` works today; `bge-m3` once pulled — set `ENGRAM_EMBED_MODEL`
accordingly). The full env list (timeouts, poll/retry, sweep cadence, summary cap) is
in `src/config.rs`.

## Deploy (WSL2)

Live via `deploy/run.sh {start|stop|status|logs}` — a `screen` session runs the release binary
(crash-restart loop) **plus a reverse SSH tunnel** that exposes engram on gateway-host's `127.0.0.1:8088`.
The litellm gateway fronts it on the LAN through an `/engram` pass-through (`include_subpath`,
master-key gated), so the single LAN endpoint is **`http://127.0.0.1:4000/engram/v1/...`**
(consumers send the litellm master key; engram's own `ENGRAM_TOKEN` is injected by the gateway).

- **Build:** `cargo build --release`.
- **Config:** `~/engram/.env` (`chmod 600`) — on native **ext4**, NOT in the repo (the repo is a
  v9fs mount: chmod doesn't stick there and SQLite WAL is flaky). Copy from `.env.example`; holds
  `ENGRAM_TOKEN` + the gateway key. DB/vault/logs live under `~/engram/` (ext4).
- **Not boot-persistent:** re-run `run.sh start` after `wsl --shutdown`/reboot.

Not yet (deferred): semantic LLM entity extraction, profile facets, vault edit-back,
moving embedding off the hot path.
