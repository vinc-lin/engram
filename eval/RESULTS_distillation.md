# Distillation vs. Simple Summary — does engram's consolidation earn its keep?

**Status: run in progress (2026-06-11).** Design + methodology below are final; the scored verdict
is pending the blind-judge pass (see Results).

## The question

engram has two halves. **Retrieval** (`search_code` / `query`, reads chunks directly) is measured
and strong. **Distillation** — the autonomous consolidation tree that folds documents into
hierarchical LLM summaries (`get_architecture` / `get_module` / `drill_down`) — is the half that
embodies the original vision: *distill knowledge from past work so an agent gains the context it
needs.* It has never been measured against a baseline. The user's observation — *"it seems worse
than a simple summary"* — is the hypothesis this experiment tests, head-on:

> **Given the same corpus, does engram's distilled context help an agent answer questions better
> than a single flat summary of that corpus?**

## Design

A controlled, answerer-constant A/B/C/D. The agent receives **one arm's context as its only
context** and must answer a battery of ground-truth questions; a blind judge scores each answer
against the known-correct answer.

**Corpus** — engram's own docs (`README.md`, `ARCHITECTURE.md`, `docs/ROADMAP.md`, `CLAUDE.md`, the
repo-knowledge design spec): 5 files, ~98k chars (~25k tokens). Chosen because (a) every answer is
verifiable from the source and (b) prose consolidation runs by default, so the distillation arm is
real engram output.

**Four context arms**

| Arm | Context | Built by |
|-----|---------|----------|
| **A — distillation** | engram's consolidation digest, per question (`POST /tree` drill_down over the Global tree: top sealed summary nodes + reranked leaves) | engram cold pipeline (deepseek summaries) |
| **B — simple summary** | one flat ~900-word LLM summary of the whole corpus, identical for every question | a single deepseek call |
| **C — retrieval** | top-3 `POST /query` docs, windowed around the query terms | engram retrieval |
| **D — control** | corpus file list + section headers only | mechanical |

**Questions** — 12 ground-truth probes an engineer needs to work on engram, spanning architecture
(two execution planes; off-lock invariant), invariants/gotchas (signature gating; forget is
partial; ext4-not-v9fs), mechanism (seal-cascade gates; job-id idempotency; fallback_summary), and
conventions (consolidation-off-by-default; hybrid-scoring weights; native line-recall finding;
abstention floor). Each has a written ground-truth answer extracted from the corpus.

**Procedure** — the **answerer is held constant** (deepseek-chat, temp 0); arms differ *only* in
the supplied context. System prompt: *answer using ONLY the context; if absent, say "Not in
context."* Contexts are length-matched (~5–6k chars). Then a **blind judge** (independent model,
arm-anonymized and shuffled) scores each answer **0 / 1 / 2** against the ground truth:
`2` = correct and captures the key specific facts; `1` = partial / vague / missing specifics;
`0` = wrong, or "Not in context." Multiple independent judges per answer; per-answer score = the
median.

## Stack (and why it took work)

- **Embeddings → local bge-m3** (Ollama/GPU) via **`ENGRAM_EMBED_URL`**. The gateway's embedding
  endpoint returns 500s under sustained consolidation load, which failed seal jobs and left the
  distillation digest incomplete (an invalid Arm A). Routing embeddings to the reliable local
  bge-m3 fixed it.
- **LLM → gateway deepseek-chat** — the real production/agent model — for the consolidation
  summaries, the Arm B summary, **and** the answerer, so A and B are distilled by the *same* model
  (fair) and the answerer is representative.
- Pairing local-embed with gateway-LLM required decoupling the two endpoints, which engram couldn't
  do before — so this experiment produced a real feature: **`ENGRAM_EMBED_URL`** (defaults to
  `ENGRAM_GATEWAY_URL`; see `docs/EMBEDDINGS.md`).
- **Seal gates shrunk** (`ENGRAM_SEAL_INPUT_TOKEN_BUDGET=2000`, `ENGRAM_SEAL_FANOUT=4`) so a
  ~25k-token corpus actually folds into a multi-level tree rather than sitting as flat leaves under
  the 50k default budget.

**Validity check (clean run):** the corpus folded into a genuine **4-level** tree
(L0 302 → L1 29 → L2 10 → L3 4) with **0 failed jobs** — Arm A is a real hierarchical digest, not
the deterministic `fallback_summary` concat.

## Results

**Pending the blind-judge pass (in progress).** To be reported here: per-arm mean score (0–2),
the **A vs B** delta (the headline), the "Not in context" rate per arm, and a per-question
breakdown with representative answers.

*Preliminary directional signal* (from an earlier, **confounded** run in which 4/5 docs failed to
consolidate, so Arm A's digest was incomplete — superseded by the clean run above): the flat
summary surfaced more answers than the distillation — Arm B missed 4/12 questions ("Not in
context"), Arm A missed 6/12. Treat as a hint only until the clean scored numbers land.

## Reproduction

1. Serve local bge-m3: `~/start-ollama-wsl.sh` (Ollama/GPU on `:11434`).
2. Run engram decoupled: `ENGRAM_EMBED_URL=http://127.0.0.1:11434 ENGRAM_EMBED_MODEL=bge-m3
   ENGRAM_EMBED_DIM=1024 ENGRAM_LLM_MODEL=deepseek-chat ENGRAM_GATEWAY_URL=<gateway>
   ENGRAM_SEAL_INPUT_TOKEN_BUDGET=2000 ENGRAM_SEAL_FANOUT=4`.
3. Prose-ingest the corpus, wait for the job queue to drain and the tree to fold, then for each
   question build the four contexts and collect a deepseek answer per (question × arm).
4. Blind-judge the anonymized answers against ground truth; aggregate per arm.

## Why it matters

This is a direct test of engram's **core premise**. If the flat summary wins, the signal is that
engram's value lives in **retrieval**, and the **autonomous tree-consolidation** machinery —
expensive (per-leaf LLM seals) and lossy (summaries of summaries) — is a candidate to **replace
with simpler flat, regenerated digests** rather than a deep fold. If distillation wins, the
consolidation half is validated and worth turning on (`ENGRAM_CONSOLIDATE_CODE`) for real use.
Either way: evidence instead of impressions.
