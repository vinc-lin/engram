# engram

A shared, single-writer memory service — an axum HTTP API over SQLite (WAL, a read
pool + one writer) plus an autonomous cold consolidation pipeline — **extended into a
code knowledge base for coding agents.** Point the indexer at a git repo, and an agent
can search code, read auto-generated architecture/module digests, look up symbols, and
ask "why does this code exist?" against git history through 6 MCP tools.

**Status: roadmap complete.** All phases (0, 1a–1c, A/B, E, F, R, 2, 3a/3b, 4) are done;
168 tests pass, clippy + fmt clean. The code-consolidation digests (phases 2/3a/3b) are
code-complete and run on demand; their live quality bars depend on a (cost-bearing)
consolidation + history-ingest run and are tracked as pending-live.

## The closed loop

```
engram-index index <repo>  →  POST /docs (code-mode ingest)  →  search_code / MCP tools  →  agent
```

1. **Index** — `engram-index` walks a git repo's tracked files and POSTs each as a doc.
2. **Ingest** — engram chunks code on function/type boundaries, extracts symbols, embeds,
   and atomically commits.
3. **Retrieve** — the agent calls the MCP tools (or the HTTP endpoints directly) to search
   code, fetch digests, find symbols, and query history.

## Workspace

Cargo workspace with three crates:

| Crate | Role |
|-------|------|
| `crates/engram`       | Core service: HTTP API, store, ingest, retrieve, embed, tree, llm, conventions, tree-sitter, vault |
| `crates/engram-index` | Repo indexer CLI: `index` / `reindex` / `index-history` / `install-hook` |
| `crates/engram-mcp`   | MCP server exposing the agent tools over stdio and HTTP |

## Memory model → code

- A **repo** is a namespace: `repo:<id>`.
- A **file** is a document (`meta.kind = "file"` → triggers code-mode ingest).
- An **identifier** is an entity: `sym:` (symbols), `import:` (imports), `path:` (file path).

Two derived namespaces hang off each repo:

- `repo:<id>:history` — git commits (`meta.kind = "commit"`), powering `why`.
- `repo:<id>:meta` — the generated conventions doc, served by `get_conventions`.

## MCP tools (6)

`engram-mcp` speaks JSON-RPC 2.0 over **stdio** (default) and over **HTTP** (set
`ENGRAM_MCP_HTTP=<addr>` to POST JSON-RPC instead). It talks to a running engram via
`ENGRAM_URL` (default `http://127.0.0.1:8088`) + `ENGRAM_TOKEN`.

| Tool | What it does | HTTP endpoint behind it |
|------|--------------|-------------------------|
| `search_code(query)`      | Chunk-level code search → `path:line` + snippet      | `POST /v1/:ns/code/search` |
| `get_architecture()`      | The repo's global digest (cached single-pass)         | `POST /v1/:ns/code/architecture` |
| `get_module(path)`        | A directory's digest (cached single-pass)             | `POST /v1/:ns/code/module` |
| `why(query)`              | Relevant git-history commits                           | `POST /v1/:ns/query` (history ns) |
| `find_symbol(name)`       | Where a symbol is defined / used                      | `POST /v1/:ns/code/search` |
| `get_conventions()`       | The generated conventions doc                          | `GET /v1/:ns/conventions` |

## Ingest pipeline (hot path)

`POST /v1/:ns/docs` dispatches on `meta.kind`. For `"file"` it runs **code mode**:

- **tree-sitter function/type-boundary chunking** (`src/treesit.rs`; Rust, Python,
  JS, TS, TSX, Go, Kotlin, Java, C, C++ — `.h` routes to the C++ grammar) — falls back
  to a heuristic line/symbol chunker on unsupported languages or parse failures.
- **AST symbol extraction** → accurate `sym:` entities, plus a `path:` entity.
- embed each chunk **off-lock**, then one atomic `commit_ingest`.

It is token-aware, CJK-safe, and per-chunk-tolerant: a bad chunk is skipped, the rest
of the file still ingests. Prose docs (no `meta.kind = "file"`) take the original
paragraph-chunking + co-occurrence-graph path.

## Retrieval

- **`search_code`** — chunk-level: vector cosine + keyword, plus a **path-type ranking
  prior** that down-weights docs/tests/config so source ranks first. Returns `path:line`
  + snippet.
- **`drill_down`** (`POST /v1/:ns/tree`) — BFS over the consolidation trees; query-less
  returns the top digests.
- **`query`** (`POST /v1/:ns/query`) — prose hybrid scoring (vector + keyword + entity
  graph).

## Autonomous consolidation (cold path)

Background `std::thread` workers + a stale-flush sweeper drain a persisted job queue.
`process_doc` fans code chunks into a directory-keyed **module** tree, a **global** tree,
and **topic** trees (on `sym:`/`import:` entities); prose uses the author-keyed source
tree. Sealing summarizes a buffer via the LLM gateway with a deterministic concat
fallback, so a cascade never aborts.

Code consolidation is **off by default** (`ENGRAM_CONSOLIDATE_CODE=false`): `search_code`
reads chunks, not trees, so the trees only matter once the digest tools
(`get_architecture` / `get_module`) are in use.

The digest tools serve a **cached single-pass digest** from the `repo:<id>:meta`
namespace (key `architecture` or `module:<dir>`), built by
`POST /v1/:ns/code/architecture/rebuild` → `rebuild_architecture_digest` — **one** LLM
summary over the repo's whole source (with extractive overflow). The consolidation-tree
drill is only a **fallback** when no single-pass digest is cached. One compression pass
preserves the specifics a deep summary-of-summaries fold loses (measured: one-shot 1.58
vs tree 1.08 of 2; see `eval/RESULTS_distillation.md`).

## Quickstart

```bash
# 1. Build the workspace (release; deploy uses target/release/engram).
cargo build --release

# 2. Run engram. Env vars are the only config surface (see crates/engram/src/config.rs).
ENGRAM_DB=$HOME/engram/engram.db \
ENGRAM_BIND=127.0.0.1:8088 \
ENGRAM_TOKEN=secret \
ENGRAM_GATEWAY_URL=http://127.0.0.1:4000 \
ENGRAM_GATEWAY_KEY=... \
  target/release/engram

# 3. Index a repo (namespace by convention is repo:<id>).
ENGRAM_TOKEN=secret \
  target/release/engram-index index ../my-repo --namespace repo:my-repo
# optional: git history for `why`, and a post-commit hook for incremental reindex
ENGRAM_TOKEN=secret target/release/engram-index index-history ../my-repo --namespace repo:my-repo:history
ENGRAM_TOKEN=secret target/release/engram-index install-hook ../my-repo --namespace repo:my-repo

# 4. Point an MCP client at it (stdio by default).
ENGRAM_URL=http://127.0.0.1:8088 ENGRAM_TOKEN=secret target/release/engram-mcp
# or serve HTTP JSON-RPC instead:
ENGRAM_MCP_HTTP=127.0.0.1:8765 ENGRAM_URL=http://127.0.0.1:8088 ENGRAM_TOKEN=secret target/release/engram-mcp
```

> SQLite must live on native ext4, not the v9fs repo mount, so point `ENGRAM_DB` at an
> ext4 path (e.g. under `$HOME/engram/`) for local runs too.

## Embeddings & LLM

Both embeddings and LLM summaries route through the litellm gateway by default.
`GatewayEmbedder` is the default embedder; an optional `FallbackEmbedder`
(`ENGRAM_EMBED_FALLBACK=true`) wraps it with a local `OllamaEmbedder` and delegates
`signature()` to the primary, so a failover never orphans chunks. The deploy runs
`mxbai-embed-large` (dim 1024) for embeddings and `deepseek-chat` for summaries.

Set `ENGRAM_EMBED_URL` (defaults to `ENGRAM_GATEWAY_URL`) to route **embeddings** to a
separate backend — e.g. a local bge-m3 Ollama — while LLM/chat calls stay on the
gateway. bge-m3 is **not** a better prose embedder (measured ~equal or worse as a drop-in);
its one proven win is enabling wide C/C++ chunks (`ENGRAM_CODE_NATIVE_PACK` +
`ENGRAM_CODE_NATIVE_BUDGET≈1500`), which lifted native line-recall 0.59 → 0.89. The
embedder decision matrix, serving topology, and the 512-token ceiling are documented in
**`docs/EMBEDDINGS.md`**.

> An embedder's `signature()` is stored on every chunk and read-filtered, so changing
> `ENGRAM_EMBED_MODEL` / `ENGRAM_EMBED_DIM` orphans existing chunks — a full re-index is
> required.

## Key env vars

The full list (~21 vars) is in `crates/engram/src/config.rs`; defaults in parens. Config
parsing is permissive — an unparseable value silently falls back to its default.

| Variable | Default | Description |
|----------|---------|-------------|
| `ENGRAM_DB`              | `engram.db`             | SQLite database path (use an ext4 path) |
| `ENGRAM_BIND`           | `127.0.0.1:8088`        | listen address |
| `ENGRAM_TOKEN`          | `dev-token`             | shared Bearer auth token |
| `ENGRAM_GATEWAY_URL`    | `http://127.0.0.1:4000` | litellm gateway base URL (LLM, and embeddings unless overridden) |
| `ENGRAM_GATEWAY_KEY`    | (empty)                 | gateway Bearer key |
| `ENGRAM_EMBED_URL`      | = `ENGRAM_GATEWAY_URL`  | embeddings endpoint, decoupled from the LLM URL (point at a local Ollama; see `docs/EMBEDDINGS.md`) |
| `ENGRAM_EMBED_MODEL`    | `bge-m3`                | embedding model (deploy: `mxbai-embed-large`) |
| `ENGRAM_EMBED_DIM`      | `1024`                  | embedding vector dimension |
| `ENGRAM_EMBED_TIMEOUT_SECS` | `30`                | embed HTTP timeout (seconds) |
| `ENGRAM_EMBED_FALLBACK` | `false`                 | wrap the gateway embedder with a local Ollama fallback |
| `ENGRAM_LLM_MODEL`      | `qwen3`                 | summarizer model (deploy: `deepseek-chat`) |
| `ENGRAM_CONSOLIDATE_CODE` | `false`               | run the cold pipeline on code chunks |
| `ENGRAM_CODE_SYMBOL_SPLIT` | `true`               | split code chunks on symbol boundaries |
| `ENGRAM_CODE_PATH_PRIOR` | `true`                 | path-type ranking prior in `search_code` |
| `ENGRAM_CODE_TREE_SITTER` | `true`                | tree-sitter chunking (off → heuristic chunker) |
| `ENGRAM_CODE_MIN_SCORE` | `0.0`                   | `search_code` abstention floor (0 = off); drops confident-but-wrong hits on absent topics |
| `ENGRAM_CODE_NATIVE_PACK` | `false`               | pack consecutive C/C++ defs into wider boundary-aligned chunks (needs a long-context embedder) |
| `ENGRAM_CODE_NATIVE_BUDGET` | `480`                | token budget for native packing (raise to ~1500 with a long-context embedder) |
| `ENGRAM_MCP_HTTP`       | (unset)                 | if set, `engram-mcp` serves HTTP JSON-RPC |
| `ENGRAM_INDEX_TIMEOUT_SECS` | `120`               | `engram-index` per-request POST/DELETE timeout (raise for large files) |

## Validation

`eval/validate.py` measures ingest-rate, recall@1/5/10, line-recall, and hard negatives
over `eval/agentmemory_gold.json` (35 labeled NL→file queries, ≤2 per file). Live
production numbers on the tree-sitter config:

| Metric | Value |
|--------|-------|
| ingest        | 0.998 |
| recall@1      | 0.543 |
| recall@5      | 0.771 |
| recall@10     | 0.914 |
| line-recall@10| 0.771 |

The recall journey: 0.533 (baseline) → 0.800 (Phase F) → tree-sitter, which raised
precision (recall@1), coverage (recall@10), and line accuracy; the small recall@5 dip is
an accepted trade. Details and the full phase history are in `eval/RESULTS.md`. A second
suite covers Android feature-migration — tree-sitter grammars, the native line-recall fix,
and agent A/B (`eval/RESULTS_android.md`) — and the digest distillation A/B (single-pass
summary beats the tree fold) is in `eval/RESULTS_distillation.md`.

## Deploy (WSL2)

`deploy/run.sh {start|stop|status|logs}` runs the release binary in a `screen` session
(crash-restart loop) plus a reverse SSH tunnel exposing engram on the gateway host's
`127.0.0.1:8088`. The litellm gateway fronts it on the LAN via an `/engram` pass-through
(master-key gated), so the single endpoint is `http://127.0.0.1:4000/engram/v1/...`. The
DB, vault, logs, and `.env` (`chmod 600`) live under `$HOME/engram/` on ext4 — not in the
repo (v9fs mount: `chmod` doesn't stick and WAL is flaky). Re-run `run.sh start` after a
WSL shutdown/reboot; it is not boot-persistent.
