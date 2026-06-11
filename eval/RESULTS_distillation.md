# Distillation vs. Simple Summary — does engram's consolidation earn its keep?

**Status: complete (2026-06-11). Verdict: the flat summary beats engram's distillation, 1.50 vs
1.00 (of 2).** The user's instinct was correct. Two scalable flat-digest *replacements* were then
prototyped and **both failed** (map-reduce 0.58, rolling-refine 0.33 of 2) — the deeper lesson is to
retire the digest fold entirely in favour of a single-pass summary + raw retrieval. See
**Replacement prototypes**.

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

Blind 3-judge scoring (independent Claude judges, arm-anonymized + shuffled, scored 0/1/2 vs ground
truth; per-answer = median of judges). n = 12 questions per arm.

| Arm | Mean (0–2) | Total /24 |
|-----|-----------|-----------|
| **B — simple summary** | **1.50** | 18 |
| C — retrieval | 1.08 | 13 |
| **A — engram distillation** | **1.00** | 12 |
| D — control (headers only) | 0.00 | 0 |

**The flat summary wins decisively — 1.50 vs distillation's 1.00 (+50% relative)** — and the
distillation digest even edged *below* coarse doc-level retrieval. The control scored zero,
confirming the questions genuinely require the corpus content (no answers leaked from the question
text).

**Where distillation lost.** It scored **0 on 5 of 12** questions — q01 (spawn_blocking), q03
(off-lock invariant), q05 (fallback_summary), q07 (ext4-not-v9fs), q08 (job idempotency) — every
one a *specific mechanism fact*. The flat summary retained those. This is the **compounding-loss**
failure mode the design predicted: folding summaries-of-summaries (here a real 4-level tree)
abstracts away the exact detail an agent needs — a function name, a gate, a reason — and leaves
generic prose. Compressing the corpus **once** (the flat summary) kept more specifics. Distillation
won only **1** question outright (q11, the native-recall finding: A=2, B=0).

Per-question medians (2 = correct, 1 = partial, 0 = wrong/absent):

| q | A distill | B summary | C retrieval | D control |
|---|:--:|:--:|:--:|:--:|
| q01 | 0 | 2 | 2 | 0 |
| q02 | 2 | 2 | 1 | 0 |
| q03 | 0 | 1 | 2 | 0 |
| q04 | 1 | 2 | 0 | 0 |
| q05 | 0 | 2 | 0 | 0 |
| q06 | 2 | 2 | 0 | 0 |
| q07 | 0 | 2 | 0 | 0 |
| q08 | 0 | 0 | 2 | 0 |
| q09 | 2 | 2 | 2 | 0 |
| q10 | 2 | 2 | 2 | 0 |
| q11 | 2 | 0 | 0 | 0 |
| q12 | 1 | 1 | 2 | 0 |

Note: the "Not in context" *presence* rate was close (A 3/12, B 2/12), but the *scored* gap is
wider (A 1.0, B 1.5) because two of distillation's present answers were confidently **wrong** and
scored 0 — exactly what scoring-vs-counting is meant to catch.

**Caveats.** Single corpus (engram's own docs), 12 questions, n = 12/arm — directional, not
definitive. 6 of 36 judge calls failed to return structured output, so a few questions were scored
by 2 judges instead of 3 (median still applied; does not change the ranking). The distillation arm
used shrunk seal gates to force a multi-level fold on a small corpus; production gates fold less
aggressively. Judge = Claude, independent of the deepseek answerer.

## Verdict

**Confirmed: engram's distillation does not beat a simple summary as agent context — it loses, 1.00
vs 1.50.** The autonomous tree-consolidation is both *expensive* (per-leaf LLM seals) and *lossy*
(summary-of-summaries drops the specifics an agent needs), and a one-shot flat summary of the same
material serves the agent better. With retrieval being the separately-proven-strong half, the
actionable conclusion for the "distilled context" use case: **prefer a flat, regenerated summary
(or retrieval) over the deep consolidation tree.** The tree-fold is a candidate to **replace**, not
tune — which directly answers the question that opened this experiment.

## Replacement prototypes (2026-06-11) — both failed, and the failure is the lesson

Having shown the deep tree loses to a one-shot summary, the obvious next step was a *scalable* flat
digest that recovers the one-shot's quality without needing the whole corpus in one context. Two
were prototyped (LLM-only, no consolidation machinery) and re-judged blind against the tree and the
one-shot, all four together:

| Arm | Method | Summarization hops | Mean (0–2) |
|-----|--------|:--:|:--:|
| **B — one-shot summary** | one pass over the whole corpus | **1** | **1.58** |
| A — deep tree (engram) | 4-level fold **+ surfaces raw leaves** | mixed | 1.08 |
| A′ — map-reduce digest | per-doc digest → combine | 2 | 0.58 |
| A″ — rolling-refine digest | fold each doc into a running digest | 5 (serial) | 0.33 |

**Both replacement prototypes lost — and lost worse the more they compressed.** For pure-abstractive
methods the score decays **monotonically with the number of summarization hops**: 1 hop → 1.58,
2 hops → 0.58, 5 hops → 0.33. The rolling-refine digest (which rewrites the digest once per doc) is
just a *serial* summary-of-summaries and fared worst of all.

The deep tree's 1.08 is the apparent exception — but only because `drill_down` surfaces **fresh raw
leaves** alongside the summaries, so its context smuggles in some un-compressed source. That is the
tell: **what helps is keeping the raw specifics; what hurts is every extra compression hop between
source and agent.**

### Revised recommendation

The problem was never the tree's *structure* — it's **multi-hop abstractive compression itself**.
A fancier digest does not fix it; every prototype that pre-summarizes lost. So:

1. **Don't pre-distill.** The autonomous consolidation tree, map-reduce, and rolling-refine are the
   same mistake at different depths. Minimize hops between source and the agent.
2. **For an overview digest:** a **single-pass** summary regenerated from the raw source, for any
   namespace that fits a context window (most do). One hop, global budget allocation — the only
   method that scored well.
3. **For specifics:** lean on **retrieval of raw chunks** (`search_code` / `query`) — engram's
   already-strong half — not a digest.
4. **If a namespace exceeds the context window:** prefer *extractive* selection (pull the most
   relevant raw passages) over another abstractive fold; never stack summaries of summaries.

In short: engram's value is **retrieval + a thin single-pass overview**, not hierarchical
distillation. The consolidation tree should be **retired for the digest path**, not replaced with a
cleverer fold.

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
