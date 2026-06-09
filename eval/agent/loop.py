"""Bounded ReAct loop for the eval coding agent + CLI entry.

Run one migration task under one arm/model; always writes a transcript + metrics.json. The harness
(Lens 1) shells out to this script per (feature x arm) so each run is isolated.

  python3 eval/agent/loop.py --task-file t.json --arm with_engram --model deepseek-chat \
      --repo ~/engram-eval/proxies/ndk-samples --namespace repo:ndk-samples \
      --url http://127.0.0.1:8089 --out eval/runs/ndk-samples/camera/with_engram
"""

import argparse
import hashlib
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import llm  # noqa: E402
from tools import ToolBox  # noqa: E402

SYSTEM = """You are a coding agent helping migrate a feature from one Android project to another.
Your job for THIS repo: locate EVERY file that implements the given feature, across all layers —
app (Kotlin/Java), JNI bridge, native (C/C++), AIDL/HIDL interfaces, and build files
(Android.bp / CMakeLists / Gradle). Use the tools to search and read; be thorough but efficient.

When confident you have the full set, output ONLY a fenced json block, then stop:
```json
{"feature_footprint": [{"path": "<repo-relative path>", "layer": "app|jni|native|aidl|build", "why": "<one line>"}]}
```
Do not call any tool after emitting the final JSON."""

_JSON_BLOCK = re.compile(r"```(?:json)?\s*(\{.*?\})\s*```", re.DOTALL)


def parse_footprint(content):
    """Extract the feature_footprint list from the model's final message (tolerant)."""
    for m in _JSON_BLOCK.finditer(content or ""):
        try:
            obj = json.loads(m.group(1))
            if isinstance(obj.get("feature_footprint"), list):
                return obj["feature_footprint"]
        except Exception:
            continue
    # last resort: a bare {...} with feature_footprint
    m = re.search(r"\{.*\"feature_footprint\".*\}", content or "", re.DOTALL)
    if m:
        try:
            return json.loads(m.group(0)).get("feature_footprint", [])
        except Exception:
            pass
    return []


def _assistant_msg(turn):
    """Rebuild the OpenAI assistant message (with tool_calls) for the conversation follow-up."""
    msg = {"role": "assistant", "content": turn.content or ""}
    if turn.tool_calls:
        msg["tool_calls"] = [{
            "id": tc.id, "type": "function",
            "function": {"name": tc.name, "arguments": json.dumps(tc.args)},
        } for tc in turn.tool_calls]
    return msg


def run(task, arm, model, toolbox, caps):
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": task}]
    transcript = [{"role": "user", "content": task}]
    usage_tot = {"prompt": 0, "completion": 0, "total": 0}
    tool_calls_total = 0
    tool_by_name = {}
    seen = {}
    strikes = 0
    final = []
    observations = []
    terminated = "max_turns"
    start = time.time()

    for _turn in range(caps["max_turns"]):
        if time.time() - start > caps["deadline"]:
            terminated = "deadline"
            break
        try:
            at = llm.chat(messages, toolbox.specs(arm), model=model,
                          max_tokens=caps["max_tokens"], temperature=caps["temperature"],
                          enable_thinking=caps["enable_thinking"], timeout=caps["deadline"])
        except Exception as e:  # noqa: BLE001 — never crash a run; record + stop
            transcript.append({"role": "error", "where": "loop", "detail": str(e)[:300]})
            terminated = "error"
            break
        for k in usage_tot:
            usage_tot[k] += at.usage.get(k, 0)
        transcript.append({"role": "assistant", "content": at.content,
                           "tool_calls": [(tc.name, tc.args) for tc in at.tool_calls],
                           "reasoning": at.reasoning})
        if not at.tool_calls:
            final = parse_footprint(at.content)
            terminated = "final"
            break
        messages.append(_assistant_msg(at))
        stop = False
        for tc in at.tool_calls:
            if tool_calls_total >= caps["max_tool_calls"]:
                terminated = "tool_budget"
                stop = True
                break
            key = hashlib.sha1(f"{tc.name}:{json.dumps(tc.args, sort_keys=True)}".encode()).hexdigest()
            if seen.get(key, 0) >= 1:
                result = "DUPLICATE CALL — this exact call already returned above. Choose a different action or emit your final JSON."
                strikes += 1
            else:
                result = toolbox.execute(tc.name, tc.args)
            seen[key] = seen.get(key, 0) + 1
            tool_calls_total += 1
            tool_by_name[tc.name] = tool_by_name.get(tc.name, 0) + 1
            messages.append({"role": "tool", "tool_call_id": tc.id, "content": result})
            transcript.append({"role": "tool", "name": tc.name, "args": tc.args,
                               "result_len": len(result)})
            observations.append(f"[{tc.name} {json.dumps(tc.args)[:80]}]\n{result[:300]}")
            if strikes >= caps["repeat_strikes"]:
                terminated = "repeat_strikes"
                stop = True
                break
        if stop:
            break

    # If we stopped without a final footprint, do a CLEAN-context final turn: no tool_calls in the
    # history (avoids a gateway 400 and DeepSeek's tool-markup leak), just the investigation notes.
    if terminated != "final":
        notes = "\n".join(observations[-50:]) or "(no tool observations)"
        clean = [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": task},
            {"role": "assistant", "content": "My investigation notes (tool observations):\n" + notes[:12000]},
            {"role": "user", "content": "Based ONLY on the notes above, output your final "
                                        "feature_footprint JSON now. No prose, no tools."},
        ]
        try:
            at = llm.chat(clean, [], model=model, max_tokens=caps["max_tokens"],
                          temperature=caps["temperature"], enable_thinking=False,
                          timeout=caps["deadline"])
            for k in usage_tot:
                usage_tot[k] += at.usage.get(k, 0)
            final = parse_footprint(at.content) or final
            transcript.append({"role": "assistant", "content": at.content, "forced_final": True})
        except Exception as e:  # noqa: BLE001
            transcript.append({"role": "error", "where": "forced_final", "detail": str(e)[:300]})

    return {
        "arm": arm, "model": model, "terminated": terminated,
        "turns": _turn + 1, "tool_calls_total": tool_calls_total,
        "tool_calls_by_name": tool_by_name,
        "prompt_tokens": usage_tot["prompt"], "completion_tokens": usage_tot["completion"],
        "total_tokens": usage_tot["total"], "est_cost_usd": round(llm.est_cost_usd(model, usage_tot), 5),
        "wall_secs": round(time.time() - start, 1),
        "final_footprint": final,
    }, transcript


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--task-file", help="JSON file with {id, task}")
    ap.add_argument("--task", help="inline task text (alternative to --task-file)")
    ap.add_argument("--arm", choices=["with_engram", "without_engram"], required=True)
    ap.add_argument("--model", default="deepseek-chat")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--namespace", default="default")
    ap.add_argument("--url", default="http://127.0.0.1:8088")
    ap.add_argument("--token", default=os.environ.get("ENGRAM_TOKEN", ""))
    ap.add_argument("--out", required=True, help="output dir for transcript + metrics")
    ap.add_argument("--max-turns", type=int, default=int(os.environ.get("AGENT_MAX_TURNS", 12)))
    ap.add_argument("--max-tool-calls", type=int, default=int(os.environ.get("AGENT_MAX_TOOL_CALLS", 30)))
    ap.add_argument("--max-tokens", type=int, default=int(os.environ.get("AGENT_MAX_TOKENS", 1024)))
    ap.add_argument("--temperature", type=float, default=float(os.environ.get("AGENT_TEMPERATURE", 0.1)))
    ap.add_argument("--deadline", type=int, default=int(os.environ.get("AGENT_DEADLINE_SECS", 180)))
    ap.add_argument("--repeat-strikes", type=int, default=3)
    ap.add_argument("--enable-thinking", action="store_true")
    args = ap.parse_args()

    if args.task_file:
        spec = json.load(open(args.task_file))
        task = spec["task"]
        task_id = spec.get("id", "task")
    else:
        task = args.task
        task_id = "task"

    caps = {"max_turns": args.max_turns, "max_tool_calls": args.max_tool_calls,
            "max_tokens": args.max_tokens, "temperature": args.temperature,
            "deadline": args.deadline, "repeat_strikes": args.repeat_strikes,
            "enable_thinking": args.enable_thinking}
    tb = ToolBox(args.repo, args.url, args.token, args.namespace)

    metrics, transcript = run(task, args.arm, args.model, tb, caps)
    metrics["task_id"] = task_id
    os.makedirs(args.out, exist_ok=True)
    with open(os.path.join(args.out, "metrics.json"), "w") as f:
        json.dump(metrics, f, indent=2)
    with open(os.path.join(args.out, "transcript.jsonl"), "w") as f:
        for row in transcript:
            f.write(json.dumps(row) + "\n")
    print(json.dumps({k: metrics[k] for k in
                      ("task_id", "arm", "model", "terminated", "turns", "tool_calls_total",
                       "total_tokens", "est_cost_usd", "wall_secs")}, indent=2))
    print(f"footprint: {len(metrics['final_footprint'])} files -> {args.out}")


if __name__ == "__main__":
    main()
