#!/usr/bin/env python3
"""Graph evaluation harness for engram's TRACK-1 code-graph feature.

Measures caller precision@k / recall@k and callee coverage against
eval/graph_gold.json (Rust) and eval/android/graph_gold_native.json (C/C++).

LIVE RESOURCES REQUIRED (cost-bearing — NOT a CI / offline harness):
  * A running engram HTTP service (default http://127.0.0.1:8088), reachable
    with ENGRAM_TOKEN, that was *ingested with ENGRAM_CODE_GRAPH_EXTRACT=true*
    (the default-off gate must be on at index time or code_edges is empty).
  * The target repos indexed into their namespaces, e.g.:
      ENGRAM_CODE_GRAPH_EXTRACT=true \\
        cargo run -p engram-index -- index . --namespace repo:engram --url ...
    plus the 3 C/C++ proxy repos for the native gold.
  * An embedder/LLM backend is NOT needed here — these endpoints read SQLite
    only (no embed / no spawn_blocking on the graph path).
With no live engram the harness still parses and runs; every request returns
http=0 / score 0.0 and it exits non-zero (useful for shape smoke-tests).

Prerequisites:
  1. engram running with ENGRAM_CODE_GRAPH_EXTRACT=true
  2. Repos indexed (ENGRAM_CODE_GRAPH_EXTRACT=true cargo run -p engram-index -- index ...)
  3. ENGRAM_TOKEN set

Endpoints:
  POST /v1/{ns}/code/graph/callers  body: {"sym": "<name>", "limit": k}
  POST /v1/{ns}/code/graph/callees  body: {"path": "<repo-rel-path>", "limit": k}
"""
import argparse, json, os, sys, urllib.parse, urllib.request, urllib.error


def http(method, url, token, body=None, timeout=30):
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


def graph_callers(url, ns, token, sym, limit):
    st, raw = http("POST", f"{url}/v1/{ns}/code/graph/callers", token,
                   {"sym": sym, "limit": limit})
    if st != 200:
        return st, []
    try:
        return st, json.loads(raw)
    except Exception:
        return st, []


def graph_callees(url, ns, token, path, limit):
    st, raw = http("POST", f"{url}/v1/{ns}/code/graph/callees", token,
                   {"path": path, "limit": limit})
    if st != 200:
        return st, []
    try:
        return st, json.loads(raw)
    except Exception:
        return st, []


def precision_at_k(returned_paths, expected_set, unexpected_set, k):
    top = returned_paths[:k]
    if not top:
        return 0.0
    correct = sum(1 for p in top if p in expected_set and p not in unexpected_set)
    return correct / len(top)


def recall_at_k(returned_paths, expected_set, k):
    if not expected_set:
        return 1.0
    top_set = set(returned_paths[:k])
    return sum(1 for e in expected_set if e in top_set) / len(expected_set)


def callee_coverage(returned_syms, expected_syms):
    if not expected_syms:
        return 1.0
    ret_set = set(returned_syms)
    return sum(1 for s in expected_syms if s in ret_set) / len(expected_syms)


def eval_dataset(url, token, namespace, entries, k, label):
    caller_rows = []
    callee_rows = []
    for entry in entries:
        if entry["kind"] == "callers":
            sym = entry["query_sym"]
            expected = set(entry.get("expected_callers", []))
            unexpected = set(entry.get("unexpected_callers", []))
            st, hits = graph_callers(url, namespace, token, sym, k)
            paths = [h.get("path", "") for h in hits]
            p = precision_at_k(paths, expected, unexpected, k)
            r = recall_at_k(paths, expected, k)
            caller_rows.append((sym, p, r, len(paths), st))
        elif entry["kind"] == "callees":
            path = entry["query_path"]
            expected = entry.get("expected_callees_contain", [])
            st, hits = graph_callees(url, namespace, token, path, 200)
            returned_syms = [h.get("sym", "") for h in hits]
            cov = callee_coverage(returned_syms, expected)
            callee_rows.append((path, cov, len(returned_syms), st))
    return caller_rows, callee_rows


def print_caller_table(rows, k, label):
    if not rows:
        return
    print(f"\n=== callers ({label}, precision@{k} / recall@{k}) ===")
    print(f"  {'sym':<35} {'prec@k':>7} {'rec@k':>7} {'n_ret':>6} {'http':>5}")
    print(f"  {'-'*35} {'-'*7} {'-'*7} {'-'*6} {'-'*5}")
    for sym, p, r, n, st in rows:
        print(f"  {sym:<35} {p:>7.3f} {r:>7.3f} {n:>6} {st:>5}")
    avg_p = sum(p for _, p, _, _, _ in rows) / len(rows)
    avg_r = sum(r for _, _, r, _, _ in rows) / len(rows)
    print(f"  {'MEAN':<35} {avg_p:>7.3f} {avg_r:>7.3f}")


def print_callee_table(rows, label):
    if not rows:
        return
    print(f"\n=== callees ({label}, sym coverage) ===")
    print(f"  {'path':<60} {'coverage':>9} {'n_ret':>6} {'http':>5}")
    print(f"  {'-'*60} {'-'*9} {'-'*6} {'-'*5}")
    for path, cov, n, st in rows:
        short = path[-58:] if len(path) > 60 else path
        print(f"  {short:<60} {cov:>9.3f} {n:>6} {st:>5}")
    avg = sum(c for _, c, _, _ in rows) / len(rows)
    print(f"  {'MEAN':<60} {avg:>9.3f}")


BARS = {
    "rust_callers_prec":  ("repo:engram", "callers", "prec", 0.70, ">="),
    "rust_callers_rec":   ("repo:engram", "callers", "rec",  0.50, ">="),
    "rust_callees_cov":   ("repo:engram", "callees", "cov",  0.90, ">="),
    "cpp_callers_prec":   ("repo:ndk-samples", "callers", "prec", 0.50, ">="),
    "cpp_callees_cov":    ("repo:ndk-samples", "callees", "cov",  0.70, ">="),
}


def check_bars(results_by_ns):
    passed = []
    failed = []
    for bar_id, (ns, kind, metric, threshold, op) in BARS.items():
        key = f"{kind}_{metric}"
        val = results_by_ns.get(ns, {}).get(key)
        if val is None:
            failed.append((bar_id, "no data", threshold))
            continue
        ok = (val >= threshold) if op == ">=" else (val <= threshold)
        (passed if ok else failed).append((bar_id, round(val, 3), threshold))
    return passed, failed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rust-gold",
                    default=os.path.join(os.path.dirname(__file__), "graph_gold.json"))
    ap.add_argument("--native-gold",
                    default=os.path.join(os.path.dirname(__file__), "android", "graph_gold_native.json"))
    ap.add_argument("--url",  default=os.environ.get("ENGRAM_URL", "http://127.0.0.1:8088"))
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--k", type=int, default=10, help="k for precision@k / recall@k on callers")
    args = ap.parse_args()

    all_caller_rows = {}
    all_callee_rows = {}

    if os.path.exists(args.rust_gold):
        rust = json.load(open(args.rust_gold))
        ns = rust["namespace"]
        entries = rust.get("entries", [])
        cr, ce = eval_dataset(args.url, args.token, ns, entries, args.k, ns)
        all_caller_rows[ns] = cr
        all_callee_rows[ns] = ce
        print_caller_table(cr, args.k, ns)
        print_callee_table(ce, ns)
    else:
        print(f"[warn] rust gold not found: {args.rust_gold}", file=sys.stderr)

    if os.path.exists(args.native_gold):
        native = json.load(open(args.native_gold))
        for ds in native.get("datasets", []):
            ns = ds["namespace"]
            entries = ds.get("entries", [])
            cr, ce = eval_dataset(args.url, args.token, ns, entries, args.k, ns)
            all_caller_rows.setdefault(ns, []).extend(cr)
            all_callee_rows.setdefault(ns, []).extend(ce)
            print_caller_table(cr, args.k, ns)
            print_callee_table(ce, ns)
    else:
        print(f"[warn] native gold not found: {args.native_gold}", file=sys.stderr)

    results_by_ns = {}
    for ns in set(list(all_caller_rows.keys()) + list(all_callee_rows.keys())):
        cr = all_caller_rows.get(ns, [])
        ce = all_callee_rows.get(ns, [])
        results_by_ns[ns] = {}
        if cr:
            results_by_ns[ns]["callers_prec"] = sum(p for _, p, _, _, _ in cr) / len(cr)
            results_by_ns[ns]["callers_rec"]  = sum(r for _, _, r, _, _ in cr) / len(cr)
        if ce:
            results_by_ns[ns]["callees_cov"] = sum(c for _, c, _, _ in ce) / len(ce)

    passed, failed = check_bars(results_by_ns)

    print("\n=== acceptance bars ===")
    for bar_id, val, threshold in passed:
        print(f"  PASS  {bar_id:<35} got={val}  bar>={threshold}")
    for bar_id, val, threshold in failed:
        print(f"  FAIL  {bar_id:<35} got={val}  bar>={threshold}")

    ok = len(failed) == 0
    print(f"\nRESULT: {'PASS' if ok else 'FAIL'}  ({len(passed)} bars passed, {len(failed)} failed)")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
