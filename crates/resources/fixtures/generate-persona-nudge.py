#!/usr/bin/python3
"""Records the auto-nudge rule of the user's Claude Code persona hook (ultron-persona.py) on fixed inputs.

NUDGE, ANNOUNCE, last_turn and should_nudge below are copied verbatim from that hook (checked with diff when
this fixture was generated); the Rust port (resources::persona) replays persona-nudge.json and must agree.
Deterministic: run it twice, same sha256. Usage: /usr/bin/python3 -I generate-persona-nudge.py > persona-nudge.json
"""
import json, os, re, sys, tempfile

MAX_NUDGES = 5
NUDGE = ("Continue: take that next step now. End the turn only when the work is done and verified, "
         "or when you need the user; then ask one direct question.")
ANNOUNCE = re.compile(
    r"\b(next,? I(?:'ll| will)|I(?:'ll| will) (?:now|next|then|start|run|add|fix|write|check)|"
    r"then I(?:'ll| will)|let me|next step|going to|about to)\b", re.I)


def last_turn(transcript):
    """(final assistant text, tool calls) since the last real user prompt."""
    try:
        with open(transcript) as f:
            entries = [json.loads(line) for line in f if line.strip()]
    except (OSError, ValueError):
        return "", 0
    text, tools = "", 0
    for entry in reversed(entries):
        msg = entry.get("message") or {}
        content = msg.get("content")
        if entry.get("type") == "user":
            if isinstance(content, str) or any(
                    isinstance(c, dict) and c.get("type") == "text" for c in content or []):
                break
            continue
        if entry.get("type") != "assistant" or not isinstance(content, list):
            continue
        for c in content:
            if c.get("type") == "tool_use":
                tools += 1
        if not text:
            text = "\n".join(c.get("text", "") for c in content if c.get("type") == "text").strip()
    return text, tools


def should_nudge(text, tools):
    if tools == 0 or not text:
        return False
    tail = text.strip().split("\n\n")[-1]
    if tail.rstrip().endswith("?"):
        return False
    return bool(ANNOUNCE.search(tail))


TEXTS = [
    "Next I'll run the tests.",
    "Next, I will update the docs.",
    "next I'll do it",
    "I'll now write the parser.",
    "I will check the logs.",
    "I'll fix it.",
    "I'll add a test.",
    "I'll start the build.",
    "I'll then commit.",
    "I'll look at it later.",
    "Then I'll rebuild.",
    "Let me look.",
    "LET ME check that.",
    "The next step is the migration.",
    "I'm going to refactor this.",
    "About to deploy.",
    "Should I run the tests?",
    "Next I'll run the tests?  ",
    "Next I'll run the tests.\n\nAll done.",
    "All done.\n\nNext I'll run the tests.",
    "Next I'll run the tests.\n\nShall I continue?",
    "Done: 14 tests pass.",
    "Outlet me know",
    "nextstep",
    "I'llrun",
    "I’ll run the tests.",
    "  \n\nLet me run it.\n\n  ",
    "Paragraph one.\n \nLet me run it.",
    "Ich werde es tun. Let me see.",
    "",
    "Let me",
    "going to?",
    "a\n\nb\n\n\nlet me x",
    "I will  run it.",
    "Next , I'll run",
    "next, i will",
]


def entry(kind, content):
    return {"type": kind, "message": {"role": kind, "content": content}}


TRANSCRIPTS = {
    "one tool then announce": [
        entry("user", "fix the bug"),
        entry("assistant", [{"type": "text", "text": "Looking."}, {"type": "tool_use", "id": "t1", "name": "rlm", "input": {}}]),
        entry("user", [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
        entry("assistant", [{"type": "text", "text": "Patched.\n\nNext I'll run the tests."}]),
    ],
    "earlier turn tools do not count": [
        entry("user", "first"),
        entry("assistant", [{"type": "tool_use", "id": "t1", "name": "rlm", "input": {}}]),
        entry("user", [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
        entry("assistant", [{"type": "text", "text": "done"}]),
        entry("user", [{"type": "text", "text": "second"}]),
        entry("assistant", [{"type": "text", "text": "Let me think."}]),
    ],
    "two tools in one message": [
        entry("user", "go"),
        entry("assistant", [{"type": "tool_use", "id": "a", "name": "rlm", "input": {}},
                            {"type": "tool_use", "id": "b", "name": "rlm", "input": {}}]),
        entry("user", [{"type": "tool_result", "tool_use_id": "a", "content": "ok"},
                       {"type": "tool_result", "tool_use_id": "b", "content": "ok"}]),
        entry("assistant", [{"type": "text", "text": "One"}, {"type": "text", "text": "Two  "}]),
    ],
    "last assistant without text": [
        entry("user", "go"),
        entry("assistant", [{"type": "text", "text": "I'll run it."}]),
        entry("assistant", [{"type": "tool_use", "id": "a", "name": "rlm", "input": {}}]),
    ],
    "other entry kinds are skipped": [
        {"type": "summary", "summary": "x"},
        entry("user", "go"),
        {"type": "system", "message": {"content": "note"}},
        entry("assistant", [{"type": "tool_use", "id": "a", "name": "rlm", "input": {}}]),
        entry("assistant", "a string content is skipped"),
        entry("assistant", [{"type": "text", "text": "Going to retry."}]),
    ],
    "no user prompt at all": [
        entry("assistant", [{"type": "tool_use", "id": "a", "name": "rlm", "input": {}}]),
        entry("assistant", [{"type": "text", "text": "About to finish."}]),
    ],
    "empty": [],
}


def main():
    cases = [{"text": t, "tools": n, "nudge": should_nudge(t, n)} for t in TEXTS for n in (0, 1, 3)]
    transcripts = []
    with tempfile.TemporaryDirectory() as tmp:
        for name, entries in TRANSCRIPTS.items():
            path = os.path.join(tmp, "t.jsonl")
            with open(path, "w") as f:
                for e in entries:
                    f.write(json.dumps(e) + "\n")
                f.write("\n")
            with open(path) as f:
                raw = f.read()
            text, tools = last_turn(path)
            transcripts.append({"name": name, "jsonl": raw, "text": text, "tools": tools,
                                "nudge": should_nudge(text, tools)})
        path = os.path.join(tmp, "bad.jsonl")
        raw = "{\"type\": \"assistant\", \"message\": {\"content\": [{\"type\": \"tool_use\"}]}}\nnot json\n"
        with open(path, "w") as f:
            f.write(raw)
        text, tools = last_turn(path)
        transcripts.append({"name": "malformed line", "jsonl": raw, "text": text, "tools": tools,
                            "nudge": should_nudge(text, tools)})
    json.dump({"nudge": NUDGE, "maxNudges": MAX_NUDGES, "cases": cases, "transcripts": transcripts},
              sys.stdout, indent=1, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
