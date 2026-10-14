# Fake MCP stdio server for the gateway fixtures (generate-gateway.mjs, tests/gateway_fixture.rs).
# Usage: fake_gateway_server.py <variant> <log path or ->
#   alpha: tools (list_changed), resources, logging, instructions; tools that echo, fail, crash, hang, log, ...
#   beta:  tools only, no instructions
#   plain: resources only (no tools capability; resources/templates/list is not implemented)
#   dies:  writes to stderr and exits before answering initialize
# With a log path, every request method read is appended as one line, and "eof" when stdin closes.
import json
import os
import sys

variant = sys.argv[1]
log_path = sys.argv[2] if len(sys.argv) > 2 else "-"


def log(line):
    if log_path != "-":
        with open(log_path, "a", encoding="utf-8") as f:
            f.write(line + "\n")


if variant == "dies":
    sys.stderr.write("fatal: cannot start\n")
    sys.stderr.flush()
    sys.exit(1)


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def text(value):
    return {"content": [{"type": "text", "text": value}]}


def obj(properties=None, required=None):
    schema = {"type": "object", "properties": properties or {}}
    if required:
        schema["required"] = required
    return schema


ALPHA_TOOLS = [
    {"name": "echo", "description": "Echo the text back\nSecond line of the description",
     "inputSchema": obj({"text": {"type": "string"}}, ["text"])},
    {"name": "add", "title": "Adder", "description": "Add two numbers",
     "inputSchema": obj({"a": {"type": "number"}, "b": {"type": "number"}}),
     "outputSchema": obj({"sum": {"type": "number"}})},
    {"name": "structured", "description": "Only structured content", "inputSchema": obj()},
    {"name": "fails", "description": "Reports an error without text", "inputSchema": obj()},
    {"name": "fails_text", "description": "Reports an error with text", "inputSchema": obj()},
    {"name": "image", "description": "Returns an image", "inputSchema": obj()},
    {"name": "link", "description": "Returns a resource link", "inputSchema": obj()},
    {"name": "blob", "description": "Returns a binary resource", "inputSchema": obj()},
    {"name": "jsonblob", "description": "Returns a JSON blob", "inputSchema": obj()},
    {"name": "env", "description": "Returns GW_TOKEN", "inputSchema": obj()},
    {"name": "cwd", "description": "Returns the working directory", "inputSchema": obj()},
    {"name": "hidden_tool", "description": "Hidden by toolExposure", "inputSchema": obj()},
    {"name": "log", "description": "Sends a log message", "inputSchema": obj()},
    {"name": "change", "description": "Changes the tool list", "inputSchema": obj()},
    {"name": "crash", "description": "Exits without answering", "inputSchema": obj()},
    {"name": "hang", "description": "Never answers", "inputSchema": obj()},
    {"name": "quit", "description": "Exits quietly without answering", "inputSchema": obj()},
]
BETA_TOOLS = [
    {"name": "echo", "description": "Beta echo", "inputSchema": obj({"text": {"type": "string"}})},
    {"name": "beta_only", "description": "Only on beta: search words", "inputSchema": obj()},
]
changed = False

while True:
    line = sys.stdin.readline()
    if not line:
        log("eof")
        sys.exit(0)
    if not line.strip():
        continue
    message = json.loads(line)
    method = message.get("method")
    params = message.get("params") or {}
    if "id" not in message:
        if method == "notifications/cancelled":
            log("cancelled " + str(params.get("requestId")))
        continue
    rid = message["id"]
    log(str(method) + (" " + params["name"] if method == "tools/call" else ""))

    def ok(result):
        send({"jsonrpc": "2.0", "id": rid, "result": result})

    def fail(code, msg):
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": msg}})

    if method == "initialize":
        capabilities = {}
        if variant == "alpha":
            capabilities = {"tools": {"listChanged": True}, "resources": {}, "logging": {}}
        elif variant == "beta":
            capabilities = {"tools": {}}
        elif variant == "plain":
            capabilities = {"resources": {}}
        result = {"protocolVersion": params.get("protocolVersion", "2025-06-18"), "capabilities": capabilities,
                  "serverInfo": {"name": "gw-" + variant, "version": "1.0.0"}}
        if variant == "alpha":
            result["instructions"] = "  Alpha: call echo first.\n"
        ok(result)
    elif method == "ping":
        ok({})
    elif method == "tools/list":
        if variant == "alpha":
            tools = ALPHA_TOOLS + ([{"name": "added", "description": "Added later", "inputSchema": obj()}] if changed else [])
        else:
            tools = BETA_TOOLS
        ok({"tools": tools})
    elif method == "resources/list":
        if variant == "plain":
            ok({"resources": [{"uri": "file:///plain.txt", "name": "plain"}]})
        elif params.get("cursor") is None:
            ok({"resources": [
                {"uri": "file:///notes.txt", "name": "notes", "mimeType": "text/plain", "_meta": {"x": 1},
                 "icons": [{"src": "https://example.com/i.png"}]},
                {"uri": "ui://widget", "name": "widget"},
            ], "nextCursor": "p2"})
        else:
            ok({"resources": [{"uri": "file:///data.bin", "name": "data", "mimeType": "application/octet-stream"}]})
    elif method == "resources/templates/list":
        if variant == "plain":
            fail(-32601, "Method not found")
        else:
            ok({"resourceTemplates": [{"uriTemplate": "repo://{name}", "name": "repo"},
                                      {"uriTemplate": "ui://app/{id}", "name": "app"}]})
    elif method == "resources/read":
        uri = params.get("uri")
        if uri == "file:///notes.txt":
            ok({"contents": [{"uri": uri, "mimeType": "text/plain", "text": "hello notes"}]})
        elif uri == "file:///data.bin":
            ok({"contents": [{"uri": uri, "mimeType": "application/octet-stream", "blob": "AAECAw=="}]})
        else:
            fail(-32002, "Resource not found: " + str(uri))
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if variant != "alpha":
            if name == "echo":
                ok(text("beta:" + str(args.get("text"))))
            elif name == "beta_only":
                ok(text("only beta"))
            else:
                fail(-32602, "Unknown tool: " + str(name))
        elif name == "echo":
            ok(text(str(args.get("text"))))
        elif name == "add":
            total = args.get("a", 0) + args.get("b", 0)
            ok({"content": [{"type": "text", "text": json.dumps({"sum": total})}], "structuredContent": {"sum": total}})
        elif name == "structured":
            ok({"content": [], "structuredContent": {"items": [1, 2], "ok": True}})
        elif name == "fails":
            ok({"content": [], "isError": True})
        elif name == "fails_text":
            ok({"content": [{"type": "text", "text": "bad input: x"}], "isError": True})
        elif name == "image":
            ok({"content": [{"type": "text", "text": "an image:"},
                            {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]})
        elif name == "link":
            ok({"content": [{"type": "resource_link", "uri": "file:///notes.txt", "name": "notes",
                             "mimeType": "text/plain", "size": 2048, "description": "Some notes"}]})
        elif name == "blob":
            ok({"content": [{"type": "resource", "resource": {"uri": "file:///out/report.pdf",
                                                               "mimeType": "application/pdf", "blob": "JVBERi0x"}}]})
        elif name == "jsonblob":
            ok({"content": [{"type": "resource", "resource": {"uri": "file:///d.json", "mimeType": "application/json",
                                                               "blob": "eyJrIjogMX0="}}]})
        elif name == "env":
            ok(text("token=" + os.environ.get("GW_TOKEN", "")))
        elif name == "cwd":
            ok(text(os.getcwd()))
        elif name == "log":
            send({"jsonrpc": "2.0", "method": "notifications/message",
                  "params": {"level": "warning", "logger": "gw", "data": "first line\nsecond line"}})
            send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": {"n": 1}}})
            ok(text("logged"))
        elif name == "change":
            changed = True
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            ok(text("changed"))
        elif name == "crash":
            sys.stderr.write("crashing now\n")
            sys.stderr.flush()
            sys.exit(3)
        elif name == "hang":
            pass
        elif name == "quit":
            sys.exit(0)
        else:
            fail(-32602, "Unknown tool: " + str(name))
    else:
        fail(-32601, "Method not found: " + str(method))
