# engram — The Coding-Agent Knowledge Loop

How a coding agent gets *and stays* smarter about a repository through engram. The
**closed loop** the repo-knowledge work was building toward is now **complete and shipped**:
what it is, which parts are wired, and how the loop closes end-to-end. For build order see
`docs/ROADMAP.md`; for the system's internal layering see `docs/STRUCTURE.md`; for the full
design see the spec (`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`);
for live recall numbers see `eval/RESULTS.md`.

---

## 1. What "closed loop" means

The goal is a **self-updating** knowledge loop where a coding agent both *reads from* and
*writes back into* the same memory. The agent queries engram to write better code; the agent's
commits feed back into engram so the next query is current — with no manual re-indexing.

```
        ┌──────────────────────────────────────────────────────┐
        │                                                      │
        ▼                                                      │
   ┌─────────┐   index / reindex   ┌──────────┐  search_code   │
   │  REPO   │ ──────────────────▶ │  ENGRAM  │  why / get_*   ┌─────────┐
   │ (files, │   (HTTP POST/DEL)   │ store +  │  find_symbol   │  CODE   │
   │ commits)│                     │ consolid.│ ◀───────────── │  AGENT  │
   └─────────┘                     └──────────┘   (6 MCP tools)└─────────┘
        ▲                                                          │
        │                  agent commits code                     │
        └─────────────────────────────────────────────────────────┘
                      post-commit hook → reindex
```

Mapping that makes it work: **repo = namespace, file = document, identifier = entity.** A repo is
`repo:<id>`; a file is a document (`meta.kind:"file"` → code mode); an identifier is an entity
(`sym:` / `import:` / `path:`). Two derived namespaces extend it: `repo:<id>:history` (git
commits, `meta.kind:"commit"`) and `repo:<id>:meta` (the conventions doc). engram's existing
memory engine carries all of this with no parallel system.

The implementation is a **three-crate cargo workspace**:

- `crates/engram` — the core service (HTTP API, store, ingest, retrieve, embed, tree, llm,
  conventions, treesit, vault).
- `crates/engram-index` — the repo indexer CLI (`index` / `reindex` / `index-history` /
  `install-hook`).
- `crates/engram-mcp` — the MCP server exposing the agent tools over stdio and HTTP.

---

## 2. The two nested loops

The closed loop is really two loops at different cadences.

### Query loop (per task — many times per session)
Agent hits a question → calls an MCP tool → engram retrieves → agent writes better code. This is
the **read** path. All six tools are live: `search_code` (semantic code search),
`get_architecture` / `get_module` (architecture digests), `why` / `find_symbol` (history +
symbols), `get_conventions` (conventions).

### Freshness loop (per commit — the feedback arc)
Someone — often the agent itself — commits → a `post-commit` hook runs `reindex` → engram
updates → the knowledge the agent queries is current for the next task. **This arc is what makes
the loop "closed":** the agent's own output re-enters the knowledge base automatically.

The headline success criterion (spec §13.7) sits on top: *with* these tools the agent answers a
fixed question suite better than *without*. That is the operational definition of "perform
better."

---

## 3. The arcs and their status — all closed

| Arc | Segment | Status |
|-----|---------|--------|
| **Repo → engram** | code ingest HTTP path (`POST /docs`, `meta.kind:"file"`) | ✅ done |
| | `engram-index index` — full walk + per-file POST | ✅ done |
| | `engram-index index-history` — git commits → `repo:<id>:history` | ✅ done |
| **Inside engram** | store (single-writer SQLite) + chunk-level code search | ✅ done |
| | `POST /v1/:ns/code/search` HTTP endpoint | ✅ done |
| | code-tuned consolidation (module/global trees) | ✅ done (gated by `ENGRAM_CONSOLIDATE_CODE`) |
| | conventions extraction → `repo:<id>:meta` | ✅ done |
| **Engram → agent** | `search_code` MCP tool | ✅ done |
| | `get_architecture` / `get_module` / `why` / `find_symbol` / `get_conventions` | ✅ done |
| **Agent → repo (feedback)** | `reindex` (git-diff incremental) + `install-hook` | ✅ done |

The entire loop — ingest in, store, consolidate, search/digests/history/conventions out, and the
post-commit feedback arc — is wired and tested (168 tests, `clippy` + `fmt` clean).

---

## 4. What runs the loop

The minimum viable loop is the indexer plus the MCP server over the core endpoints; both ship:

- **`engram-index` CLI** closes *two* arcs at once:
  - **write-in:** `index <path> --namespace repo:<id>` walks the repo and POSTs each file;
    `index-history` ingests git commits into `repo:<id>:history`.
  - **feedback:** `reindex` (git-diff → POST/DELETE) + `install-hook` (post-commit) keep it
    current automatically.
- **`engram-mcp` server** closes the **read-out** arc: six MCP tools an agent actually calls,
  served over **stdio** (default, newline-delimited JSON-RPC 2.0) **or HTTP** (set
  `ENGRAM_MCP_HTTP=addr` to serve HTTP JSON-RPC instead).

```
engram-index index .          # repo → engram (one time)
engram-index index-history .  # git commits → repo:<id>:history (powers `why`)
engram-index install-hook .   # arm the feedback arc
# … agent works …
agent calls search_code("…")  # engram → agent
agent commits                 # post-commit hook → reindex → engram current again
```

---

## 5. The six MCP tools

All six are exposed by `engram-mcp` over stdio and HTTP, each backed by an HTTP endpoint on the
core service:

| MCP tool | What it answers | Backing endpoint |
|----------|-----------------|------------------|
| `search_code(query)` | semantic code search → ranked `path:line` + snippet | `POST /v1/:ns/code/search` |
| `get_architecture(query)` | the global architecture digest | `POST /v1/:ns/code/architecture` |
| `get_module(path, query)` | a directory's module digest | `POST /v1/:ns/code/module` |
| `why(query)` | the git-history commits that explain a change | `POST /v1/:ns/query` (over `:history`) |
| `find_symbol(name)` | where a symbol is defined/used (`sym:` entity) | `POST /v1/:ns/code/search` |
| `get_conventions()` | the repo's conventions doc | `GET /v1/:ns/conventions` |

(`conventions/rebuild` and `POST /v1/:ns/tree` round out the HTTP surface;
`/conventions/rebuild` regenerates the `:meta` doc, `/tree` drives the digest BFS.)

### Retrieval shapes
- **`search_code`** is chunk-level: vector cosine + keyword + a **path-type ranking prior** that
  down-weights docs/tests/config so source ranks first. Returns `path:line` + snippet.
- **digests** (`get_architecture` / `get_module`) BFS the consolidation trees; a query-less call
  returns the top digests.
- **`query`** (prose / `why`) is the hybrid scorer with the entity graph.

---

## 6. Ingest: code-mode dispatch (hot path)

`POST /docs` with `meta.kind:"file"` dispatches into code mode:

1. **tree-sitter function/type-boundary chunking** (`src/treesit.rs`; Rust / Python / JS / TS /
   TSX / Go) — isolates each definition into its own chunk. Falls back to the heuristic
   line/symbol chunker (`chunk_code`) on unsupported languages or parse failure.
2. **AST symbol extraction** for accurate `sym:` entities, plus a `path:` entity.
3. **embed off-lock** → atomic `commit_ingest`.

The path is token-aware, CJK-safe, and per-chunk-tolerant: a bad chunk is skipped and the rest of
the file still ingests.

---

## 7. Consolidation (cold path)

`std::thread` workers + a stale-flush sweeper run `process_doc`, which fans **code** chunks into a
directory-keyed **module** tree + a **global** tree + **topic** trees keyed on `sym:` / `import:`
entities; **prose** uses the author-keyed source tree. `seal_cascade` summarizes via the LLM
gateway with a deterministic fallback so a cascade never aborts.

Code consolidation is gated by **`ENGRAM_CONSOLIDATE_CODE` (default OFF)**: `search_code` reads
chunks directly, not trees, so the digest trees are unread until the digest tools
(`get_architecture` / `get_module`) are exercised. Turn the gate on to populate code-tuned
digests; the sweeper then keeps them fresh.

---

## 8. End-to-end walkthrough

1. **Onboard a repo.** `engram-index index . --namespace repo:engram` → the indexer walks files
   (respecting `.gitignore` + denylist), POSTs each as `{meta.kind:"file"}`; engram code-mode
   chunks/embeds/extracts and enqueues consolidation. `index-history .` ingests commits.
2. **Arm the feedback arc.** `engram-index install-hook .` writes a `post-commit` hook.
3. **Agent works a task.** Its MCP client calls `search_code("where is the write lock taken")` →
   `engram-mcp` → `POST /v1/repo:engram/code/search` → ranked `path:line` + snippet → the agent
   jumps straight to the source instead of grepping. `why(...)` surfaces the commit rationale;
   `get_conventions()` returns the repo's coding norms.
4. **Agent commits.** The hook fires `reindex`: `git diff lastSha..HEAD --name-status -M` →
   `POST` for added/modified, `DELETE …/by-key` for removed, advance the SHA marker.
5. **Next task sees the change.** The knowledge base already reflects the agent's own commit —
   the loop is closed and self-maintaining.

---

## 9. Embedder & LLM

- **`GatewayEmbedder`** routes embeddings through the litellm gateway (production model
  `mxbai-embed-large`, dim 1024). An optional **`FallbackEmbedder`** (`ENGRAM_EMBED_FALLBACK`)
  wraps it with a local `OllamaEmbedder`; its `signature()` delegates to the primary so a failover
  never orphans chunks.
- **LLM summaries** route to `deepseek-chat` via the same gateway.

Key env vars (all in `src/config.rs`): `ENGRAM_DB`, `ENGRAM_BIND`, `ENGRAM_TOKEN`,
`ENGRAM_GATEWAY_URL`, `ENGRAM_GATEWAY_KEY`, `ENGRAM_EMBED_MODEL`, `ENGRAM_EMBED_DIM`,
`ENGRAM_EMBED_TIMEOUT_SECS`, `ENGRAM_EMBED_FALLBACK`, `ENGRAM_CONSOLIDATE_CODE`,
`ENGRAM_CODE_SYMBOL_SPLIT` (default true), `ENGRAM_CODE_PATH_PRIOR` (default true),
`ENGRAM_CODE_TREE_SITTER` (default true), `ENGRAM_MCP_HTTP` (unset → stdio), `ENGRAM_LLM_MODEL`.

---

## 10. Live recall (validation)

`eval/validate.py` measures ingest-rate + recall@1/5/10 + line-recall + hard negatives over
`eval/agentmemory_gold.json` (35 labeled NL→file queries, ≤2 per file). Live production numbers on
the tree-sitter config:

| Metric | Value |
|--------|-------|
| ingest success | 0.998 |
| recall@1 | 0.543 |
| recall@5 | 0.771 |
| recall@10 | 0.914 |
| line-recall@10 | 0.771 |

**The recall journey:** 0.533 (baseline) → 0.800 (Phase F) → tree-sitter, which lifts precision
(recall@1), coverage (recall@10 = 32/35 gold files in top-10), and line accuracy. recall@5 0.771
is a small, accepted trade for those gains (see `eval/RESULTS.md` for the full per-phase analysis,
including the tested-and-rejected best-chunk-per-file dedup).

---

## 11. Caveats carried into the loop

- **Partial erasure.** `reindex` delete clears live chunks/entities/unsealed leaves only; text
  already folded into **sealed** summaries (and the write-only vault) persists.
- **Embedding-model lock.** Changing the embed model/dim orphans existing chunks (signature
  gating) — a repo must be re-indexed after such a change. `FallbackEmbedder` avoids this by
  delegating its `signature()` to the primary.
- **Digest lag.** Sealed digests are immutable, so architecture/module digests trail recent edits
  until they re-seal; `search_code` is always current (chunks re-ingest immediately).
- **Auth & isolation.** One shared Bearer token, namespace = path string; no per-namespace
  authz. Each repo is its own namespace by convention (`repo:<id>`).
- **Pending live-quality bars.** The digest and history tools are code-complete and tested, but
  their *live* quality bars — non-fallback (real-LLM) digests, and `why` ≥ 0.7 — need a
  cost-bearing consolidation / history-ingest run before they're certified; these are tracked as
  pending-live in the roadmap.
