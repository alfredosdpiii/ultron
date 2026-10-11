# Scripted fake MCP Streamable HTTP server for crates/mcp fixtures. Prints its port, then logs every request to
# $MCP_FAKE_LOG as JSON lines {method, headers (MCP-relevant, lowercased), body} so TypeScript and Rust transports
# can be compared request by request.
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = open(os.environ["MCP_FAKE_LOG"], "a", encoding="utf-8")
KEEP = ["accept", "content-type", "mcp-session-id", "mcp-protocol-version", "authorization", "x-custom", "last-event-id"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", ctype=None, headers=()):
        self.send_response(status)
        if ctype:
            self.send_header("content-type", ctype)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def record(self, body):
        headers = {k: self.headers[k] for k in KEEP if self.headers.get(k) is not None}
        LOG.write(json.dumps({"method": self.command, "headers": headers, "body": body}) + "\n")
        LOG.flush()

    def do_GET(self):
        self.record(None)
        self.reply(405)

    def do_DELETE(self):
        self.record(None)
        self.reply(200)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", "0")))
        msg = json.loads(raw)
        self.record(msg)
        if "id" not in msg:
            return self.reply(202)
        rid, method, params = msg["id"], msg.get("method"), msg.get("params") or {}

        def ok(result, headers=()):
            body = json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}).encode()
            self.reply(200, body, "application/json; charset=utf-8", headers)

        if method == "initialize":
            return ok({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                       "serverInfo": {"name": "fake-http", "version": "1"}}, [("mcp-session-id", "sess-1")])
        if method == "tools/list":
            event = json.dumps({"jsonrpc": "2.0", "id": rid, "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}})
            body = (": hello\n\nevent: message\nid: e1\ndata: " + event + "\n\n").encode()
            return self.reply(200, body, "text/event-stream")
        name = params.get("name")
        if name == "echo":
            return ok({"content": [{"type": "text", "text": params["arguments"]["text"]}]})
        if name == "overloaded":
            return self.reply(503, b"busy, try later", "text/plain")
        if name == "expire":
            return self.reply(404, b"no such session", "text/plain")
        if name == "accepted":
            return self.reply(202)
        if name == "weird":
            return self.reply(200, b"<html/>", "text/html")
        if name == "unanswered":
            return self.reply(200, b"data: \n\n", "text/event-stream")
        return self.reply(401, b"", None, [("www-authenticate", 'Bearer error="invalid_token"')])


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
