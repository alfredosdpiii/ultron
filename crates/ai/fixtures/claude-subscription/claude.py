"""A stand-in for the official `claude` CLI in the claude-subscription tests: never the real one.

It speaks what the provider relies on: `--version`, `auth status`, the stream-json `initialize` handshake, replay
acknowledgments for `shouldQuery: false` frames, the inert MCP server from `--mcp-config` (initialize, tools/list,
tools/call), and one Messages request to ANTHROPIC_BASE_URL built from the frames, the system prompt file and the
CLAUDE_CODE_EXTRA_BODY of the `--settings` file, whose SSE answer it re-emits as stream-json.

Behaviour switches (environment):
  FAKE_LOG               append one JSON record per run: argv, env, cwd, settings, system, frames, inventory, attempts
  FAKE_VERSION           `--version` output
  FAKE_AUTH              `auth status` JSON (exit 1 when not logged in)
  FAKE_MODELS / FAKE_ACCOUNT   the initialize handshake's picker and account
  FAKE_HANDSHAKE_UPSTREAM      the handshake also POSTs to ANTHROPIC_BASE_URL (must be refused or never happen)
  FAKE_GARBAGE           print a non-JSON line and exit
  FAKE_NATIVE_ERROR      `code:text`: answer with native's own API error, no upstream request
  FAKE_RATE_LIMIT        JSON `rate_limit_info`: emit native's `rate_limit_event` before answering (shape of the TypeScript
                         fakes: {"status": "rejected", "rateLimitType": "five_hour", "resetsAt": <epoch s>})
  FAKE_HANG              print one text delta, start a grandchild, sleep (FAKE_PID_FILE records both pids)
  FAKE_BAD_ACK           acknowledge a replayed frame with num_turns 1
  FAKE_OFFLINE_BLOCKS    JSON content blocks to answer without any upstream request (FAKE_OFFLINE_USAGE, FAKE_TOOL_NAME,
                         FAKE_DELTA_DELAY seconds between deltas, FAKE_SLEEP before answering)
  FAKE_RETRIES           extra Messages attempts after the first (native recovery; the relay must refuse them)
  FAKE_REMINDER          append native's per-request reminder to the newest tool result / user text
  FAKE_PREPEND           on a first turn, prepend native's context block to the first user message
  FAKE_MARK              put a cache_control marker on the newest block
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

REMINDER = "\n<system-reminder>userEmail: someone@example.invalid</system-reminder>"
argv = sys.argv[1:]
env = dict(os.environ)
record = {"argv": argv, "env": env, "cwd": os.getcwd(), "pid": os.getpid(), "attempts": []}


def log():
    if env.get("FAKE_LOG"):
        with open(env["FAKE_LOG"], "a", encoding="utf-8") as out:
            out.write(json.dumps(record) + "\n")


def emit(value):
    print(json.dumps(value), flush=True)


def flag(name):
    return argv[argv.index(name) + 1] if name in argv else None


if env.get("FAKE_PID_FILE"):
    with open(env["FAKE_PID_FILE"], "w") as out:
        out.write(str(os.getpid()))
if argv[:1] == ["--version"]:
    print(env.get("FAKE_VERSION", "2.1.263 (Claude Code)"))
    sys.exit(0)
if argv[:2] == ["auth", "status"]:
    record["kind"] = "auth"
    log()
    auth = json.loads(env.get("FAKE_AUTH", '{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "pro"}'))
    print(json.dumps(auth))
    sys.exit(0 if auth.get("loggedIn") else 1)
if env.get("FAKE_GARBAGE"):
    print("mise WARN tool not activated", flush=True)
    sys.exit(0)

settings = {}
if flag("--settings"):
    with open(flag("--settings"), encoding="utf-8") as f:
        record["settings"] = f.read()
    settings = json.loads(record["settings"])
system = ""
if flag("--system-prompt-file"):
    with open(flag("--system-prompt-file"), encoding="utf-8") as f:
        system = record["system"] = f.read()


def post(payload):
    request = urllib.request.Request(env["ANTHROPIC_BASE_URL"] + "/v1/messages?beta=true", data=payload, headers={
        "Content-Type": "application/json", "Authorization": "Bearer fake-subscription-token",
        "anthropic-version": "2023-06-01", "x-app": "cli"})
    return urllib.request.urlopen(request, timeout=60)


# ------------------------------------------------------------------------------------------------- stdin frames
frames = []
for line in sys.stdin:
    if not line.strip():
        continue
    frame = json.loads(line)
    if frame.get("type") == "control_request":
        record["kind"] = "handshake"
        if env.get("FAKE_HANDSHAKE_UPSTREAM"):
            try:
                post(b"{}").read()
            except Exception:
                pass
        emit({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"],
              "response": {"models": json.loads(env.get("FAKE_MODELS", "[]")),
                           "account": json.loads(env.get("FAKE_ACCOUNT", "{}"))}}})
        log()
        sys.exit(0)
    frames.append(frame)
    if frame.get("shouldQuery") is False:
        emit({"type": "result", "num_turns": 1 if env.get("FAKE_BAD_ACK") else 0, "is_error": False})
record["frames"] = frames
if env.get("FAKE_RATE_LIMIT"):
    emit({"type": "rate_limit_event", "rate_limit_info": json.loads(env["FAKE_RATE_LIMIT"])})

if env.get("FAKE_NATIVE_ERROR"):
    code, text = env["FAKE_NATIVE_ERROR"].split(":", 1)
    emit({"type": "assistant", "error": code, "is_api_error_message": True, "message": {
        "role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": text}], "stop_reason": "stop_sequence"}})
    emit({"type": "result", "subtype": "success", "is_error": True, "num_turns": 1, "result": text})
    log()
    sys.exit(1)

if env.get("FAKE_HANG"):
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(3600)"],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if env.get("FAKE_PID_FILE"):
        with open(env["FAKE_PID_FILE"] + ".child", "w") as out:
            out.write(str(child.pid))
    emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "started"}}})
    log()
    time.sleep(3600)
    sys.exit(0)

# ----------------------------------------------------------------------------------------- the inert MCP server
inventory = []
config = json.loads(flag("--mcp-config") or '{"mcpServers": {}}')
for name, server in config.get("mcpServers", {}).items():
    proc = subprocess.Popen([server["command"], *server.get("args", [])], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            text=True)

    def rpc(message):
        proc.stdin.write(json.dumps(message) + "\n")
        proc.stdin.flush()
        if "id" in message:
            return json.loads(proc.stdout.readline())

    init = rpc({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
               "capabilities": {}, "clientInfo": {"name": "fake-claude", "version": "0"}}})
    rpc({"jsonrpc": "2.0", "method": "notifications/initialized"})
    listed = rpc({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})["result"]["tools"]
    called = rpc({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "rlm", "arguments": {"code": "1"}}})
    proc.stdin.close()
    proc.wait(timeout=10)
    record.setdefault("mcp", {})[name] = {"initialize": init["result"], "tools": listed, "call": called["result"]}
    inventory += [{"name": f"mcp__{name}__{t['name']}", "description": t.get("description", ""),
                   "input_schema": t.get("inputSchema", {})} for t in listed]


def finish(blocks, usage, stop):
    tool = any(b.get("type") == "tool_use" for b in blocks)
    emit({"type": "assistant", "message": {"id": "msg_native", "role": "assistant", "model": flag("--model"),
                                           "content": blocks, "stop_reason": stop, "usage": usage}})
    emit({"type": "result", "subtype": "error_max_turns" if tool else "success", "is_error": tool,
          "num_turns": 2 if tool else 1, "usage": usage, "total_cost_usd": 0.012345,
          "modelUsage": {flag("--model"): {"costBasis": "list"}}})
    log()
    sys.exit(1 if tool else 0)


# ------------------------------------------------------------------------------------------- answer without HTTP
if env.get("FAKE_OFFLINE_BLOCKS"):
    blocks = json.loads(env["FAKE_OFFLINE_BLOCKS"])
    time.sleep(float(env.get("FAKE_SLEEP", "0")))
    for index, block in enumerate(blocks):
        if block.get("type") == "tool_use" and env.get("FAKE_TOOL_NAME"):
            block["name"] = env["FAKE_TOOL_NAME"]
        field, kind = {"text": ("text", "text_delta"), "thinking": ("thinking", "thinking_delta")}.get(block["type"], (None, None))
        if field:
            for piece in ([block[field][i:i + 1] for i in range(len(block[field]))] if env.get("FAKE_DELTA_DELAY") else [block[field]]):
                emit({"type": "stream_event", "event": {"type": "content_block_delta", "index": index,
                                                        "delta": {"type": kind, field: piece}}})
                time.sleep(float(env.get("FAKE_DELTA_DELAY", "0")))
    emit({"type": "stream_event", "event": {"type": "message_stop"}})
    usage = json.loads(env.get("FAKE_OFFLINE_USAGE", '{"input_tokens": 1, "output_tokens": 1}'))
    finish(blocks, usage, "tool_use" if any(b["type"] == "tool_use" for b in blocks) else "end_turn")

# ------------------------------------------------------------------------------------------ one Messages request
messages = []
for frame in frames:
    message = json.loads(json.dumps(frame["message"]))
    if messages and messages[-1]["role"] == message["role"]:
        messages[-1]["content"] += message["content"]
    else:
        messages.append(message)
newest = messages[-1]["content"] if messages else []
if env.get("FAKE_REMINDER") and newest:
    last = newest[-1]
    if last.get("type") == "tool_result":
        if isinstance(last.get("content"), str):
            last["content"] += REMINDER
        else:
            last.setdefault("content", []).append({"type": "text", "text": REMINDER})
        last["is_error"] = last.get("is_error", False)
    else:
        newest.append({"type": "text", "text": REMINDER})
if env.get("FAKE_PREPEND") and not any(m["role"] == "assistant" for m in messages):
    messages[0]["content"].insert(0, {"type": "text", "text": REMINDER.strip()})
if env.get("FAKE_MARK") and newest:
    newest[-1]["cache_control"] = {"type": "ephemeral", "ttl": "1h"}
body = {"model": flag("--model"), "max_tokens": int(env.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "32000")),
        "system": [{"type": "text", "text": "x-anthropic-billing-header: fake"}, {"type": "text", "text": system}],
        "messages": messages, "tools": inventory, "stream": True}
record["native_tools"] = [t["name"] for t in inventory]
body.update(json.loads(settings.get("env", {}).get("CLAUDE_CODE_EXTRA_BODY", "{}")))

assistant, error = None, None
for attempt in range(1 + int(env.get("FAKE_RETRIES", "0"))):
    try:
        response = post(json.dumps(body).encode())
    except urllib.error.HTTPError as failure:
        detail = failure.read().decode()
        record["attempts"].append({"status": failure.code, "body": detail})
        if error is None and assistant is None:
            error = f"API Error: {failure.code} {detail}"
        continue
    except Exception as failure:
        record["attempts"].append({"status": None, "error": type(failure).__name__})
        if error is None and assistant is None:
            error = "API Error: Connection error."
        continue
    record["attempts"].append({"status": response.status})
    current = None
    try:
        for raw in response:
            line = raw.decode().rstrip("\r\n")
            if not line.startswith("data:"):
                continue
            event = json.loads(line[5:].strip())
            if attempt == 0:
                emit({"type": "stream_event", "event": event})
            kind = event["type"]
            if kind == "message_start":
                current = event["message"]
                current.setdefault("content", [])
            elif kind == "content_block_start":
                current["content"].append(event["content_block"])
            elif kind == "content_block_delta":
                block, delta = current["content"][event["index"]], event["delta"]
                for name, field in (("text_delta", "text"), ("thinking_delta", "thinking"), ("signature_delta", "signature")):
                    if delta["type"] == name:
                        block[field] = block.get(field, "") + delta[field]
                if delta["type"] == "input_json_delta":
                    block["_json"] = block.get("_json", "") + delta["partial_json"]
            elif kind == "content_block_stop":
                block = current["content"][event["index"]]
                if "_json" in block:
                    raw_json = block.pop("_json")
                    block["input"] = json.loads(raw_json) if raw_json.strip() else {}
            elif kind == "message_delta":
                current.update(event.get("delta", {}))
                current.setdefault("usage", {}).update(event.get("usage", {}))
    except Exception as failure:
        record["attempts"][-1]["error"] = type(failure).__name__
        if error is None and assistant is None:
            error = "API Error: Connection error."
        continue
    if attempt == 0 and current is not None and current.get("stop_reason"):
        assistant = current
    elif attempt == 0 and error is None:
        error = "API Error: incomplete stream"

if assistant is None:
    emit({"type": "assistant", "error": "unknown", "is_api_error_message": True, "message": {
        "role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": error or "API Error"}],
        "stop_reason": "stop_sequence"}})
    emit({"type": "result", "subtype": "success", "is_error": True, "num_turns": 1, "result": error})
    log()
    sys.exit(1)
finish(assistant["content"], assistant.get("usage", {"input_tokens": 0, "output_tokens": 0}), assistant["stop_reason"])
