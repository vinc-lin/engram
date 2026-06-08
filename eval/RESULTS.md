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
