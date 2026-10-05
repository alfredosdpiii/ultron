"""Find and read Ultron session files from the REPL (read-only).

Load with `s = runpy.run_path(f"{skill_dir}/scripts/sessions.py")`, then call `s["session_files"](...)` and
`s["read_session"](path)`.

Two layouts exist, both one directory per working directory (`--<cwd with / and : as ->--`):
- native Ultron sessions: `<agent dir>/experimental/sessions/` (or ULTRON_CODING_AGENT_SESSION_DIR), each line a
  storage record or a list of them; messages are `{"kind": "entry", "type": "message", "message": {...}}`, and the
  main conversation is the parent chain from the last `pi.branch.tip` value for lane `main` (other lanes in the same
  file are inference frames);
- Pi-format sessions: `<agent dir>/sessions/`, each line one entry (`{"type": "message", "message": {...}}`).
"""

import json
import os
import re
import time
from pathlib import Path


def agent_dir():
    return Path(os.path.expanduser(os.environ.get("ULTRON_CODING_AGENT_DIR") or "~/.ultron/agent"))


def session_roots():
    """Directories that hold per-cwd session folders, existing ones only."""
    roots = []
    custom = os.environ.get("ULTRON_CODING_AGENT_SESSION_DIR")
    if custom:
        roots.append(Path(os.path.expanduser(custom)))
    roots += [agent_dir() / "experimental" / "sessions", agent_dir() / "sessions"]
    return [root for root in roots if root.is_dir()]


def cwd_folder(cwd=None):
    """The folder name a working directory's sessions are stored under."""
    resolved = os.path.abspath(cwd or os.getcwd())
    return "--" + re.sub(r"[/\\:]", "-", resolved.lstrip("/\\")) + "--"


def session_files(cwd=None, days=7, all_projects=False):
    """Session files modified in the last `days` days, newest first by real modification time.

    Scoped to one working directory (default: the current one) unless `all_projects` is True.
    """
    cutoff = time.time() - days * 86400
    pattern = "*/*.jsonl" if all_projects else f"{cwd_folder(cwd)}/*.jsonl"
    files = [path for root in session_roots() for path in root.glob(pattern)]
    files = [(path.stat().st_mtime, path) for path in files]
    return [path for mtime, path in sorted(files, reverse=True) if mtime >= cutoff]


def _text(content, tool_chars):
    if isinstance(content, str):
        return content
    parts = []
    for part in content or []:
        if not isinstance(part, dict):
            continue
        kind = part.get("type")
        if kind == "text":
            parts.append(part.get("text", ""))
        elif kind == "toolCall":
            args = part.get("arguments") or {}
            body = args.get("code") or args.get("command") or json.dumps(args)[:tool_chars]
            parts.append(f"[{part.get('name')}]\n{body}")
        elif kind == "image":
            parts.append("[image]")
    return "\n".join(parts)


def _message(message, tool_chars):
    role = message.get("role")
    text = _text(message.get("content"), tool_chars)
    if role == "toolResult":
        text = text[:tool_chars] + ("..." if len(text) > tool_chars else "")
    return {"role": role, "text": text, "tool": message.get("toolName")}


def read_session(path, tool_chars=2000):
    """{path, id, cwd, parent, name, messages: [{role, text, tool, ts}]} for the session's main conversation.

    Tool results are cut to `tool_chars`; thinking is left out. `parent` is set for a subagent's own session file.
    """
    header = {}
    entries = {}
    parents = {}
    order = []
    tips = {}
    name = None
    with open(path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            try:
                record = json.loads(line)
            except ValueError:
                continue  # a torn last line
            for item in record if isinstance(record, list) else [record]:
                if not isinstance(item, dict):
                    continue
                if "id" in item and "parentId" in item:
                    parents[item["id"]] = item["parentId"]
                if item.get("kind") == "header" or item.get("type") == "session":
                    header = item
                elif item.get("type") == "session_info":
                    name = item.get("name") or name
                elif item.get("namespace") == "pi.branch.tip":
                    tips[item.get("key")] = item.get("value")
                elif item.get("type") == "message" and isinstance(item.get("message"), dict):
                    entries[item.get("id")] = item
                    order.append(item.get("id"))
    def chain(tip):
        found, seen = [], set()
        while tip in parents and tip not in seen:
            seen.add(tip)
            if tip in entries:
                found.append(tip)
            tip = parents[tip]
        return list(reversed(found))

    if tips.get("main") in parents:
        ids = chain(tips["main"])
    else:  # no main tip: drop the inference-frame lanes, keep the rest in file order
        frames = {i for key, tip in tips.items() if key != "main" for i in chain(tip)}
        ids = [i for i in order if i not in frames]
    messages = []
    for entry_id in ids:
        item = entries[entry_id]
        message = _message(item["message"], tool_chars)
        message["ts"] = item.get("timestamp")
        messages.append(message)
    return {
        "path": str(path),
        "id": header.get("id"),
        "cwd": header.get("cwd"),
        "parent": header.get("parentSession") or header.get("parentSessionId"),
        "name": name,
        "messages": messages,
    }


def grep_sessions(files, pattern, flags=re.I):
    """[(path, count)] of files whose raw text matches `pattern`, most matches first."""
    found = []
    for path in files:
        try:
            count = len(re.findall(pattern, Path(path).read_text(encoding="utf-8", errors="replace"), flags))
        except OSError:
            continue
        if count:
            found.append((str(path), count))
    return sorted(found, key=lambda item: -item[1])
