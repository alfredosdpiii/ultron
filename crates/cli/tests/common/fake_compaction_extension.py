"""A fake Ultron extension process (crates/extensions/PROTOCOL.md) for the compaction hook tests: it subscribes to
`session_before_compact`, `session_compact` and `session_compact_failed`, appends every event it receives to
FAKE_COMPACTION_LOG as a JSON line, and answers `session_before_compact` by FAKE_COMPACTION_MODE:

  cancel   {"cancel": true}: the compaction is cancelled
  summary  {"compaction": {...}}: its own summary, keeping the preparation's first kept entry, with usage and details
  wait     no answer: the hook waits for its signal (`notifications/cancelled`, logged as
           {"type": "notifications/cancelled", "reason": ...}) and then answers {"cancel": true}, as the TypeScript
           test's hook resolves on `event.signal`'s abort
  (other)  null: the compaction goes on

It also subscribes to `session_before_tree` and `session_tree`, answering `session_before_tree` by FAKE_TREE_MODE:

  cancel    {"cancel": true}
  summary   {"summary": {...}}: its own branch summary, with details and usage
  override  {"customInstructions": "focus on names", "replaceInstructions": true, "label": "checkpoint"}
  wait      as `wait` above, then {"cancel": true}
  (other)   null

With FAKE_HOLD_AFTER set, `session_compact`, `session_compact_failed` and `session_tree` are logged and then held
until the file `<FAKE_COMPACTION_LOG>.release` exists (the test reads the session's state meanwhile).

Standard library only.
"""

import json
import os
import sys
import threading
import time

PROTOCOL_VERSION = "2026-10-10"
EVENTS = ["session_before_compact", "session_compact", "session_compact_failed", "session_before_tree", "session_tree"]
HELD_AFTER = ["session_compact", "session_compact_failed", "session_tree"]

_write_lock = threading.Lock()
_log_lock = threading.Lock()


def send(message):
    with _write_lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def log(event):
    with _log_lock, open(os.environ["FAKE_COMPACTION_LOG"], "a") as out:
        out.write(json.dumps(event) + "\n")


_signals = {}
_signals_lock = threading.Lock()


def signal_for(request_id):
    with _signals_lock:
        return _signals.setdefault(json.dumps(request_id), threading.Event())


def hold_until_released():
    release = os.environ["FAKE_COMPACTION_LOG"] + ".release"
    while not os.path.exists(release):
        time.sleep(0.01)


def on_tree(request_id):
    mode = os.environ.get("FAKE_TREE_MODE", "")
    if mode == "cancel":
        return {"cancel": True}
    if mode == "summary":
        return {"summary": {
            "summary": "Branch summary from the extension.",
            "details": {"by": "fake-compaction"},
            "usage": {"input": 4, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 6,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        }}
    if mode == "override":
        return {"customInstructions": "focus on names", "replaceInstructions": True, "label": "checkpoint"}
    if mode == "wait":
        signal_for(request_id).wait()
        return {"cancel": True}
    return None


def on_event(event, request_id):
    log(event)
    kind = event.get("type")
    if kind in HELD_AFTER and os.environ.get("FAKE_HOLD_AFTER"):
        hold_until_released()
    if kind == "session_before_tree":
        return on_tree(request_id)
    if kind != "session_before_compact":
        return None
    mode = os.environ.get("FAKE_COMPACTION_MODE", "")
    if mode == "wait":
        signal_for(request_id).wait()
        return {"cancel": True}
    if mode == "cancel":
        return {"cancel": True}
    if mode == "summary":
        preparation = event["preparation"]
        return {"compaction": {
            "summary": "## Goal\nSummary from the extension.",
            "firstKeptEntryId": preparation["firstKeptEntryId"],
            "tokensBefore": preparation["tokensBefore"],
            "details": {"by": "fake-compaction"},
            "usage": {"input": 7, "output": 3, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 10,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        }}
    return None


def handle_request(message):
    request_id = message["id"]
    method = message["method"]
    params = message.get("params") or {}
    try:
        if method == "initialize":
            result = {
                "protocolVersion": PROTOCOL_VERSION,
                "extensionInfo": {"name": "fake-compaction", "version": "1.0.0"},
                "capabilities": {"events": EVENTS},
            }
        elif method == "event":
            result = on_event(params, request_id)
        elif method == "shutdown":
            result = {}
        else:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Method not found: %s" % method}})
            return
        send({"jsonrpc": "2.0", "id": request_id, "result": result})
    except Exception as error:
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32603, "message": str(error)}})


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        message = json.loads(line)
        if message.get("method") == "notifications/cancelled":
            params = message.get("params") or {}
            log({"type": "notifications/cancelled", "reason": params.get("reason")})
            signal_for(params.get("requestId")).set()
        elif "method" in message and "id" in message:
            threading.Thread(target=handle_request, args=(message,), daemon=True).start()


if __name__ == "__main__":
    main()
