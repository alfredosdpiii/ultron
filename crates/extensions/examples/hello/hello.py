"""A tiny Ultron extension process (crates/extensions/PROTOCOL.md), standard library only.

Registers:
  functions  greet(name), slow(seconds), cancelled(), crash(), info() (running: slow calls in flight),
             shout(channel, data)
  command    /hello            asks ui/select, then sets a status line and a panel
  events     session_before_switch (vetoes "resume"), tool_call (blocks "rm -rf"), input ("hi" -> "hello"),
             session_start (status line), turn_start (exits when HELLO_CRASH_ON_TURN matches), ui_prompt_start / ui_prompt_end (recorded)
  bus        "greetings"       messages are recorded and returned by info()
"""

import json
import os
import sys
import threading
import time

PROTOCOL_VERSION = "2026-10-10"

_write_lock = threading.Lock()
_pending = {}
_pending_lock = threading.Lock()
_next_id = [1]
_cancelled = set()
_running = set()
_seen = {"prompts": [], "bus": [], "restart": None, "session": None}


def send(message):
    with _write_lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def request(method, params):
    """Calls the host and waits for its answer (handlers run on their own threads, so this cannot deadlock)."""
    with _pending_lock:
        request_id = _next_id[0]
        _next_id[0] += 1
        waiter = {"event": threading.Event(), "response": None}
        _pending[request_id] = waiter
    send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
    waiter["event"].wait()
    response = waiter["response"]
    if "error" in response:
        raise RuntimeError(response["error"]["message"])
    return response["result"]


class HandlerError(Exception):
    pass


def text(value):
    return {"content": [{"type": "text", "text": value}]}


def call_function(request_id, name, args):
    if name == "greet":
        return text("Hello, %s!" % args.get("name", "world"))
    if name == "slow":
        _running.add(request_id)
        try:
            deadline = time.monotonic() + float(args.get("seconds", 1))
            while time.monotonic() < deadline:
                if request_id in _cancelled:
                    return text("stopped")
                time.sleep(0.01)
            return text("slept")
        finally:
            _running.discard(request_id)
    if name == "cancelled":
        return {"content": [], "details": sorted(_cancelled)}
    if name == "crash":
        os._exit(3)
    if name == "info":
        return {"content": [], "details": {"pid": os.getpid(), "running": sorted(_running), **_seen}}
    if name == "shout":
        notify("bus/emit", {"channel": args["channel"], "data": args.get("data")})
        return text("sent")
    raise HandlerError("no function %s" % name)


def on_event(event):
    kind = event["type"]
    if kind == "session_before_switch":
        return {"cancel": True} if event.get("reason") == "resume" else None
    if kind == "tool_call":
        code = json.dumps(event.get("input"))
        if "rm -rf" in code:
            return {"block": True, "reason": "hello refuses rm -rf"}
        return None
    if kind == "input":
        if event["text"] == "hi":
            return {"action": "transform", "text": "hello"}
        return {"action": "continue"}
    if kind == "session_start":
        notify("ui/setStatus", {"key": "hello", "text": "hello: %s" % event.get("reason")})
        return None
    if kind == "turn_start":
        # Lets tests crash one process in the middle of an event dispatch.
        if str(event.get("turnIndex")) == os.environ.get("HELLO_CRASH_ON_TURN"):
            os._exit(4)
        return None
    if kind in ("ui_prompt_start", "ui_prompt_end"):
        _seen["prompts"].append([kind, event.get("kind"), event.get("title")])
        return None
    return None


def run_command(name, args):
    if name != "hello":
        raise HandlerError("no command %s" % name)
    choice = request("ui/select", {"title": "Greet whom?", "options": ["world", args or "you"]})
    if choice is None:
        notify("ui/notify", {"message": "hello cancelled", "type": "warning"})
        return None
    notify("ui/setStatus", {"key": "hello", "text": "greeted %s" % choice})
    notify("ui/setPanel", {"key": "hello", "lines": ["Hello,", choice], "placement": "belowEditor"})
    return None


def handle_request(message):
    request_id = message["id"]
    method = message["method"]
    params = message.get("params") or {}
    try:
        if method == "initialize":
            _seen["restart"] = params.get("restart")
            _seen["session"] = params.get("session")
            result = {
                "protocolVersion": PROTOCOL_VERSION,
                "extensionInfo": {"name": "hello", "version": "1.0.0"},
                "capabilities": {
                    "events": [
                        "session_before_switch",
                        "tool_call",
                        "input",
                        "session_start",
                        "turn_start",
                        "ui_prompt_start",
                        "ui_prompt_end",
                    ],
                    "functions": [
                        {
                            "name": "greet",
                            "description": "Greet someone",
                            "parameters": {"type": "object", "properties": {"name": {"type": "string"}}},
                        },
                        {"name": "slow", "description": "Sleep, stopping early when cancelled"},
                        {"name": "cancelled", "description": "Request ids the host cancelled"},
                        {"name": "crash", "description": "Exit without answering"},
                        {"name": "info", "description": "Process facts"},
                        {"name": "shout", "description": "Emit on the extension bus"},
                    ],
                    "commands": [{"name": "hello", "description": "Say hello"}],
                    "bus": ["greetings"],
                },
            }
        elif method == "event":
            result = on_event(params)
        elif method == "functions/call":
            result = call_function(request_id, params["name"], params.get("arguments") or {})
        elif method == "commands/run":
            result = run_command(params["name"], params.get("args", ""))
        elif method == "shutdown":
            result = {}
        else:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Method not found: %s" % method}})
            return
        send({"jsonrpc": "2.0", "id": request_id, "result": result})
    except Exception as error:  # a handler error becomes a JSON-RPC error; the process keeps running
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32603, "message": str(error)}})


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        message = json.loads(line)
        if "method" in message and "id" in message:
            threading.Thread(target=handle_request, args=(message,), daemon=True).start()
        elif "method" in message:
            params = message.get("params") or {}
            if message["method"] == "notifications/cancelled":
                _cancelled.add(params.get("requestId"))
            elif message["method"] == "notifications/event":
                on_event(params)
            elif message["method"] == "bus/message":
                _seen["bus"].append([params.get("channel"), params.get("data")])
        else:
            with _pending_lock:
                waiter = _pending.pop(message.get("id"), None)
            if waiter is not None:
                waiter["response"] = message
                waiter["event"].set()


if __name__ == "__main__":
    main()
