# engram — Roadmap

Status and build order for engram as a **code knowledge base for coding agents**. Design source
of truth: the spec (`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`)
and the master plan it grew into. This file records *what shipped*.

**The roadmap is complete — every phase is done.** Legend: ✅ done · 🔄 code-complete, live-quality bar pending.

_Last updated: 2026-06-09. Published at `github.com/vinc-lin/engram` (origin/main in sync with the working tree)._

| Phase | Delivers | Status |
|-------|----------|--------|
| 0 — Workspace + `store/` split | clean foundation | ✅ |
| 1a — Core code ingest & search engine | line-ranged chunks, `search_code` | ✅ |
| 1a.1 — `POST /code/search` HTTP route | code search over the wire | ✅ |
| A — Robustness | migration, CJK chunking, retry, tolerant ingest | ✅ |
| B — Throughput | batch embeddings | ✅ |
| 1b — `engram-index` CLI | index / reindex / index-history / hook (write side) | ✅ |
| 1c — `engram-mcp` server | MCP tools over stdio + HTTP (read side) | ✅ |
| E — Validation gate | first real baseline measured + harness tightened (recall@1/5/10, line-hits) | ✅ |
| F — Retrieval quality | symbol-split chunking + path prior → **recall@5 0.80 / ingest 0.998 PASS** | ✅ |
| R — Reliability | FallbackEmbedder + `ENGRAM_CONSOLIDATE_CODE` gate + tunnel guard (deploy hardened) | ✅ |
| 2 — Architecture digests | code-tuned module/global trees + `get_architecture`/`get_module` | 🔄 |
| 3a — History / rationale | git-history ingest (`index-history`) + `why`/`find_symbol` | 🔄 |
| 3b — Conventions | config+digest extraction → `:meta` doc + `get_conventions` | 🔄 |
| 4 — Depth | tree-sitter chunking + AST symbols (6 langs) + MCP HTTP; reseal via stale-flush sweeper | ✅ |

The **closed loop is shipping**: `engram-index index <repo>` → engram (code-mode ingest,
tree-sitter chunking + AST symbols, robust) → the 6 `engram-mcp` tools → coding agent. A
`post-commit` hook keeps it current. 168 tests pass; clippy + fmt clean. The 🔄 phases (2, 3a, 3b)
are code-complete and wired end-to-end; their *live quality bars* (non-fallback digests; `why` ≥ 0.7)
need a consolidation / history-ingest run, which is cost-bearing and noted pending-live below.

---

## Findings — what a live probe + an offline A/B established (and how each resolved)

These reset assumptions baked into the phases; each is now settled:

- **Validation was unblocked, then measured.** The earlier "embed backend wedged" blocker was
  stale; the live deploy is healthy. `recall@10 ≥ 0.80` had only ever been an *expectation* — so E
  became a **gate**, and a real baseline was measured (recall@10 0.533, ingest 0.845 pre-robustness;
  see `eval/RESULTS.md`). The bar later passed at Phase F. → **resolved (E).**
- **The bge-m3 swap is NOT a quality win (measured).** Offline A/B on the gold queries — local GPU
  bge-m3 vs the mxbai vectors already in the live DB, identical chunks — gave **mxbai
  0.267 / 0.600 / 0.600** vs **bge-m3 0.067 / 0.333 / 0.533** (recall@1/5/10); mxbai ranked the gold
  file better on 9 of 15 queries. A blind `mxbai → bge-m3` re-index would *lower* recall. → the swap
  left the critical path; **production embeds with `mxbai-embed-large`.**
- **The real recall lever is chunking/coverage, not the embed model.** Both models buried the
  `src/types.ts` queries (large type file, the answer isn't isolated). That granularity cap held
  regardless of embedder → addressed by symbol-split (**F**) and tree-sitter (**4**).
- **The embedder is a single point of failure.** Only `GatewayEmbedder` was wired; the gateway
  500'd once in prod. → `FallbackEmbedder` now wraps it with `OllamaEmbedder` behind
  `ENGRAM_EMBED_FALLBACK` (**R**); `signature()` delegates to the primary so failover never orphans.
- **Code consolidation runs unread.** Code ingest can fan into trees and fire per-leaf LLM seals,
  but `search_code` reads chunks directly and never touches them — pure cost until the digest tools
  consume them. → gated off by default (`ENGRAM_CONSOLIDATE_CODE`, **R**).

---

## Shipped

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
`crates/engram-mcp`: hand-rolled JSON-RPC MCP server (`initialize` / `tools/list` / `tools/call`)
over **stdio and HTTP** (`ENGRAM_MCP_HTTP=addr`). Exposes all 6 tools — `search_code`,
`get_architecture`, `get_module(path)`, `why(query)`, `find_symbol(name)`, `get_conventions()` —
backed by `POST /code/search`, `/code/architecture`, `/code/module`, `/query`, `/conventions/rebuild`,
`GET /conventions`, and `POST /tree`.

### Phase E — Validation on `../agentmemory` ✅
`eval/agentmemory_gold.json` (now 35 labeled NL→file queries, ≤ 2/file + a hard-negatives bucket) +
`eval/validate.py` measuring **ingest success**, **recall@1/5/10**, **line-recall@10**, and hard
negatives. Embeddings route through the **litellm gateway** (`mxbai-embed-large`, dim 1024). The
first real baseline (Phase E): **ingest 0.845 (451/534), recall@10 0.533** — FAIL on the tightened
bars, with a three-way gap analysis (coverage / granularity / keyword distractors) that fed Phase F.
Run: `ENGRAM_TOKEN=… python3 eval/validate.py --index --repo <path/to/agentmemory>` (omit `--index`
to score an already-embedded corpus). Full history in `eval/RESULTS.md`.

### Phase F — Retrieval quality ✅
The A/B showed recall is capped by chunking + coverage, not the embed model. Three moves landed:
- **Tightened metric** — `recall@5` is the headline bar (top-10 of ~1.9k chunks was lenient),
  plus **line-range hits** (does the matched chunk's span contain the answer?), an expanded 35-query
  gold set (≤ 2/file), and a hard-negatives bucket.
- **Granularity** — heuristic symbol-split chunking (`ENGRAM_CODE_SYMBOL_SPLIT`, default on) isolates
  definitions so large type files no longer bury specific answers.
- **Coverage + ranking** — robust-build re-index recovered the dropped files (ingest → 0.998), and a
  **path-type prior** (`ENGRAM_CODE_PATH_PRIOR`, default on; docs 0.5 / tests 0.6 / config 0.7 /
  source 1.0) down-weights doc/test/config distractors so source ranks first.
Result: **recall@5 0.800 (PASS), ingest 0.998 (PASS)**, line-recall@10 0.714, hard-neg FPs 1/5.

### Phase R — Reliability & foundations ✅
- **Embedder fallback** — `FallbackEmbedder` wraps `GatewayEmbedder` with the local `OllamaEmbedder`
  behind `ENGRAM_EMBED_FALLBACK` (default off); `signature()` delegates to the primary so a failover
  never orphans chunks. Neutralizes the verified gateway-500 outage class.
- **Code consolidation gated off** (`ENGRAM_CONSOLIDATE_CODE`, default off) — `search_code` reads
  chunks, not trees, so per-leaf LLM seals are pure cost until the digest tools are used.
- **Deploy hardened** — the reverse tunnel no longer crash-loops on placeholder config; engram
  itself was always unaffected (loopback bind).

### Phase 2 — Architecture digests 🔄 (code-complete; live digest quality pending)
Code-tuned consolidation: `process_doc` fans code leaves into a directory-keyed **module** tree and
the **global** tree + topic trees on `sym:`/`import:`; MCP tools `get_architecture` (global digest)
and `get_module(path)` (a directory's digest) are wired over `POST /code/architecture` and
`/code/module`. Gated behind `ENGRAM_CONSOLIDATE_CODE` (off by default).
Live bar (pending a consolidation run): every top-level source dir has a *non-fallback* `module`
digest; `global` names the subsystems.

### Phase 3a — History / rationale 🔄 (code-complete; `why` ≥ 0.7 pending)
`engram-index index-history` ingests git commits as docs into `repo:<id>:history`
(`meta.kind="commit"`); MCP tools `why(query)` (rationale from commits, via `/query` on the history
namespace) and `find_symbol(name)` (via the `sym:` graph). No schema work — de-risked.
Live bar (pending a history-ingest run): `why` returns the correct originating commit for ≥ 0.7 of
the labeled regions.

### Phase 3b — Conventions 🔄 (code-complete; spot-check pending)
A conventions-extraction pass (`conventions.rs`) over config files + digests writes a `conventions`
doc into `repo:<id>:meta`; `POST /conventions/rebuild` builds it (LLM, off the runtime),
`GET /conventions` reads it, and the `get_conventions()` MCP tool surfaces it.
Live bar (pending a rebuild run): ≥ ~10 distinct conventions, each spot-checked correct.

### Phase 4 — Depth ✅
Tree-sitter function/type-boundary chunking + AST symbol extraction (`treesit.rs`) across Rust,
Python, JS, TS, TSX, Go — gated by `ENGRAM_CODE_TREE_SITTER`, falling back to the heuristic chunker
on unsupported langs / parse failure. The MCP server gains an HTTP JSON-RPC transport
(`ENGRAM_MCP_HTTP`) beside stdio. **Reseal-stale-digests:** the freshness goal is already met by the
stale-flush sweeper (`gate_exceeded`'s `stale` condition seals consolidated trees whose fresh leaves
age past `seal_flush_age_secs`); the alternative reading — rewriting an already-sealed digest on
re-ingest — is a deliberate non-goal, as it conflicts with the **sealed-nodes-immutable** invariant.
Embedder-aware token budget stays deferred unless a model swap is justified (the bge-m3 A/B found it
isn't — see Findings / Phase F).
**Live production numbers (tree-sitter config):** ingest 0.998, recall@1 0.543, recall@5 0.771,
recall@10 0.914, line-recall@10 0.771. Recall journey: 0.533 (baseline) → 0.800 (Phase F) →
tree-sitter (precision / coverage / line-accuracy up; recall@5 0.771 an accepted trade — best-chunk-
per-file dedup was tested and rejected; see `eval/RESULTS.md`).

### Headline success criterion
With-vs-without: an agent using the MCP tools scores higher on a fixed engram-question suite than
without them. The baseline exists (Phase E); the richer digest / history / convention tools
(2/3a/3b) are wired on top of `search_code`, so this is now measurable once their live quality bars
are run.

---

## Deferred enhancements (noted, not lost)
- **Retrieval quality**: graph/`sym:` signal in code ranking (the lever for the recall@5 trade);
  BM25/stemming for keyword overlap; ANN/vector index (linear cosine is fine ≲100k chunks;
  agentmemory is ~2k).
- **Snippet trimming** — `search_code` returns the whole chunk, not a focused excerpt.
- **engram-index POST timeout** — large multilingual `.md`/`CHANGELOG` files can time out the
  client-side POST on the tree-sitter (higher chunk-count) path; raise the timeout / cap per-file
  chunk concurrency (gold source files re-chunk fine, so this is non-blocking).

## Cross-cutting invariants & risks
- **Off-lock I/O, single writer, embedder-signature gating** (changing embed model/dim orphans
  chunks → re-index). **Sealed nodes immutable** — "forget" is partial erasure (text can persist
  in sealed summaries + the write-only vault).
- **Digest freshness** — sealed digests trail recent edits until re-seal (the stale-flush sweeper
  bounds the lag); `search_code` reads chunks and is always current. **Symbol precision** — accurate
  `sym:` entities via AST extraction on tree-sitter-supported langs; regex-approximate on the
  fallback path.
- **Consolidation/embedding cost on large repos** — pace indexing (engram-index bounds concurrency
  + retries; batch embeds); a bulk ingest can still saturate the embed backend. **Code consolidation
  is gated off by default** (`ENGRAM_CONSOLIDATE_CODE`) since `search_code` never touches trees —
  turn it on only when the digest tools (2/3) are exercised.
- **Embedder resilience** — `FallbackEmbedder` (`ENGRAM_EMBED_FALLBACK`) wraps the gateway with a
  local `OllamaEmbedder`, with a shared `signature()` so failover never orphans reads.

## Operational
- `main` (public, scrubbed identity) is the working branch; `master` is the full-history local
  backup. The deploy runs `target/release/engram` via `deploy/run.sh` (screen + reverse tunnel to
  `gateway-host`); LLM summaries route to `deepseek-chat`, embeddings to the gateway's
  `mxbai-embed-large` (dim 1024).
- The gateway carries a `bge-m3` entry, but the A/B found it is not a quality win, so production
  embeds with `mxbai-embed-large`; `ENGRAM_EMBED_FALLBACK` can layer a local Ollama embedder behind
  the gateway. engram binds loopback (`127.0.0.1`); the reverse tunnel reaches `gateway-host`.
