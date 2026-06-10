# eval results — engram for Android AVM feature-migration

Does engram help a coding agent migrate an **Around View Monitor (AVM)**-style feature between
Android projects? Measured on 3 open-source Android proxies (the user's real AVM repo — app-level
Kotlin/Java + AOSP/system-level C/C++/AIDL/Android.bp — is private, applied by hand later).

## Setup

- **Proxies** (pinned SHAs in `eval/android/proxy_repos.toml`), indexed into a dedicated eval engram
  (`:8089`, isolated DB; live deploy untouched), tree-sitter ON (mxbai-embed-large, dim 1024):
  - `android/ndk-samples` — 784 docs (Java/Kotlin + C/C++ NDK + CMake/Android.bp); the clean
    cross-language workhorse.
  - `intel/libxcam` — 573 docs; C/C++ 360° surround-view stitching/fisheye (the system-level core).
  - `wysaid/android-gpuimage-plus` — 515 docs; app↔C++ JNI GPU image pipeline.
- **Gold** (`eval/android/avm_gold.json`): **108 retrieval probes** (tagged by language + layer) and
  **15 migration-feature footprints** (the full cross-layer file set per feature), authored by
  reading the repos; every gold path verified to exist. C/C++ + Java heavy; Kotlin light (the
  proxies happen to be Java-based) — a coverage caveat vs the user's Kotlin+native target.
- **Agent**: a custom litellm tool-calling agent (`eval/agent/`), **DeepSeek** (`deepseek-chat`),
  Qwen3 adapter built but not run (no gateway access yet). Tools: `read_file`/`list_dir`/`ripgrep`
  in both arms; `+ search_code`/`find_symbol`/`why` in the with-engram arm (engram is *additive* to
  grep, so the A/B isolates its marginal value). Bounded loop (≤15 turns, ≤40 tool calls).

## Lens 2 — retrieval vs baselines (the rigorous backbone)

engram `search_code` vs the alternatives an agent already has (`ripgrep`; a filename-token "native"
floor), over all 108 probes.

| repo | retriever | recall@5 | recall@10 | line-recall@10 | cross-layer |
|------|-----------|----------|-----------|----------------|-------------|
| ndk-samples | **engram** | **0.829** | 0.886 | 0.593 | 0.65 |
| | ripgrep | 0.314 | 0.40 | 0.0 | 0.40 |
| | native | 0.371 | 0.457 | 0.0 | 0.80 |
| libxcam | **engram** | **0.789** | 0.816 | **0.92** | 0.70 |
| | ripgrep | 0.368 | 0.553 | 0.0 | 0.60 |
| | native | 0.526 | 0.658 | 0.0 | 0.70 |
| gpuimage-plus | **engram** | **0.829** | 0.914 | **0.76** | 0.75 |
| | ripgrep | 0.286 | 0.457 | 0.08 | 0.50 |
| | native | 0.571 | 0.629 | 0.0 | 0.63 |

**engram recall@5 ≈ 0.81 vs ripgrep ≈ 0.32 vs native ≈ 0.49** — a large, consistent retrieval win.
The standout is **line-recall** (~0.76 vs ~0): engram returns the precise `path:line` of the
answering definition, while ripgrep returns keyword-match lines that rarely land on the definition
and native has no line info. For migration ("take me to the exact code"), that precision is the
core value. C/C++ recall@5 is 0.85–0.91.

**Caveat — no abstention.** On hard negatives (out-of-corpus questions), engram returned a confident
top-1 (score ≥ 0.55) for **libxcam 3/3, ndk-samples 1/3, gpuimage 0/3** (4/9 overall). A min-score
floor is the known fix (deferred enhancement). Also, native's cross-layer coverage is competitive
(filename matching finds build/app files easily) — engram's edge is sharpest on recall + line-recall.

## Lens 1 — agent migration A/B (DeepSeek, with vs without engram)

15 features, footprint scored vs the gold cross-layer set (deterministic path precision/recall/F1 +
layer coverage). Merged from clean runs (`eval/harness/score_lens1.py`).

| | with_engram | without_engram |
|---|---|---|
| mean footprint F1 | **0.546** | 0.523 |
| **mean layer coverage** | **0.983** | 0.883 |
| total tokens | 2.35M | 1.74M |
| total cost | $0.34 | $0.25 |
| win / tie / loss (\|Δ\|>0.02) | **4 / 6 / 5** | |

**A modest edge, mostly in coverage.** with-engram finds files in **more of each feature's layers**
(0.983 vs 0.883 — the migration-relevant signal: don't miss the AIDL/build/native piece), with a
slight F1 gain, but the per-feature record is mixed (4 win / 6 tie / 5 loss) and it costs ~35% more
tokens (search results add context). Standout wins: `gles-backend` (+0.47 — engram found a module
grep missed entirely), `camera-provider-abstraction` (+0.17), `stitcher` (+0.15). Standout losses:
`fisheye-dewarp` (−0.15), `frame-renderer-pipeline` (−0.13), `video-recording` (−0.10).

**Why only modest?** These proxies are *small and well-named* (500–960 files), so `ripgrep`+`read`
is often already sufficient — 6 of 15 features tie. engram's retrieval edge (Lens 2) should convert
to a larger agent benefit on the user's **real AVM codebase**, which is far larger and messier than
these proxies — exactly where grep returns too much and semantic retrieval matters.

## Phase 7 — heuristic vs tree-sitter chunking (the grammar fork's lift), ndk-samples

Re-indexed ndk-samples with `ENGRAM_CODE_TREE_SITTER=false` (separate DB/`:8090`) and re-ran Lens 2.

| metric | tree-sitter | heuristic | Δ |
|--------|------|------|---|
| recall@1 | 0.371 | **0.486** | −0.115 |
| recall@5 | **0.829** | 0.771 | +0.058 |
| recall@10 | **0.886** | 0.857 | +0.029 |
| line-recall@10 | 0.593 | **0.815** | −0.222 |
| cpp recall@5 | **0.846** | 0.808 | +0.038 |
| java recall@5 | **1.0** | 0.5 | +0.50 |

**Mixed — not the clean win it was on TypeScript** (agentmemory: tree-sitter *raised* line-recall).
The fork clearly helps **Java** (clean definition boundaries → +0.50 recall@5) and file-level
recall@5, but **regresses line-recall and recall@1 on this C/C++-heavy repo**. Root cause: top-level
C structs/typedefs are wrapped in `declaration` nodes that `is_chunkable` doesn't isolate, so the
tight per-function chunks miss the gold line ranges that heuristic's larger windows happen to cover.
**Lever:** broaden the C/C++ node coverage (walk `declaration`/`linkage_specification` for
struct/typedef boundaries; resolve function names via `declarator`) before treating tree-sitter as a
clear win on native code. Both modes still crush ripgrep/native, so this is a second-order tuning
issue, not a regression vs the baselines.

## Re-measure — after the two fixes (2026-06-10)

Re-indexed all 3 repos on a binary carrying both fixes (fresh eval DB).

**Fix 2 — abstention floor: a clean win.** Sweeping `ENGRAM_CODE_MIN_SCORE` (offline post-filter on
the returned scores, `eval/harness/abstain_sweep.py`):

| floor | recall@5 | hard-neg FP |
|-------|----------|-------------|
| 0.00 (off) | 0.815 | 9/9 |
| 0.50 | 0.815 | 8/9 |
| 0.55 | 0.806 | 4/9 |
| **0.60** | **0.787** | **0/9** |
| 0.65 | 0.556 | 0/9 |

A floor of **0.60 eliminates all 9 hard-negative false positives at a ~3% recall cost**
(0.815 → 0.787). **Recommendation: `ENGRAM_CODE_MIN_SCORE=0.60`** on the real deployment.

**Fix 1 — C/C++ boundary chunking + declarator symbols: correct, but no metric change.** Lens-2 is
*identical* before and after (e.g. ndk-samples recall@5 0.829, line-recall@10 0.593; libxcam cpp@5
0.906). Why: the gold probes mostly target *functions* (already chunked as `function_definition`
boundaries pre-fix), and the new `sym:` entities aren't consumed by `search_code` ranking. So the
fix correctly isolates C/C++ structs into their own chunks and resolves C/C++ function names (a real
correctness improvement — useful for `find_symbol` and future symbol-index features), but the C/C++
line-recall characteristic vs heuristic is a **granularity tradeoff** (tight per-function chunks vs
heuristic's wider windows that overlap the gold range), *not* a missing-struct-boundary bug — the
hypothesis was wrong (cf. the earlier best-chunk dedup experiment). The remaining lever for native
line-recall is a **wider-window / hybrid chunk mode for C/C++**, not finer boundaries.

## Native-code chunk mode A/B — definition-packing for C/C++ (2026-06-10)

Prototyped `ENGRAM_CODE_NATIVE_PACK`: for C/C++, pack consecutive small definitions (+ gaps) into
wider, boundary-aligned chunks up to the token budget instead of one chunk per definition. Indexed
all 3 repos with it on (`:8091`) and re-ran Lens 2.

| repo | metric | no-pack | pack | Δ |
|------|--------|---------|------|---|
| ndk-samples | line-recall@10 | 0.593 | **0.667** | **+0.074** |
| | recall@10 | 0.886 | 0.914 | +0.028 |
| | recall@5 | 0.829 | 0.829 | 0 |
| libxcam | line-recall@10 | 0.92 | 0.92 | 0 (already at ceiling) |
| gpuimage-plus | line-recall@10 | 0.76 | 0.76 | 0 (large C++ defs don't merge) |

**A safe but narrow win.** Packing lifts ndk-samples line-recall +0.074 with **zero recall@5 cost**
(the intended shape — wider chunks recover line-recall, boundaries keep file recall), and is exactly
neutral on libxcam (already 0.92) and gpuimage (its C++ defs are large, so nothing merges). It
**never hurts**. But it closes only ~half the ndk gap to the heuristic's 0.815, because **both
heuristic and packed chunks are capped at the ~480-token embed budget** (mxbai's 512-token limit) —
packing can't widen native chunks further; the residual gap is boundary-aligned packing vs pure-line
windows. **Recommendation: keep it as an opt-in knob** (`ENGRAM_CODE_NATIVE_PACK`, default off);
enable for small-function native-heavy code. Full native line-recall parity would need overlapping
windows or a larger-context embed model — out of scope here.

## bge-m3 + wide native chunks (2026-06-10) — the native line-recall fix that WORKED

Set up bge-m3 locally (Ollama, GPU, 8k context — `~/start-ollama-wsl.sh`) and re-indexed ndk-samples
*through engram* with the new `ENGRAM_CODE_NATIVE_BUDGET`. Three configs, same gold:

| metric | A: mxbai @480 | B: bge-m3 @480 | C: bge-m3 @wide(1500) |
|--------|------|------|------|
| recall@1 | 0.371 | 0.286 | **0.514** |
| recall@5 | 0.829 | 0.800 | **0.829** |
| recall@10 | 0.886 | 0.857 | **0.943** |
| line-recall@10 | 0.593 | 0.630 | **0.889** |
| cpp recall@5 | 0.846 | 0.846 | **0.885** |

**Conclusive and attributed.** C (bge-m3 + wide native chunks) lifts native **line-recall
0.593 → 0.889** (+0.296 — *past* the heuristic's 0.815) while *holding* recall@5 (0.829) and
*improving* recall@1 (+0.143) and recall@10 (+0.057). The attribution is clean: **B (bge-m3 drop-in
at 480) barely moves line-recall (0.630) and slightly hurts recall@5/@1** — so the win is the
**wider chunks**, not bge-m3 per se. bge-m3's role is purely *enabling* the width — its 8k context
embeds 1500-token chunks without truncation, which mxbai's 512 cap cannot. The embed budget was the
real ceiling all along; a long-context model + wide native packing breaks through it. (Index was
fast: 95s / 0 failures via local bge-m3 on the GPU.)

**Validated recipe for native line-recall:** a long-context embedder (bge-m3, served locally on
Ollama/GPU) + `ENGRAM_CODE_NATIVE_PACK=true` + `ENGRAM_CODE_NATIVE_BUDGET=1500`, full re-index under
the `gateway:bge-m3:1024` signature. Caveat: ndk-samples is C/C++-heavy (where this shines), and
bge-m3 drop-in is slightly weaker on non-native recall@5 — so the net is strongest for native-heavy
code (AOSP AVM); verify on the real language mix. libxcam (line-recall already 0.92) and gpuimage
(0.76) were not re-tested with bge-wide.

## Verdict (for the real AVM deployment)

1. **Adopt engram for retrieval — strong, robust signal.** It beats grep/native by a wide margin on
   recall and dominates line-recall (precise `path:line`), the core value for finding migration code.
2. **Agent benefit is real but modest at this scale**, concentrated in cross-layer coverage; expect
   it to grow on the larger/messier real AVM repo. grep stays useful — keep engram *additive*.
3. **Two fixes implemented + re-measured** (see "Re-measure" above):
   - **Abstention floor — ship it.** `ENGRAM_CODE_MIN_SCORE=0.60` zeroes the hard-negative false
     positives at ~3% recall cost.
   - **C/C++ line-recall — SOLVED** (0.593 → 0.889; see the bge-m3 section). The struct-boundary fix
     and pack@480 were neutral; the real ceiling was the **512-token embed budget**. A long-context
     embedder (bge-m3, local Ollama/GPU) + `ENGRAM_CODE_NATIVE_PACK=true` + `ENGRAM_CODE_NATIVE_BUDGET=1500`
     breaks through it, at the cost of a full re-index under the bge-m3 signature (best for native-heavy repos).
4. **Indexing is reliable** at this scale (0 failures after the `ENGRAM_INDEX_TIMEOUT_SECS` fix; a
   few unrelated `.py` 500s on libxcam).

## Caveats / not measured
- Kotlin under-represented in the proxies (they're Java+C/C++); the grammar fork's Kotlin path is
  unit-tested but not exercised at scale here.
- Shallow clones (`--depth 1`) → no git history, so `why` was not evaluated.
- Qwen3.6 arm built + adapter-ready but not run (no gateway access yet).
- Android builds not used as a success gate (footprint + path-set scoring instead).
- Small proxies — the headline caveat on Lens 1's modest agent delta.
