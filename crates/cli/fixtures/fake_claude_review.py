"""A stand-in for the `claude` CLI in the autoreview-on-Claude-Code fixtures: never the real one.

It answers `--version`, `--help` (every flag the claude-code provider probes for) and `auth status --json` (logged
in), and serves one headless `-p --input-format stream-json` request per process: it reads the one user message
from stdin, picks the reply by the review prompt it sees (the reply rules of crates/cli/fixtures/autoreview_offline.mjs,
the stub chat-completions provider of the offline autoreview fixture), and prints a stream-json `result`.

FAKE_CLAUDE_LOG: one JSON line per request is appended: the model, effort, whether a JSON schema was passed, the
output cap from CLAUDE_CODE_MAX_OUTPUT_TOKENS, and the system prompt and user text.
"""

import json
import os
import sys

FLAGS = [
    "--print", "--output-format", "--input-format", "--verbose", "--include-partial-messages", "--tools",
    "--strict-mcp-config", "--mcp-config", "--setting-sources", "--permission-prompts", "--disable-slash-commands",
    "--no-session-persistence", "--model", "--system-prompt", "--settings", "--json-schema", "--effort",
]


def verdict_for(body):
    if "wrong mean" in body:
        return {"verdict": "confirmed", "evidence": "`return total(items) / count` divides the truncated sum by the full count.", "corrected_line": None, "severity": "major", "scenario_holds": True}
    if "skips the last item" in body:
        return {"verdict": "confirmed", "evidence": "`for i in range(len(items) - 1):` stops before the last index.", "corrected_line": None, "severity": "major", "scenario_holds": True}
    return {"verdict": "rejected", "evidence": "`if count == 0:` returns first.", "corrected_line": None, "severity": "minor", "scenario_holds": False}


def reply(body):
    if "You check several findings of an automated pull request review" in body:
        sections = body.split("=== Finding ")[1:]
        return json.dumps([dict(verdict_for(section), finding=index + 1) for index, section in enumerate(sections)])
    if "You check one finding of an automated pull request review" in body:
        return json.dumps(verdict_for(body))
    if "You investigate one pull request beyond its diff" in body:
        return json.dumps({"findings": [], "requests": [], "done": True})
    if "Your specialty: Correctness" in body and "File: calc.py" in body:
        return json.dumps([
            {"file": "calc.py", "line": 4, "end_line": 4, "severity": "major", "category": "correctness", "claim": "total() skips the last item.", "why": "range(len(items) - 1) stops one short.", "suggested_fix": "Use range(len(items)).", "confidence": 0.9, "replacement": "    for i in range(len(items)):", "scenario": "total([{'price': 1}, {'price': 2}]) returns 1; it should return 3."},
            {"file": "calc.py", "line": 12, "end_line": 12, "severity": "major", "category": "correctness", "claim": "average() divides by zero.", "why": "count may be 0.", "suggested_fix": "Guard it.", "confidence": 0.6, "replacement": None, "scenario": "average([], 0) raises ZeroDivisionError."},
        ])
    return "[]"


def option(args, name):
    for arg in args:
        if arg.startswith(name + "="):
            return arg[len(name) + 1:]
    return None


def main():
    args = sys.argv[1:]
    if args[:1] == ["--version"]:
        print("2.1.99 (Claude Code)")
        return 0
    if args[:1] == ["--help"]:
        for flag in FLAGS:
            print(f"  {flag} <value>  description")
        return 0
    if args[:1] == ["auth"]:
        print(json.dumps({"loggedIn": True, "authMethod": "claude.ai", "apiProvider": "firstParty"}))
        return 0
    line = sys.stdin.readline()
    if not line:
        return 0
    message = json.loads(line)["message"]
    content = message["content"]
    text = content if isinstance(content, str) else "".join(part.get("text", "") for part in content)
    system = option(args, "--system-prompt") or ""
    answer = reply(system + "\n" + text)
    record = {
        "model": option(args, "--model"),
        "effort": option(args, "--effort"),
        "schema": option(args, "--json-schema") is not None,
        "maxTokens": os.environ.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        "system": system,
        "text": text,
    }
    log = os.environ.get("FAKE_CLAUDE_LOG")
    if log:
        with open(log, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record) + "\n")
    result = {
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "result": answer,
        "stop_reason": "end_turn",
        "total_cost_usd": 0,
        "usage": {"input_tokens": 100, "output_tokens": 20},
    }
    sys.stdout.write(json.dumps(result) + "\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
