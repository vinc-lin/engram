# Testing engram (portable — another machine, another model)

How to verify engram works and measure its retrieval quality on a different computer with a
different embedder/LLM. engram is **model-agnostic**: it speaks the OpenAI HTTP API
(`/v1/embeddings`, `/v1/chat/completions`), so any backend that does too — a litellm gateway,
Ollama, vLLM, a cloud API — works. The four config knobs you'll touch are `ENGRAM_EMBED_MODEL`,
`ENGRAM_EMBED_DIM`, `ENGRAM_GATEWAY_URL`, `ENGRAM_LLM_MODEL` (full env list:
`crates/engram/src/config.rs`; embedder guidance: `docs/EMBEDDINGS.md`).

## 0. Prerequisites
- **Rust toolchain + a C compiler.** `rusqlite` (bundled SQLite) and the tree-sitter grammars
  compile in — no system libsqlite needed, but a C compiler is.
- **An embedder and a chat LLM** reachable over the OpenAI API. They can be the same endpoint or
  two different ones (see §4).
- **An ext4 (native) filesystem path for the DB** — not a network/9p/SMB mount (WAL is flaky there).

## 1. Code tests — offline, no model needed (any machine)
The inline suite uses a deterministic `HashEmbedder` and LLM test-doubles, so it touches no network:

```bash
cargo test            # the full inline suite (#[cfg(test)] in every module)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --release
```

Green here proves the build + logic are sound on this machine, with no model configured. Two
`#[ignore]`d tests in `embed.rs` need a live backend and only run under `cargo test -- --ignored`.

## 2. Run engram against your model
Env vars are the entire config surface. Minimal local example (one Ollama serving both an embed and
a chat model):

```bash
ENGRAM_DB=$HOME/engram/engram.db \           # ext4 path
ENGRAM_BIND=127.0.0.1:8088 ENGRAM_TOKEN=secret \
ENGRAM_GATEWAY_URL=http://127.0.0.1:11434 ENGRAM_GATEWAY_KEY=x \
ENGRAM_EMBED_MODEL=<your-embed-model> ENGRAM_EMBED_DIM=<its-real-dim> \
ENGRAM_LLM_MODEL=<your-chat-model> \
cargo run -p engram        # (or target/release/engram)
```

`ENGRAM_EMBED_DIM` **must equal your embed model's true output dimension** (e.g. mxbai-embed-large
1024, bge-m3 1024, nomic-embed-text 768) — a mismatch silently produces garbage vectors.

## 3. Smoke test — end-to-end with any model
After the server is up (`curl :8088/healthz` → `ok`), a one-shot check of ingest → search → digest:

```bash
T="Authorization: Bearer secret"; H="Content-Type: application/json"; U=http://127.0.0.1:8088
# ingest two code files (meta.kind=file → code mode)
curl -s -H "$T" -H "$H" $U/v1/repo:demo/docs \
  -d '{"key":"src/auth.rs","title":"src/auth.rs","content":"fn verify_token(t:&str)->bool{ /* bearer check */ true }","meta":{"kind":"file"}}'
curl -s -H "$T" -H "$H" $U/v1/repo:demo/docs \
  -d '{"key":"src/store.rs","title":"src/store.rs","content":"struct Store; // r2d2 read pool + one mutexed writer","meta":{"kind":"file"}}'
# chunk-level code search
curl -s -H "$T" -H "$H" $U/v1/repo:demo/code/search -d '{"query":"bearer token check","limit":3}'
# single-pass architecture digest (uses ENGRAM_LLM_MODEL)
curl -s -H "$T" -H "$H" $U/v1/repo:demo/code/architecture/rebuild -d '{}'
curl -s -H "$T" -H "$H" $U/v1/repo:demo/code/architecture -d '{}'   # serves the cached digest
```

`search_code` should return `src/auth.rs:line`; the architecture call should return a non-empty
`tree_kind:"digest"` body. If the digest is the deterministic "mechanical fallback" text, your
`ENGRAM_LLM_MODEL` endpoint isn't reachable.

## 4. Two backends (embedder ≠ LLM): `ENGRAM_EMBED_URL`
If your embeddings and chat live on different endpoints — e.g. a local embedder + a cloud LLM — set
`ENGRAM_EMBED_URL` for embeddings while `ENGRAM_GATEWAY_URL` keeps the LLM:

```bash
ENGRAM_GATEWAY_URL=https://your-llm-gateway  ENGRAM_LLM_MODEL=<chat-model> \
ENGRAM_EMBED_URL=http://127.0.0.1:11434  ENGRAM_EMBED_MODEL=<embed-model>  ENGRAM_EMBED_DIM=<dim>
```

## 5. Measure retrieval quality (the eval loop)
This is how to know engram is *good*, not just *running*. Index a repo, then score it:

```bash
target/release/engram-index index <path/to/repo> --namespace repo:<id> --url $U --token secret
ENGRAM_TOKEN=secret python3 eval/validate.py --url $U --token secret --gold <gold.json>
```

Reports **ingest rate, recall@1/5/10, line-recall@k, hard-negative FPs**. Headline bar:
**recall@5 ≥ 0.80**. The shipped `eval/agentmemory_gold.json` (35 NL→file queries) is specific to the
*agentmemory* repo — use it as a reference benchmark, or **write your own gold for your repo**: same
schema — `queries[]` of `{q, gold:[path], gold_lines:[{file,line_range}]}` plus a `hard_negatives[]`
bucket.

## 6. Swapping the model — the rules that bite
- **Signature gating = re-index on any model/dim change.** Each chunk stores
  `gateway:<model>:<dim>`; reads filter by it. Changing `ENGRAM_EMBED_MODEL` or `ENGRAM_EMBED_DIM`
  makes every existing chunk invisible (orphaned, not migrated). **Pin your embed model before a bulk
  index**; treat any change as a full re-index.
- **The ranking levers are model-independent — leave them on.** IDF-weighted keyword
  (`ENGRAM_CODE_KEYWORD_IDF`), the keyword weight (`ENGRAM_CODE_KW_WEIGHT=0.50`), and the path prior
  (`ENGRAM_CODE_PATH_PRIOR`) were validated to lift recall@1 on **both** mxbai and bge-m3 (see
  `eval/RESULTS.md`); they re-weight keyword/path, not the vector, so they transfer across embedders.
  `ENGRAM_CODE_GRAPH` is off for a reason — it measured *worse* on code.
- **Short-context embedders truncate.** If yours caps at ~512 tokens, native C/C++ definition-packing
  won't help (it'd clip); use a long-context embedder + `ENGRAM_CODE_NATIVE_PACK=true` +
  `ENGRAM_CODE_NATIVE_BUDGET≈1500` for native code. Decision matrix: `docs/EMBEDDINGS.md`.
- **LLM quality scales the digests.** Any OpenAI-compatible chat model works for the architecture
  digest / consolidation; a stronger model yields a better digest (deepseek/qwen were used here).

## 7. A/B-ing a change (the method used throughout)
Most ranking knobs are **query-time** (no re-index), so you A/B by restarting with the env toggled:

1. **Baseline** — index once, `validate.py`, record recall@1/5/10 + line-recall.
2. **Diagnose** — for each gold query, fetch the top-k and find the rank of the gold file; look at
   *where* it fails (ranking vs coverage).
3. **Change behind a flag**, rebuild.
4. **A/B** — same DB/embedder, flag off vs on, re-`validate.py`. Keep only if recall@1 / line-recall
   improves. Record the result (and negatives) in `eval/RESULTS.md`.

This is exactly how the IDF / keyword-weight / graph levers were settled — see `eval/RESULTS.md`.

## 8. The other suites (optional)
- **Android / AVM feature-migration** (`eval/android/` + `eval/harness/`): Lens-2 (retrieval vs
  ripgrep/native) + Lens-1 (a model-agnostic litellm coding agent, `eval/agent/`, A/B with vs without
  engram). Set your model in the agent config. Findings: `eval/RESULTS_android.md`.
- **Distillation vs summary** (the digest design A/B): `eval/RESULTS_distillation.md`.
