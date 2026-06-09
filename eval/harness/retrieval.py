"""Lens 2 — retrieval vs baselines.

Runs three retrievers over the AVM gold probes and scores them with shared metrics:
  - engram   : POST /v1/{ns}/code/search  (the system under test)
  - ripgrep  : OR the query's identifier terms, rank files by match count (what an agent greps)
  - native   : rank git-tracked files by filename/path token overlap (the no-search floor)

Reports recall@1/5/10, line-recall@k, per-language recall, cross-layer coverage, hard-neg FPs.

  python3 eval/harness/retrieval.py --gold eval/android/avm_gold.json \
      --url http://127.0.0.1:8089 --token "$ENGRAM_TOKEN" [--retrievers engram,ripgrep,native]
"""

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import metrics as M  # noqa: E402

STOP = set("the a an is are was how where what which when who why do does to of in on for and or "
           "with that this it its as by from into be used use using get set new make handle".split())
HARD_NEG_SCORE = 0.55
KS = (1, 5, 10)


def terms(q):
    toks = re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", q)
    return [t for t in toks if t.lower() not in STOP]


def expand_path(p):
    return os.path.realpath(os.path.expanduser(p))


# ── retrievers: each returns ranked list of {path, line_start, line_end, score} ──────────────
def engram_retrieve(url, ns, token, query, k):
    body = json.dumps({"query": query, "limit": k}).encode()
    req = urllib.request.Request(f"{url.rstrip('/')}/v1/{ns}/code/search", data=body,
                                 headers={"Authorization": f"Bearer {token}",
                                          "Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            hits = json.load(resp)
    except Exception as e:  # noqa: BLE001
        print(f"  engram error on {query[:40]!r}: {e}", file=sys.stderr)
        return []
    return [{"path": h.get("path"), "line_start": h.get("line_start"),
             "line_end": h.get("line_end"), "score": h.get("score", 0.0)} for h in hits]


def ripgrep_retrieve(repo, query, k):
    ts = terms(query)
    if not ts:
        return []
    pat = "|".join(re.escape(t) for t in ts)
    try:
        r = subprocess.run(["rg", "--line-number", "--no-heading", "-i", "-S", pat, repo],
                           capture_output=True, text=True, timeout=60)
    except Exception:  # noqa: BLE001
        return []
    by_file = {}  # path -> (match_count, first_line)
    for line in (r.stdout or "").splitlines():
        parts = line.split(":", 2)
        if len(parts) < 2:
            continue
        path = os.path.relpath(parts[0], repo)
        try:
            ln = int(parts[1])
        except ValueError:
            continue
        cnt, first = by_file.get(path, (0, ln))
        by_file[path] = (cnt + 1, min(first, ln))
    ranked = sorted(by_file.items(), key=lambda kv: -kv[1][0])[:k]
    return [{"path": p, "line_start": fl, "line_end": fl, "score": float(c)}
            for p, (c, fl) in ranked]


def native_retrieve(repo, query, k):
    ts = [t.lower() for t in terms(query)]
    files = subprocess.run(["git", "-C", repo, "ls-files"], capture_output=True, text=True).stdout.split()
    scored = []
    for f in files:
        fl = f.lower()
        score = sum(1 for t in ts if t in fl)
        if score:
            scored.append((score, f))
    scored.sort(key=lambda x: -x[0])
    return [{"path": p, "line_start": None, "line_end": None, "score": float(s)}
            for s, p in scored[:k]]


RETRIEVERS = {"engram": engram_retrieve, "ripgrep": ripgrep_retrieve, "native": native_retrieve}


def run_dataset(ds, url, token, which):
    repo = expand_path(ds["repo"])
    ns = ds["namespace"]
    queries = ds["queries"]
    features = {f["id"]: f for f in ds.get("features", [])}
    results = {}

    for name in which:
        rec = {k: 0 for k in KS}
        lrec = {k: 0 for k in KS}
        per_lang = {}  # lang -> [hits, total]
        n_lines = 0
        feat_hits = {fid: [] for fid in features}  # union of hits across a feature's queries

        for q in queries:
            if name == "engram":
                hits = engram_retrieve(url, ns, token, q["q"], 10)
            elif name == "ripgrep":
                hits = ripgrep_retrieve(repo, q["q"], 10)
            else:
                hits = native_retrieve(repo, q["q"], 10)
            paths = [h["path"] for h in hits]
            golds = q["gold"]
            for k in KS:
                if M.recall_hit(golds, paths, k):
                    rec[k] += 1
            if q.get("gold_lines"):
                n_lines += 1
                for k in KS:
                    if M.line_hit(q["gold_lines"], hits, k):
                        lrec[k] += 1
            lang = q.get("lang", "?")
            pl = per_lang.setdefault(lang, [0, 0])
            pl[1] += 1
            if M.recall_hit(golds, paths, 5):
                pl[0] += 1
            if q.get("feature") in feat_hits:
                feat_hits[q["feature"]].extend(paths)

        nq = len(queries)
        cov = []
        for fid, f in features.items():
            c, _, _ = M.layer_coverage(f["footprint"], feat_hits[fid])
            cov.append(c)
        # hard negatives (engram only — needs a real score)
        fp = 0
        if name == "engram":
            for hn in ds.get("hard_negatives", []):
                h = engram_retrieve(url, ns, token, hn["q"], 1)
                if h and h[0]["score"] >= HARD_NEG_SCORE:
                    fp += 1

        results[name] = {
            "recall": {k: round(rec[k] / nq, 3) for k in KS},
            "line_recall": {k: round(lrec[k] / n_lines, 3) if n_lines else None for k in KS},
            "per_lang_recall5": {lg: round(v[0] / v[1], 3) for lg, v in sorted(per_lang.items())},
            "cross_layer_coverage": round(sum(cov) / len(cov), 3) if cov else None,
            "hard_neg_fp": f"{fp}/{len(ds.get('hard_negatives', []))}" if name == "engram" else "-",
        }
    return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", required=True)
    ap.add_argument("--url", default="http://127.0.0.1:8089")
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--retrievers", default="engram,ripgrep,native")
    args = ap.parse_args()

    gold = json.load(open(args.gold))
    datasets = gold.get("datasets", [gold])  # support single-dataset or multi
    which = [w.strip() for w in args.retrievers.split(",")]

    for ds in datasets:
        print(f"\n########## {ds['namespace']}  ({len(ds['queries'])} probes) ##########")
        res = run_dataset(ds, args.url, args.token, which)
        for name in which:
            r = res[name]
            print(f"\n[{name}]")
            print(f"  recall@1/5/10 : {r['recall'][1]} / {r['recall'][5]} / {r['recall'][10]}")
            if r["line_recall"][10] is not None:
                print(f"  line-recall@10: {r['line_recall'][10]}")
            print(f"  cross-layer coverage: {r['cross_layer_coverage']}")
            print(f"  per-language recall@5: {r['per_lang_recall5']}")
            print(f"  hard-neg FPs: {r['hard_neg_fp']}")


if __name__ == "__main__":
    main()
