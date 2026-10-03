# MCP servers

Ultron connects to [Model Context Protocol](https://modelcontextprotocol.io) servers itself, over stdio or
streamable HTTP. No extension is needed. The client, transports and OAuth sign-in are ported from Pi 1.0.0.

MCP tools are not model tools. The model keeps its one tool, the Python REPL, and reaches the servers from there:

```python
await mcp.servers()                                   # [{name, status, toolCount, transport, ...}]
await mcp.tools("exa-agent")                          # tool names of one server (all servers without one)
await mcp.describe("exa-agent_exa_agent_create_run")  # {name, server, description, parameters}
await mcp.search("create run")                        # [{server, tool, score, description}]
r = await mcp.call("exa-agent_exa_agent_create_run", query="...", effort="high")
r = await mcp.exa_agent.exa_agent_create_run(query="...")   # server.tool attribute access
r.json()                                              # the reply parsed as JSON; str(r) is its text
await mcp.resources("docs")                           # {resources, resourceTemplates}
await mcp.read("docs", "file:///notes.txt")           # a resource's text
```

A tool is named `<server>_<tool>`. The server's own name for it works with `server=`, and so does Pi's
`mcp__<server>__<tool>`. Gateway errors (unknown tool, a server that cannot be connected, sign-in required, a tool
that failed) raise `McpError` with the reason in `e.status`.

## Quick setup

```bash
ultron mcp add filesystem -- npx -y @modelcontextprotocol/server-filesystem .
ultron mcp add sentry --url https://mcp.sentry.dev/mcp
ultron mcp list          # connects each server and prints its state and tools
ultron mcp login sentry  # OAuth sign-in in the browser
```

`ultron mcp add|remove|list|login|logout` manage the servers Ultron connects to. `ultron mcp` without one of
these commands is a different program: Ultron's own stdio MCP server, which `ultron claude` hands to Claude Code.

## Configuration

Servers are read from `~/.ultron/agent/mcp.json` and, in a trusted project, from `.pi/mcp.json` (the project
config directory Ultron shares with Pi; a project that has one asks for trust like one with project extensions).
`ultron mcp add -l` writes the project file. Both use the `mcpServers` shape of other MCP clients:

```json
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
    "exa-agent": { "command": "exa-mcp", "env": { "EXA_API_KEY": "${EXA_API_KEY}" }, "inheritEnv": false },
    "sentry": { "url": "https://mcp.sentry.dev/mcp" },
    "internal": { "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer ${INTERNAL_TOKEN}" } }
  }
}
```

| Field | Applies to | Meaning |
|---|---|---|
| `command`, `args`, `cwd` | stdio | The server process. `~` is expanded; a relative `cwd` is resolved against the session's directory. |
| `env` | stdio | Extra environment. Values may be `${NAME}` (an environment variable) or `!command` (its output). |
| `inheritEnv` | stdio | `false` starts the server with only `env`, not Ultron's environment. Default `true`. |
| `url` | HTTP | Streamable HTTP endpoint. The legacy SSE transport is not supported. |
| `headers` | HTTP | Request headers; values as for `env`. An `Authorization` header turns OAuth off. |
| `oauth` | HTTP | `clientId`, `clientSecret`, `callbackPort`, `callbackUrl`, `scope`, `clientName`, `authServerMetadataUrl`, for servers without dynamic client registration. |
| `auth` | HTTP | `{ "provider": "<name>" }` sends the token of a `/login` provider. Global file only. |
| `enabled` | both | `false` keeps the entry without connecting. |
| `timeout` | both | Per-request timeout in seconds (default 60). |
| `lifecycle` | both | `lazy` (default) connects on first use; `eager` and `keep-alive` connect in the background when the session starts. |
| `description` | both | Shown by `mcp.servers()` and used by `mcp.search`. |
| `exposure`, `toolExposure` | both | `hidden` makes a server's tools, or single tools (by name or `*` pattern), unreachable. Pi's other values (`codemode`, `deferred`, `direct`) are accepted and mean "reachable from the REPL". |

### Files written for pi-mcp-adapter

An `mcp.json` written for pi-mcp-adapter keeps working unchanged, and Ultron does not rewrite it. `disabled`,
`requestTimeoutMs` (also under `settings`), `lifecycle`, `inheritEnv`, `bearerToken` and `bearerTokenEnv` are
translated; `auth: "bearer" | "oauth"` is implied by the rest of the entry. The adapter's `imports`, `idleTimeout`,
`directTools`, `toolPrefix`, `exposeResources`, `bearerTokenStore` and its `$env:NAME` / `{env:NAME}` value forms
are ignored.

If the adapter is still installed, Ultron leaves it out, uses the built-in support and says so once per session:
remove it with `ultron remove npm:pi-mcp-adapter`. Any other extension that registers an `mcp` tool replaces the
built-in support instead.

## In a session

Starting a session waits for no server. A `lazy` server is started by the first call that needs it, and a dropped
connection is reopened by the next call.

`/mcp` shows the servers and their state. In the TUI it opens a menu (pick a server, then connect, disconnect,
show tools, sign in or out, enable or disable); it also takes arguments:

```
/mcp status | reload
/mcp connect|disconnect|tools|login|logout|enable|disable <server>
```

`enable` and `disable` are saved to the `mcp.json` that defines the server. `reload` reads the files again.

## Sign-in

`/mcp login <server>` (or `ultron mcp login <server>`) opens the browser. The login finishes when the browser
reaches Ultron's callback server on `127.0.0.1`. If the browser runs on another machine (SSH, a container) and ends
on a page that cannot be reached, copy that page's address from the address bar and paste it at the prompt, as with
`/login`. `ULTRON_OAUTH_CALLBACK_HOST=0.0.0.0` makes the callback server listen on another interface for a container
that publishes the port. Tokens are stored in `~/.ultron/agent/mcp-auth.json` (mode 600) and refreshed as needed.

## Secrets

The values of a server's `env` and `headers`, OAuth tokens and client secrets are masked (`[REDACTED:known_secret]`)
wherever cell output could show them, on top of the pattern-based masking of all cell output. A server's
`notifications/message` log goes to `~/.ultron/agent/mcp.log`.

## Model tools

`ULTRON_EXTENSION_TOOLS=native` (or `extensionTools.mode: "native"`) makes extension tools model tools again; the
`mcp` gateway then becomes one model tool, not one per MCP tool. Pi's codemode and tool-search tools are not part of
Ultron.
