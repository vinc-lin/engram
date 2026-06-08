# engram — Structure

A layered view of the system, organized around its **four functional modules**. For the
detailed per-module reference (dependency graph, data model, config surface, dependency list)
see `ARCHITECTURE.md`; for where the system is headed see `docs/ROADMAP.md`. This document is
the *mental model*: what the layers are, what each owns, and how a request moves through them.

---

## 1. The system in one picture

engram is a single-writer memory service: **one axum HTTP API over one SQLite file**, with an
**autonomous background pipeline** that folds ingested documents into summary trees. It runs as
two concurrent planes — a synchronous **hot path** (request/response) and a threaded **cold
path** (consolidation) — that meet only at the shared store.

```
                          ┌──────────────────────────────┐
   HTTP clients  ───────▶ │  EDGE        api · main      │
                          └───────────────┬──────────────┘
                                          │
            ┌─────────────────────────────┼─────────────────────────────┐
            │                             │                             │
   ┌────────▼─────────┐         ┌─────────▼──────────┐                  │
   │  HOT LOOP        │         │  COLD LOOP         │                  │
   │  ingest          │         │  jobs   (queue)    │                  │
   │  retrieve        │         │  tree   (consolid.)│                  │
   └────────┬─────────┘         └─────────┬──────────┘                  │
            │   writes/reads              │  drains queue, writes trees  │
            └──────────────┬──────────────┘                             │
                           ▼                                            │
                    ┌─────────────┐                                     │
                    │  STORE      │  single writer + read pool (SQLite) │
                    └──────┬──────┘                                     │
                           │ used by both loops                        │
            ┌──────────────┴───────────────┐                           │
            ▼                              ▼                            ▼
   ┌─────────────────────────────────────────────────┐   ┌──────────────────┐
   │  SERVICES   embed · llm · vault                  │   │  PLUMBING        │
   │  (shared by hot + cold; do the network I/O)      │   │  config · model  │
   └─────────────────────────────────────────────────┘   │  error · lib     │
                                                          └──────────────────┘
```

**The center of gravity is STORE.** Both loops pivot on it; everything else is arranged around
keeping the one write lock fast and crash-safe.

---

## 2. The four modules

### Module A — Hot loop (the OpenHuman triad: ingest → store ← retrieve)
Synchronous request/response. Runs inside async axum handlers; because the embedder and LLM
clients are `reqwest::blocking`, any handler that embeds wraps the work in
`tokio::task::spawn_blocking`.

| File | Owns |
|------|------|
| `ingest.rs` | chunk content → embed each chunk **off-lock** → mechanical entity extraction → `commit_ingest` (atomic doc + chunks + entities + enqueue consolidation job) |
| `retrieve.rs` | `query` (hybrid vector/keyword/graph scoring), `recall` (recency decay), `drill_down` (tree BFS), `search_code` (chunk-level `path:line` hits) |

Ingest is the only writer-of-record for documents; retrieve is read-only. They communicate only
through the store (and, indirectly, through the job ingest enqueues).

### Module B — Store (the hub)
The single shared substrate. `Store` = `read: r2d2::Pool` (≤8 connections) +
`write: Arc<Mutex<Connection>>`. **All writes go through the one mutexed connection; all reads
come from the pool.** WAL mode lets readers run concurrently with the writer. `Store` is `Clone`
(clones the `Arc`s), so cold-path workers hold cheap handles.

Split by responsibility under `store/`:

| File | Owns |
|------|------|
| `store/mod.rs` | schema (6 tables, 7 indexes), connection/pool setup, shared helpers |
| `store/docs.rs` | `memory_docs` — insert, by-key get/delete (the "forget" path), listing |
| `store/chunks.rs` | `vector_chunks` — upsert, namespace/doc queries, `code_chunks_for_namespace` |
| `store/entities.rs` | `chunk_entities` — entity rows that drive graph scoring |
| `store/jobs.rs` | `post_acquire_jobs` — claim/fail/retry/requeue queue ops |
| `store/trees.rs` | `tree_nodes` + `tree_edges` — leaf append, `seal_buffer`, drill-down reads |

There are **no FK constraints**; integrity is held by wrapping every multi-statement write in a
transaction (`commit_ingest`, `seal_buffer`, `delete_chunks_for_doc`, `claim_job`).

### Module C — Cold loop (autonomous consolidation)
Background. Runs on **dedicated `std::thread` OS threads** (not tokio tasks), spawned from
`main.rs`, sharing a `stop: Arc<AtomicBool>`. Calls blocking HTTP directly (no spawn_blocking).

| File | Owns |
|------|------|
| `jobs.rs` | `JobProcessor` trait, `worker_tick` (claim-one-and-process), `spawn_workers` |
| `tree.rs` | `process_doc` fanout into **Source/Global/Topic** trees, `seal_cascade`, the stale-buffer sweeper |

A worker claims a job ingest enqueued, fans each chunk into the trees, and seals buffers when a
gate trips — summarizing via the LLM gateway, falling back to deterministic concat on any LLM
error **so a cascade never aborts**. Output (summary trees) is what `drill_down` reads back.

### Module D — Services (shared by both loops)
Owned by neither loop; both depend on them. These are where the actual network I/O lives.

| File | Owns |
|------|------|
| `embed.rs` | `Embedder` trait + `HashEmbedder` (tests), `OllamaEmbedder`, `GatewayEmbedder` (prod) |
| `llm.rs` | `ChatClient` / `AuditSink` traits, gateway clients, `summarize_audited`, fallback, test doubles |
| `vault.rs` | optional write-only Obsidian markdown mirror of docs + sealed summaries |

### Edge & plumbing (the frame around the four)
`api.rs` (router, Bearer-auth middleware, handlers) and `main.rs` (bootstrap: load config →
open store → crash-recovery requeue → build embedder + tree processor → spawn cold threads →
serve) form the **edge**. `config.rs`, `model.rs`, `error.rs`, `lib.rs` are **plumbing** used
everywhere.

---

## 3. The overall system: how a request moves through the layers

**Ingest (`POST /docs`)** — edge → hot loop → services → store, then hands off to the cold loop:
```
api::ingest_doc (spawn_blocking)
  → ingest::ingest_document
      → chunk (800-char packs, CJK-safe)
      → embed each chunk            [services: embed]   ← off-lock
      → extract entities (regex)
      → store::commit_ingest        [store, atomic]     ← write lock
            · doc + chunks + entities
            · enqueue post_acquire_job
```

**Consolidation (background)** — cold loop drains the queue the ingest just fed:
```
jobs::worker_tick (OS thread)
  → store::claim_job                [store, atomic]
  → tree::process_doc
      → append leaf into Source / Global / Topic trees
      → seal_cascade (gate trips)
          → summarize               [services: llm]     ← off-lock
          → store::seal_buffer      [store, atomic]
```

**Retrieve (`POST /query`, `/recall`, `/tree`)** — edge → hot loop → store (reads from pool):
```
api::query_docs (spawn_blocking)
  → embed query                     [services: embed]
  → retrieve::query                 [store, read pool]
      hybrid score: vector + keyword + graph
```

The **off-lock invariant** is visible in all three: embedding and summarization (network I/O)
happen *before* the write lock is taken; only the fast atomic commit holds it.

---

## 4. The overall system: beyond the core process

The core above is one crate (`crates/engram`) in a Cargo workspace. Two **client crates** sit
*outside* it and talk to it only over HTTP — they never open the SQLite file (preserving the
single-writer guarantee):

```
   ┌──────────────────┐   walks a repo, POSTs files/commits
   │  engram-index    │ ─────────────────────────────────────┐
   │  (CLI, Phase 1b) │                                       │
   └──────────────────┘                                       ▼
                                                       ┌───────────────┐
   ┌──────────────────┐   exposes MCP tools to agents  │   engram      │
   │  engram-mcp      │ ◀───────────────────────────── │   (core API)  │
   │  (MCP, Phase 1c) │   queries over HTTP             └───────────────┘
   └──────────────────┘
```

Both are currently **stubs** (`not yet implemented`). The mapping that lets the existing memory
model carry code knowledge: **repo = namespace, file = document, identifier = entity.** See
`docs/ROADMAP.md` for the build order.

---

## 5. Cross-cutting invariants (true across all layers)

1. **Off-lock I/O.** Never hold the write mutex across network I/O. Embed/summarize first, then
   atomic commit.
2. **spawn_blocking for embedding handlers.** Any async handler that embeds or calls the gateway
   must use `tokio::task::spawn_blocking`, or it stalls the runtime.
3. **Single writer, pooled readers.** One mutexed write connection; all reads from the r2d2 pool;
   WAL makes them concurrent.
4. **Atomic multi-statement writes.** No FKs — transactions uphold referential integrity.
5. **Embedder signature gates reads.** `gateway:model:dim` / `hash:dim` is stored per chunk/node;
   reads filter by it. Changing the model/dim orphans all existing chunks (full re-ingest
   required).
6. **Sealed tree nodes are immutable.** Re-ingest and "forget" drop only *unsealed* leaves —
   sealed summaries (and the write-only vault) retain forgotten text (partial erasure).
7. **One shared Bearer token, namespace = path string.** No per-namespace authz.

These seven are the constraints the four-layer structure exists to satisfy — the layering is
downstream of them, not the other way around.
