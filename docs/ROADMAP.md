# engram — Roadmap

Status and build order for engram as a **code knowledge base for coding agents**. Design source
of truth: the spec (`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`)
and the master plan it grew into. This file tracks *what's done and what's next*.

Each phase is independently shippable. Legend: ✅ done · 🔄 done, live-validation pending · ◻️ planned.

_Last updated: 2026-06-08. `main` published at `github.com/vinc-lin/engram`; 138 tests green._

| Phase | Delivers | Status |
|-------|----------|--------|
| 0 — Workspace + `store/` split | clean foundation | ✅ |
| 1a — Core code ingest & search engine | line-ranged chunks, `search_code` | ✅ |
| 1a.1 — `POST /code/search` HTTP route | code search over the wire | ✅ |
| A — Robustness | migration, CJK chunking, retry, tolerant ingest | ✅ |
| B — Throughput | batch embeddings | ✅ |
| 1b — `engram-index` CLI | index / reindex / hook (write side) | ✅ |
| 1c — `engram-mcp` server | `search_code` MCP tool (read side) | ✅ |
| E — Validation harness | ingest-rate + recall@10 on agentmemory | 🔄 |
| 2 — Architecture digests | `get_architecture` / `get_module` | ◻️ |
| 3a — History / rationale | `why` / `find_symbol` | ◻️ |
| 3b — Conventions | `get_conventions` | ◻️ |
| 4 — Depth | tree-sitter chunking, reseal, more languages | ◻️ |

The **MVP closed loop is complete**: `engram-index index <repo>` → engram (code-mode ingest,
robust) → `engram-mcp search_code` → coding agent. A `post-commit` hook keeps it current.

---

## Done

### Phase 0 — Workspace + `store/` split ✅
Cargo workspace (`crates/engram` + `engram-index` + `engram-mcp`); split the 1031-line
`store.rs` into `store/{mod,docs,chunks,entities,jobs,trees}.rs`. No behavior change.

### Phase 1a — Core code ingest & search engine ✅
`vector_chunks` line columns; `chunk_code` (line-aware); `extract_code_entities`
(`sym:`/`import:`/`path:`); `meta.kind=="file"` dispatch; `search_code` → `CodeHit`
(`path:line` + snippet + scores) via `code_chunks_for_namespace`.

### Phase 1a.1 — `POST /v1/:ns/code/search` ✅
The HTTP route for code search (Phase 1a left `search_code` library-only). Unblocked the MCP read side.

### Phase A — Robustness ✅
The fixes surfaced by ingesting a real, multilingual repo (agentmemory, ~47% CJK — only 84%
ingested before):
- **Auto-migration** in `Store::open` — adds missing `line_start`/`line_end` via `ALTER` +
  `user_version`; deploys self-heal (no manual `ALTER`).
- **Token-aware CJK-safe chunking** — `estimate_tokens` (CJK≈1 tok/char), `CODE_CHUNK_TOKEN_BUDGET`,
  flush on char-cap **or** token-budget, hard-split oversized CJK lines. Fixes the embed-model 500s.
- **Embedder resilience** — configurable request timeout (`ENGRAM_EMBED_TIMEOUT_SECS`),
  retry-with-backoff on transient errors only (not 4xx / decode).
- **Tolerant ingest** — skip a chunk that can't embed, ingest the rest, warn with a count; error
  only if *all* chunks fail. No more whole-file loss from one bad chunk.

### Phase B — Throughput ✅
**Batch embeddings** (`embed_batch`, one gateway call per 64 chunks) with a safe per-chunk
fallback when a batch fails — fast path + isolation. Config knob + docs.

### Phase 1b — `engram-index` CLI ✅
`crates/engram-index`: `index` (git-tracked walk honoring `.gitignore` + denylist, skip
binary/oversized/lockfiles, bounded concurrency + retry, `.git/engram/last-sha`), `reindex`
(`git diff --name-status -M` → POST/DELETE/rename, full-reindex fallback), `install-hook`
(post-commit). Pure logic (`should_index`, `parse_diff_line`, by-key URL) unit-tested.

### Phase 1c — `engram-mcp` server ✅
`crates/engram-mcp`: hand-rolled stdio JSON-RPC MCP server exposing `search_code(query, limit?)`
→ `POST /code/search` → ranked `path:line` + snippets. `initialize` / `tools/list` / `tools/call`.
Smoke-tested over stdio.

---

## In progress

### Phase E — Validation on `../agentmemory` 🔄
Committed: `eval/agentmemory_gold.json` (15 labeled NL→file queries) + `eval/validate.py`
measuring two bars — **ingest success ≥ 0.99** and **recall@10 ≥ 0.80**.
**Live run is pending** the embed backend (Ollama `mxbai-embed-large`) recovering — it is
currently wedged (every query/ingest embeds). The robustness build is deployed; once the backend
is healthy: `ENGRAM_TOKEN=… python3 eval/validate.py --index --repo <path/to/agentmemory>`.
Expectation: ingest 84% → ~100%; recall@10 ≥ 0.8.

---

## Planned

### Phase 2 — Architecture digests ◻️
Code-tuned consolidation: fan code leaves into `module` (by directory) and `global` trees +
topic trees on `sym:`/`import:`; MCP tools `get_architecture` / `get_module`.
*Until then, code consolidates into the prose-shaped Source/Global/Topic trees — usable
per-file/per-symbol summaries, not a designed architecture digest. `search_code` reads chunks
directly, so it's unaffected.*
Bar: every top-level source dir has a non-fallback `module` digest; `global` names the subsystems.

### Phase 3a — History / rationale ◻️
Ingest git history as docs; MCP tools `why(query)` (rationale from commits) and `find_symbol(name)`
(via the `sym:` graph). No schema work — de-risked.
Bar: `why` returns the correct originating commit for ≥ 0.7 of ~15 labeled regions.

### Phase 3b — Conventions ◻️
Conventions-extraction pass over digests + `get_conventions()`.
Bar: ≥ ~10 distinct conventions, each spot-checked correct (no fabrication).

### Phase 4 — Depth ◻️
Tree-sitter function-boundary chunking + richer symbols (better `find_symbol` precision);
reseal-stale-digests pass; more languages; embedder-aware token budget (e.g. bge-m3's 8k vs
mxbai's 512, instead of the conservative const).

### Headline success criterion
With-vs-without: an agent using the MCP tools scores higher on a fixed engram-question suite than
without them. Measurable once 2/3 add the richer tools on top of today's `search_code`.

---

## Deferred enhancements (noted, not lost)
- **MCP streamable-HTTP transport** (1c shipped stdio only) and the later tools
  (`get_architecture`/`get_module`/`why`/`find_symbol`/`get_conventions`).
- **Retrieval quality**: graph/`sym:` signal in code ranking; BM25/stemming for keyword overlap;
  ANN/vector index (linear cosine is fine ≲100k chunks; agentmemory is ~2k).
- **Snippet trimming** — `search_code` returns the whole chunk, not a focused excerpt.

## Cross-cutting invariants & risks
- **Off-lock I/O, single writer, embedder-signature gating** (changing embed model/dim orphans
  chunks → re-index). **Sealed nodes immutable** — "forget" is partial erasure (text can persist
  in sealed summaries + the write-only vault).
- **Digest freshness** — sealed digests trail recent edits until re-seal; `search_code` is always
  current. **Symbol precision** — regex-approximate until tree-sitter.
- **Consolidation/embedding cost on large repos** — pace indexing (engram-index bounds concurrency
  + retries; batch embeds); a bulk ingest can still saturate a single local embed backend.

## Operational
- `main` (public, scrubbed identity) is the working branch; `master` is the full-history local
  backup. The deploy runs `target/release/engram` via `deploy/run.sh` (screen + reverse tunnel);
  LLM summaries route to DeepSeek, embeddings to the gateway's `mxbai-embed-large`.
