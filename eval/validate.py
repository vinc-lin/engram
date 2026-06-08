#!/usr/bin/env python3
"""Validate engram's code knowledge base on a labeled corpus (default: agentmemory).

Bars:
  1. Ingest success rate  >= 0.99  — of the indexable git-tracked files, how many are
     present in engram (checked via GET /docs/by-key).
  2. recall@5             >= 0.80  — headline bar (Phase F tightened it from recall@10).
                                     recall@1/@5/@10 are all reported.

Optional per-query enrichments (Phase F):
  - "gold_lines": [{"file": "...", "line_range": [s, e]}]  → a *line-range hit* requires a
    returned chunk for that file whose [line_start,line_end] overlaps [s,e] (not just the file
    in top-k). Reported as line-recall@k over the queries that carry gold_lines.
  - top-level "hard_negatives": [{"q": "..."}]  → queries whose answer is NOT in the corpus;
    informational — flags any that still return a *confident* top-1 (score >= HARD_NEG_SCORE).

Run `engram-index index <repo> --namespace <ns>` first (or pass --index). Requires a running
engram and a healthy embed backend (every query embeds the query text).

Usage:
  ENGRAM_TOKEN=... python3 eval/validate.py [--gold eval/agentmemory_gold.json]
                                            [--url http://127.0.0.1:8088]
                                            [--index] [--repo PATH] [--k 10]
"""
import argparse, json, os, subprocess, sys, urllib.parse, urllib.request, urllib.error

# File-selection rules — mirror crates/engram-index/src/walk.rs::should_index.
BIN_EXT = {"svg","png","gif","jpg","jpeg","ico","webp","woff","woff2","ttf","eot",
           "pdf","mp3","mp4","wav","lockb"}
LOCKFILES = {"package-lock.json","pnpm-lock.yaml","yarn.lock","Cargo.lock","bun.lockb"}
CAP = 256_000
HARD_NEG_SCORE = 0.55   # a hard-negative query returning top-1 >= this is a likely false positive

def indexable_files(repo):
    out = subprocess.check_output(["git","-C",repo,"ls-files"], text=True).splitlines()
    keep = []
    for f in out:
        ext = f.rsplit(".",1)[-1].lower() if "." in f else ""
        base = os.path.basename(f)
        if ext in BIN_EXT or f.endswith(".map") or f.endswith(".min.js"): continue
        if base in LOCKFILES: continue
        p = os.path.join(repo, f)
        try:
            if os.path.getsize(p) > CAP: continue
            open(p, encoding="utf-8").read()
        except Exception:
            continue
        keep.append(f)
    return keep

def http(method, url, token, body=None, timeout=60):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method,
        headers={"Authorization": f"Bearer {token}",
                 "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except Exception as e:
        return 0, str(e).encode()

def search(url, ns, token, query, limit):
    """Return (status, [hit dicts in rank order]) for POST /code/search."""
    st, raw = http("POST", f"{url}/v1/{ns}/code/search", token, {"query": query, "limit": limit})
    if st != 200:
        return st, []
    try:
        return st, json.loads(raw)
    except Exception:
        return st, []

def overlaps(h, lo, hi):
    s, e = h.get("line_start"), h.get("line_end")
    return s is not None and e is not None and s <= hi and e >= lo

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", default=os.path.join(os.path.dirname(__file__), "agentmemory_gold.json"))
    ap.add_argument("--url", default=os.environ.get("ENGRAM_URL", "http://127.0.0.1:8088"))
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--k", type=int, default=10, help="extra recall@k to also report")
    ap.add_argument("--index", action="store_true", help="run `engram-index index` first")
    ap.add_argument("--repo", default=None, help="override the repo path from the gold file")
    args = ap.parse_args()

    gold = json.load(open(args.gold))
    ns = gold["namespace"]
    repo = args.repo or gold["repo"]
    files = indexable_files(repo)
    KS = sorted({1, 5, 10, args.k})
    maxk = max(KS)

    if args.index:
        print(f"indexing {repo} -> {ns} ...", flush=True)
        subprocess.check_call(["cargo","run","-q","-p","engram-index","--","index",repo,
                               "--namespace",ns,"--url",args.url,"--token",args.token])

    # --- Bar 1: ingest success rate (by-key presence) ---
    present = 0
    for f in files:
        u = f"{args.url}/v1/{ns}/docs/by-key/{urllib.parse.quote(f, safe='')}"
        st, _ = http("GET", u, args.token, timeout=30)
        if st == 200: present += 1
    rate = present / len(files) if files else 0.0

    # --- Bar 2: recall@k (path) + line-recall@k ---
    queries = gold["queries"]
    path_hits = {k: 0 for k in KS}
    line_hits = {k: 0 for k in KS}
    line_total = 0
    details = []
    for item in queries:
        st, hits = search(args.url, ns, args.token, item["q"], maxk)
        paths = [h.get("path") for h in hits]
        golds = item["gold"]
        gl = item.get("gold_lines")
        if gl: line_total += 1
        rank = next((i + 1 for i, p in enumerate(paths) if p in golds), None)
        for k in KS:
            if any(g in paths[:k] for g in golds):
                path_hits[k] += 1
            if gl and any(
                any(h.get("path") == g["file"] and overlaps(h, g["line_range"][0], g["line_range"][1])
                    for h in hits[:k])
                for g in gl
            ):
                line_hits[k] += 1
        details.append((rank, golds[0], item["q"][:50], st, paths[0] if paths else "-"))

    n = len(queries)
    recall = {k: (path_hits[k] / n if n else 0.0) for k in KS}
    line_recall = {k: (line_hits[k] / line_total if line_total else None) for k in KS}

    # --- hard negatives (informational) ---
    negs = gold.get("hard_negatives", [])
    neg_fp = []
    for item in negs:
        st, hits = search(args.url, ns, args.token, item["q"], 1)
        top = hits[0] if hits else None
        if top and top.get("score", 0) >= HARD_NEG_SCORE:
            neg_fp.append((item["q"][:50], top.get("path"), round(top.get("score", 0), 3)))

    # --- report ---
    print("\n=== ingest ===")
    print(f"  indexable files: {len(files)}   present: {present}")
    print(f"  success rate: {rate:.3f}   (bar >= 0.99  -> {'PASS' if rate>=0.99 else 'FAIL'})")

    print("\n=== recall (path; doc = best chunk) ===")
    for k in KS:
        tag = "  <== headline bar >= 0.80" if k == 5 else ""
        print(f"  recall@{k:<2}: {recall[k]:.3f}{tag}")
    if line_total:
        print(f"\n=== line-recall (over {line_total} queries with gold_lines) ===")
        for k in KS:
            print(f"  line-recall@{k:<2}: {line_recall[k]:.3f}")

    print("\n=== per-query (rank of first gold; >N = not in top-%d) ===" % maxk)
    for rank, g, q, st, top in details:
        r = str(rank) if rank else f">{maxk}"
        print(f"  rank={r:<4} {q:<50} gold={g}  top1={top}  (http {st})")

    if negs:
        print(f"\n=== hard negatives ({len(negs)}) — likely false positives (top1 score >= {HARD_NEG_SCORE}) ===")
        if neg_fp:
            for q, p, sc in neg_fp:
                print(f"  [FP] {q:<50} top1={p} score={sc}")
        else:
            print("  none — no hard-negative returned a confident top-1.")

    ok = rate >= 0.99 and recall.get(5, 0.0) >= 0.80
    print(f"\nRESULT: {'PASS' if ok else 'FAIL'}  (ingest {rate:.3f}, recall@5 {recall.get(5,0):.3f})")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
