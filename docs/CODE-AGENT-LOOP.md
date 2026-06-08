# engram — The Coding-Agent Knowledge Loop

How a coding agent gets *and stays* smarter about a repository through engram. This document
describes the **closed loop** the repo-knowledge work is building toward: what it is, which
parts are wired today, and what closes it. For build order see `docs/ROADMAP.md`; for the
system's internal layering see `docs/STRUCTURE.md`; for the full design see the spec
(`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`).

---

## 1. What "closed loop" means

The goal is a **self-updating** knowledge loop where a coding agent both *reads from* and
*writes back into* the same memory. The agent queries engram to write better code; the agent's
commits feed back into engram so the next query is current — with no manual re-indexing.

```
        ┌──────────────────────────────────────────────────────┐
        │                                                      │
        ▼                                                      │
   ┌─────────┐   index / reindex   ┌──────────┐   search_code  │
   │  REPO   │ ──────────────────▶ │  ENGRAM  │ ─────────────▶ ┌─────────┐
   │ (files, │   (HTTP POST/DEL)   │ store +  │  why / get_*   │  CODE   │
   │ commits)│                     │ consolid.│ ◀───────────── │  AGENT  │
   └─────────┘                     └──────────┘   (MCP tools)  └─────────┘
        ▲                                                          │
        │                  agent commits code                     │
        └─────────────────────────────────────────────────────────┘
                      post-commit hook → reindex
```

Mapping that makes it work: **repo = namespace, file = document, identifier = entity.** engram's
existing memory engine carries code knowledge with no parallel system.

---

## 2. The two nested loops

The closed loop is really two loops at different cadences.

### Query loop (per task — many times per session)
Agent hits a question → calls an MCP tool → engram retrieves → agent writes better code. This is
the **read** path. Example tools (by phase): `search_code` (semantic code search), later
`get_architecture` / `get_module` (architecture map), `why` / `find_symbol` (history),
`get_conventions` (conventions).

### Freshness loop (per commit — the feedback arc)
Someone — often the agent itself — commits → a `post-commit` hook runs `reindex` → engram
updates → the knowledge the agent queries is current for the next task. **This arc is what makes
the loop "closed":** the agent's own output re-enters the knowledge base automatically.

The headline success criterion (spec §13.7) sits on top: *with* these tools the agent answers a
fixed question suite better than *without*. That is the operational definition of "perform
better."

---

## 3. The arcs and their status

| Arc | Segment | Status |
|-----|---------|--------|
| **Repo → engram** | code ingest HTTP path (`POST /docs`, `meta.kind:"file"`) | ✅ done |
| | `engram-index index` — full walk + per-file POST | ❌ stub — **Phase 1b** |
| **Inside engram** | store (single-writer SQLite) + chunk-level search function | ✅ done |
| | `POST /v1/:ns/code/search` HTTP endpoint | ✅ done |
| | architecture digests (code-tuned module/global trees) | ⚠️ trees populate, but run the *prose* fanout; code digests are **Phase 2** |
| **Engram → agent** | `search_code` MCP tool | ❌ stub — **Phase 1c** |
| | `get_architecture` / `why` / `find_symbol` / `get_conventions` | ❌ later phases |
| **Agent → repo (feedback)** | `reindex` (git-diff incremental) + `install-hook` | ❌ stub — **Phase 1b** |

**Read:** the **server/core arc is fully closed** — ingest in, store, search out, all over HTTP
and tested. The two *open* arcs are both **client-side**, living in the two stub crates.

---

## 4. What closes the loop

**Minimum viable closed loop = Phase 1b + Phase 1c** (the core endpoints they depend on already
exist):

- **Phase 1b — `engram-index` CLI** closes *two* arcs at once:
  - **write-in:** `index <path> --namespace repo:<id>` walks the repo and POSTs each file.
  - **feedback:** `reindex` (git-diff → POST/DELETE) + `install-hook` (post-commit) keep it
    current automatically.
- **Phase 1c — `engram-mcp` server** closes the **read-out** arc: a `search_code` MCP tool an
  agent actually calls (stdio + HTTP transport).

With those two plus the existing `/code/search` endpoint, the full cycle runs end-to-end:

```
engram-index index .          # repo → engram (one time)
engram-index install-hook .   # arm the feedback arc
# … agent works …
agent calls search_code("…")  # engram → agent
agent commits                 # post-commit hook → reindex → engram current again
```

At that point the agent has the first knowledge type — **semantic code search** — flowing
through a self-refreshing loop.

---

## 5. Later phases widen the loop (they don't close it)

Once 1b+1c close the loop, the remaining phases push **richer knowledge through the same already-
closed pipes** — same ingest, same store, same MCP transport, more valuable answers per turn:

- **Phase 2 — Architecture map:** code-tuned consolidation (module/global digests) +
  `get_architecture` / `get_module`. *(Until then, code chunks still consolidate, but into the
  prose-shaped Source/Global/Topic trees — usable per-file/per-symbol summaries, not a designed
  architecture digest. `search_code` is unaffected: it reads chunks directly, not trees.)*
- **Phase 3a — History / rationale:** git-history ingest + `why` + `find_symbol`.
- **Phase 3b — Conventions:** conventions extraction + `get_conventions`.
- **Phase 4 — Depth:** tree-sitter function-boundary chunking, richer symbols, reseal-stale
  digests, more languages.

---

## 6. End-to-end walkthrough (target state)

1. **Onboard a repo.** `engram-index index . --namespace repo:engram` → the indexer walks files
   (respecting `.gitignore` + denylist), POSTs each as `{meta.kind:"file"}`; engram code-mode
   chunks/embeds/extracts and enqueues consolidation.
2. **Arm the feedback arc.** `engram-index install-hook .` writes a `post-commit` hook.
3. **Agent works a task.** Its MCP client calls `search_code("where is the write lock taken")` →
   `engram-mcp` → `POST /v1/repo:engram/code/search` → ranked `path:line` + snippet → the agent
   jumps to `store/mod.rs:43` instead of grepping.
4. **Agent commits.** The hook fires `reindex`: `git diff lastSha..HEAD --name-status -M` →
   `POST` for added/modified, `DELETE …/by-key` for removed, advance the SHA marker.
5. **Next task sees the change.** The knowledge base already reflects the agent's own commit —
   the loop is closed and self-maintaining.

---

## 7. Caveats carried into the loop

- **Partial erasure.** `reindex` delete clears live chunks/entities/unsealed leaves only; text
  already folded into **sealed** summaries (and the write-only vault) persists.
- **Embedding-model lock.** Changing the embed model/dim orphans existing chunks (signature
  gating) — a repo must be re-indexed after such a change.
- **Digest lag.** Sealed digests are immutable, so architecture/module digests trail recent
  edits until they re-seal; `search_code` is always current (chunks re-ingest immediately).
- **Auth & isolation.** One shared Bearer token, namespace = path string; no per-namespace
  authz. Each repo is its own namespace by convention (`repo:<id>`).
- **Symbol precision.** `sym:` extraction is regex-approximate until tree-sitter (Phase 4):
  good recall, imperfect precision.
