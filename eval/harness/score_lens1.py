"""Recompute the Lens-1 A/B table from all on-disk agent runs (eval/runs/<ns>/<feature>/<arm>/).

Lets a partial re-run (migration.py --features ...) be merged with prior runs: this reads whatever
metrics.json exist and scores them all against the gold footprints. Prints per-feature F1/coverage/
efficiency + aggregates (mean F1, wins/ties/losses, token totals).

  python3 eval/harness/score_lens1.py --gold eval/android/avm_gold.json
"""

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import metrics as M  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ARMS = ("with_engram", "without_engram")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", required=True)
    ap.add_argument("--runs", default=os.path.join(ROOT, "eval", "runs"))
    args = ap.parse_args()
    gold = json.load(open(args.gold))

    rows = []
    agg = {a: {"f1": [], "cov": [], "tok": 0, "cost": 0.0} for a in ARMS}
    wins = {"with_engram": 0, "without_engram": 0, "tie": 0}

    for ds in gold["datasets"]:
        nsdir = ds["namespace"].replace(":", "_")
        for feat in ds.get("features", []):
            per = {}
            for arm in ARMS:
                mp = os.path.join(args.runs, nsdir, feat["id"], arm, "metrics.json")
                if not os.path.exists(mp):
                    per[arm] = None
                    continue
                m = json.load(open(mp))
                preds = [f.get("path", "") for f in m.get("final_footprint", [])]
                p, r, f1 = M.path_prf([f["path"] for f in feat["footprint"]], preds)
                cov, nc, nt = M.layer_coverage(feat["footprint"], preds)
                per[arm] = {"f1": f1, "p": p, "r": r, "cov": cov, "layers": f"{nc}/{nt}",
                            "tok": m.get("total_tokens", 0), "cost": m.get("est_cost_usd", 0),
                            "n": len(preds), "term": m.get("terminated")}
                agg[arm]["f1"].append(f1)
                agg[arm]["cov"].append(cov)
                agg[arm]["tok"] += m.get("total_tokens", 0)
                agg[arm]["cost"] += m.get("est_cost_usd", 0)
            if per.get("with_engram") and per.get("without_engram"):
                d = per["with_engram"]["f1"] - per["without_engram"]["f1"]
                wins["with_engram" if d > 0.02 else "without_engram" if d < -0.02 else "tie"] += 1
            rows.append((ds["namespace"], feat["id"], per))

    hdr = f"{'feature':28} {'with_F1':>8} {'wout_F1':>8} {'Δ':>6} {'with_cov':>9} {'wout_cov':>9} {'with_tok':>9} {'wout_tok':>9}"
    print(hdr)
    print("-" * len(hdr))
    for ns, fid, per in rows:
        w, wo = per.get("with_engram"), per.get("without_engram")
        if not (w and wo):
            print(f"{fid[:28]:28} (incomplete)")
            continue
        d = w["f1"] - wo["f1"]
        print(f"{fid[:28]:28} {w['f1']:>8} {wo['f1']:>8} {d:>+6.3f} {w['layers']:>9} {wo['layers']:>9} {w['tok']:>9} {wo['tok']:>9}")

    def mean(xs):
        return round(sum(xs) / len(xs), 3) if xs else 0.0
    print("\n=== aggregate ===")
    for a in ARMS:
        print(f"  {a:16} mean_F1={mean(agg[a]['f1'])}  mean_layer_cov={mean(agg[a]['cov'])}  "
              f"total_tok={agg[a]['tok']}  total_$={round(agg[a]['cost'], 3)}  n={len(agg[a]['f1'])}")
    print(f"  win/tie/loss (with vs without, |Δ|>0.02): "
          f"with={wins['with_engram']}  tie={wins['tie']}  without={wins['without_engram']}")


if __name__ == "__main__":
    main()
