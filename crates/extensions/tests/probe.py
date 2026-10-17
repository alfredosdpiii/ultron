"""A scriptable test extension process (crates/extensions/PROTOCOL.md), standard library only.

PROBE_CAPABILITIES (JSON): the `capabilities` it answers `initialize` with (default: none).
PROBE_LOG (path): every `initialize`, event, notification and command it receives is appended as a JSON line.
PROBE_ANSWERS (JSON): `{event type: result}` returned for events (default null). A result `{"$host": [method, params]}`
    makes the handler call the host first and return `{"host": <answer or {"error": message}>}`.
PROBE_COMMANDS (JSON): `{command name: [[method, params], ...]}`: `commands/run` calls the host with each request in
    order and logs the answers. A params value `{"$result": method}` is replaced by the latest answer to that method.
PROBE_SHORTCUT_ERRORS (JSON): `{key: message}`: `shortcuts/run` for that key fails with the message.

Functions: `host(method, params)` calls the host and returns its answer in `details` (`isError` with the message when it
fails); `notify(method, params)` sends a notification; `progress(updates, gate?)` reports each text of `updates` as
`functions/update` for its `toolCallId`, then waits for the file `gate` (when given) and returns "done" with the call's
`toolCallId` in `details`.
"""

import json
import os
import sys
import threading

PROTOCOL_VERSION = "2026-10-10"

_write_lock = threading.Lock()
_log_lock = threading.Lock()
_pending = {}
_pending_lock = threading.Lock()
_next_id = [1]


def send(message):
    with _write_lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()


def log(entry):
    path = os.environ.get("PROBE_LOG")
    if not path:
        return
    with _log_lock:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")


def env_json(name, default):
    text = os.environ.get(name)
    return json.loads(text) if text else default


def call_host(method, params):
    with _pending_lock:
        request_id = _next_id[0]
        _next_id[0] += 1
        waiter = {"event": threading.Event(), "response": None}
        _pending[request_id] = waiter
    send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
    waiter["event"].wait()
    response = waiter["response"]
    if "error" in response:
        return {"error": response["error"]["message"]}
    return {"result": response["result"]}


def on_event(event):
    log({"event": event})
    answer = env_json("PROBE_ANSWERS", {}).get(event["type"])
    if isinstance(answer, dict) and "$host" in answer:
        method, params = answer["$host"]
        outcome = call_host(method, params)
        log({"host": method, "outcome": outcome})
        return {"host": outcome.get("result", outcome)}
    return answer


def handle_request(message):
    request_id = message["id"]
    method = message["method"]
    params = message.get("params") or {}
    if method == "initialize":
        log({"initialize": params})
        result = {
            "protocolVersion": PROTOCOL_VERSION,
            "extensionInfo": {"name": "probe", "version": "1.0.0"},
            "capabilities": env_json("PROBE_CAPABILITIES", {}),
        }
    elif method == "event":
        result = on_event(params)
    elif method == "functions/call":
        args = params.get("arguments") or {}
        if params["name"] == "host":
            outcome = call_host(args["method"], args.get("params") or {})
            if "error" in outcome:
                result = {"content": [{"type": "text", "text": outcome["error"]}], "isError": True}
            else:
                result = {"content": [], "details": outcome["result"]}
        elif params["name"] == "progress":
            call_id = params.get("toolCallId")
            for text in args.get("updates") or []:
                send({"jsonrpc": "2.0", "method": "functions/update",
                      "params": {"toolCallId": call_id, "content": [{"type": "text", "text": text}]}})
            gate = args.get("gate")
            while gate and not os.path.exists(gate):
                threading.Event().wait(0.01)
            result = {"content": [{"type": "text", "text": "done"}], "details": {"toolCallId": call_id}}
        elif params["name"] == "notify":
            send({"jsonrpc": "2.0", "method": args["method"], "params": args.get("params") or {}})
            result = {"content": [{"type": "text", "text": "sent"}]}
        else:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32603, "message": "no function"}})
            return
    elif method == "commands/run":
        log({"command": params})
        answers = {}
        for host_method, host_params in env_json("PROBE_COMMANDS", {}).get(params["name"], []):
            host_params = {
                key: answers.get(value["$result"]) if isinstance(value, dict) and "$result" in value else value
                for key, value in host_params.items()
            }
            outcome = call_host(host_method, host_params)
            answers[host_method] = outcome.get("result")
            log({"host": host_method, "outcome": outcome})
        result = None
    elif method == "shortcuts/run":
        log({"shortcut": params})
        failure = env_json("PROBE_SHORTCUT_ERRORS", {}).get(params.get("key"))
        if failure:
            send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32603, "message": failure}})
            return
        result = None
    elif method == "shutdown":
        log({"shutdown": True})
        result = {}
    else:
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "Method not found: %s" % method}})
        return
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def main():
    for line in sys.stdin:
        if not line.strip():
            continue
        message = json.loads(line)
        if "method" in message and "id" in message:
            threading.Thread(target=handle_request, args=(message,), daemon=True).start()
        elif "method" in message:
            if message["method"] == "notifications/event":
                log({"event": message.get("params")})
            else:
                log({"notification": message["method"], "params": message.get("params")})
        else:
            with _pending_lock:
                waiter = _pending.pop(message.get("id"), None)
            if waiter is not None:
                waiter["response"] = message
                waiter["event"].set()


if __name__ == "__main__":
    main()
