# Repo Knowledge for Coding Agents — Design

- **Date:** 2026-06-07
- **Status:** Draft (brainstorming → design approved; pending spec review)
- **Repo:** engram (`/mnt/x/code/engram`)
- **Related:** `ARCHITECTURE.md`, `CLAUDE.md`, `README.md`

## 1. Summary

Teach engram to **ingest and learn from a local code repository** and **serve that
knowledge to coding agents** (Claude Code, Cursor, …) over MCP. The core insight: engram
is already a general `acquire → consolidate → recall` knowledge pipeline; learning a repo
is a **mapping problem**, not a new engine. A repo becomes a namespace, files become
documents, identifiers become entities, commits become history docs, and the existing
consolidation trees auto-derive the architecture/conventions digests. Two thin new client
crates surround a **near-unchanged core** — an indexer (writes knowledge in) and an MCP
server (reads knowledge out); the only core change is bounded: two nullable columns on
`vector_chunks` plus a chunk-level retrieval mode for code search (see §6/§7.1). The work is
organized as a Cargo **workspace**, which is also the "cleaner structure" goal.

## 2. Goals / Non-goals

**Goals**
- Index any local repo and keep it fresh (on-commit incremental).
- Produce four knowledge types, maximized: semantic code search, architecture/structure map,
  conventions/patterns, history/rationale.
- Expose them to coding agents as native **MCP tools**.
- Language-agnostic baseline that works on any repo day one.
- Clean up structure as we go: split `store.rs`; isolate engine vs agent-facing surfaces.

**Non-goals (deferred)**
- Tree-sitter / LSP AST parsing and call graphs (future enhancement layer).
- Multi-language deep parsing; per-language symbol precision.
- Editing code or write-back; agents only *read* knowledge.
- Cross-repo / org-wide knowledge graphs.
- True erasure of history from sealed summaries (see Risks).

## 3. Principle concept

engram learns a codebase the way the memory design models a mind:

```
 PERCEIVE → ENCODE → CONSOLIDATE → RECALL
 read repo  chunk+    file→module   agent asks
 + git log  embed +   →repo digests via MCP
            entities  (summary trees)
```

Principles:
1. **A repo is a namespace; a file is a document; an identifier is an entity.** Reuse the
   existing primitives; minimal new schema.
2. **Understanding is derived, not authored.** Architecture/conventions emerge from the
   consolidation trees, not hand-writing. Learning = consolidation.
3. **The four knowledge types are four queries, not four subsystems.** All ride
   `query` / `drill_down` / `recall` over code docs vs history docs.
4. **engram stays a backend; agents meet it through a thin MCP skin.** Engine and
   agent-facing surface are cleanly separated (and, via the workspace, compiler-enforced).

## 4. Decisions (from brainstorming)

| Decision | Choice |
|---|---|
| Sequencing | Feature-first; fold in targeted cleanup |
| Knowledge types | All four, maximized |
| Delivery | MCP server (HTTP API as transport underneath) |
| Freshness | On-commit incremental (full index once, then git-diff) |
| Language scope | Language-agnostic baseline; tree-sitter later |
| Storage approach | A — reuse the memory model; **no new tables**, but one bounded core change: 2 nullable cols on `vector_chunks` + a chunk-level retrieval mode for code search (§6/§7.1) |
| Structure | Cargo **workspace**: core + two client crates |
| Pipeline locus | Code chunking/extraction runs **server-side**; clients are thin |

## 5. Architecture

```
            engram-index            (writes INTO memory)
          read repo + git → POST /docs, DELETE …/by-key
                          │
                          ▼
   ┌───────────────────────────────────────────┐
   │ engram (core, single writer)               │
   │  ingest (+ code-mode)  store/ (split)       │
   │  tree (+ code fanout)  retrieval  HTTP API  │
   │  jobs/workers → consolidation               │
   └───────────────────────────────────────────┘
                          ▲
                          │
            engram-mcp               (reads OUT of memory)
          MCP tools → query / drill_down / recall
```

**Workspace layout**
```
engram/                      [workspace] virtual manifest
  Cargo.lock  target/        one lockfile, one build dir
  crates/
    engram/      lib + server  (axum, rusqlite, tokio "full")
      src/store/  (split)   ingest  tree  retrieve  …
    engram-index/ CLI client  (reqwest, git, ignore, clap)
    engram-mcp/   MCP server  (reqwest, mcp/json-rpc)
    engram-types/ (only if needed) shared request/response structs
```

`engram-types` is created **only if** request/response struct duplication between the two
clients becomes painful; until then each uses minimal local structs / `serde_json::Value`.
Not tied to a phase.

Rationale: the two surfaces are **HTTP clients** with light dependencies (no SQLite/axum).
Separate crates keep client builds fast and make "reach memory only through its API" a
compile-time rule. New surfaces later (web UI, more CLIs) are just new members.

## 6. The repo → engram mapping

**Namespaces (per repo):**
- `repo:<id>` — code docs.
- `repo:<id>:history` — commit docs (kept separate so commit text doesn't pollute code
  search; `query` has no meta-filter today, so a separate namespace is the no-core-change
  option — a single-ns `meta.kind` filter is the considered alternative, deferred).
- `repo:<id>:meta` — generated digests (the conventions doc, plus any future repo-level
  digests), kept out of code search.
- **Index-state marker** (last-indexed git sha) is stored **client-side** by `engram-index`
  (e.g. `.git/engram/last-sha`), *not* as a doc in engram — so it can never pollute code
  search or consolidation. If absent/unreadable, the indexer does a full re-index (idempotent).

**Code file → document** (`POST /docs`, code-mode):
- `key` = repo-relative path, `title` = path, `content` = file text, `author` = `"code"`.
- `meta` = `{ kind: "file", lang, sha, size, … }` (uses the existing opaque `meta` field).
- Re-ingest replaces the file's chunks via the existing upsert-by-`(namespace,key)` (stable
  `document_id`).

**Code chunking (server-side, code-mode):** split on logical boundaries (blank lines /
brace-depth heuristics), respect line boundaries, cap **~1500 chars (~40–60 lines; tunable
via config)**, language-agnostic. The chunker returns each chunk **with its line range**
`(start, end)`, for citeable `path:line` results. (Tree-sitter function-boundary chunking is a
later enhancement.)

> **Schema change required (line ranges).** Today `vector_chunks` has no line/meta columns
> (`src/store.rs`), `ChunkRow` has no line fields, and `retrieve::query` returns whole-doc
> `Hit`s — not chunk snippets. To deliver `path:line` code search we must: add
> `line_start`/`line_end` columns to `vector_chunks` (a **column addition — still no new
> tables**, approach A holds); thread line ranges through the code chunker → `ChunkRow` →
> `commit_ingest`; and have code search return **chunk-level** results (matching chunk text +
> `path:line`) instead of the full document. This is the one place the feature reaches into
> the core schema + retrieval.

**Code entity extraction (server-side, code-mode):** regex/heuristic → entities:
- `sym:<ident>` — definitions / notable identifiers (generic `fn`/`def`/`class`/`func`/`type`
  patterns; case-preserving, unlike the prose extractor).
- `import:<module>` — import/use/require/include targets.
- `path:<relpath>` — the file's own path + referenced paths.
These populate `chunk_entities`, powering the graph signal ("what relates to X", "where used").

**Commit → history document** (`POST /docs` to the history ns):
- `key` = `commit:<sha>`, `content` = subject + body + changed-file list, `author` = commit
  author, `meta` = `{ kind: "commit", sha, ts, files }`. Entity-linked to touched `path:`.

**Consolidation → digests (code fanout):** for code docs, `process_doc` fans into:
- **module** tree (new `tree_kind`), `tree_key` = directory → per-module summaries. This
  **replaces** the prose `source` tree (which keys by author — meaningless for code, where
  `author="code"`); `source` is simply unused in code-mode.
- **global** tree → whole-repo architecture digest (same `tree_kind` as prose).
- **topic** tree, `tree_key` = `sym:`/`import:` entities **only** → concept summaries.
  (`path:` entities still feed the retrieval graph signal but do **not** spawn topic trees, to
  avoid a per-file topic explosion.)
The fanout set is chosen from `meta.kind`; everything downstream (seal cascade, LLM summarize,
drill-down) is unchanged.

**Conventions digest:** a dedicated pass (separate from per-doc fanout). **Inputs:** the
`module` digests + the `global` digest + convention/config files (`.editorconfig`,
`rustfmt.toml`/clippy config, `CONTRIBUTING`, linter configs). **Output:** a *structured* list
of conventions — each a short rule + evidence (`path` refs), not free prose — stored as one
doc in `repo:<id>:meta` and served by `get_conventions`. **Refresh:** after a full index and
on a cadence (reuse the existing sweeper), **not** on every commit.

## 7. Components

### 7.1 Core changes (inside `crates/engram`)

**`store/` split** — refactor `store.rs` (~1031 LOC) into a directory module: `mod` (Store,
open, pool/writer), `docs`, `chunks`, `entities`, `jobs`, `trees`. **No behavior change**;
pure reorganization to make room for code-mode and improve navigability.

**Code-mode ingest** — `ingest_document` gains a branch on the incoming doc's `meta.kind`
(`"file"` → code-mode; otherwise prose-mode = today's unchanged behavior). Code-mode uses a
new code chunker + a new `extract_code_entities` (`sym:`/`import:`/`path:`); both share the
one `commit_ingest` path. (Today `chunk()` / `extract_entities()` are prose-only and called
unconditionally — this adds the dispatch.)

**Chunk line ranges (schema)** — add **nullable** `line_start`/`line_end` to `vector_chunks`
**and** `ChunkRow`; the code chunker emits line ranges, `commit_ingest` persists them, and
code search returns chunk-level snippets with `path:line`. **Migration:** nullable, no default;
pre-existing prose chunks stay `NULL` (no backfill, no forced re-index — prose retrieval is
unaffected). Column addition only — no new tables.

**Code consolidation mapping** — `tree::process_doc` (today always fans `source`/`global`/
`topic`) gains a branch on the doc's `meta.kind`: code files fan to **`module`** (new
`tree_kind`, key = directory; replaces author-keyed `source`) / `global` / `topic` (key =
`sym:`/`import:` only). It already loads the doc, so `meta` is in hand. The conventions pass
(separate from per-doc fanout) lands in Phase 3b.

### 7.2 `crates/engram-index` (new CLI, HTTP client)

Walks the repo and pushes to a running engram. Runs where the repo lives (the server need
not have the repo on disk).
- `index <path> --namespace repo:<id>` — full index: walk (respect `.gitignore`, skip
  binary/oversized/vendored), `POST /docs` per file; `git log` → `POST` history docs; record
  last sha.
- `reindex <path>` — incremental: `git diff <lastSha>..HEAD --name-status -M` (rename
  detection on); per status — `A`/`M` → `POST /docs`; `D` → `DELETE …/docs/by-key/<path>`;
  `R` → delete old key + `POST` new; new commits → history; advance the client-side marker.
  (`DELETE` clears live chunks only; sealed-summary/vault residue persists — see §9/§12.)
- `install-hook <path>` — write a `post-commit` hook that runs `reindex`.
- Deps: `reqwest`, a git interface, `ignore` (gitignore walk), `clap`. (git2-vs-shell decided
  in writing-plans.)

### 7.3 `crates/engram-mcp` (new MCP server, HTTP client)

Exposes MCP tools, each a thin translation to an existing engram endpoint.

| Tool | Maps to | Returns |
|---|---|---|
| `search_code(query, limit?)` | `query` (code ns)¹ | ranked `path:line` + snippet + scores |
| `get_architecture(depth?)` | `drill_down` global | repo digest |
| `get_module(path, depth?)` | `drill_down` module | one directory's digest |
| `get_conventions()` | conventions digest doc | conventions/gotchas |
| `why(query, limit?)` | `query` (history ns) | rationale from commits |
| `find_symbol(name)` | `query` via `sym:` graph | where defined/used |

¹ `search_code` depends on the chunk line-range change (§7.1): code search returns
chunk-level hits (snippet + `path:line`), not the whole-doc `Hit` that `query` returns today.

- Transport: stdio (local agents) + streamable HTTP.
- Config: engram base URL, token, repo namespace.
- Deps: `reqwest`, an MCP/JSON-RPC library.

## 8. Data flow

**Full index** — `engram-index index` → walk → per file `POST /docs {meta.kind:file}` →
server code-mode chunk/embed/extract → `commit_ingest` (atomic) → enqueue job; `git log` →
history docs; workers consolidate → module/repo digests; conventions pass.

**Incremental (post-commit hook)** — `reindex` → `git diff lastSha..HEAD`: add/modify →
`POST /docs` (re-ingest replaces chunks, re-enqueues); delete → `DELETE …/by-key`; new
commits → history; advance marker; workers re-consolidate.

**Agent read** — agent → MCP tool → `engram-mcp` → HTTP → engram retrieval → shaped result
→ agent.

## 9. Error handling & edge cases

- **Binary / oversized / vendored files:** skip via extension + size cap + `.gitignore` (and
  a small built-in denylist: `target/`, `node_modules/`, `.git/`). **Lockfiles excluded by
  default** (`Cargo.lock`, `package-lock.json`, …); opt-in flag to include.
- **Secrets:** `.gitignore` already excludes `.env`; document that engram indexes whatever is
  tracked — do not point it at secrets. (Optional later: a denylist/redaction pass.)
- **Deleted/renamed files:** `git diff --name-status -M` → `DELETE …/by-key` for `D`; rename
  (`R`) = delete old key + add new. `DELETE` clears live chunks/entities/unsealed leaves only —
  text already folded into **sealed** summaries (and the vault) persists (partial erasure; §12).
- **Incremental correctness:** the last-indexed sha marker is the source of truth; if missing
  or the diff fails, fall back to a full re-index (idempotent — upsert-by-key).
- **Sealed-summary lag:** sealed digests are immutable, so a module/repo digest can trail
  recent edits until it re-seals. `search_code` is always current (chunks re-ingest
  immediately). Accepted for v1; a future "reseal stale digests" pass can tighten it.
- **Single writer:** indexer and MCP are HTTP clients; they never open the DB. Prevents
  multi-writer SQLite contention. Large initial indexes should batch/pace `POST`s.
- **Service down / partial index:** indexer retries with backoff; a failed file logs and is
  retried next run; consolidation already degrades to deterministic fallback summaries.
- **Auth & isolation:** all calls use the existing Bearer token; each repo is its own
  namespace. (Note: engram has one shared token and no per-namespace authz — unchanged.)
- **Embedding model change:** changing the embed model/dim orphans chunks (existing
  signature-gating). A repo must be re-indexed after an embed-model change (document this).

## 10. Testing

- **Unit (core):** code chunker (line ranges, CJK-safe, oversized split), code entity
  extractor (`sym:`/`import:`/`path:`, case-preserving), mode selection by `meta.kind`. Reuse
  `HashEmbedder` + `FakeChatClient`/`NullAuditSink`; temp SQLite.
- **Consolidation (core):** code fanout into module/global/topic with shrunk seal gates
  (deterministic), conventions pass with `FakeChatClient`.
- **`engram-index`:** a fixture mini-repo (a temp git repo) → assert the right `POST`/`DELETE`
  calls (against a mock or a real test engram on `127.0.0.1:0`); incremental diff handling
  incl. deletes/renames; full-reindex fallback.
- **`engram-mcp`:** each tool maps to the expected endpoint and shapes results correctly,
  driven against a test engram seeded with a tiny repo.
- **Dogfood / acceptance:** index engram itself; run the per-knowledge-type acceptance bars in
  §13 against the result.

## 11. Phasing (build order)

- **Phase 0 — Workspace + `store/` split.** Convert to a workspace; split `store.rs`. No
  behavior change; everything still green. (The cleanup, first, so later work lands cleanly.)
- **Phase 1 — MVP: searchable code** *(knowledge: semantic code search)*. Chunk line-range
  schema change + code-mode ingest (incl. `sym:`/`import:`/`path:` extraction) + `engram-index`
  (full + incremental + hook) + `engram-mcp` with `search_code` (chunk-level `path:line`).
  Entities are extracted here; their dedicated consumers come later (topic fanout P2,
  `find_symbol` P3a).
- **Phase 2 — Architecture digests** *(knowledge: architecture map)*. Code consolidation
  mapping (`module`/`global`, topic on `sym:`/`import:`) + `get_architecture` / `get_module`.
- **Phase 3a — History/rationale** *(knowledge: history)*. History ingest + `why` +
  `find_symbol`. No schema work — de-risked so it lands even if conventions proves hard.
- **Phase 3b — Conventions** *(knowledge: conventions)*. The conventions pass +
  `get_conventions`.
- **Phase 4 — Depth (later).** Tree-sitter function-boundary chunking + richer symbols;
  reseal-stale-digests pass; more languages.

Each phase is independently useful and shippable. (Phase 0 = cleanup only, delivers no
knowledge type.)

## 12. Risks / open questions

- **Digest freshness vs immutability** — sealed summaries lag edits; revisit if v1 feedback
  shows stale architecture digests hurt agents.
- **"Forget" completeness** — a deleted file's text can persist in sealed summaries and the
  write-only vault; true erasure needs a future reseal/GC (noted in `ARCHITECTURE.md` §12).
- **Chunking quality without ASTs** — heuristic code chunking may split awkwardly; acceptable
  for retrieval, improved by Phase 4.
- **Consolidation cost on large repos** — many files → many jobs/LLM calls; pace indexing and
  tune seal gates; consolidation is already async + fallback-safe.
- **MCP library choice** — pick a maintained Rust MCP/JSON-RPC crate vs hand-rolling stdio
  JSON-RPC (decide in writing-plans).
- **Symbol precision** — `find_symbol`/`sym:` extraction is **regex-approximate** (no AST):
  expect good recall, imperfect precision until tree-sitter (Phase 4). "Maximized" here means
  *within the language-agnostic baseline*, not full symbol resolution.
- **History blended with code** — `search_code` (code ns) and `why` (history ns) link only via
  shared `path:` entities; an agent can't get code + rationale in one query. If tighter
  blending matters, the deferred single-ns `meta.kind` filter would enable it.

## 13. Success criteria

**Operational**
1. `engram-index index .` then on-commit `reindex` keeps a repo current with no manual steps.
2. The workspace cleanly separates core from the two client surfaces; `store.rs` is no longer
   a single 1000+ line file; existing tests stay green.

**Per-knowledge-type acceptance bars** (measured on the dogfood repo — engram itself — each
against a small hand-labeled fixture set built during implementation). These operationalize
"maximized":
3. **Semantic code search** — recall@10 ≥ 0.8 on ~20 labeled natural-language queries → the
   file/region a human picks.
4. **Architecture** — every top-level source directory has a non-fallback (LLM, not concat)
   `module` digest, and the `global` digest exists and names the major subsystems.
5. **Conventions** — ≥ ~10 distinct conventions extracted, each spot-checked correct (no
   fabricated rules).
6. **History/rationale** — `why` returns the correct originating commit for ≥ 0.7 of ~15
   labeled code regions.

**Headline**
7. With-vs-without: on a fixed suite of ~10 representative engram questions, an agent using the
   MCP tools scores higher on a simple correctness rubric than the same agent without them
   (this defines the otherwise-vague "performs better").
