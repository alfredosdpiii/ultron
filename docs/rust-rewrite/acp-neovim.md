# Ultron in Neovim over ACP

`ultron acp` runs Ultron as an [Agent Client Protocol](https://agentclientprotocol.com) agent (protocol v1) on
stdin/stdout. Any ACP client can start it; this page covers the two Neovim plugins that speak ACP,
[CodeCompanion.nvim](https://github.com/olimorris/codecompanion.nvim) and [avante.nvim](https://github.com/yetone/avante.nvim).
Zed works the same way (`agent_servers` entry with `command: "ultron", args: ["acp"]`).

Before you start: `ultron` must be on `PATH` and work in a terminal first (`ultron setup` once, so a model and its
credentials are configured). `ultron acp` takes the usual session options after `acp`, for example
`ultron acp --model anthropic/claude-sonnet-4-5` or `ultron acp --no-session` (sessions are not kept and cannot be
loaded).

## CodeCompanion.nvim

CodeCompanion starts the agent with Neovim's environment (`vim.system` inherits it), so `HOME` and any provider keys
in your shell reach Ultron as they are.

```lua
require("codecompanion").setup({
  adapters = {
    acp = {
      ultron = function()
        return require("codecompanion.adapters").extend("opencode", {
          name = "ultron",
          formatted_name = "Ultron",
          commands = {
            default = { "ultron", "acp" },
            -- a fixed model:  sonnet = { "ultron", "acp", "--model", "anthropic/claude-sonnet-4-5" },
          },
          defaults = {
            mcpServers = {},   -- Ultron reads its own mcp.json; servers listed here are not used
            timeout = 60000,   -- the first session/new starts Ultron's server and session worker
          },
        })
      end,
    },
  },
  interactions = {
    chat = { adapter = "ultron" },
  },
})
```

What you get:

- Each `rlm` cell shows as one tool line, `Execute: <first line of the cell>`, updated in place when it ends. A cell
  that changed files reports them (`kind: "edit"` with their locations), so CodeCompanion fires `FileEdited` and the
  open buffers reload.
- The model picker and the reasoning level come from the session's config options (`model`, `thought_level`), so
  `defaults.session_config_options = { model = "anthropic/claude-opus-4-1" }` works too. Model values are
  `provider/id`, as `ultron --list-models` shows them.
- Stop (`q`/`<C-c>` in the chat) sends `session/cancel`; the running cell is interrupted and the turn ends as
  `cancelled`.
- Saved chats reopen their Ultron session: Ultron advertises `loadSession` and `session/list`, and replays the
  conversation on load.
- When an Ultron extension asks a yes/no or choice question during a turn, CodeCompanion shows it as a permission
  prompt. Ultron never asks per tool call.

## avante.nvim

avante starts the agent with **only** `PATH` and the `env` table of the provider, so `HOME` must be passed (Ultron
finds its settings, sessions and stored credentials under it), and so must any provider key Ultron reads from the
environment.

```lua
require("avante").setup({
  provider = "ultron",
  acp_providers = {
    ultron = {
      command = "ultron",
      args = { "acp" },
      env = {
        HOME = os.getenv("HOME"),
        -- only the keys your configured provider reads from the environment, e.g.:
        ANTHROPIC_API_KEY = os.getenv("ANTHROPIC_API_KEY"),
        -- optional: a non-default agent directory
        -- ULTRON_CODING_AGENT_DIR = os.getenv("ULTRON_CODING_AGENT_DIR"),
      },
      -- mcp_servers: leave unset; Ultron's MCP gateway reads its own mcp.json
    },
  },
})
```

Notes:

- avante sends its own system prompt as the first text block of each prompt; Ultron passes it to the model as part of
  the user message (Ultron's runtime guide still applies). Set `behaviour.acp_follow_agent_locations = true` to have
  avante open the files an `rlm` cell edited.
- Session recovery works: a prompt for a session Ultron no longer has fails with code `-32603` and
  `data.details = "Session not found: <id>"`, which avante answers by opening a new session.
- avante's permission dialog answers Ultron's extension questions (`confirm`/`select`); there is no per-tool approval.
- avante stops the agent with SIGTERM (then SIGKILL). Ultron releases the session; a turn still running finishes in its
  session worker and is there when the session is loaded again.

## Troubleshooting

- stdout carries only ACP JSON-RPC lines; Ultron's own messages (for example a note that client MCP servers are not
  used) go to stderr. CodeCompanion's log (`:CodeCompanion log` / `codecompanion.log`) shows both directions.
- `Session not found` right after starting: the session was created in another working directory; Ultron keeps
  sessions per project directory.
- A first `session/new` that times out: raise the client timeout (CodeCompanion `defaults.timeout`); starting the
  server and the Python kernel the first time can take several seconds.
