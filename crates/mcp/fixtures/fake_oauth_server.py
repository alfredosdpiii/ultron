# Fake MCP server protected by OAuth, with its own authorization server (discovery, dynamic client registration,
# PKCE, refresh with rotating refresh tokens): a port of packages/coding-agent/test/suite/mcp-oauth-server.ts for
# generate-oauth.mjs and tests/oauth_fixture.rs. Prints its port, then serves:
#   /mcp                MCP over HTTP; 401 with a resource_metadata challenge without a valid access token
#   /prov-mcp           MCP over HTTP accepting any "prov-*" bearer token (servers with auth.provider)
#   /.well-known/...    protected resource and authorization server metadata
#   /register /authorize /token
#   GET /__log          every event so far (JSON list), for comparing clients
#   POST /__expire      access tokens stop being valid
# Usage: fake_oauth_server.py [--iss <issuer to send>]
import base64
import hashlib
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse, urlunparse

ISS = sys.argv[sys.argv.index("--iss") + 1] if "--iss" in sys.argv else None
LOCK = threading.Lock()
LOG = []
VALID = set()
REFRESH = set()
CHALLENGES = {}
STATE = {"issued": 0, "codes": 0, "origin": ""}


def log(entry):
    with LOCK:
        LOG.append(entry)


def issue_tokens():
    with LOCK:
        STATE["issued"] += 1
        n = STATE["issued"]
        VALID.add(f"access-{n}")
        REFRESH.add(f"refresh-{n}")
    return {"access_token": f"access-{n}", "refresh_token": f"refresh-{n}", "token_type": "Bearer", "expires_in": 3600}


def redirect_shape(uri):
    url = urlparse(uri)
    return f"{url.scheme}://{url.hostname}:<PORT>{url.path}" if url.port else uri


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", headers=()):
        self.send_response(status)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def json(self, status, value, headers=()):
        self.reply(status, json.dumps(value).encode(), [("content-type", "application/json"), *headers])

    def body(self):
        return self.rfile.read(int(self.headers.get("content-length", "0"))).decode()

    def mcp(self, accept):
        if self.command != "POST":
            return self.reply(405 if self.command == "GET" else 200)
        auth = self.headers.get("authorization") or ""
        token = auth[len("Bearer "):] if auth.startswith("Bearer ") else None
        raw = self.body()
        if not token or not accept(token):
            log(f"401 {token or 'none'}")
            origin = STATE["origin"]
            return self.reply(401, b"", [("www-authenticate", f'Bearer resource_metadata="{origin}/.well-known/oauth-protected-resource/mcp"')])
        message = json.loads(raw)
        if "id" not in message:
            return self.reply(202)
        method = message.get("method")
        if method == "initialize":
            result = {"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}},
                      "serverInfo": {"name": "issues", "version": "1.0.0"}}
        elif method == "tools/list":
            result = {"tools": [{"name": "whoami", "inputSchema": {"type": "object", "properties": {}}}]}
        elif method == "tools/call":
            log(f"call {token}")
            result = {"content": [{"type": "text", "text": f"token {token}"}]}
        else:
            result = {}
        self.json(200, {"jsonrpc": "2.0", "id": message["id"], "result": result})

    def do_GET(self):
        self.route()

    def do_POST(self):
        self.route()

    def do_DELETE(self):
        self.route()

    def route(self):
        url = urlparse(self.path)
        origin = STATE["origin"]
        if url.path == "/mcp":
            return self.mcp(lambda token: token in VALID)
        if url.path == "/prov-mcp":
            return self.mcp(lambda token: token.startswith("prov-"))
        if url.path == "/__log":
            with LOCK:
                return self.json(200, list(LOG))
        if url.path == "/__expire":
            self.body()
            with LOCK:
                VALID.clear()
            return self.json(200, {})
        if url.path == "/.well-known/oauth-protected-resource/mcp":
            log("metadata resource")
            return self.json(200, {"resource": f"{origin}/mcp", "authorization_servers": [origin]})
        if url.path == "/.well-known/oauth-authorization-server":
            log("metadata authorization server")
            return self.json(200, {
                "issuer": origin, "authorization_endpoint": f"{origin}/authorize", "token_endpoint": f"{origin}/token",
                "registration_endpoint": f"{origin}/register", "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"], "token_endpoint_auth_methods_supported": ["none"],
            })
        if url.path == "/register":
            metadata = json.loads(self.body())
            shown = dict(metadata)
            shown["redirect_uris"] = [redirect_shape(u) for u in metadata.get("redirect_uris", [])]
            log("register " + json.dumps(shown, sort_keys=True))
            return self.json(201, {**metadata, "client_id": "client-1"})
        if url.path == "/authorize":
            query = {k: v[0] for k, v in parse_qs(url.query).items()}
            with LOCK:
                STATE["codes"] += 1
                code = f"code-{STATE['codes']}"
                CHALLENGES[code] = query.get("code_challenge", "")
            shown = {k: v for k, v in query.items() if k not in ("state", "code_challenge")}
            shown["redirect_uri"] = redirect_shape(query.get("redirect_uri", ""))
            shown["state"] = "<set>" if query.get("state") else "<missing>"
            log("authorize " + json.dumps(shown, sort_keys=True))
            redirect = urlparse(query.get("redirect_uri", ""))
            params = {"code": code, "state": query.get("state", "")}
            if ISS:
                params["iss"] = ISS
            location = urlunparse(redirect._replace(query=urlencode(params)))
            return self.reply(302, b"", [("location", location)])
        if url.path == "/token":
            params = {k: v[0] for k, v in parse_qs(self.body()).items()}
            shown = {k: v for k, v in params.items() if k not in ("code_verifier", "redirect_uri")}
            if "redirect_uri" in params:
                shown["redirect_uri"] = redirect_shape(params["redirect_uri"])
            if params.get("grant_type") == "authorization_code":
                challenge = CHALLENGES.pop(params.get("code", ""), None)
                digest = hashlib.sha256(params.get("code_verifier", "").encode()).digest()
                verifier = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
                if not challenge or challenge != verifier:
                    log("token code invalid " + json.dumps(shown, sort_keys=True))
                    return self.json(400, {"error": "invalid_grant"})
                log("token code " + json.dumps(shown, sort_keys=True))
                return self.json(200, issue_tokens())
            with LOCK:
                known = params.get("refresh_token", "") in REFRESH
                REFRESH.discard(params.get("refresh_token", ""))
            if not known:
                log("token refresh invalid " + json.dumps(shown, sort_keys=True))
                return self.json(400, {"error": "invalid_grant"})
            log("token refresh " + json.dumps(shown, sort_keys=True))
            return self.json(200, issue_tokens())
        self.reply(404)


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
STATE["origin"] = f"http://127.0.0.1:{server.server_address[1]}"
print(server.server_address[1], flush=True)
server.serve_forever()
