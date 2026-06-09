# eval — code knowledge base validation

Reproducible validation of engram's code search against a labeled corpus, with acceptance
bars. Default corpus: `agentmemory` (a ~530-file TypeScript repo, heavily i18n/CJK — a good
stress test for the chunking/embedding robustness work). Recorded production numbers live in
[`RESULTS.md`](RESULTS.md).

## Acceptance bars
- **Ingest success ≥ 0.99** — of the indexable git-tracked files, the fraction present in
  engram (via `GET /docs/by-key`). The token-aware CJK-safe chunking + per-chunk-tolerant
  ingest are what move this from the historical 84% to ~100%.
- **recall@5 ≥ 0.80** — the headline bar (tightened from recall@10 in Phase F). For each
  labeled NL→file query, whether a gold file appears in the top-5 of `POST /code/search`.
  recall@1 and recall@10 are also reported.

The harness additionally reports two diagnostics (neither gates PASS/FAIL):
- **line-recall@k** — over queries carrying `gold_lines`, whether a returned chunk for the gold
  file has a `[line_start,line_end]` span overlapping the gold line range (a true *span* hit, not
  just the file in top-k).
- **hard negatives** — queries whose answer is *not* in the corpus; flags any that still return a
  confident top-1 (score ≥ `HARD_NEG_SCORE` = 0.55), i.e. a missing-abstention false positive.

## Files
- `agentmemory_gold.json` — 35 labeled NL→file queries (≤ 2 per file, 25 distinct source files),
  each with gold target file(s) and a `gold_lines` span, plus a top-level `hard_negatives` list.
- `validate.py` — stdlib-only harness; computes the bars + diagnostics and prints PASS/FAIL.

## Run
Requires a running engram (with the tree-sitter build) **and a healthy embed backend** — every
query embeds its text. Set `ENGRAM_TOKEN`.

```bash
# 1. Index the repo (once), then validate:
cargo build --release
ENGRAM_TOKEN=... cargo run -q -p engram-index -- index ../agentmemory \
    --namespace repo:agentmemory --url http://127.0.0.1:8088 --token "$ENGRAM_TOKEN"
ENGRAM_TOKEN=... python3 eval/validate.py

# …or let the harness index first:
ENGRAM_TOKEN=... python3 eval/validate.py --index
```

The `repo` path and `namespace` are read from the gold file (`../agentmemory`, `repo:agentmemory`
by default); override the path for a checkout elsewhere with `--repo /path/to/agentmemory`.

## Notes
- The file-selection rules in `validate.py` mirror `crates/engram-index/src/walk.rs::should_index`
  (binary/lockfile/oversized skips) so the ingest-rate denominator matches what the indexer
  actually attempts.
- Re-indexing is idempotent (upsert-by-key); a forgotten/renamed file uses `DELETE …/by-key`.
- A wedged embed backend surfaces as `http 0`/timeouts on the search calls — recover the embed
  service before trusting a FAIL.
- **Production config is tree-sitter chunking** (`ENGRAM_CODE_TREE_SITTER=true`). The recall
  journey: 0.533 (baseline) → 0.800 recall@5 (Phase F, path-type ranking prior) → tree-sitter,
  which trades recall@5 (0.771) for clear gains in rank-1 precision, top-10 coverage, and line
  accuracy. Recorded numbers and the full rationale are in [`RESULTS.md`](RESULTS.md).
