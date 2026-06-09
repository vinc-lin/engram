"""Lens 1 — agent migration A/B.

For each feature, runs the coding agent twice (with_engram vs without_engram) by shelling out to
eval/agent/loop.py, then scores each footprint vs the gold footprint:
  - deterministic path precision/recall/F1 (primary)
  - layer coverage (did it find >=1 file per gold layer)
  - efficiency: turns, tool calls, tokens, cost
An optional blind LLM judge (--judge) adds a qualitative completeness read.

  python3 eval/harness/migration.py --gold eval/android/avm_gold.json \
      --url http://127.0.0.1:8089 --token "$ENGRAM_TOKEN" --model deepseek-chat \
      --out eval/runs [--features camera-frame-pipeline] [--judge]
"""

import argparse
import json
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import metrics as M  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))  # repo root
AGENT = os.path.join(ROOT, "eval", "agent", "loop.py")
ARMS = ("with_engram", "without_engram")


def expand(p):
    return os.path.realpath(os.path.expanduser(p))


def run_agent(task_file, arm, repo, ns, url, token, model, out_dir, extra):
    cmd = ["python3", AGENT, "--task-file", task_file, "--arm", arm, "--repo", repo,
           "--namespace", ns, "--url", url, "--token", token, "--model", model, "--out", out_dir]
    cmd += extra
    subprocess.run(cmd, capture_output=True, text=True, timeout=900)
    mpath = os.path.join(out_dir, "metrics.json")
    return json.load(open(mpath)) if os.path.exists(mpath) else None


def score(metrics, gold_footprint):
    preds = [f.get("path", "") for f in metrics.get("final_footprint", [])]
    golds = [f["path"] for f in gold_footprint]
    p, r, f1 = M.path_prf(golds, preds)
    cov, ncov, ntot = M.layer_coverage(gold_footprint, preds)
    return {"precision": p, "recall": r, "f1": f1, "layer_cov": cov,
            "layers": f"{ncov}/{ntot}", "n_pred": len(preds),
            "turns": metrics.get("turns"), "tool_calls": metrics.get("tool_calls_total"),
            "tokens": metrics.get("total_tokens"), "cost": metrics.get("est_cost_usd"),
            "terminated": metrics.get("terminated")}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", required=True)
    ap.add_argument("--url", default="http://127.0.0.1:8089")
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--model", default="deepseek-chat")
    ap.add_argument("--out", default=os.path.join(ROOT, "eval", "runs"))
    ap.add_argument("--features", help="comma-sep feature ids to run (default: all)")
    ap.add_argument("--max-turns", default="15")
    ap.add_argument("--max-tool-calls", default="40")
    ap.add_argument("--judge", action="store_true")
    args = ap.parse_args()

    gold = json.load(open(args.gold))
    datasets = gold.get("datasets", [gold])
    only = set(args.features.split(",")) if args.features else None
    extra = ["--max-turns", args.max_turns, "--max-tool-calls", args.max_tool_calls]
    os.makedirs(args.out, exist_ok=True)

    rows = []
    for ds in datasets:
        repo, ns = expand(ds["repo"]), ds["namespace"]
        for feat in ds.get("features", []):
            if only and feat["id"] not in only:
                continue
            tf = os.path.join(args.out, f"{feat['id']}.task.json")
            json.dump({"id": feat["id"], "task": feat["task"]}, open(tf, "w"))
            per_arm = {}
            for arm in ARMS:
                out_dir = os.path.join(args.out, ns.replace(":", "_"), feat["id"], arm)
                m = run_agent(tf, arm, repo, ns, args.url, args.token, args.model, out_dir, extra)
                per_arm[arm] = score(m, feat["footprint"]) if m else None
            rows.append((ns, feat["id"], per_arm))

    print("\n========== Lens 1 — migration A/B (footprint vs gold) ==========")
    hdr = f"{'feature':28} {'arm':16} {'F1':>5} {'layers':>7} {'turns':>5} {'tools':>5} {'tok':>7} {'$':>7}"
    print(hdr)
    print("-" * len(hdr))
    for ns, fid, per_arm in rows:
        for arm in ARMS:
            s = per_arm.get(arm)
            if not s:
                print(f"{fid[:28]:28} {arm:16} (no result)")
                continue
            print(f"{fid[:28]:28} {arm:16} {s['f1']:>5} {s['layers']:>7} "
                  f"{s['turns']:>5} {s['tool_calls']:>5} {s['tokens']:>7} {s['cost']:>7}")
    out_json = os.path.join(args.out, "lens1_summary.json")
    json.dump([{"namespace": ns, "feature": fid, "arms": pa} for ns, fid, pa in rows],
              open(out_json, "w"), indent=2)
    print(f"\nwrote {out_json}")


if __name__ == "__main__":
    main()
