"""Shared scoring for the AVM eval. Mirrors eval/validate.py's definitions so numbers are
comparable, plus per-language recall, cross-layer coverage, and footprint precision/recall/F1.
"""


def overlaps(hit, lo, hi):
    """True if a hit's [line_start,line_end] overlaps the gold [lo,hi] range."""
    s, e = hit.get("line_start"), hit.get("line_end")
    if s is None or e is None:
        return False
    return not (e < lo or s > hi)


def recall_hit(golds, ranked_paths, k):
    """Path-level recall@k: is any gold path in the top-k ranked results?"""
    top = ranked_paths[:k]
    return any(g in top for g in golds)


def line_hit(gold_lines, hits, k):
    """Line-level recall@k: does a top-k hit for the gold file overlap the gold line range?"""
    for gl in gold_lines:
        f, (lo, hi) = gl["file"], gl["line_range"]
        if any(h.get("path") == f and overlaps(h, lo, hi) for h in hits[:k]):
            return True
    return False


def _norm(p):
    """Normalize a path for fuzzy set-matching: lowercase, strip leading ./, collapse slashes."""
    return p.strip().lstrip("./").replace("\\", "/").lower()


def path_prf(gold_paths, pred_paths):
    """Precision/recall/F1 of a predicted path set vs gold, with suffix-tolerant matching
    (a prediction matches a gold if one path ends with the other — handles repo-relative vs
    basename drift)."""
    gold = [_norm(p) for p in gold_paths]
    pred = [_norm(p) for p in pred_paths]

    def match(a, b):
        return a == b or a.endswith("/" + b) or b.endswith("/" + a)

    tp = sum(1 for g in gold if any(match(g, p) for p in pred))
    matched_preds = sum(1 for p in pred if any(match(g, p) for g in gold))
    precision = matched_preds / len(pred) if pred else 0.0
    recall = tp / len(gold) if gold else 0.0
    f1 = 2 * precision * recall / (precision + recall) if (precision + recall) else 0.0
    return round(precision, 3), round(recall, 3), round(f1, 3)


def layer_coverage(gold_footprint, pred_paths):
    """Fraction of gold *layers* for which the prediction includes >=1 of that layer's gold files."""
    layers = {}
    for item in gold_footprint:
        layers.setdefault(item["layer"], []).append(item["path"])
    pred = [_norm(p) for p in pred_paths]

    def hit(gold_path):
        g = _norm(gold_path)
        return any(g == p or g.endswith("/" + p) or p.endswith("/" + g) for p in pred)

    covered = sum(1 for files in layers.values() if any(hit(fp) for fp in files))
    return round(covered / len(layers), 3) if layers else 0.0, covered, len(layers)
