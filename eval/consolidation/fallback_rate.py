#!/usr/bin/env python3
"""Fallback-rate monitor for the consolidation cold pipeline (Track 2b).

Ingests a small prose corpus (engram's own README / ARCHITECTURE / CLAUDE.md /
ROADMAP) into a fresh namespace, lets the cold pipeline drain, then reads the
llm-audit events emitted by every seal's summarize_audited call and reports the
fraction that errored out and fell back to deterministic concat:

    fallback_rate = error / (ok + error)

A high fallback rate means the summary tree is not trusted LLM text — it is
mostly the deterministic fallback_summary concat, so consolidation is not
earning its keep.

LIVE RESOURCES REQUIRED (cost-bearing — NOT a CI / offline harness):
  * A running engram HTTP service started with REAL gateway credentials
    (ENGRAM_GATEWAY_URL / ENGRAM_GATEWAY_KEY) and an HttpAuditSink/audit DB, so
    that seals call a real deepseek-chat and emit llm-audit op='chat' events.
  * SHRUNK SEAL GATES on that engram process so a tiny corpus actually seals:
        ENGRAM_SEAL_INPUT_TOKEN_BUDGET=500 ENGRAM_SEAL_FANOUT=3
  * ENGRAM_TOKEN for the API, and ONE of:
      - ENGRAM_DB pointing at engram's SQLite file (drain polling + audit read), or
      - ENGRAM_AUDIT_DB / ENGRAM_AUDIT_URL for the llm-audit events.
Without a live engram this script exits non-zero (connection refused, or
"no corpus files found", or "no audit events").

Exit codes:
  0  PASS   fallback_rate <= FALLBACK_RATE_GATE (0.10)
  1  FAIL   fallback_rate >  FALLBACK_RATE_GATE
  2  NODATA no audit events were collected (cannot judge)
"""
import json, os, sqlite3, sys, time, urllib.error, urllib.request

ROOT = os.environ.get("ENGRAM_ROOT", "/mnt/x/code/engram")
BASE = os.environ.get("ENGRAM_URL", "http://127.0.0.1:8096").rstrip("/")
TOK = os.environ.get("ENGRAM_TOKEN", "")
NS = "fallback-eval"

FALLBACK_RATE_GATE = 0.10

CORPUS_PATHS = ["README.md", "ARCHITECTURE.md", "CLAUDE.md", "docs/ROADMAP.md"]

# engram's SQLite DB (for drain polling + audit-via-db); audit may also live elsewhere.
ENGRAM_DB = os.environ.get("ENGRAM_DB", "")
AUDIT_DB = os.environ.get("ENGRAM_AUDIT_DB", "")
AUDIT_URL = os.environ.get("ENGRAM_AUDIT_URL", "")


def http(url, body=None, headers=None, timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data,
                                 method="POST" if data is not None else "GET",
                                 headers=headers or {})
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
    return http(f"{BASE}/v1/{NS}{path}", body,
                {"Authorization": f"Bearer {TOK}", "Content-Type": "application/json"})


def ingest_corpus():
    n = 0
    for rel in CORPUS_PATHS:
        p = os.path.join(ROOT, rel)
        if not os.path.exists(p):
            print(f"  (skip missing {rel})")
            continue
        txt = open(p, encoding="utf-8", errors="replace").read()
        eng("/docs", {"key": rel, "title": rel, "content": txt,
                      "author": "engram-docs", "meta": {}})
        print(f"  ingested {rel} ({len(txt)} chars)")
        n += 1
    return n


def wait_drain(db_path, max_wait=600):
    """Poll post_acquire_jobs until no job is pending/running (then a little settle)."""
    if not db_path or not os.path.exists(db_path):
        print(f"[warn] no engram DB at {db_path!r}; sleeping 60s instead of polling")
        time.sleep(60)
        return
    deadline = time.time() + max_wait
    stable = 0
    while time.time() < deadline:
        time.sleep(5)
        c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            pend = c.execute(
                "SELECT count(*) FROM post_acquire_jobs "
                "WHERE status IN ('pending','running')").fetchone()[0]
        finally:
            c.close()
        print(f"  pending/running jobs = {pend}")
        if pend == 0:
            stable += 1
            if stable >= 2:
                return
        else:
            stable = 0
    print("[warn] drain wait timed out")


def read_audit_events_from_db(db_path):
    """llm-audit events for engram chat calls: returns list of status strings."""
    if not db_path or not os.path.exists(db_path):
        return None
    c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        # The audit schema records app, op, and a status/outcome column. We tolerate
        # either a 'status' or 'outcome' column name.
        cols = [r[1] for r in c.execute("PRAGMA table_info(events)").fetchall()]
        status_col = "status" if "status" in cols else ("outcome" if "outcome" in cols else None)
        if status_col is None:
            return None
        rows = c.execute(
            f"SELECT {status_col} FROM events WHERE app='engram' AND op='chat'").fetchall()
        return [str(r[0]) for r in rows]
    except sqlite3.Error:
        return None
    finally:
        c.close()


def read_audit_events_from_api(url):
    """Fetch llm-audit chat events over HTTP. Returns list of status strings or None."""
    if not url:
        return None
    try:
        data = http(f"{url.rstrip('/')}/events?app=engram&op=chat", None,
                    {"Authorization": f"Bearer {TOK}"})
    except Exception as e:
        print(f"[warn] audit API read failed: {e}")
        return None
    events = data.get("events", data) if isinstance(data, dict) else data
    out = []
    for e in events or []:
        out.append(str(e.get("status", e.get("outcome", ""))))
    return out


def classify(statuses):
    ok = sum(1 for s in statuses if s.lower() in ("ok", "success", "succeeded", "200"))
    err = sum(1 for s in statuses if s.lower() in ("error", "err", "fail", "failed", "fallback"))
    # Anything not clearly ok/err but non-empty counts as ok (a completed call).
    other = len(statuses) - ok - err
    ok += other
    return ok, err


def main():
    print(f"engram={BASE} ns={NS} gate={FALLBACK_RATE_GATE}")
    n = ingest_corpus()
    if n == 0:
        print("no corpus files found", file=sys.stderr)
        sys.exit(2)

    wait_drain(ENGRAM_DB)

    statuses = (read_audit_events_from_db(AUDIT_DB or ENGRAM_DB)
                or read_audit_events_from_api(AUDIT_URL))
    if not statuses:
        print("no audit events collected (set ENGRAM_AUDIT_DB / ENGRAM_AUDIT_URL / ENGRAM_DB)",
              file=sys.stderr)
        sys.exit(2)

    ok, err = classify(statuses)
    total = ok + err
    if total == 0:
        print("no chat audit events collected", file=sys.stderr)
        sys.exit(2)
    rate = err / total
    print(f"\nllm-audit chat events: ok={ok} error={err} total={total}")
    print(f"fallback_rate = {rate:.4f}  (gate <= {FALLBACK_RATE_GATE})")

    if rate > FALLBACK_RATE_GATE:
        print(f"RESULT: FAIL  ({rate:.4f} > {FALLBACK_RATE_GATE})")
        sys.exit(1)
    print(f"RESULT: PASS  ({rate:.4f} <= {FALLBACK_RATE_GATE})")
    sys.exit(0)


if __name__ == "__main__":
    main()
