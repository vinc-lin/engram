# Testing engram = measuring retrieval quality

"Testing" here does **not** mean "does it run" — it means **does it retrieve the right code?**, scored
as three numbers over a labeled query set:

| metric | meaning |
|--------|---------|
| **recall@1** | the gold file is the **#1** `search_code` hit |
| **recall@5** | the gold file is in the **top 5** |
| **line-recall@5** | a top-5 hit's **line range actually contains the answer** |

(+ `recall@10` and hard-negative false-positives as guards.) **Headline bar: recall@5 ≥ 0.80.** This is
the measurement used to tune everything in `eval/RESULTS.md` (e.g. IDF + weight took the production
config from recall@1 0.543 / recall@5 0.771 → 0.714 / 0.857). This guide reproduces it on **another
machine with another model** — engram is model-agnostic (OpenAI HTTP API), so any embedder/LLM works.

---

## 1. Run engram with your model
Env vars are the whole config surface (full list: `crates/engram/src/config.rs`):

```bash
ENGRAM_DB=$HOME/engram/engram.db ENGRAM_BIND=127.0.0.1:8088 ENGRAM_TOKEN=secret \
ENGRAM_GATEWAY_URL=<openai-compatible-url> ENGRAM_GATEWAY_KEY=<key> \
ENGRAM_EMBED_MODEL=<embed-model> ENGRAM_EMBED_DIM=<its-real-dim> \
ENGRAM_LLM_MODEL=<chat-model> \
target/release/engram      # cargo build --release first
```

`ENGRAM_EMBED_DIM` **must equal the model's true output dim** (mxbai 1024, bge-m3 1024, nomic 768) — a
mismatch silently produces garbage vectors and tanks recall. Use `ENGRAM_EMBED_URL` if your embedder
and LLM live on different endpoints (embeddings there, LLM stays on `ENGRAM_GATEWAY_URL`).

## 2. Produce the measurement (the core)
Index a repo, then score it against a gold set:

```bash
target/release/engram-index index <path/to/repo> --namespace repo:<id> \
    --url http://127.0.0.1:8088 --token secret

ENGRAM_TOKEN=secret python3 eval/validate.py \
    --url http://127.0.0.1:8088 --token secret --gold <gold.json>
```

Output is the table — `ingest`, `recall@1/5/10`, `line-recall@1/5/10`, hard-neg FPs, and `PASS/FAIL`
on the recall@5 ≥ 0.80 bar. That **is** the test result.

## 3. The gold set (required — recall is measured against it)
Recall needs labeled answers. `eval/agentmemory_gold.json` (35 NL→file queries on the *agentmemory*
repo) is the reference benchmark. To measure **your** repo, write your own with the same schema:

```jsonc
{ "namespace": "repo:<id>", "repo": "<path>",
  "queries": [
    { "q": "where is cosine similarity computed between two vectors",
      "gold": ["src/providers/embedding/index.ts"],
      "gold_lines": [{ "file": "src/providers/embedding/index.ts", "line_range": [40, 58] }] }
  ],
  "hard_negatives": [ { "q": "a topic the repo does NOT contain" } ] }
```

~30–40 natural-language questions, each with the file(s) that answer it (and the line range for
line-recall), spread across the repo. This authoring is the real effort of honest evaluation —
without it you can only smoke-test, not *measure*. (Quick smoke check, no gold:
`POST /v1/repo:<id>/code/search {"query":"...","limit":5}` and eyeball the paths.)

## 4. A/B-ing a model or config change against the numbers
This is the loop the whole project was tuned with. Most ranking knobs are **query-time** (no
re-index), so you A/B by restarting with the env toggled and re-running `validate.py`:

```
baseline   → index once, validate.py, record recall@1 / recall@5 / line-recall@5
change     → flip a flag (or swap the model), restart, validate.py again
keep       → only if recall@1 / line-recall improves; record it (incl. negatives)
```

Compare model A vs model B the same way (note: a model swap needs a **re-index** — see §5). Example
from this repo (`eval/RESULTS.md`):

| config | recall@1 | recall@5 | line-recall@5 |
|--------|:--:|:--:|:--:|
| baseline | 0.543 | 0.771 | 0.629 |
| tuned (IDF + kw 0.50) | **0.714** | **0.857** | **0.771** |

## 5. Model-swap rules (what changes the numbers)
- **Changing the embed model/dim orphans all chunks → re-index.** The embedder signature
  (`gateway:<model>:<dim>`) is stored on every chunk and reads filter by it. Pin the embed model
  before indexing; treat any change as a full re-index. (Re-indexing a large repo can be slow and may
  hit rate-limits/500s on a hosted embedder — pace it / lower `ENGRAM_JOBS_WORKERS`.)
- **The ranking levers are model-independent — leave them on.** IDF keyword
  (`ENGRAM_CODE_KEYWORD_IDF`), keyword weight (`ENGRAM_CODE_KW_WEIGHT=0.50`), path prior
  (`ENGRAM_CODE_PATH_PRIOR`) lifted recall on **both** mxbai and bge-m3; they re-weight keyword/path,
  not the vector, so they transfer across models. `ENGRAM_CODE_GRAPH` measured *worse* — keep it off.
- **Short-context embedders** (~512 tok) truncate; for native C/C++ use a long-context embedder +
  `ENGRAM_CODE_NATIVE_PACK` + `ENGRAM_CODE_NATIVE_BUDGET≈1500`. See `docs/EMBEDDINGS.md`.
- **The recall@5 ≥ 0.80 bar is calibrated on agentmemory** — a sane starting target for any repo, not
  a law for yours; what matters is the *delta* when you change something.

## 6. Prerequisite: the code builds
Before any of the above, on a fresh machine: `cargo build --release` and `cargo test` (offline,
network-free — proves the binary is sound before you measure retrieval with it).

## 7. Beyond single-query retrieval (optional)
- **Agent-level value** — does a coding agent do better *with* engram than without? `eval/android/` +
  `eval/harness/` (model-agnostic litellm agent in `eval/agent/`); `eval/RESULTS_android.md`.
- **Digest quality** — single-pass vs the tree fold: `eval/RESULTS_distillation.md`.
