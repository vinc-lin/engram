"""Abstention-floor sweep: how does a min-score cutoff trade recall for hard-negative suppression?

Queries engram once per gold probe + hard-negative (capturing scores), then sweeps thresholds
offline — recall@5 (gold in top-5 of hits scoring >= floor) vs hard-neg false positives (a
hard-negative whose top-1 score >= floor still returns a confident hit). Picks the floor that
zeroes false positives at minimal recall cost; that value is what to set ENGRAM_CODE_MIN_SCORE to.

  python3 eval/harness/abstain_sweep.py --gold eval/android/avm_gold.json --url http://127.0.0.1:8089
"""

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import retrieval as R  # noqa: E402

THRESHOLDS = [0.0, 0.40, 0.45, 0.50, 0.55, 0.60, 0.65, 0.70]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gold", required=True)
    ap.add_argument("--url", default="http://127.0.0.1:8089")
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    args = ap.parse_args()
    gold = json.load(open(args.gold))

    gold_hits = []   # (gold_paths, hits sorted desc)
    hardneg_top = []  # top-1 score per hard-negative
    for ds in gold["datasets"]:
        ns = ds["namespace"]
        for q in ds["queries"]:
            gold_hits.append((q["gold"], R.engram_retrieve(args.url, ns, args.token, q["q"], 10)))
        for hn in ds.get("hard_negatives", []):
            h = R.engram_retrieve(args.url, ns, args.token, hn["q"], 1)
            hardneg_top.append(h[0]["score"] if h else 0.0)

    nq, nhn = len(gold_hits), len(hardneg_top)
    print(f"probes={nq}  hard_negatives={nhn}\n")
    print(f"{'floor':>6} {'recall@5':>9} {'hardneg_FP':>11}")
    print("-" * 28)
    for thr in THRESHOLDS:
        rec = 0
        for golds, hits in gold_hits:
            kept = [h["path"] for h in hits if h["score"] >= thr][:5]
            if any(g in kept for g in golds):
                rec += 1
        fp = sum(1 for s in hardneg_top if s >= thr)
        print(f"{thr:>6.2f} {rec / nq:>9.3f} {fp:>6}/{nhn}")


if __name__ == "__main__":
    main()
