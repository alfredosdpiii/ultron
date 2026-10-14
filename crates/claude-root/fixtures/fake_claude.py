# A fake `claude` for the differential tests (never the real CLI). The generator and the Rust tests install it as
# an executable named `claude` with a `#!<python> -I` line in front. Behaviour by FAKE_CLAUDE_HELP (full, basic,
# old) and FAKE_CLAUDE_LOGGED_IN (1 or 0); a launch writes what it was started with to FAKE_CLAUDE_OUT and exits 7.
import json
import os
import stat
import sys

args = sys.argv[1:]
FULL = ["mcp-config", "strict-mcp-config", "tools", "allowedTools", "settings", "setting-sources",
        "system-prompt-file", "system-prompt", "dangerously-skip-permissions", "model", "print"]
BASIC = [f for f in FULL if f not in ("system-prompt-file", "dangerously-skip-permissions")]
OLD = ["mcp-config", "tools", "model"]

if args == ["--help"]:
    flags = {"full": FULL, "basic": BASIC, "old": OLD}[os.environ.get("FAKE_CLAUDE_HELP", "full")]
    print("Usage: claude [options] [command] [prompt]\n\nOptions:")
    for flag in flags:
        print(f"  --{flag} <value>   something")
    sys.exit(0)
if args == ["--version"]:
    print("2.1.7 (Claude Code)")
    sys.exit(0)
if args == ["auth", "status", "--json"]:
    if os.environ.get("FAKE_CLAUDE_LOGGED_IN", "1") == "1":
        print(json.dumps({"loggedIn": True, "authMethod": "claude.ai", "apiProvider": "firstParty",
                          "subscriptionType": "max", "email": "someone@example.com"}))
    else:
        print(json.dumps({"loggedIn": False}))
    sys.exit(0)

if "--output-format" in args and args[args.index("--output-format") + 1] == "stream-json":
    # A Claude Code subagent (`claude -p --output-format stream-json`): the brief on stdin. It relays its verdict
    # to the parent server like `rlm.finish` in its own `ultron mcp --child` would, then streams a reply.
    import socket
    brief = sys.stdin.read()
    config = json.loads(args[args.index("--mcp-config") + 1])
    server = config["mcpServers"]["ultron"]
    server_args = server["args"]
    parent = server_args[server_args.index("--parent-socket") + 1]
    reply = None
    if "verdict" in brief:
        conn = socket.socket(socket.AF_UNIX)
        conn.connect(parent)
        payload = {"status": "passed", "summary": "relayed verdict", "evidence": ["python3 -c pass: exit 0"], "changed_files": []}
        conn.sendall((json.dumps({"id": 1, "op": "child.finish", "token": server["env"]["ULTRON_PARENT_TOKEN"],
                                  "payload": payload}) + "\n").encode())
        reply = json.loads(conn.makefile().readline())
        conn.close()
    with open(os.environ["FAKE_CHILD_LOG"], "a", encoding="utf-8") as log:
        log.write(json.dumps({"args": args, "brief": brief, "env_has_session": "CLAUDE_CODE_SESSION_ID" in os.environ,
                              "finish": reply}) + "\n")
    for line in [
        {"type": "system", "subtype": "init", "model": "claude-opus-5-5"},
        {"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "child did: " + brief.strip()}]}},
        {"type": "result", "subtype": "success", "is_error": False, "result": "child did: " + brief.strip(),
         "num_turns": 1, "usage": {"input_tokens": 3, "output_tokens": 2}, "total_cost_usd": 0},
    ]:
        print(json.dumps(line), flush=True)
    sys.exit(0)

files = {}
for flag in ("--mcp-config", "--settings", "--system-prompt-file"):
    if flag in args:
        value = args[args.index(flag) + 1]
        if os.path.isfile(value):
            with open(value, encoding="utf-8") as handle:
                files[flag] = {"mode": oct(stat.S_IMODE(os.stat(value).st_mode)), "content": handle.read()}
record = {
    "args": args,
    "files": files,
    "MCP_TOOL_TIMEOUT": os.environ.get("MCP_TOOL_TIMEOUT"),
    "cwd": os.getcwd(),
}
with open(os.environ["FAKE_CLAUDE_OUT"], "w", encoding="utf-8") as out:
    json.dump(record, out)
sys.exit(7)
