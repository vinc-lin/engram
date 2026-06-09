"""Tool set for the eval coding agent.

Filesystem tools (read_file, list_dir, ripgrep) are jailed to the repo root and exposed in BOTH
arms. engram tools (search_code, find_symbol, why) are added ONLY in the `with_engram` arm — engram
is *additive* to grep, so the A/B isolates its marginal value. Stdlib-only.
"""

import json
import os
import subprocess
import urllib.error
import urllib.request

# engram tools added on top of the filesystem tools in the with_engram arm.
_ENGRAM_TOOL_NAMES = ("search_code", "find_symbol", "why")


class ToolBox:
    def __init__(self, repo_root, engram_url, engram_token, namespace,
                 read_max_lines=400, rg_max_lines=80, max_result_chars=4000):
        self.root = os.path.realpath(repo_root)
        self.url = engram_url.rstrip("/")
        self.token = engram_token
        self.ns = namespace
        self.read_max_lines = read_max_lines
        self.rg_max_lines = rg_max_lines
        self.max_result_chars = max_result_chars
        self._has_rg = subprocess.run(["which", "rg"], capture_output=True).returncode == 0

    # ── path jail ────────────────────────────────────────────────────────────
    def _resolve(self, path):
        p = os.path.realpath(os.path.join(self.root, path or "."))
        if p != self.root and not p.startswith(self.root + os.sep):
            raise ValueError(f"path escapes repo root: {path}")
        return p

    def _cap(self, s):
        return s if len(s) <= self.max_result_chars else s[: self.max_result_chars] + "\n…[truncated]"

    # ── filesystem tools ───────────────────────────────────────────────────────
    def read_file(self, path, start=None, end=None):
        p = self._resolve(path)
        if not os.path.isfile(p):
            return f"error: not a file: {path}"
        with open(p, encoding="utf-8", errors="replace") as f:
            lines = f.readlines()
        lo = max(1, int(start)) if start else 1
        hi = min(len(lines), int(end)) if end else min(len(lines), lo + self.read_max_lines - 1)
        if hi - lo + 1 > self.read_max_lines:
            hi = lo + self.read_max_lines - 1
        body = "".join(f"{i:>5}\t{lines[i - 1]}" for i in range(lo, hi + 1))
        return self._cap(f"{path} [{lo}-{hi} of {len(lines)}]\n{body}")

    def list_dir(self, path="."):
        p = self._resolve(path)
        if not os.path.isdir(p):
            return f"error: not a directory: {path}"
        entries = sorted(os.listdir(p))[:200]
        out = []
        for e in entries:
            full = os.path.join(p, e)
            out.append(f"{e}/" if os.path.isdir(full) else e)
        return self._cap(f"{path}/\n" + "\n".join(out))

    def ripgrep(self, pattern, path=None, glob=None):
        target = self._resolve(path) if path else self.root
        if self._has_rg:
            cmd = ["rg", "--line-number", "--no-heading", "--max-count", "5",
                   "--max-columns", "300", "-S"]
            if glob:
                cmd += ["--glob", glob]
            cmd += [pattern, target]
        else:
            cmd = ["grep", "-rnI", pattern, target]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, cwd=self.root)
        except subprocess.TimeoutExpired:
            return "error: ripgrep timed out"
        out = (r.stdout or "").replace(self.root + os.sep, "")
        lines = out.splitlines()
        if not lines:
            return f"(no matches for {pattern!r})"
        shown = "\n".join(lines[: self.rg_max_lines])
        if len(lines) > self.rg_max_lines:
            shown += f"\n…[{len(lines) - self.rg_max_lines} more matches]"
        return self._cap(shown)

    # ── engram HTTP tools ───────────────────────────────────────────────────────
    def _post(self, path, body):
        req = urllib.request.Request(
            f"{self.url}/v1/{self.ns}{path}", data=json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json"},
            method="POST")
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.load(resp)

    def _engram_search(self, ns_suffix, query, limit):
        try:
            req = urllib.request.Request(
                f"{self.url}/v1/{self.ns}{ns_suffix}/code/search"
                if not ns_suffix else f"{self.url}/v1/{self.ns}{ns_suffix}/query",
                data=json.dumps({"query": query, "limit": limit}).encode(),
                headers={"Authorization": f"Bearer {self.token}",
                         "Content-Type": "application/json"}, method="POST")
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            return {"_error": f"HTTP {e.code}"}
        except Exception as e:  # noqa: BLE001 — surface to the model, never crash the run
            return {"_error": str(e)[:120]}

    def search_code(self, query, limit=10):
        hits = self._engram_search("", query, limit)
        if isinstance(hits, dict) and "_error" in hits:
            return f"engram error: {hits['_error']}"
        out = [f"{h.get('path')}:{h.get('line_start')}-{h.get('line_end')} (score {h.get('score', 0):.2f})\n"
               f"  {(h.get('snippet') or '').strip()[:200]}" for h in hits]
        return self._cap("\n".join(out) or "(no results)")

    def find_symbol(self, name, limit=10):
        return self.search_code(name, limit)

    def why(self, query, limit=10):
        hits = self._engram_search(":history", query, limit)
        if isinstance(hits, dict) and "_error" in hits:
            return f"engram error: {hits['_error']}"
        out = [f"{(h.get('meta') or {}).get('sha', '?')[:10]} {h.get('title', '')[:80]}" for h in hits]
        return self._cap("\n".join(out) or "(no commits)")

    # ── dispatch + specs ─────────────────────────────────────────────────────
    def execute(self, name, args):
        try:
            fn = {
                "read_file": lambda: self.read_file(args.get("path", ""),
                                                    args.get("start"), args.get("end")),
                "list_dir": lambda: self.list_dir(args.get("path", ".")),
                "ripgrep": lambda: self.ripgrep(args.get("pattern", ""),
                                                args.get("path"), args.get("glob")),
                "search_code": lambda: self.search_code(args.get("query", ""), args.get("limit", 10)),
                "find_symbol": lambda: self.find_symbol(args.get("name", ""), args.get("limit", 10)),
                "why": lambda: self.why(args.get("query", ""), args.get("limit", 10)),
            }.get(name)
            if fn is None:
                return f"error: unknown tool {name}"
            return fn()
        except Exception as e:  # noqa: BLE001
            return f"error running {name}: {str(e)[:160]}"

    def specs(self, arm):
        fs = [
            _spec("read_file", "Read a file (optionally a line range) within the repo.",
                  {"path": _s("repo-relative file path"),
                   "start": _i("first line (1-based, optional)"),
                   "end": _i("last line (optional)")}, ["path"]),
            _spec("list_dir", "List one directory level within the repo.",
                  {"path": _s("repo-relative dir (default '.')")}, []),
            _spec("ripgrep", "Search file contents with ripgrep (regex).",
                  {"pattern": _s("regex/text to search"),
                   "path": _s("repo-relative subdir to scope (optional)"),
                   "glob": _s("file glob filter, e.g. '*.kt' (optional)")}, ["pattern"]),
        ]
        if arm != "with_engram":
            return fs
        engram = [
            _spec("search_code", "Semantic + keyword code search over the indexed repo. "
                  "Returns path:line + snippet. Best for 'where is X implemented'.",
                  {"query": _s("natural-language or keyword query"),
                   "limit": _i("max results (default 10)")}, ["query"]),
            _spec("find_symbol", "Find where a symbol (function/class/type) is defined/used.",
                  {"name": _s("symbol name, e.g. 'dewarp'"),
                   "limit": _i("max results (default 10)")}, ["name"]),
            _spec("why", "Search git history for commits explaining why/when something changed.",
                  {"query": _s("question about a change"),
                   "limit": _i("max commits (default 10)")}, ["query"]),
        ]
        return fs + engram


def _s(desc):
    return {"type": "string", "description": desc}


def _i(desc):
    return {"type": "integer", "description": desc}


def _spec(name, desc, props, required):
    return {"type": "function", "function": {
        "name": name, "description": desc,
        "parameters": {"type": "object", "properties": props, "required": required}}}
