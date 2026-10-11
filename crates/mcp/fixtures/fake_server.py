# Scripted fake MCP stdio server for crates/mcp fixtures. Logs every line it reads and writes to $MCP_FAKE_LOG
# as JSON lines {"dir": "in"|"out", "line": <raw text>} so the TypeScript and Rust clients can be compared frame by frame.
import json
import os
import sys
import time

log = open(os.environ["MCP_FAKE_LOG"], "a", encoding="utf-8")
sys.stderr.write("fake server ready\n")
sys.stderr.flush()


def record(direction, line):
    log.write(json.dumps({"dir": direction, "line": line}) + "\n")
    log.flush()


def send_raw(line):
    record("out", line)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def send(message):
    send_raw(json.dumps(message, separators=(",", ":")))


def read():
    line = sys.stdin.readline()
    if not line:
        record("eof", "")
        sys.exit(0)
    line = line.rstrip("\n")
    record("in", line)
    return json.loads(line)


TOOLS = [
    [{"name": "echo", "description": "Echo text", "inputSchema": {"type": "object"}}],
    [{"name": "slow", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}}],
]

while True:
    message = read()
    if "id" not in message:
        continue
    rid, method, params = message["id"], message.get("method"), message.get("params") or {}
    if method == "initialize":
        version = "2024-11-05" if params.get("clientInfo", {}).get("name") == "old" else "2025-06-18"
        if params.get("clientInfo", {}).get("name") == "ancient":
            version = "1999-01-01"
        result = {"protocolVersion": version, "capabilities": {"tools": {"listChanged": True}, "resources": {}},
                  "serverInfo": {"name": "fake", "version": "1.0.0"}, "instructions": "Use fake tools."}
    elif method == "tools/list":
        page = 0 if params.get("cursor") is None else 1
        result = {"tools": TOOLS[page], "nextCursor": "p2" if page == 0 else ""}
    elif method == "resources/list":
        if params.get("cursor") is None:
            result = {"resources": [{"uri": "file:///a", "name": "a", "mimeType": "text/plain"}], "nextCursor": "2"}
        else:
            result = {"resources": [{"uri": "file:///b"}], "nextCursor": None}
    elif method == "resources/templates/list":
        result = {"resourceTemplates": [{"uriTemplate": "repo://{owner}/{repo}"}]}
    elif method == "resources/read":
        if params["uri"] == "file:///bad":
            result = {"contents": [{"uri": "file:///bad"}]}
        else:
            result = {"contents": [{"uri": params["uri"], "text": "hello"}]}
    elif method == "ping":
        result = {}
    elif method == "tools/call":
        name, args = params["name"], params.get("arguments")
        if name == "echo":
            result = {"content": [{"type": "text", "text": str(args["text"])}]}
        elif name == "structured":
            result = {"structuredContent": {"ok": True}}
        elif name == "broken":
            result = {"content": "not a list"}
        elif name == "fail":
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": 1234, "message": "tool failed", "data": {"retryable": False}}})
            continue
        elif name == "hang":
            continue
        elif name == "garbage":
            send_raw('{"jsonrpc":"2.0"}')
            send_raw('{"jsonrpc":"2.0","id":999,"result":{}}')
            send_raw("")
            result = {"content": []}
        elif name == "roots":
            send({"jsonrpc": "2.0", "id": "r1", "method": "roots/list"})
            reply = read()
            while "id" not in reply or reply["id"] != "r1":
                reply = read()
            send({"jsonrpc": "2.0", "id": "r2", "method": "sampling/createMessage", "params": {}})
            reply = read()
            while "id" not in reply or reply["id"] != "r2":
                reply = read()
            send({"jsonrpc": "2.0", "id": 7, "method": "ping"})
            reply = read()
            while "id" not in reply or reply["id"] != 7:
                reply = read()
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            result = {"content": [{"type": "text", "text": "roots done"}]}
        elif name == "progress":
            token = params["_meta"]["progressToken"]
            time.sleep(0.3)
            send({"jsonrpc": "2.0", "method": "notifications/progress",
                  "params": {"progressToken": token, "progress": 1, "total": 2, "message": "half"}})
            time.sleep(0.3)
            result = {"content": [{"type": "text", "text": "done"}]}
        else:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32602, "message": "Unknown tool: " + name}})
            continue
    else:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "Method not found: " + str(method)}})
        continue
    send({"jsonrpc": "2.0", "id": rid, "result": result})
