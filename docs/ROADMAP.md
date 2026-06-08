# engram — Repo-Knowledge Roadmap

Build order and scope for turning engram into a repo-knowledge service for coding agents.
Source of truth for *design* is the spec
(`docs/superpowers/specs/2026-06-07-repo-knowledge-for-coding-agents-design.md`); this file
tracks *status and per-phase scope* so the remaining work is documented in one place.

Each phase is independently shippable. Status legend: ✅ done · ◻️ planned.

| Phase | Knowledge type | Status |
|-------|----------------|--------|
| 0 — Workspace + `store/` split | — (cleanup) | ✅ merged `28e3c0e` |
| 1a — Core code ingest & search | semantic code search (engine) | ✅ merged `63b0751` |
| 1b — `engram-index` CLI | semantic code search (write side) | ◻️ |
| 1c — `engram-mcp` server | semantic code search (read side) | ◻️ |
| 2 — Architecture digests | architecture map | ◻️ |
| 3a — History / rationale | history | ◻️ |
| 3b — Conventions | conventions | ◻️ |
| 4 — Depth (tree-sitter, reseal) | (quality) | ◻️ |

---

## Done

### Phase 0 — Workspace + `store/` split ✅
Converted the crate to a Cargo workspace (`crates/engram`, stub `crates/engram-index`,
stub `crates/engram-mcp`) and split the 1031-line `store.rs` into
`store/{mod,docs,chunks,entities,jobs,trees}.rs`. No behavior change; tests green. This was
the cleanup-first step so later work lands cleanly.

### Phase 1a — Core code ingest & search ✅
The **engine half** of searchable code, all inside `crates/engram`:
- `vector_chunks` gained nullable `line_start`/`line_end`; `commit_ingest` threads a
  `line_ranges` slice (prose stores NULL).
- `chunk_code` — line-aware chunker (~1500-char cap, never splits a line, 1-based inclusive
  ranges).
- `extract_code_entities` — regex `sym:` / `import:` (case-preserving, sorted + deduped).
- `ingest_document` dispatch on `meta.kind == "file"` → code-mode (chunk_code + code entities
  + `path:<key>` + line ranges); else the unchanged prose path.
- `search_code` → `CodeHit { path, line_start, line_end, snippet, score, vector, keyword }`
  via `code_chunks_for_namespace` (joins `memory_docs`, code-only filter on
  `line_start IS NOT NULL`).

Reachable via the library/HTTP only — no CLI or MCP surface yet (that's 1b/1c).

---

## Following phases

### Phase 1b — `engram-index` CLI (write side) ◻️
**Goal:** a client that walks a local repo and pushes it into a running engram over HTTP, then
keeps it current on every commit. Runs where the repo lives; never opens the DB (preserves the
single-writer invariant).

**Crate:** `crates/engram-index` (currently a stub). Deps: `reqwest`, `ignore` (gitignore
walk), `clap`, and a git interface — **git2 crate vs. shelling out to `git`** is a
writing-plans decision.

**Subcommands:**
- `index <path> --namespace repo:<id>` — full index. Walk respecting `.gitignore` + built-in
  denylist (`target/`, `node_modules/`, `.git/`), skipping binary / oversized / vendored files.
  **Lockfiles excluded by default** (`Cargo.lock`, `package-lock.json`, …), opt-in flag to
  include. `POST /docs` per file with `meta.kind:"file"` and the file path as the doc key.
  Record the last-indexed git SHA **client-side** at `.git/engram/last-sha`.
- `reindex <path>` — incremental. `git diff <lastSha>..HEAD --name-status -M` (rename detection
  on); per status: `A`/`M` → `POST /docs`; `D` → `DELETE …/docs/by-key/<path>`; `R` → delete old
  key + `POST` new. Advance the marker. If the marker is missing or the diff fails → **fall back
  to a full re-index** (idempotent upsert-by-key).
- `install-hook <path>` — write a `post-commit` hook that runs `reindex`.

**Key behaviors & gotchas:**
- **Partial erasure:** `DELETE …/by-key` clears live chunks/entities/unsealed leaves only; text
  already folded into **sealed** summaries (and the write-only vault) persists.
- **On-disk DB migration:** the 1a `line_start`/`line_end` columns are added only via
  `CREATE TABLE IF NOT EXISTS`, so a pre-existing deployed DB lacks them. 1b is the first time a
  real repo is indexed against a deployed DB, so the migration must be handled here.
- **Pacing / resilience:** large initial indexes batch/pace POSTs; retry with backoff; a failed
  file logs and retries next run.
- **Secrets:** engram indexes whatever git tracks — `.gitignore` already excludes `.env`;
  document "do not point it at secrets."

**Testing:** a fixture mini-repo (temp git repo) → assert the right `POST`/`DELETE` calls fire
against a test engram on `127.0.0.1:0`; cover incremental deletes/renames and the
full-reindex fallback.

**Acceptance (§13.1):** `index .` then on-commit `reindex` keeps a repo current with no manual
steps.

### Phase 1c — `engram-mcp` server (read side) ◻️
**Goal:** expose engram retrieval as MCP tools so coding agents can query indexed knowledge.
Thin translation layer; each tool maps to one engram endpoint.

**Crate:** `crates/engram-mcp` (stub). Transport: stdio (local agents) + streamable HTTP.
Config: engram base URL, token, repo namespace. Deps: `reqwest` + a maintained Rust
MCP/JSON-RPC crate (vs. hand-rolled stdio JSON-RPC — writing-plans decision).

**Phase 1c tool:** `search_code(query, limit?)` → `retrieve::search_code` → ranked `path:line`
+ snippet + scores. (The remaining tools below land with their knowledge phases.)

**Testing:** drive each tool against a test engram seeded with a tiny repo; assert it maps to
the expected endpoint and shapes results correctly.

**Completes the Phase 1 MVP loop:** 1b writes, 1c reads — `index .` → agent `search_code`.

### Phase 2 — Architecture digests ◻️
**Goal:** the *architecture map* knowledge type — let agents ask "what is this repo / module?"
**Scope:** code consolidation mapping in the cold pipeline — fan code leaves into `module` and
`global` trees, plus topic trees on `sym:`/`import:` entities — and two new MCP tools:
- `get_architecture(depth?)` → `drill_down` global → repo digest naming major subsystems.
- `get_module(path, depth?)` → `drill_down` module → one directory's digest.

**Testing:** code fanout into module/global/topic with shrunk seal gates (deterministic),
`FakeChatClient` for summaries.

**Acceptance (§13.4):** every top-level source directory has a non-fallback (LLM, not concat)
`module` digest; the `global` digest exists and names the major subsystems.

### Phase 3a — History / rationale ◻️
**Goal:** the *history* knowledge type — "why does this code exist / why was it changed?"
**Scope:** ingest git history as docs (the `git log` → history-docs path deferred from 1b), plus
two MCP tools:
- `why(query, limit?)` → `query` over the history namespace → rationale from commits.
- `find_symbol(name)` → `query` via the `sym:` graph → where a symbol is defined/used.

No schema work — deliberately de-risked so it lands even if conventions (3b) proves hard.

**Acceptance (§13.6):** `why` returns the correct originating commit for ≥ 0.7 of ~15 labeled
code regions.

### Phase 3b — Conventions ◻️
**Goal:** the *conventions* knowledge type — coding patterns, idioms, gotchas.
**Scope:** a conventions extraction pass (over consolidated digests) + `get_conventions()` MCP
tool returning conventions/gotchas.

**Acceptance (§13.5):** ≥ ~10 distinct conventions extracted, each spot-checked correct (no
fabricated rules).

### Phase 4 — Depth (later) ◻️
**Goal:** raise quality past the language-agnostic regex baseline.
**Scope:** tree-sitter function-boundary chunking + richer symbol extraction (better
`find_symbol`/`sym:` precision); a "reseal stale digests" pass to tighten digest freshness;
more languages.

---

## Headline success criterion (§13.7)
With-vs-without: on a fixed suite of ~10 representative engram questions, an agent using the MCP
tools scores higher on a correctness rubric than the same agent without them. This is what
"perform better" means, and it can only be measured once 1b+1c (and ideally 2/3) are in.

## Cross-cutting risks (spec §12)
- **Digest freshness vs. immutability** — sealed summaries lag edits; `search_code` is always
  current (chunks re-ingest immediately), digests trail until re-seal.
- **"Forget" completeness** — deleted-file text can persist in sealed summaries + vault; true
  erasure needs a future reseal/GC.
- **Symbol precision** — regex-approximate until tree-sitter (Phase 4): good recall, imperfect
  precision.
- **Consolidation cost on large repos** — many files → many jobs/LLM calls; pace indexing, tune
  seal gates; consolidation is already async + fallback-safe.
