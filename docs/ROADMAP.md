# engram — Roadmap

Status and build order for engram as a **code knowledge base for coding agents**. Design source
of truth: the spec (`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`)
and the master plan it grew into. This file tracks *what's done and what's next*.

Each phase is independently shippable. Legend: ✅ done · 🔄 done, live-validation pending · 🚦 validation gate · ◻️ planned.

_Last updated: 2026-06-09. Published at `github.com/vinc-lin/engram` (origin/main `3ab3d6f`); this roadmap revision is local, pending push._

| Phase | Delivers | Status |
|-------|----------|--------|
| 0 — Workspace + `store/` split | clean foundation | ✅ |
| 1a — Core code ingest & search engine | line-ranged chunks, `search_code` | ✅ |
| 1a.1 — `POST /code/search` HTTP route | code search over the wire | ✅ |
| A — Robustness | migration, CJK chunking, retry, tolerant ingest | ✅ |
| B — Throughput | batch embeddings | ✅ |
| 1b — `engram-index` CLI | index / reindex / hook (write side) | ✅ |
| 1c — `engram-mcp` server | `search_code` MCP tool (read side) | ✅ |
| E — Validation gate | first real baseline measured + harness tightened (recall@1/5/10, line-hits) | ✅ |
| F — Retrieval quality | symbol-split chunking + path prior → **recall@5 0.80 / ingest 0.998 PASS** | ✅ |
| R — Reliability | FallbackEmbedder + `ENGRAM_CONSOLIDATE_CODE` gate + tunnel guard (deploy hardened) | ✅ |
| 2 — Architecture digests | code-tuned module/global trees + `get_architecture`/`get_module` (3 MCP tools) | 🔄 |
| 3a — History / rationale | git-history ingest (`index-history`) + `why`/`find_symbol` (5 MCP tools) | 🔄 |
| 3b — Conventions | config+digest extraction → `:meta` doc + `get_conventions` (6 MCP tools) | 🔄 |
| 4 — Depth | tree-sitter chunking + AST symbols (6 langs) + MCP HTTP; reseal via stale-flush sweeper | ✅ |

The **MVP closed loop is complete**: `engram-index index <repo>` → engram (code-mode ingest,
robust) → `engram-mcp search_code` → coding agent. A `post-commit` hook keeps it current.

---

## Findings (2026-06-09) — what a live probe + an offline A/B established

These reset assumptions baked into the phases below:

- **Validation is unblocked, but unmeasured.** The live deploy is healthy (451/534 docs embedded,
  `/healthz` 200); the old "embed backend wedged" blocker was stale. Yet `recall@10 ≥ 0.80` has
  only ever been an *expectation* — **no real number has been measured.** So E is now a **gate**,
  not a peer line item.
- **The bge-m3 swap is NOT a quality win (measured).** Offline A/B on the 15 gold queries — local
  GPU bge-m3 vs the mxbai vectors already in the live DB, identical chunks — gave **mxbai
  0.267 / 0.600 / 0.600** vs **bge-m3 0.067 / 0.333 / 0.533** (recall@1/5/10); mxbai ranked the gold
  file better on 9 of 15 queries. A blind `mxbai → bge-m3` re-index would *lower* recall. bge-m3's
  only un-tested upside is **coverage** of the 83 CJK files mxbai dropped (2 of which are gold and
  absent from the corpus) — pending a full re-chunk eval. → the swap leaves the critical path.
- **The real recall lever is chunking/coverage, not the embed model.** Both models bury the 3
  `src/types.ts` queries at rank 41–137 (large type file, the answer isn't isolated). That's
  granularity, and it caps recall regardless of embedder → **Phase F**.
- **The embedder is a single point of failure.** Only `GatewayEmbedder` is wired; `OllamaEmbedder`
  exists but isn't used; the gateway already 500'd once in prod. → wire a fallback (**Phase R**).
- **Code consolidation runs unread.** Code ingest fans into the prose-shaped trees and fires
  per-leaf LLM seals (6077 `tree_nodes` built), but `search_code` reads chunks directly and never
  touches them — pure cost until Phase 2 consumes them. → gate it off (**Phase R**) until then.

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
**Unblocked — the live deploy is healthy** (engram up on `127.0.0.1:8088`, `/healthz` → 200; the
earlier "embed backend wedged" note was stale). Embeddings route through the **litellm gateway**
(`mxbai-embed-large`, returning 200) — not a direct Ollama, as the old note implied. The live
namespace `repo:agentmemory` currently holds **451/534 docs (84%) fully embedded** under
`gateway:mxbai-embed-large:1024` — the pre-robustness ingest; a clean re-index with the robust
build should lift it to ~100%. Run:
`ENGRAM_TOKEN=… python3 eval/validate.py --index --repo <path/to/agentmemory>` (omit `--index` to
score the already-embedded corpus). Expectation: ingest 84% → ~100%; recall@10 ≥ 0.8 — **no real
number has been measured yet.**
**E is now a gate:** no phase past F starts until E reports a real PASS on a tightened bar; the
metric-tightening work itself lives in Phase F.

---

## Planned

### Phase F — Retrieval quality (next) ◻️
The 2026-06-09 A/B showed recall is capped by chunking + coverage, not the embed model. Three moves:
- **Tighten the metric** — add `recall@5` as the headline bar (top-10 of ~1.9k chunks is lenient),
  score **line-range hits** (does the matched chunk's span contain the answer?) not whole-chunk
  hits, and de-dupe/expand the gold set (today 3 of 15 queries collapse onto `src/types.ts`; ~10
  distinct files) to ~30 queries at ≤ 2/file + a hard-negatives bucket.
- **Fix granularity** — large type/definition files (`types.ts`) bury specific answers; smaller or
  symbol-aware chunks (a down-payment on Phase 4's tree-sitter) should lift the buried queries.
- **Coverage** — re-index agentmemory with the robust build to recover the 83 dropped files
  (84% → ~100%), then re-measure. *(Optional: a full re-chunk bge-m3 eval to settle its coverage
  upside — the one bge-m3 advantage the drop-in A/B couldn't test.)*
Bar: tightened `recall@5 ≥ 0.80` on the expanded gold set; ingest ≥ 0.99.

### Phase R — Reliability & foundations ◻️
Cheap, high-leverage substrate fixes, all using code that already exists:
- **Embedder fallback** — wire the existing `OllamaEmbedder` behind `GatewayEmbedder` (a single
  stable `signature()` to avoid orphaning reads on failover) + a startup embed-reachability probe.
  Neutralizes the verified gateway-500 outage class.
- **Gate code consolidation off** (`ENGRAM_CONSOLIDATE_CODE`, default off) — `search_code` never
  reads the trees; stop paying per-leaf LLM seals until Phase 2 ships a consumer.
- **Fix/remove the crash-looping reverse tunnel** (placeholder `gateway.example` / missing key) so
  "deploy is healthy" is honest.
*(The bge-m3 swap — once expected here / in Phase 4 — is **deprioritized by evidence**; see Findings.)*

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

### Headline success criterion
With-vs-without: an agent using the MCP tools scores higher on a fixed engram-question suite than
without them. Depends on **E** producing a real baseline first; meaningfully measurable once 2/3
add the richer tools on top of today's `search_code`.

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
  + retries; batch embeds); a bulk ingest can still saturate the single embed backend. **Code
  consolidation currently runs unread** (`search_code` never touches trees) — Phase R gates it off.
- **Embedder is a single point of failure** — only `GatewayEmbedder` is wired (the gateway 500'd
  once in prod); `OllamaEmbedder` exists but isn't used. Phase R wires the fallback.

## Operational
- `main` (public, scrubbed identity) is the working branch; `master` is the full-history local
  backup. The deploy runs `target/release/engram` via `deploy/run.sh` (screen + reverse tunnel);
  LLM summaries route to DeepSeek, embeddings to the gateway's `mxbai-embed-large`.
- The gateway has a `bge-m3` entry but it's **misrouted** (→ an Ollama host without bge-m3 → 500);
  a working bge-m3 exists only as a local GPU venv (`~/bge-m3`). The reverse tunnel is currently
  **crash-looping** on placeholder config — engram itself is unaffected (loopback bind).
