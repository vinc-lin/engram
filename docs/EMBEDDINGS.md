# Embeddings in engram

How engram embeds, which model to use when, where they're served, and the operational gotchas.
Embeddings are the one config choice that is **sticky** (changing it orphans data), so this is worth
getting right before a bulk index. Companion to `CLAUDE.md`, `docs/ROADMAP.md`, and
`eval/RESULTS_android.md`.

## TL;DR — which embedder

| Corpus / use | Embedder | Why |
|--------------|----------|-----|
| **Prose memory, general code retrieval** | **`mxbai-embed-large`** (dim 1024, 512-tok ctx) | Production default. Measured ≥ bge-m3 as a drop-in (see Evidence). Served on the gateway and *works*. |
| **Native code (C/C++) with wide chunks** | **`bge-m3`** (dim 1024, 8k ctx) + `ENGRAM_CODE_NATIVE_PACK=true` + `ENGRAM_CODE_NATIVE_BUDGET≈1500` | Only a long-context model can embed chunks wider than mxbai's 512-tok cap. Lifts native line-recall 0.59→0.89. **Serve bge-m3 locally** (the gateway route is broken). |
| **Tests** | `HashEmbedder` (deterministic, network-free) | No network; `cargo test` never touches a backend. |

**Do not swap the embedder for a general "upgrade."** bge-m3 is *not* a better embedder than mxbai
for prose — it was slightly worse in our A/B. Its only proven win is **enabling wide native chunks**.
Swap only when that specific lever applies, and budget for a full re-index.

## The signature invariant (why the choice is sticky)

Every chunk and tree node stores the embedder's `signature()` — e.g. `gateway:mxbai-embed-large:1024`,
`hash:64`. Reads filter by it (`chunks_for_namespace(ns, sig)`). **Changing `ENGRAM_EMBED_MODEL` or
`ENGRAM_EMBED_DIM` makes all existing chunks invisible** (orphaned, not migrated) — a full re-ingest
under the new signature is required. There is no online migration. Pin the model/dim before a bulk
index and treat any change as a re-index event.

`FallbackEmbedder`'s `signature()` **delegates to the primary**, so a primary→fallback failover never
writes chunks under a different signature (no silent orphaning).

## The models

| Model | Dim | Context | Where served | Notes |
|-------|-----|---------|--------------|-------|
| `mxbai-embed-large` | 1024 | **512 tok** | litellm gateway (`ENGRAM_GATEWAY_URL`) | Production. BERT-large class. The 512-tok cap is the ceiling that motivated native-pack. |
| `bge-m3` | 1024 | **8192 tok** | **local** WSL Ollama/GPU (`http://127.0.0.1:11434`) | Same dim as mxbai (drop-in dims), long context. Gateway route is **broken** — serve locally. |
| `HashEmbedder` | 64 | n/a | in-process | Tests only; bag-of-words, deterministic. |

Embedder code lives in `crates/engram/src/embed.rs`: the `Embedder` trait + `HashEmbedder`,
`OllamaEmbedder`, `GatewayEmbedder` (prod), `FallbackEmbedder` (primary+fallback wrapper).

## Serving topology & the two gotchas

```
   ENGRAM_GATEWAY_URL (LLM/chat) ──────────────► litellm gateway ──► mxbai  ✅ 200
                                                                   ├─► bge-m3 ❌ 500 (dead route)
                                                                   └─► deepseek-chat ✅ (LLM model)

   ENGRAM_EMBED_URL (embeddings; defaults to ───► local WSL Ollama :11434 ──► bge-m3 ✅ (GPU)
     gateway_url, override to decouple)            [embeddings only — LLM stays on the gateway]
```

1. **The gateway's `bge-m3` route is broken.** `POST {gateway}/v1/embeddings` with `mxbai-embed-large`
   returns 200; with `bge-m3` it returns 500 (the gateway forwards to an Ollama host that isn't there).
   The *working* bge-m3 is the **local** one — start it with `~/start-ollama-wsl.sh` (Linux Ollama on
   the GPU, serving `/v1/embeddings`). See `eval/RESULTS_android.md` for the setup story.

2. **Embedder and LLM can use separate endpoints — `ENGRAM_EMBED_URL`.** By default both ride
   `ENGRAM_GATEWAY_URL`. Set `ENGRAM_EMBED_URL` to send embeddings to a different backend — e.g. the
   local bge-m3 Ollama — while chat/LLM calls (consolidation summaries, audits) stay on the gateway
   (deepseek). This is the clean way to pair **reliable local embeddings** with a **real gateway LLM**,
   and it replaces the old fallback hack (which paid the gateway's 500-retry latency on every embed).

3. **Concurrent workers can overload the gateway embed endpoint** → transient 500s. The cold-path
   consolidation embeds run on `ENGRAM_JOBS_WORKERS` threads; several in parallel can burst the gateway
   into 500s (which engram retries with backoff). If you see `embed: HTTP 500` storms, **lower
   `ENGRAM_JOBS_WORKERS`** (serial embeds don't trip it) or let the retry ride it out.

## The 512-token ceiling (the real native-code lever)

The native line-recall fix was **not** a better embedder — it was a **wider chunk**. mxbai truncates
input at 512 tokens, so a packed C/C++ chunk wider than that is silently clipped and its tail never
embeds. A long-context model (bge-m3, 8k) embeds the whole wide chunk, so the win comes from
`ENGRAM_CODE_NATIVE_BUDGET≈1500` *paired with* a long-context embedder. The lever is **chunk width /
embed budget**, not the model identity. (Attribution: bge-m3 at the default 480 budget barely moved
line-recall; only bge-m3 + wide did.)

## Config knobs (`crates/engram/src/config.rs`)

| Env var | Default | Meaning |
|---------|---------|---------|
| `ENGRAM_EMBED_MODEL` | `mxbai-embed-large` | Embedding model name sent to the backend. Part of the signature. |
| `ENGRAM_EMBED_DIM` | `1024` | Vector dim. Part of the signature. Must match the model. |
| `ENGRAM_EMBED_TIMEOUT_SECS` | `30` | Per-request embed HTTP timeout. |
| `ENGRAM_EMBED_FALLBACK` | `false` | Wrap `GatewayEmbedder` with a local `OllamaEmbedder` failover. |
| `ENGRAM_OLLAMA_URL` | `http://127.0.0.1:11434` | Fallback (or primary, if URL points here) Ollama endpoint. |
| `ENGRAM_EMBED_URL` | = `ENGRAM_GATEWAY_URL` | Embeddings endpoint, **decoupled** from the LLM URL. Point at a local Ollama for reliable embeds while the LLM stays on the gateway. |
| `ENGRAM_CODE_NATIVE_PACK` | `false` | Pack consecutive C/C++ defs into wider boundary-aligned chunks. |
| `ENGRAM_CODE_NATIVE_BUDGET` | `480` | Token budget for native packing. Raise to ~1500 with a long-context embedder. |

Changing the model/dim ⇒ re-index. The native-pack knobs only help with a long-context embedder
(raising the budget under mxbai just clips at 512).

## Operational playbook

- **Production / prose / mixed code:** `mxbai-embed-large` dim 1024 via the gateway. Leave native-pack
  off. This is the deploy default and the only gateway embed route that works.
- **Native-heavy repo (AOSP/AVM C/C++):** serve bge-m3 locally (`~/start-ollama-wsl.sh`), set
  `ENGRAM_EMBED_URL=http://127.0.0.1:11434` (embeddings local) while `ENGRAM_GATEWAY_URL` keeps the LLM
  on the gateway, plus `ENGRAM_EMBED_MODEL=bge-m3`, `ENGRAM_EMBED_DIM=1024`,
  `ENGRAM_CODE_NATIVE_PACK=true`, `ENGRAM_CODE_NATIVE_BUDGET=1500`; full re-index under
  `gateway:bge-m3:1024`.
- **Gateway embed 500 storm:** drop `ENGRAM_JOBS_WORKERS` to 1; embeds serialize and stop overloading.
- **Never** mix signatures in one namespace — a half-migrated namespace silently returns zero recall
  for the orphaned half.

## Evidence (measured A/Bs)

- **Prose (agentmemory gold), drop-in mxbai vs bge-m3, identical chunks** — recall@1/5/10:
  **mxbai 0.267 / 0.600 / 0.600** vs **bge-m3 0.067 / 0.333 / 0.533**; mxbai ranked the gold file
  better on 9/15 queries. → bge-m3 is **not** a prose upgrade. (`docs/ROADMAP.md` Findings.)
- **Native code (ndk-samples), 3-way** — line-recall@10: **mxbai@480 0.593**, **bge-m3@480 0.630**
  (drop-in barely helps), **bge-m3@wide(1500) 0.889**; recall@5 held at 0.829. → the win is chunk
  **width** enabled by bge-m3's long context, not the model. (`eval/RESULTS_android.md`.)
