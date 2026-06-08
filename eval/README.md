# eval — code knowledge base validation

Reproducible validation of engram's code search against a labeled corpus, with acceptance
bars. Default corpus: `agentmemory` (a ~530-file TypeScript repo, heavily i18n/CJK — a good
stress test for the chunking/embedding robustness work).

## Acceptance bars
- **Ingest success ≥ 0.99** — of the indexable git-tracked files, the fraction present in
  engram (via `GET /docs/by-key`). The token-aware CJK-safe chunking + per-chunk-tolerant
  ingest are what move this from the historical 84% to ~100%.
- **recall@10 ≥ 0.80** — for each labeled NL→file query, whether a gold file appears in the
  top-10 of `POST /code/search`.

## Files
- `agentmemory_gold.json` — 15 labeled queries with gold target file(s).
- `validate.py` — stdlib-only harness; computes both bars and prints PASS/FAIL.

## Run
Requires a running engram (with the robustness build) **and a healthy embed backend** — every
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

The `repo` path is read from the gold file (`../agentmemory` by default); override it for a
checkout elsewhere with `--repo /path/to/agentmemory`.

## Notes
- The file-selection rules in `validate.py` mirror `crates/engram-index/src/walk.rs::should_index`
  (binary/lockfile/oversized skips) so the ingest-rate denominator matches what the indexer
  actually attempts.
- Re-indexing is idempotent (upsert-by-key); a forgotten/renamed file uses `DELETE …/by-key`.
- A wedged embed backend surfaces as `http 0`/timeouts on the search calls — recover the embed
  service (e.g. restart Ollama on its host) before trusting a FAIL.
