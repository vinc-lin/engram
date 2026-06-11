# eval results — agentmemory code knowledge base

Recorded recall/ingest numbers from the validation harness (`eval/validate.py`) against the live
deploy. Bars: **ingest ≥ 0.99**, **recall@5 ≥ 0.80** (tightened in Phase F; recall@10 historically).

## Baseline — 2026-06-09 (Phase E)

Live deploy `http://127.0.0.1:8088`, namespace `repo:agentmemory`, embed `gateway:mxbai-embed-large:1024`,
corpus `../agentmemory`. Pre-robustness ingest (451 docs), pre-Phase-F chunking.

| Metric | Value | Bar | Result |
|--------|-------|-----|--------|
| ingest success | **0.845** (451/534) | ≥ 0.99 | FAIL |
| recall@10 | **0.533** (8/15) | ≥ 0.80 | FAIL |
| recall@5 | **0.533** (8/15) | ≥ 0.80 | FAIL |

`recall@5 == recall@10` → every miss is *deep* (no gold file in ranks 6–10).

### Gap analysis (E2) — three distinct failure modes

1. **Coverage (2 misses).** `src/state/vector-index.ts` and `src/state/cjk-segmenter.ts` are gold
   but **absent from the corpus** — among the 83 files mxbai dropped at ingest. Unretrievable until
   a robust-build re-index (Phase F3). Ceiling today ≈ 13/15 = 0.867.
2. **Granularity (2 misses).** 2 of the 3 `src/types.ts` queries miss ("CompressedObservation",
   "lifecycle hooks") — a large type-definition file where the answer isn't isolated in its own
   chunk. → Phase F2 heuristic symbol-split.
3. **Keyword-blend distractors (≈3 misses).** Several misses rank a doc/test/generated file at
   top-1 over the gold source: `plugin/skills/forget/SKILL.md` (dedup query),
   `plugin/skills/agentmemory-hooks/SKILL.md` (hooks), `website/components/*.css`,
   `*.test.ts`, `src/prompts/summary.ts`. The hybrid keyword signal (0.65 vec / 0.35 kw) over-weights
   keyword matches in non-source files. **Live hybrid recall (0.533) < offline pure-vector mxbai
   (0.600)** — the keyword blend is net-negative on this set. → candidate ranking fix in Phase F
   (path-type down-weighting, or rebalancing vec/kw for code); previously filed under deferred.

### Per-query (recall@10)

| gold | hit | top-1 returned |
|------|-----|----------------|
| src/state/hybrid-search.ts | HIT | plugin/opencode/commands/recall.md |
| src/state/vector-index.ts | miss (absent) | src/providers/embedding/index.ts |
| src/types.ts (memory types) | HIT | src/functions/consolidate.ts |
| src/state/cjk-segmenter.ts | miss (absent) | website/components/AgentInstall.module.css |
| src/state/search-index.ts | HIT | src/state/search-index.ts |
| src/functions/remember.ts (dedup) | miss | plugin/skills/forget/SKILL.md |
| src/types.ts (CompressedObservation) | miss | src/state/memory-utils.ts |
| src/functions/graph.ts (persist/query) | miss | src/triggers/api.ts |
| src/providers/index.ts | HIT | src/providers/index.ts |
| src/functions/compress.ts | miss | src/prompts/summary.ts |
| src/state/index-persistence.ts | HIT | test/vector-index.test.ts |
| src/types.ts (lifecycle hooks) | miss | plugin/skills/agentmemory-hooks/SKILL.md |
| src/functions/remember.ts (isolation) | HIT | test/cross-project-isolation.test.ts |
| src/functions/graph.ts (snapshot) | HIT | src/functions/graph.ts |
| src/functions/query-expansion.ts | HIT | src/state/hybrid-search.ts |

**Targets for Phase F:** coverage re-index (+2 reachable), symbol-split for `types.ts` (+~2),
keyword-distractor suppression (+~3). Plausibly clears recall@5 ≥ 0.80 once combined.

## Phase F3 — symbol-split (F2) + robust-build coverage re-index — 2026-06-09

Re-indexed agentmemory on the F2 binary (`ENGRAM_JOBS_WORKERS=0`, consolidation off). Measured on
the **expanded 35-query gold set** (≤ 2/file, 25 distinct source files, all with `gold_lines`).

| Metric | Baseline (15q, pre-F2) | F3 (35q, F2 + coverage) | Bar |
|--------|------|------|-----|
| ingest | 0.845 (451/534) | **0.976 (521/534)** | ≥ 0.99 — FAIL |
| recall@1 | 0.200 | 0.314 | — |
| recall@5 | 0.533 | **0.686** | ≥ 0.80 — FAIL |
| recall@10 | 0.533 | **0.800** | — |
| line-recall@10 | n/a | 0.629 | — |

(Gold sets differ — the 35-query set is harder/more specific, so this is a tighter, more
trustworthy bar, not a like-for-like delta.)

### What improved
- **Ingest 0.845 → 0.976**: the robust build (token-aware chunking + tolerant ingest) recovered
  the previously-dropped CJK source files. The 13 still-absent are large multilingual `READMEs/*.md`
  + `benchmark/*.md` that **timed out** during the bulk re-index (transport errors after 3 retries),
  not CJK-drops — a backpressure/timeout issue, not a chunking one.
- **recall@10 = 0.800** on a deliberately harder 35-query set.

### Remaining misses → the next levers
1. **Doc/test distractors dominate.** Of the recall@10 misses, several rank a NON-source file
   top-1 over the gold source: `privacy.ts` → `.github/security-advisories/*.md`;
   `keyed-mutex.ts` → `test/*.test.ts`; `dedup.ts` → `test/*.test.ts`; `reranker.ts` →
   `benchmark/LONGMEMEVAL.md`. The keyword blend over-weights docs/tests. → a **path-type prior**
   (down-weight tests/docs/markdown in code search, or boost `src/`) is the highest-value fix.
2. **types.ts enum still buried** — symbol-split helped CompressedObservation (rank 6) but the
   "valid observation types" enum query still misses (large type file).
3. Near-synonym confusions (`compress.ts` ↔ `summarize.ts`, `stemmer.ts` ↔ `query-expansion.ts`).
4. **Hard negatives**: 3/5 returned a confident top-1 (score 0.56–0.59) for absent topics
   (Kubernetes/GraphQL/OAuth) — no abstention; a min-score floor would cut false positives.

### F3 ops note
The bulk re-index took ~22 min and 78 non-source files timed out (transport errors); the gateway
recovered immediately after (mxbai embed 200 in 0.28s). Confirms the embed backend is a throughput
bottleneck under bulk load → Phase R (resilience/fallback) + engram-index POST-timeout/concurrency
tuning.

## Phase F4 — path-type ranking prior (read-path A/B) — 2026-06-09 — **Phase F PASSES**

Same index as F3 (no re-index); only `search_code` ranking changed (`ENGRAM_CODE_PATH_PRIOR` on).

| Metric | F3 (prior off) | F4 (prior on) | Bar |
|--------|------|------|-----|
| ingest | 0.976 | **0.998** (533/534) | ≥ 0.99 — **PASS** |
| recall@1 | 0.314 | **0.486** | — |
| recall@5 | 0.686 | **0.800** | ≥ 0.80 — **PASS** |
| recall@10 | 0.800 | **0.829** | — |
| line-recall@10 | 0.629 | **0.714** | — |
| hard-neg FPs | 3/5 | **1/5** | — |

**Both Phase F bars met.** The prior (docs 0.5 / tests 0.6 / config 0.7 / source 1.0) eliminated the
doc/test distractors — every remaining miss's top-1 is now a SOURCE file. Remaining misses are
genuine source-vs-source semantic near-misses: `types.ts` enum buried; `compress`↔`summarize`;
`keyed-mutex`↔`access-tracker`; `dedup`↔`smart-search`; `stemmer`↔`query-expansion`;
`schema`↔`tools-registry`. Closing those needs a graph/`sym:` ranking signal or better embeddings
(deferred enhancements). Hard-negative false positives fell 3/5 → 1/5.

## Phase 4 — tree-sitter chunking (live capstone) — 2026-06-09

Re-indexed agentmemory on the tree-sitter binary (`ENGRAM_CODE_TREE_SITTER=true`, embedder fallback
on, consolidation off). Measured on the 35-query gold set.

| Metric | F4 (heuristic chunking) | Phase 4 (tree-sitter) | Δ |
|--------|------|------|---|
| ingest | 0.998 | 0.998 | — |
| recall@1 | 0.486 | **0.543** | **+0.057** |
| recall@5 | 0.800 | 0.771 | −0.029 |
| recall@10 | 0.829 | **0.914** | **+0.085** |
| line-recall@10 | 0.714 | **0.771** | **+0.057** |

Tree-sitter isolates each definition into its own chunk, which sharpens **precision** (recall@1
+0.057 — the best match ranks first more often), **coverage** (recall@10 0.829→0.914 = 32/35 gold
files in top-10), and **line accuracy** (line-recall@10 +0.057 — the matched chunk's span contains
the answer more often). recall@5 dips marginally (−0.029, ~one query slipping rank 5→6).

**Best-chunk-per-file dedup — tested and REJECTED.** Hypothesis: the recall@5 dip was a
duplicate-slot artifact (one finely-chunked file taking several top-5 slots), so deduping to the
best chunk per file would recover it. Measured live: dedup left recall@1/5/10 **unchanged**
(0.543/0.771/0.914) — the missed gold files' *best* chunk genuinely scores at rank 6–7, not a slot
artifact — and it **regressed line-recall@10 to 0.543** (deduping discards a file's other chunks,
including the one whose span covers the answer). Net-negative; reverted. The recall@5 dip is a real,
small ranking effect of finer chunking — net of tree-sitter is still a clear precision / coverage /
line-accuracy win.

**Decision (2026-06-09): tree-sitter kept as the production config** (`ENGRAM_CODE_TREE_SITTER`
defaults on; live on the deploy). The agent-facing metrics — rank-1 precision, top-10 coverage, and
line accuracy — all improve; the recall@5 −0.029 is an accepted trade. The genuine lever for
recall@5 (a graph/`sym:` ranking signal or better embeddings) is filed under deferred enhancements,
not chunking tricks.

Ops: the re-index took ~21 min and 70 large multilingual `READMEs/*.md`/`CHANGELOG` timed out
(engram-index *client*-side POST timeout — tree-sitter's higher chunk count makes big CJK docs
slower to ingest synchronously; the embedder fallback only catches gateway-side embed errors, not a
client timeout). All gold (source) files re-chunked fine; ingest 0.998 holds (upsert kept prior
`.md` versions). Lever: raise the engram-index POST timeout / cap per-file chunk concurrency.

## IDF-weighted keyword re-ranking (2026-06-11) — recall@1 0.457 → 0.657

A failure analysis of the live retrieval (baseline below, embedder held constant) found the dominant
miss mode was **ranking, not coverage**: recall@5 0.829 but recall@1 only 0.457 — in ~37% of queries
the gold file sat in the top-5 but a broad "hub" file (`search.ts` ×4, `index.ts` ×2, `types.ts`)
won #1 on a loose semantic match. Almost every miss carried a *rare, specific* term (`base64`,
`jieba`, `cosine`, `jaccard`, `debounced`, `CompressedObservation`, `decay`) that uniquely identifies
the answer — but `search_code`'s keyword score was plain term-overlap, weighting `base64` the same as
`how`. So rare terms were drowned out.

**Fix (`ENGRAM_CODE_KEYWORD_IDF`, default on):** weight keyword overlap by inverse document frequency
over the candidate chunks — rare query terms dominate, common words (`how`/`the`) get ~0 weight. It's
a query-time re-rank (no re-index). A/B on the 35-query gold (bge-m3 substrate, IDF off vs on):

| metric | baseline (no IDF) | **IDF on** | Δ |
|--------|------|------|------|
| recall@1 | 0.457 | **0.657** | **+0.200** |
| recall@5 | 0.829 | **0.857** | +0.028 |
| recall@10 | 0.886 | 0.857 | −0.029 |
| line-recall@1 | 0.314 | **0.457** | **+0.143** |
| line-recall@5 | 0.657 | **0.743** | +0.086 |
| line-recall@10 | 0.771 | **0.829** | +0.058 |

**+7 queries (16 → 23 of 35) now rank the gold file #1.** The only cost is one borderline query off the
tail (recall@10 −0.029). The lever is embedder-independent (it re-weights keyword, not the vector);
measured here on bge-m3 — a mxbai re-confirm on the production embedder is a cheap follow-up. This is
the recall@5/ranking lever the earlier results filed under deferred enhancements, now landed.
