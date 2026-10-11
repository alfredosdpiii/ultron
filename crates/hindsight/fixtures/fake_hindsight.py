"""A fake Hindsight 0.9.2 server for differential tests of Ultron's Hindsight memory client.

Both the TypeScript generator (generate.mjs) and the Rust tests start this same server, drive their client against
it, and compare what it received. It listens on 127.0.0.1 on a free port, prints the port on the first line of
stdout, and exits when its stdin closes. It never forwards anything anywhere.

Endpoints (any path prefix before /v1/default/banks/ is accepted, so a base URL with a path works):
  PUT    .../banks/<bank>                   create the bank (idempotent)
  POST   .../banks/<bank>/memories          retain items (update_mode replace, by document_id)
  POST   .../banks/<bank>/memories/recall   recall items whose tags equal the request's (tags_match exact) and that
                                            contain a word (3+ letters) of the query
  GET    .../banks/<bank>/documents/<id>    a retained document, else 404
  DELETE .../banks/<bank>/documents/<id>    delete it, else 404
  GET    .../banks/<bank>/operations/<id>   an operation's status
Control (not logged): GET /__log (every request received, raw), POST /__reset (clear log and banks).

Some bank names misbehave on purpose: fail-500, redirect, slow, slow-body, big, big-chunked, bad-json, empty,
no-content, big-slow, flaky-bank (its first PUT answers 503).
"""

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote

lock = threading.Lock()
log = []
banks = {}
puts = {}
MARK = "/v1/default/banks/"


def bank_state(name):
    return banks.setdefault(name, {"docs": {}, "ops": {}})


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.serve()

    def do_POST(self):
        self.serve()

    def do_PUT(self):
        self.serve()

    def do_DELETE(self):
        self.serve()

    def reply(self, status, payload=None, raw=None, headers=()):
        body = raw if raw is not None else (b"" if payload is None else json.dumps(payload).encode())
        self.send_response(status)
        self.send_header("content-type", "application/json")
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def serve(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else b""
        if self.path == "/__log":
            with lock:
                return self.reply(200, list(log))
        if self.path == "/__reset":
            with lock:
                log.clear()
                banks.clear()
                puts.clear()
            return self.reply(200, {"ok": True})
        with lock:
            log.append(
                {
                    "line": self.requestline,
                    "headers": [[name, value] for name, value in self.headers.items()],
                    "body": body.decode("utf-8", "replace"),
                }
            )
        at = self.path.find(MARK)
        if at < 0:
            return self.reply(404, {"detail": "Not Found"})
        rest = self.path[at + len(MARK):]
        bank_raw, _, sub = rest.partition("/")
        bank = unquote(bank_raw)
        sub = "/" + sub if sub else ""
        method = self.command
        if bank == "fail-500":
            return self.reply(500, {"detail": "boom"})
        if bank == "redirect":
            return self.reply(302, {}, headers=[("location", "/v1/default/banks/ultron" + sub)])
        if bank == "slow":
            time.sleep(1.5)
            return self.reply(200, {"results": []})
        if bank == "slow-body":
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", "40")
            self.end_headers()
            self.wfile.write(b'{"results": [')
            self.wfile.flush()
            time.sleep(1.5)
            self.wfile.write(b"]}" + b" " * 25)
            return None
        if bank == "big-slow":
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", "5000")
            self.end_headers()
            self.wfile.write(b'{"results": [')
            self.wfile.flush()
            time.sleep(1.5)
            self.wfile.write(b" " * 4987)
            return None
        if bank == "big":
            return self.reply(200, raw=json.dumps({"results": [], "pad": "x" * 2000}).encode())
        if bank == "big-chunked":
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("transfer-encoding", "chunked")
            self.end_headers()
            for _ in range(20):
                chunk = b"x" * 100
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.write(b"0\r\n\r\n")
            return None
        if bank == "bad-json":
            return self.reply(200, raw=b"not json")
        if bank == "empty":
            return self.reply(200, raw=b"")
        if bank == "no-content":
            self.send_response(204)
            self.end_headers()
            return None
        payload = json.loads(body) if body else None
        with lock:
            state = bank_state(bank)
            if sub == "" and method == "PUT":
                count = puts[bank] = puts.get(bank, 0) + 1
                if bank == "flaky-bank" and count == 1:
                    return self.reply(503, {"detail": "starting"})
                return self.reply(200, {"bank_id": bank, "name": bank})
            if sub == "/memories" and method == "POST":
                for item in payload["items"]:
                    state["docs"][item["document_id"]] = {
                        "content": item["content"],
                        "tags": item.get("tags", []),
                        "metadata": item.get("metadata", {}),
                    }
                result = {"success": True, "bank_id": bank, "items_count": len(payload["items"])}
                if payload.get("async"):
                    state["ops"][payload["operation_id"]] = "completed"
                    result.update({"async": True, "operation_id": payload["operation_id"]})
                else:
                    result["async"] = False
                return self.reply(200, result)
            if sub == "/memories/recall" and method == "POST":
                wanted = set(payload.get("tags", []))
                words = [w for w in "".join(c if c.isalnum() else " " for c in str(payload.get("query", "")).lower()).split() if len(w) >= 3]
                results = []
                for doc_id, doc in state["docs"].items():
                    text = doc["content"].lower()
                    if set(doc["tags"]) == wanted and any(w in text for w in words):
                        results.append(
                            {
                                "id": "mem-" + doc_id,
                                "text": doc["content"],
                                "type": "world",
                                "tags": doc["tags"],
                                "document_id": doc_id,
                                "metadata": doc["metadata"],
                            }
                        )
                return self.reply(200, {"results": results})
            if sub.startswith("/documents/"):
                doc_id = unquote(sub[len("/documents/"):])
                doc = state["docs"].get(doc_id)
                if doc is None:
                    return self.reply(404, {"detail": "Document not found"})
                if method == "GET":
                    return self.reply(
                        200,
                        {"id": doc_id, "bank_id": bank, "original_text": doc["content"], "tags": doc["tags"]},
                    )
                if method == "DELETE":
                    del state["docs"][doc_id]
                    return self.reply(200, {"success": True, "document_id": doc_id, "memory_units_deleted": 1})
            if sub.startswith("/operations/") and method == "GET":
                op_id = unquote(sub[len("/operations/"):])
                return self.reply(200, {"operation_id": op_id, "status": state["ops"].get(op_id, "not_found")})
        return self.reply(404, {"detail": "Not Found"})


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        # A client that gave up (timeout, abort, size limit) closes its end; nothing to report.
        if not isinstance(sys.exc_info()[1], (BrokenPipeError, ConnectionResetError)):
            super().handle_error(request, client_address)


def main():
    server = Server(("127.0.0.1", 0), Handler)
    sys.stdout.write(f"{server.server_address[1]}\n")
    sys.stdout.flush()

    def watch():
        sys.stdin.read()
        os._exit(0)

    threading.Thread(target=watch, daemon=True).start()
    server.serve_forever()


if __name__ == "__main__":
    main()
