# eval results — agentmemory code knowledge base

Recorded recall/ingest numbers from the validation harness (`eval/validate.py`) against the live
deploy. Bars: **ingest ≥ 0.99**, **recall@5 ≥ 0.80** (tightened in Phase F; recall@10 historically).

## Baseline — 2026-06-09 (Phase E)

Live deploy `http://127.0.0.1:8088`, namespace `repo:agentmemory`, embed `gateway:mxbai-embed-large:1024`,
corpus `/home/vinc/code/agentmemory`. Pre-robustness ingest (451 docs), pre-Phase-F chunking.

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
