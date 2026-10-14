"""A fake Ultron extension process (crates/extensions/PROTOCOL.md) for the `tools.*` tests, ported from
packages/coding-agent/test/support/fake-mcp-extension.ts. It registers, as functions (TypeScript `registerTool`):

  mcp    an MCP-gateway-shaped tool over two fake servers: `{tool, args, server?}` calls exa-agent's
         exa_agent_create_run (returns a run id at once) or exa_agent_wait_run (waits until the file named by
         FAKE_MCP_GATE exists, else returns at once), unknown tools fail like the gateway (`details.error`)
  probe  echoes its text (repeated), with `details: {length, echo}`
  slow   waits until the file `gate` exists (else `ms` milliseconds), stopping early when cancelled
  boom   always fails

Every call is appended to FAKE_CALLS_LOG as a JSON line {"tool", "params"}; a call stopped by a cancellation adds
{"tool", "aborted": true}. Standard library only.
"""

import json
import os
import sys
import threading
import time

PROTOCOL_VERSION = "2026-10-10"

_write_lock = threading.Lock()
_log_lock = threading.Lock()
_cancelled = set()
_runs = {"count": 0, "queries": {}}


def send(message):
    with _write_lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def log(entry):
    path = os.environ.get("FAKE_CALLS_LOG")
    if not path:
        return
    with _log_lock, open(path, "a") as out:
        out.write(json.dumps(entry) + "\n")


def text(value, details=None):
    result = {"content": [{"type": "text", "text": value}]}
    if details is not None:
        result["details"] = details
    return result


def wait(request_id, tool, until):
    """Polls `until()` (a file appearing, a deadline) or a cancellation; False when cancelled."""
    while not until():
        if request_id in _cancelled:
            log({"tool": tool, "aborted": True})
            return False
        time.sleep(0.01)
    return True


def mcp(request_id, params):
    tool = params.get("tool")
    args = params.get("args") or {}
    if isinstance(args, str):
        args = json.loads(args)
    name = tool if params.get("server") is None else "%s_%s" % (params["server"], tool)
    if name == "exa-agent_exa_agent_create_run":
        _runs["count"] += 1
        run_id = "run-%d" % _runs["count"]
        _runs["queries"][run_id] = args.get("query")
        return text(json.dumps({"id": run_id, "status": "running", "query": args.get("query"),
                                "effort": args.get("effort", "medium")}), {"server": "exa-agent"})
    if name == "exa-agent_exa_agent_wait_run":
        gate = os.environ.get("FAKE_MCP_GATE")
        if gate and not wait(request_id, "mcp", lambda: os.path.exists(gate)):
            raise RuntimeError("aborted")
        run_id = args.get("run_id")
        return text(json.dumps({"id": run_id, "status": "completed",
                                "output": "answer for %s" % _runs["queries"].get(run_id, "?")}), {"server": "exa-agent"})
    return text('Tool "%s" not found.' % name, {"error": "tool_not_found"})


def call_function(request_id, name, args):
    log({"tool": name, "params": args})
    if name == "mcp":
        return mcp(request_id, args)
    if name == "probe":
        body = str(args.get("text", "probed")) * int(args.get("repeat", 1))
        return text(body, {"length": len(body), "echo": args})
    if name == "slow":
        gate = args.get("gate")
        if gate:
            until = lambda: os.path.exists(gate)  # noqa: E731
        else:
            deadline = time.monotonic() + float(args.get("ms", 0)) / 1000
            until = lambda: time.monotonic() >= deadline  # noqa: E731
        if not wait(request_id, "slow", until):
            raise RuntimeError("aborted")
        return text("slept %s" % args.get("ms") if "ms" in args else "slept", {"ms": args.get("ms")})
    if name == "boom":
        raise RuntimeError("boom went the tool")
    raise RuntimeError("no function %s" % name)


FUNCTIONS = [
    {"name": "mcp", "description": "MCP gateway - status and calls.\n\nServers: exa-agent, docs",
     "parameters": {"type": "object", "properties": {"tool": {"type": "string"}, "args": {}, "server": {"type": "string"}}}},
    {"name": "probe", "description": "An extension tool that echoes its arguments.\n\nSecond paragraph that the list leaves out.",
     "parameters": {"type": "object", "properties": {"text": {"type": "string"}, "repeat": {"type": "integer"}}}},
    {"name": "slow", "description": "Waits `ms` milliseconds.",
     "parameters": {"type": "object", "properties": {"ms": {"type": "integer"}, "gate": {"type": "string"}}}},
    {"name": "boom", "description": "Always throws.", "parameters": {"type": "object", "properties": {}}},
]


def handle_request(message):
    request_id = message["id"]
    method = message["method"]
    params = message.get("params") or {}
    try:
        if method == "initialize":
            result = {
                "protocolVersion": PROTOCOL_VERSION,
                "extensionInfo": {"name": "fake-tools", "version": "1.0.0"},
                "capabilities": {"functions": FUNCTIONS},
            }
        elif method == "functions/call":
            result = call_function(request_id, params["name"], params.get("arguments") or {})
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
        if "method" in message and "id" in message:
            threading.Thread(target=handle_request, args=(message,), daemon=True).start()
        elif message.get("method") == "notifications/cancelled":
            _cancelled.add((message.get("params") or {}).get("requestId"))


if __name__ == "__main__":
    main()
