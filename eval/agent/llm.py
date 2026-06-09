"""LLM client for the eval coding agent.

Talks to the litellm gateway (OpenAI-compatible /v1/chat/completions) with the standard
`tools`/`tool_calls` protocol. Stdlib-only (urllib), matching eval/validate.py — no pip needed.

Model-agnostic with a thin adapter so the same agent runs on DeepSeek (primary) and Qwen3.x
(deferred until gateway access exists). Both normalize to one `AssistantTurn`.
"""

import json
import os
import re
import urllib.error
import urllib.request

# ── config (env, reuse engram's gateway vars) ─────────────────────────────────
GATEWAY_URL = os.environ.get("ENGRAM_GATEWAY_URL", "http://127.0.0.1:4000").rstrip("/")
GATEWAY_KEY = os.environ.get("ENGRAM_GATEWAY_KEY", "")

# Rough price table ($/1M tokens) for cost estimation; override via env if needed.
PRICE = {
    "deepseek-chat": (0.14, 0.28),
    "deepseek-reasoner": (0.55, 2.19),
    "qwen3": (0.0012, 0.0024),
}


class ToolCall:
    """One normalized tool call: a name + parsed-args dict + the provider's call id."""

    def __init__(self, call_id, name, args):
        self.id = call_id
        self.name = name
        self.args = args  # dict (tolerant-parsed)

    def __repr__(self):
        return f"ToolCall({self.name}, {self.args})"


class AssistantTurn:
    """Normalized model response: text content, tool calls, token usage, raw finish reason."""

    def __init__(self, content, tool_calls, usage, finish_reason, reasoning=None):
        self.content = content or ""
        self.tool_calls = tool_calls  # list[ToolCall]
        self.usage = usage  # {prompt, completion, total}
        self.finish_reason = finish_reason
        self.reasoning = reasoning  # logged, never re-fed


def _tolerant_json(s):
    """Parse a tool-call arguments string; repair the common breakages cheap models emit."""
    if isinstance(s, dict):
        return s
    if not s or not s.strip():
        return {}
    try:
        return json.loads(s)
    except Exception:
        pass
    # Strip trailing commas, then retry; else extract the first {...} block.
    repaired = re.sub(r",\s*([}\]])", r"\1", s)
    try:
        return json.loads(repaired)
    except Exception:
        m = re.search(r"\{.*\}", s, re.DOTALL)
        if m:
            try:
                return json.loads(re.sub(r",\s*([}\]])", r"\1", m.group(0)))
            except Exception:
                pass
    return {"_raw": s}  # surface unparseable args rather than crashing the run


# Qwen (and some others) sometimes emit tool calls as a <tool_call>{...}</tool_call> block or a
# fenced ```json {"name":..,"arguments":..} inside `content` instead of structured tool_calls.
_TOOLCALL_TAG = re.compile(r"<tool_call>\s*(\{.*?\})\s*</tool_call>", re.DOTALL)
_FENCED = re.compile(r"```(?:json|tool_call)?\s*(\{.*?\})\s*```", re.DOTALL)


def _recover_tool_calls_from_content(content):
    """Fallback parser for models that inline tool calls in the message content."""
    found = []
    for rx in (_TOOLCALL_TAG, _FENCED):
        for m in rx.finditer(content or ""):
            try:
                obj = json.loads(m.group(1))
            except Exception:
                continue
            name = obj.get("name") or obj.get("tool")
            if not name:
                continue
            args = obj.get("arguments", obj.get("args", {}))
            found.append(ToolCall(f"recovered_{len(found)}", name, _tolerant_json(args)))
    return found


def chat(messages, tools, model="deepseek-chat", max_tokens=1024, temperature=0.1,
         enable_thinking=False, timeout=180, tool_choice="auto"):
    """One chat-completions round-trip. Returns an AssistantTurn (normalized across models)."""
    body = {
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": temperature,
    }
    if tools:
        body["tools"] = tools
        body["tool_choice"] = tool_choice
    # Qwen3 thinking-mode controls (ignored by DeepSeek/the gateway if unsupported).
    if "qwen" in model.lower():
        body["extra_body"] = {"enable_thinking": bool(enable_thinking)}

    req = urllib.request.Request(
        GATEWAY_URL + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {GATEWAY_KEY}", "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.load(resp)
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:400]
        raise RuntimeError(f"gateway HTTP {e.code}: {detail}") from None

    choice = data["choices"][0]
    msg = choice.get("message", {})
    usage_raw = data.get("usage") or {}
    usage = {
        "prompt": usage_raw.get("prompt_tokens", 0),
        "completion": usage_raw.get("completion_tokens", 0),
        "total": usage_raw.get("total_tokens", 0),
    }

    tool_calls = []
    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function", {})
        tool_calls.append(ToolCall(tc.get("id", f"call_{len(tool_calls)}"),
                                   fn.get("name", ""), _tolerant_json(fn.get("arguments"))))
    content = msg.get("content") or ""
    # Fallback: recover inline tool calls (Qwen Hermes-style) when none were structured.
    if not tool_calls and content:
        tool_calls = _recover_tool_calls_from_content(content)

    return AssistantTurn(
        content=content,
        tool_calls=tool_calls,
        usage=usage,
        finish_reason=choice.get("finish_reason"),
        reasoning=msg.get("reasoning_content"),  # logged only; never re-fed
    )


def est_cost_usd(model, usage):
    """Estimate $ cost from a usage dict using the PRICE table (0 if model unknown)."""
    base = model.split("/")[-1]
    rate = PRICE.get(base) or PRICE.get(base.split(":")[0])
    if not rate:
        return 0.0
    pin, pout = rate
    return usage.get("prompt", 0) / 1e6 * pin + usage.get("completion", 0) / 1e6 * pout
