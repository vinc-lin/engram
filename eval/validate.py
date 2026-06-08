#!/usr/bin/env python3
"""Validate engram's code knowledge base on a labeled corpus (default: agentmemory).

Measures two acceptance bars:
  1. Ingest success rate  >= 0.99  — of the indexable git-tracked files, how many are
     present in engram (checked via GET /docs/by-key).
  2. recall@10            >= 0.80  — for each labeled query, is a gold file in the top-10
     results of POST /code/search.

It does NOT ingest; run `engram-index index <repo> --namespace <ns>` first (or pass
--index to have this script invoke it). Requires a running engram and a healthy embed
backend (every query embeds the query text).

Usage:
  ENGRAM_TOKEN=... python3 eval/validate.py [--gold eval/agentmemory_gold.json]
                                            [--url http://127.0.0.1:8088]
                                            [--index] [--k 10]
"""
import argparse, json, os, subprocess, sys, urllib.parse, urllib.request, urllib.error

# File-selection rules — mirror crates/engram-index/src/walk.rs::should_index.
BIN_EXT = {"svg","png","gif","jpg","jpeg","ico","webp","woff","woff2","ttf","eot",
           "pdf","mp3","mp4","wav","lockb"}
LOCKFILES = {"package-lock.json","pnpm-lock.yaml","yarn.lock","Cargo.lock","bun.lockb"}
CAP = 256_000

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

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", default=os.path.join(os.path.dirname(__file__), "agentmemory_gold.json"))
    ap.add_argument("--url", default=os.environ.get("ENGRAM_URL", "http://127.0.0.1:8088"))
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--index", action="store_true", help="run `engram-index index` first")
    ap.add_argument("--repo", default=None, help="override the repo path from the gold file")
    args = ap.parse_args()

    gold = json.load(open(args.gold))
    ns = gold["namespace"]
    repo = args.repo or gold["repo"]
    files = indexable_files(repo)

    if args.index:
        print(f"indexing {repo} -> {ns} ...", flush=True)
        subprocess.check_call(["cargo","run","-q","-p","engram-index","--","index",repo,
                               "--namespace",ns,"--url",args.url,"--token",args.token])

    # --- Bar 1: ingest success rate (by-key presence) ---
    present = 0
    for f in files:
        url = f"{args.url}/v1/{ns}/docs/by-key/{urllib.parse.quote(f, safe='')}"
        st, _ = http("GET", url, args.token, timeout=30)
        if st == 200: present += 1
    rate = present / len(files) if files else 0.0

    # --- Bar 2: recall@k ---
    hits, details = 0, []
    for item in gold["queries"]:
        st, raw = http("POST", f"{args.url}/v1/{ns}/code/search", args.token,
                       {"query": item["q"], "limit": args.k})
        paths = []
        if st == 200:
            try: paths = [h["path"] for h in json.loads(raw)]
            except Exception: paths = []
        hit = any(g in paths[:args.k] for g in item["gold"])
        hits += 1 if hit else 0
        details.append((hit, item["gold"][0], item["q"][:54], st,
                        paths[0] if paths else "-"))
    recall = hits / len(gold["queries"]) if gold["queries"] else 0.0

    print("\n=== ingest ===")
    print(f"  indexable files: {len(files)}")
    print(f"  present in engram: {present}")
    print(f"  success rate: {rate:.3f}   (bar >= 0.99  -> {'PASS' if rate>=0.99 else 'FAIL'})")
    print("\n=== recall@%d ===" % args.k)
    for hit, g, q, st, top in details:
        print(f"  [{'HIT ' if hit else 'miss'}] {q:<54} gold={g}  top1={top}  (http {st})")
    print(f"\n  recall@{args.k}: {recall:.3f}   (bar >= 0.80  -> {'PASS' if recall>=0.80 else 'FAIL'})")

    ok = rate >= 0.99 and recall >= 0.80
    print(f"\nRESULT: {'PASS' if ok else 'FAIL'}")
    sys.exit(0 if ok else 1)

if __name__ == "__main__":
    main()
