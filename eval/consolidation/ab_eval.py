#!/usr/bin/env python3
"""Consolidation Value A/B/C harness (Track 2c — the decision gate).

Does drill_down over the consolidated tree (Arm A) actually answer cross-document
synthesis questions better than flat chunk retrieval (Arm B)? If it does not beat
B by the margin, consolidation stays off — for prose namespaces too.

Three arms, one answerer (deepseek-chat) held constant across all arms:
  A  drill_down   -> POST /v1/{ns}/tree   (BFS over the consolidated Global tree)
  B  chunk query  -> POST /v1/{ns}/query  (flat top-matching docs, windowed)
  C  one-shot     -> a single deepseek summary of the whole corpus (length-matched)
Each arm's context is the ONLY context the answerer gets. Answers are scored by a
blind 0-2 multi-judge (JUDGE_CALLS=3, median per (question, arm)) against the gold
ground_truth.

GATE (MARGIN = 0.20): sys.exit(0 if (A_mean - B_mean) >= 0.20 else 1).
If A - B >= 0.20, consolidation provides measurable synthesis benefit and
ENGRAM_CONSOLIDATE_CODE=true may be enabled for prose namespaces; otherwise off.

LIVE RESOURCES REQUIRED (cost-bearing, ~20 min, ~400 LLM calls — NOT for CI):
  * A running engram (ENGRAM_URL, ENGRAM_TOKEN) whose target namespace has ALREADY
    been primed and DRAINED with consolidation on. Prime it via the distill
    harness (~/engram-eval/distill/harness.py) which ingests engram's docs and
    waits for the Global tree to seal, then run this against the same namespace.
  * A real LLM gateway: ENGRAM_GATEWAY_URL + ENGRAM_GATEWAY_KEY routing
    deepseek-chat (the answerer + judge + Arm-C summarizer).
Without these the script raises KeyError('ENGRAM_TOKEN') or connection-refused.

Outputs (under OUT, default ~/engram-eval/consolidation/):
  answers.json   every (question, arm) context + answer
  scores.json    per-judge + median scores, arm means
  RESULTS.md     the summary table + PASS/FAIL verdict
"""
import json, os, re, statistics, sys, time, urllib.error, urllib.request

ROOT = os.environ.get("ENGRAM_ROOT", "/mnt/x/code/engram")
BASE = os.environ.get("ENGRAM_URL", "http://127.0.0.1:8095").rstrip("/")
TOK = os.environ["ENGRAM_TOKEN"]
GURL = os.environ["ENGRAM_GATEWAY_URL"].rstrip("/")
GKEY = os.environ["ENGRAM_GATEWAY_KEY"]
NS = os.environ.get("ENGRAM_NS", "distill")
OUT = os.environ.get("AB_OUT", os.path.expanduser("~/engram-eval/consolidation"))
GOLD = os.environ.get("AB_GOLD",
                      os.path.join(os.path.dirname(__file__), "synthesis_gold.json"))
CHAT_MODEL = os.environ.get("CHAT_MODEL", "deepseek-chat")

MARGIN = 0.20
JUDGE_CALLS = 3

CORPUS = ["README.md", "ARCHITECTURE.md", "CLAUDE.md", "docs/ROADMAP.md", "eval/RESULTS.md"]


def http_post(url, body, headers, timeout=240, method=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data,
                                 method=method or ("POST" if data is not None else "GET"),
                                 headers=headers)
    for attempt in range(4):
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                raw = r.read()
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503) and attempt < 3:
                time.sleep(2 * (attempt + 1)); continue
            raise
        except Exception:
            if attempt < 3:
                time.sleep(2 * (attempt + 1)); continue
            raise


def eng(path, body):
    return http_post(f"{BASE}/v1/{NS}{path}", body,
                     {"Authorization": f"Bearer {TOK}", "Content-Type": "application/json"})


def gw_chat(system, user, max_tokens=400, temp=0.0):
    d = http_post(f"{GURL}/v1/chat/completions",
                  {"model": CHAT_MODEL, "temperature": temp, "max_tokens": max_tokens,
                   "messages": [{"role": "system", "content": system},
                                {"role": "user", "content": user}]},
                  {"Authorization": f"Bearer {GKEY}", "Content-Type": "application/json"},
                  timeout=240)
    return d["choices"][0]["message"]["content"].strip()


def clip(s, n):
    return s if len(s) <= n else s[:n] + " ...[truncated]"


def node_text(n):
    for k in ("summary", "text", "body", "content", "title"):
        v = n.get(k) if isinstance(n, dict) else None
        if v:
            return str(v)
    return json.dumps(n)[:500]


def ctx_A(q):  # Arm A: consolidated drill_down
    nodes = eng("/tree", {"query": q, "limit": 6})
    return clip("\n\n---\n\n".join(node_text(n) for n in nodes), 6000) if nodes else "(empty)"


def window(text, query, size=2000):
    words = sorted(set(re.findall(r"[A-Za-z_]{4,}", query.lower())), key=len, reverse=True)
    low = text.lower()
    for w in words:
        i = low.find(w)
        if i >= 0:
            s = max(0, i - 300)
            return text[s:s + size]
    return text[:size]


def ctx_B(q):  # Arm B: flat chunk query
    hits = eng("/query", {"query": q, "limit": 3})
    parts = [f"[{h.get('key','?')}]\n{window(h.get('content',''), q, 2000)}" for h in hits]
    return clip("\n\n---\n\n".join(parts), 6000) if parts else "(empty)"


def build_ctx_C():  # Arm C: one length-matched one-shot summary of the whole corpus
    docs = {}
    for rel in CORPUS:
        p = os.path.join(ROOT, rel)
        if os.path.exists(p):
            docs[rel] = open(p, encoding="utf-8", errors="replace").read()
    corpus_text = "\n\n==== ".join(f"{rel} ====\n{txt}" for rel, txt in docs.items())
    print("building one-shot summary (arm C)...")
    summary = gw_chat(
        "You are summarizing a software project's documentation into a single "
        "architecture+conventions overview an engineer would read before working on it. "
        "Be dense and specific; preserve concrete facts (names, defaults, invariants). "
        "Target ~900 words.",
        clip(corpus_text, 110000), max_tokens=1500, temp=0.0)
    print(f"  summary C: {len(summary)} chars")
    return summary


ANS_SYS = ("You are given CONTEXT about a software project and a QUESTION. Answer the question "
           "using ONLY information in the CONTEXT. Be concise (2-5 sentences). If the context does "
           "not contain the answer, reply exactly: Not in context.")


def answer(ctx, q):
    return gw_chat(ANS_SYS, f"CONTEXT:\n{ctx}\n\nQUESTION: {q}", max_tokens=350, temp=0.0)


JUDGE_SYS = ("You are a strict grader. Given a QUESTION, a reference GROUND_TRUTH, and a candidate "
             "ANSWER, score how well the ANSWER captures the ground truth on a 0-2 scale:\n"
             "  0 = wrong, missing, or 'Not in context'\n"
             "  1 = partially correct (some key facts, but incomplete or with an error)\n"
             "  2 = fully correct (all the key facts, no contradictions)\n"
             "Respond with ONLY the single digit 0, 1, or 2.")


def judge_once(question, ground_truth, ans):
    out = gw_chat(JUDGE_SYS,
                  f"QUESTION: {question}\n\nGROUND_TRUTH: {ground_truth}\n\nANSWER: {ans}",
                  max_tokens=4, temp=0.0)
    m = re.search(r"[012]", out)
    return int(m.group(0)) if m else 0


def judge_median(question, ground_truth, ans):
    scores = [judge_once(question, ground_truth, ans) for _ in range(JUDGE_CALLS)]
    return int(statistics.median(scores)), scores


def arm_stats(scores_by_arm):
    out = {}
    for arm, scores in scores_by_arm.items():
        out[arm] = statistics.mean(scores) if scores else 0.0
    return out


def write_results_md(path, means, n, verdict, gap):
    lines = []
    lines.append("# Consolidation Value A/B/C — RESULTS\n")
    lines.append(f"- namespace: `{NS}`  questions: {n}  answerer/judge: `{CHAT_MODEL}`")
    lines.append(f"- judge: blind 0-2, median of {JUDGE_CALLS} calls per (question, arm)")
    lines.append(f"- gate: A - B >= {MARGIN}\n")
    lines.append("| Arm | Description | Mean score (0-2) |")
    lines.append("|-----|-------------|------------------|")
    desc = {"A_drill_down": "drill_down over consolidated tree (POST /tree)",
            "B_chunk_query": "flat chunk query (POST /query)",
            "C_one_shot": "one-shot deepseek corpus summary"}
    for arm in ("A_drill_down", "B_chunk_query", "C_one_shot"):
        if arm in means:
            lines.append(f"| {arm} | {desc[arm]} | {means[arm]:.3f} |")
    lines.append("")
    lines.append(f"**A - B = {gap:.3f}**  (gate >= {MARGIN}) -> **{verdict}**\n")
    open(path, "w").write("\n".join(lines))


def main():
    os.makedirs(OUT, exist_ok=True)
    gold = json.load(open(GOLD))
    questions = gold["questions"]
    print(f"engram={BASE} ns={NS} questions={len(questions)} gate(A-B>={MARGIN})")

    summary_C = build_ctx_C()

    answers = []
    for i, item in enumerate(questions):
        q = item["question"]
        gt = item["ground_truth"]
        ctxs = {"A_drill_down": ctx_A(q), "B_chunk_query": ctx_B(q), "C_one_shot": summary_C}
        for arm, ctx in ctxs.items():
            ans = answer(ctx, q)
            answers.append({"qid": item["id"], "question": q, "ground_truth": gt,
                            "arm": arm, "answer": ans, "ctx_chars": len(ctx)})
        print(f"  answered {item['id']} ({i + 1}/{len(questions)})")
    json.dump(answers, open(os.path.join(OUT, "answers.json"), "w"), indent=1)

    scores_by_arm = {"A_drill_down": [], "B_chunk_query": [], "C_one_shot": []}
    score_rows = []
    for rec in answers:
        med, raw = judge_median(rec["question"], rec["ground_truth"], rec["answer"])
        scores_by_arm[rec["arm"]].append(med)
        score_rows.append({"qid": rec["qid"], "arm": rec["arm"],
                           "median": med, "judges": raw})
    means = arm_stats(scores_by_arm)
    json.dump({"means": means, "rows": score_rows},
              open(os.path.join(OUT, "scores.json"), "w"), indent=1)

    a = means.get("A_drill_down", 0.0)
    b = means.get("B_chunk_query", 0.0)
    gap = a - b
    ok = gap >= MARGIN
    verdict = "PASS" if ok else "FAIL"

    print("\n=== arm means (0-2) ===")
    for arm in ("A_drill_down", "B_chunk_query", "C_one_shot"):
        if arm in means:
            print(f"  {arm:<14} {means[arm]:.3f}")
    print(f"\nA - B = {gap:.3f}  (gate >= {MARGIN}) -> RESULT: {verdict}")

    write_results_md(os.path.join(OUT, "RESULTS.md"), means, len(questions), verdict, gap)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
