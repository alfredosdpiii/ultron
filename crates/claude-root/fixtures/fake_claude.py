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
