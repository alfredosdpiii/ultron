# Ultron extension protocol

Protocol version `2026-10-10`. Implemented by the host in `crates/extensions`; a working extension is
`examples/hello/hello.py` (standard library only).

Third-party extensions are separate processes. They never run code inside the host or the Python kernel; the
TypeScript loader (`jiti`, `ExtensionAPI` factories) is gone. Built-in extensions are Rust code in the product.

Ultron is RLM-first: the model has exactly one tool, the persistent Python REPL (`rlm`). Nothing an extension
registers ever enters the model's tool list. `registerTool` and every other way TypeScript had to add model tools map
to kernel-API functions: the kernel installs them under `ext.<extension>.<function>(**arguments)`, they appear in
`help(ext)` and in an "Extension functions" section of the runtime guide, and a cell calls them through the host
operation `ext.call` (`kernel_api.rs`). The test `extensions_functions_never_become_model_tools` loads an extension
with six functions and checks that the provider request still declares exactly `[rlm]`.

## 1. Transport and framing

- JSON-RPC 2.0 over the process's stdin (host to extension) and stdout (extension to host), MCP stdio framing: one
  JSON message per line, UTF-8, no embedded newlines, `\n` terminated (`\r\n` accepted). Lines over 16 MiB are
  dropped.
- stderr is free-form logging. The host keeps the last 64 KiB and shows it when the process fails to start.
- Both sides send requests, responses and notifications. Request ids are unique per sender; responses may arrive in
  any order. An extension must keep reading stdin while a handler runs (run handlers concurrently), because a handler
  may call the host and wait for the answer.
- A request either side does not implement is answered with error `-32601` (`Method not found: <method>`). A handler
  failure is answered with any JSON-RPC error; its `message` is what the user sees (TypeScript: the thrown error's
  message).
- Cancellation, as in MCP: `notifications/cancelled` `{ requestId, reason? }`. The host sends it when a request it
  made times out (`reason: "Request timed out"`) or its caller cancels; the extension should stop the work and may
  skip the response. The host honours the same notification for requests the extension made to it.

## 2. Configuration and process lifecycle

An extension is a directory with an `extension.json`:

```json
{
  "name": "hello",
  "command": "python3",
  "args": ["hello.py"],
  "env": { "HELLO_LEVEL": "2" },
  "eventTimeoutMs": 30000,
  "startupTimeoutMs": 10000,
  "maxRestarts": 3
}
```

| Field | Meaning |
| --- | --- |
| `name` | Identity in the session (`ext.<name>`, status keys, errors). Default: the directory name. |
| `command` | Executable. A relative path containing `/` is relative to the manifest; a bare name is looked up on `PATH`. |
| `args`, `env` | Arguments; variables added to the host environment. |
| `eventTimeoutMs` | How long one event may take before it counts as that extension's failure. Default 30000. |
| `startupTimeoutMs` | Spawn plus `initialize`. Default 10000. |
| `maxRestarts` | Restarts after the process dies before it is disabled for the session. Default 3. |

The process runs in the manifest's directory, in its own process group.

Locations, in the order the TypeScript package manager resolves extensions (`core/package-manager.ts`):

1. `extensions` entries in `<cwd>/.pi/settings.json`, relative to `<cwd>/.pi` (trusted projects only);
2. `extensions` entries in `<agentDir>/settings.json`, relative to the agent directory (`~` expands);
3. `<cwd>/.pi/extensions/*/extension.json` (trusted projects only);
4. `<agentDir>/extensions/*/extension.json`.

An entry is a manifest file, a directory with `extension.json`, or a directory whose subdirectories have one (one
level). The same manifest loads once, at its first position, and that order is the dispatch order below. Settings
entries may be patterns, applied as the package manager applies them to an extension's entry file (here its
`extension.json`, matched with minimatch against its path relative to the settings' directory, its file name or its
absolute path): plain globs and `!glob` filter the manifests that scope's entries list, `+path` / `-path` force one in
or out, and the scope's `!`/`+`/`-` entries also apply to its auto-discovered directory. A disabled manifest keeps its
place (a later listing does not enable it). Entries that are JavaScript/TypeScript extensions (`*.ts`, `*.js`,
`index.ts`, `package.json` with `pi.extensions`), missing paths and invalid manifests are reported as diagnostics and
skipped.

Lifecycle:

1. The host spawns every extension concurrently and sends `initialize` (section 3). An extension that fails to
   spawn, exits, answers with an error, an unknown event name or another protocol version is not loaded; the
   failure is a load diagnostic (TypeScript: `LoadExtensionsResult.errors`).
2. After a successful handshake the host sends `notifications/initialized`.
3. Crash isolation: when a process exits, only its own in-flight requests fail (`Extension <name> exited: ...`);
   every other extension keeps running and the current dispatch continues with the next extension. The next use of
   the extension (an event, function, command) restarts it and repeats `initialize` with `restart: n`. Nothing is
   replayed. After `maxRestarts` restarts it is disabled for the session: the use that finds it dead fails with
   `Extension <name> is disabled: it exited after <n> restart(s)`, after which it receives no events.
4. Shutdown: the host emits `session_shutdown`, sends `shutdown` (2 s), closes stdin, then SIGTERM and SIGKILL to
   the process group.

## 3. Handshake

Host to extension, request `initialize`:

```json
{
  "protocolVersion": "2026-10-10",
  "hostInfo": { "name": "ultron", "version": "0.1.0" },
  "session": { "cwd": "/work/project", "mode": "interactive", "hasUI": true },
  "flags": { "plan": true },
  "restart": 0,
  "capabilities": {
    "ui": { "status": true, "panels": true, "prompts": true },
    "events": ["project_trust", "resources_discover", "session_start", "..."]
  }
}
```

`mode` is `interactive`, `print`, `json` or `rpc`. `flags` holds the values given on the command line for flags
the extension registered. `capabilities.events` lists the events this host delivers (`DELIVERED_EVENTS`):
`project_trust`, `session_start`, `session_info_changed`, `session_before_fork`, `session_before_compact`,
`session_compact`, `session_compact_failed`, `session_shutdown`, `session_before_tree`, `session_tree`, `context`,
`context_with_system`, `before_agent_start`, `agent_end`, `ui_prompt_start`, `ui_prompt_end`, `user_bash`,
`before_file_write`, `after_cell_changes`. An extension may still subscribe to any other event type of section 5
(`pi.on` accepts every `ExtensionEvent` type): it loads, and the handler never runs, as an AgentSession-only
handler never runs in the TypeScript native worker. An unknown event name fails the load.

Result:

```json
{
  "protocolVersion": "2026-10-10",
  "extensionInfo": { "name": "hello", "version": "1.0.0" },
  "capabilities": {
    "events": ["session_before_switch", "tool_call", "input"],
    "functions": [{ "name": "greet", "label": "Greet", "description": "Greet someone", "parameters": { "type": "object" },
                    "executionMode": "sequential", "promptGuidelines": ["Greet before working."] }],
    "commands": [{ "name": "hello", "description": "Say hello", "completions": true }],
    "shortcuts": [{ "key": "ctrl+shift+h", "description": "Hello" }],
    "flags": [{ "name": "plan", "type": "boolean", "default": false, "description": "Start in plan mode" }],
    "bus": ["greetings"]
  }
}
```

`protocolVersion` must equal the host's. Function names must be Python identifiers. A function's optional `label`
(shown by `tools.list()`/`tools.describe()`), `executionMode` (`"sequential"`: its REPL calls run alone among the
lane's calls, first come first served; `"parallel"`; anything else fails the load) and `promptGuidelines`
(reported by `tools/list`) are `registerTool`'s. A flag's `type` is `boolean` or
`string`, and a `default` must have that type (`Invalid default for flag "<name>": expected <type>, got <typeof>`
fails the load, as `registerFlag` throws). The first registered default of a name is the session's value;
command-line values given in `initialize.session` flags override it (`applyExtensionFlags`: a boolean flag becomes
`true`, a string flag needs a value). `flags/get` answers only for a flag the asking extension registered. A restart's
`initialize.flags` holds the session's values of the flags the extension registered. `providers` declares
`registerProvider(name, config)` without code: `[{ name, baseUrl?, apiKey?, api?, headers?, authHeader?, models? }]`,
the shape of a models.json provider. The extension may later replace
its whole registration set with the notification `registrations/changed` (same shape as `capabilities`), which is
how `registerCommand`/`on` after load and `setActiveTools`-style changes are expressed.

## 4. Methods

Host to extension:

| Method | Kind | Params | Result |
| --- | --- | --- | --- |
| `initialize` | request | section 3 | section 3 |
| `notifications/initialized` | notification | `{}` | |
| `event` | request | the event object (`type` plus fields, section 5) | the handler result or `null` |
| `notifications/event` | notification | an event delivered without waiting (`ui_prompt_start`, `ui_prompt_end`) | |
| `functions/call` | request | `{ name, arguments, toolCallId? }` (`toolCallId` for REPL tool calls: `repl-call-<hex>`) | `{ content: (TextContent or ImageContent)[], details?, isError? }` |
| `commands/run` | request | `{ name, args }` (no timeout: commands may prompt) | `null` |
| `commands/complete` | request | `{ name, prefix }` | `AutocompleteItem[]` or `null` |
| `shortcuts/run` | request | `{ key }` | `null` |
| `bus/message` | notification | `{ channel, data }` | |
| `shutdown` | request | `{}` | `{}` |
| `notifications/cancelled` | notification | `{ requestId, reason? }` | |

Extension to host (requests unless marked; the host answers unknown methods with `-32601`):

| Method | Params | Result |
| --- | --- | --- |
| `ui/select` | `{ title, options: string[], timeoutMs? }` | the chosen option or `null` |
| `ui/confirm` | `{ title, message, timeoutMs? }` | boolean |
| `ui/input` | `{ title, placeholder?, timeoutMs? }` | string or `null` |
| `ui/editor` | `{ title, prefill? }` | string or `null` |
| `ui/getEditorText`, `ui/getToolsExpanded`, `ui/getAllThemes` | `{}` | string, boolean, `{ name, path }[]` |
| `ui/setTheme` | `{ name }` | `{ success, error? }` (also accepted as a notification, without an answer) |
| `ui/setStatus` (notification) | `{ key, text \| null }` | |
| `ui/setPanel` (notification) | `{ key, lines: string[] \| null, placement?: "aboveEditor" \| "belowEditor" }` | |
| `ui/notify` (notification) | `{ message, type?: "info" \| "warning" \| "error" }` | |
| `ui/setWorkingMessage`, `ui/setWorkingVisible`, `ui/setHiddenThinkingLabel`, `ui/setTitle`, `ui/setEditorText`, `ui/pasteToEditor`, `ui/setToolsExpanded`, `ui/setTheme` (notifications) | `{ message }`, `{ visible }`, `{ label }`, `{ title }`, `{ text }`, `{ text }`, `{ expanded }`, `{ name }` | |
| `session/sendMessage` | `{ message: { customType, content, display, details? }, triggerTurn?, deliverAs? }` | `null` |
| `session/sendUserMessage` | `{ content, deliverAs?, expandPromptTemplates? }` | `null` |
| `session/appendEntry` | `{ customType, data? }` | `null` |
| `session/setName`, `session/getName`, `session/setLabel` | `{ name }`, `{}`, `{ entryId, label \| null }` | `null`, string or `null`, `null` |
| `session/entries`, `session/branch`, `session/leaf`, `session/leafEntry`, `session/tree`, `session/contextEntries` | `{}` | `getEntries`, `getBranch`, `getLeafId`, `getLeafEntry`, `getTree`, `buildContextEntries` |
| `session/entry`, `session/label` | `{ id }` | `getEntry`, `getLabel` (`null` when absent) |
| `session/file`, `session/id`, `session/dir`, `session/cwd`, `session/header` | `{}` | `getSessionFile`, `getSessionId`, `getSessionDir`, `getCwd`, `getHeader` |
| `session/new`, `session/fork`, `session/navigateTree`, `session/switch`, `session/reload` | as `ExtensionCommandContext` | `{ cancelled }` (commands only) |
| `host/exec` | `{ command, args, cwd?, timeoutMs? }` | `{ stdout, stderr, code, killed }` |
| `host/context` | `{}` | `{ cwd, mode, hasUI, model, thinkingLevel, isIdle, projectTrusted, hasPendingMessages, contextUsage, systemPrompt, scopedModels, systemPromptOptions }` |
| `models/all`, `models/available` | `{}` | `Model[]` (`modelRegistry.getAll`, `getAvailable`) |
| `models/find` | `{ provider, id }` | `Model` or `null` |
| `models/getApiKeyAndHeaders` | `{ provider, id }` | `{ ok: true, apiKey?, headers?, baseUrl? }` or `{ ok: false, error }` |
| `models/getApiKeyForProvider`, `models/hasConfiguredAuth`, `models/isUsingOAuth` | `{ provider }` | string or `null`, boolean, boolean |
| `agent/abort`, `agent/compact`, `agent/waitForIdle`, `host/shutdown` | `{}`, `{ customInstructions? }`, `{}`, `{}` | `null` |
| `model/set`, `thinking/get`, `thinking/set` | `{ provider, id }`, `{}`, `{ level }` | boolean, level, `null` |
| `tools/list`, `commands/list` | `{}` | `ToolInfo[]` (with `promptGuidelines`, `sourceInfo`), `SlashCommandInfo[]` |
| `tools/active`, `tools/setActive` | `{}`, `{ names: string[] }` | `string[]`, `null` |
| `functions/update` (notification) | `{ toolCallId, content, details? }`: a partial result of the `functions/call` with that id | |
| `flags/get` | `{ name }` | the flag value or `null` |
| `bus/emit` (notification) | `{ channel, data }` | |
| `registrations/changed` (notification) | section 3 `capabilities` | |
| `log` (notification) | `{ level, message }` | |

Without a UI (print and JSON modes, `hasUI: false`) the prompts answer at once like TypeScript's `noOpUIContext`:
`select`, `input`, `editor` give `null`, `confirm` gives `false`; UI notifications are dropped. Every blocking prompt
is bracketed by `ui_prompt_start` / `ui_prompt_end` events to subscribers (outer prompt only when prompts nest).

Status lines and panels are keyed by (extension, key), so two extensions never overwrite each other.

## 5. Events and veto semantics

An extension lists the events it handles in `capabilities.events`; the host sends each subscribed event once with
the `event` request and waits for the result (or the event timeout). Extensions are visited in load order. A handler
error, timeout or dead process is reported as that extension's error (`ExtensionError { extensionPath, event,
error }`) and the dispatch moves on to the next extension, except where marked "propagates". Several handlers inside
one extension are the extension's business; the bundled example runs one.

Results are JSON objects or `null`. Where TypeScript tests `!== undefined`, a member that is present (even `null`)
counts and an omitted member does not. TypeScript handlers that mutate the event in place return the change
instead: `tool_call` `{ input }`, `before_provider_headers` `{ headers }`, `before_provider_request` `{ payload }`.
Over the wire there is no object identity, so "unchanged" means "deep-equal".

| Event | Result | Rule (`runner.ts`) |
| --- | --- | --- |
| `project_trust` `{ cwd }` | `{ trusted: "yes"\|"no"\|"undecided", remember? }` | First yes/no wins; undecided falls through; `null` is an error. Errors are returned to the trust prompt, not to the session listeners. |
| `resources_discover` `{ cwd, reason }` | `{ skillPaths?, promptPaths?, themePaths? }` | All collected, tagged with the extension path. |
| `session_start` `{ reason, previousSessionFile? }`, `session_info_changed`, `session_compact`, `session_compact_failed`, `session_shutdown`, `session_tree` | ignored | Every subscriber runs in order. |
| `session_before_switch` `{ reason, targetSessionFile? }`, `session_before_fork` `{ entryId, position }`, `session_before_compact` `{ preparation, branchEntries, customInstructions?, reason, willRetry }`, `session_before_tree` `{ preparation }` | `{ cancel?, ... }` (`compaction`, `skipConversationRestore`, `summary`, `customInstructions`, `replaceInstructions`, `label`) | The first truthy `cancel` stops the dispatch and is returned; otherwise the last non-null result. |
| `context` `{ messages }` (no system messages) | `{ messages? }` | Chained; a changed list gets the replayed system message (`getCurrentSystemMessage`) back in front. |
| `context_with_system` `{ messages }` (full) | `{ messages? }` | Chained, used as returned; dropping the leading system message is reported but honoured. |
| `cache_warming_decision` `{ warmCost, missCost, continuationProbability, action }` | `{ action? }` | Last override wins. |
| `before_provider_request` `{ payload }` | `{ payload? }` | Chained. |
| `before_provider_headers` `{ headers }` | `{ headers? }` (a patch; `null` values mean delete) | Patches applied in order. |
| `after_provider_response` `{ status, headers }` | ignored | |
| `before_agent_start` `{ prompt, images?, systemPrompt, systemPromptOptions }` | `{ message?, systemPrompt? }` | Messages collected; a `systemPrompt` forces the prompt, and later handlers see it. |
| `agent_start`, `agent_end` `{ messages }`, `agent_settled`, `turn_start` `{ turnIndex, timestamp }`, `message_start`, `message_update`, `tool_execution_start`/`update`/`end`, `model_select`, `thinking_level_select`, `ui_prompt_start`, `ui_prompt_end` | ignored | |
| `turn_end`, `agent_before_settle` (`entries`, `continue`, `context`, `outcome`, ...) | `{ entries?, continue? }` | Chained; the host rebuilds the context preview after each extension, and entries it rejects void the result (`valid: false`). |
| `message_end` `{ message }` | `{ message? }` | Chained; a replacement must keep the role, else an error and it is skipped. |
| `tool_call` `{ toolCallId, toolName, input }` | `{ block?, reason?, terminate?, input? }` | `input` patches chain; the first truthy `block` wins; else the last decision. Errors propagate (the call fails). In Ultron the call is the REPL cell (`toolName: "rlm"`). |
| `tool_result` `{ toolCallId, toolName, input, content, details, isError, usage? }` | `{ content?, details?, isError?, usage? }` | Chained; `null` overall when nothing changed. |
| `user_bash` `{ command, excludeFromContext, cwd }` | `{ result: { output, exitCode, cancelled, truncated, fullOutputPath? } }` | First result wins; an invalid result or error is reported and propagates. |
| `input` `{ text, images?, source, streamingBehavior? }` | `{ action: "continue" \| "transform" \| "handled", text?, images? }` | Transforms chain; `handled` stops. |
| `before_file_write` `{ path, content, cwd, lane }` | `{ block?, reason?, message? }` | First block wins (`reason` defaults to "blocked by an extension"); messages join with `\n`. Errors propagate. |
| `after_cell_changes` `{ files, checked, deleted, complete, cwd, lane }` | `{ message? }` | All run; messages join with `\n`. |

`AbortSignal` members (`signal` on `session_before_compact`/`_tree`) are not sent: when the operation is aborted
(the user's `abort` during a compaction) the host sends `notifications/cancelled` (`reason: "Aborted"`) for the
event request, stops waiting for it and calls no further handler; a late response is ignored and the compaction
ends aborted, as AgentSession ends it whatever a hook returns after the abort. An event that outlives
`eventTimeoutMs` is cancelled with `notifications/cancelled` too.

## 6. Mapping from `ExtensionAPI` (`core/extensions/types.ts`)

| TypeScript | Protocol |
| --- | --- |
| `on(event, handler)` | `capabilities.events` + `event` requests (section 5); the returned unsubscribe is `registrations/changed`. |
| `registerTool(tool)` | `capabilities.functions` + `functions/call`: a kernel-API function `ext.<extension>.<name>(**arguments)` and a REPL tool (`tools.<name>(...)`), never a model tool. `label`, `executionMode` and `promptGuidelines` are function fields; `execute`'s `toolCallId` is the call's `toolCallId` and `onUpdate(partial)` is `functions/update` (the running call's preview). `renderCall`/`renderResult`, `prepareArguments` and tool `state` are dropped (code); arguments are validated against `parameters`. |
| `registerCommand(name, { description, getArgumentCompletions, handler })` | `capabilities.commands` + `commands/run`, `commands/complete`. Duplicate names get `name:1`, `name:2`, ... in load order (`getRegisteredCommands`). |
| `registerShortcut(key, { description, handler })` | `capabilities.shortcuts` + `shortcuts/run`. The conflict rules of `getShortcuts` apply (`ExtensionHost::shortcuts`): keys of reserved editor actions are refused, other built-in keys and keys two extensions register warn and go to the later one. |
| `registerFlag(name, options)`, `getFlag(name)` | `capabilities.flags`; values in `initialize.flags` and `flags/get`. |
| `registerMessageRenderer`, `registerEntryRenderer`, `registerMarkdownTransformer` | Dropped: arbitrary render code. Custom messages and entries render with the host's generic renderer; their `display` text is the declarative form. |
| `sendMessage`, `sendUserMessage`, `appendEntry` | `session/sendMessage`, `session/sendUserMessage`, `session/appendEntry`. |
| `setSessionName`, `getSessionName`, `setLabel` | `session/setName`, `session/getName`, `session/setLabel`. |
| `exec(command, args, options)` | `host/exec` (the host runs it with its guards; an extension may also spawn processes itself). |
| `getActiveTools`, `getAllTools`, `setActiveTools`, `refreshTools` | `tools/active`, `tools/list`, `tools/setActive` as the worker filters it: the REPL (`rlm`) stays pinned (in front unless the list names it) and the extensions' REPL tools are left out, so the model's tool list stays `[rlm]`; the list is what `before_agent_start` reports as `selectedTools`. `refreshTools` is a no-op. |
| `getCommands` | `commands/list`. |
| `setModel`, `getThinkingLevel`, `setThinkingLevel` | `model/set`, `thinking/get`, `thinking/set`. |
| `registerProvider`, `unregisterProvider` | `capabilities.providers` (declarative config only; `registrations/changed` without one unregisters it), read with `ExtensionHost::providers` and applied to the model registry in load order (a later declaration of a name merges over an earlier one). Code members (`streamSimple`, OAuth `login`/`refreshToken`, `refreshModels`) cannot cross the process boundary. |
| `events` (EventBus `emit`/`on`) | `bus/emit` notification; subscribers list channels in `capabilities.bus` and get `bus/message` (the sender too, like `EventEmitter`). |
| `ctx.ui.select`, `confirm`, `input`, `editor`, `notify`, `setStatus`, `setWidget(string[])`, `setWorkingMessage`, `setWorkingVisible`, `setHiddenThinkingLabel`, `setTitle`, `pasteToEditor`, `setEditorText`, `getEditorText`, `setTheme(name)`, `getAllThemes`, `getToolsExpanded`, `setToolsExpanded` | The `ui/*` methods; `setWidget` is `ui/setPanel`. |
| `ctx.ui.custom`, `setWidget(factory)`, `setFooter`, `setHeader`, `setEditorComponent`, `getEditorComponent`, `addAutocompleteProvider`, `onTerminalInput`, `setWorkingIndicator(frames)`, `getTheme`/`theme` objects | Dropped: TUI components and raw input handlers are code in the host's render loop. Declarative replacements: panels, status lines, selectors (`ui/select`), command completions. |
| `ctx.cwd`, `mode`, `hasUI`, `model`, `scopedModels`, `thinkingLevel`, `isIdle`, `isProjectTrusted`, `hasPendingMessages`, `getContextUsage`, `getSystemPrompt`, command context `getSystemPromptOptions` | `initialize.session` and `host/context` (`scopedModels`: `[{ model, thinkingLevel? }]` from `--models` or `enabledModels`; `systemPromptOptions`: the worker's `{ cwd }`). |
| `ctx.sessionManager` (read-only) | `session/*` getters (section 4) on the real session (the Pi view of its entries). |
| `ctx.modelRegistry` | `models/*` (section 4): the profile's models and the auth a request carries, for the extension's own model calls. `stream`/`complete`, `refresh` and code providers do not cross the boundary. |
| `ctx.abort`, `compact`, `shutdown`, `signal` | `agent/abort`, `agent/compact`, `host/shutdown`; `signal` is `notifications/cancelled`. |
| Command context `waitForIdle`, `newSession`, `fork`, `navigateTree`, `switchSession`, `reload`, `getSystemPromptOptions` | `agent/waitForIdle`, `session/new`, `session/fork`, `session/navigateTree`, `session/switch`, `session/reload`; `withSession`/`setup` callbacks are dropped (the extension continues after the result, and receives `session_start`). |
| `user_bash` result `{ operations }` | Dropped (it carries functions); return `{ result }` after running the command yourself. |
| Inline extensions, `jiti` module loading, `package.json` `pi.extensions` | Removed. |

## 7. Host implementation status (`crates/extensions`)

Implemented and tested: transport and peer (timeouts, cancellation both ways, concurrent requests), discovery with
settings patterns, handshake and registrations, `event` fan-out with every rule in section 5 (compared with the
TypeScript runner on recorded scripted extension sets), `functions/call`, `commands/run`, command invocation names,
shortcut conflict rules, flags (`flags/get`, defaults, command-line values), `registrations/changed`, `bus/*`, UI
notifications into a `UiModel`, prompt brackets and no-UI defaults (`hasUI` follows `HostApi::has_ui`), `host/exec`
(`exec.rs`), crash isolation, restart and disable, shutdown.

The session (`crates/session-core` `extension_host`, `core/extension_ops`) answers the `session/*`, `host/*`,
`agent/*`, `model/*`, `thinking/*`, `tools/list`, `commands/list` and `ui/*` requests as the TypeScript worker's
`LegacyExtensionAdapter` binds them, sends UI to the client over the worker's extension UI bridge, and emits the
events that worker emits: `session_start`, `before_agent_start`, `context`/`context_with_system`, `agent_end`,
`session_before_fork`, `session_shutdown`, `before_file_write`, the compaction and tree events, `ui_prompt_*`. The
worker emits no other AgentSession events (`turn_*`, `message_*`, `tool_*`, `input`, `user_bash`, provider hooks,
`model_select`, ...), so neither does the Rust session; the handshake advertises exactly what it emits
(`DELIVERED_EVENTS`).

`ctx.modelRegistry` (`models/*`, session-core `extension_registry`), `ctx.sessionManager` (`session/*`,
`extension_session`), the active tool list, `scopedModels`/`systemPromptOptions`, function metadata and progress
(`functions/update` into the `tools.*` call's preview; `executionMode: "sequential"` through the lane gate of
kernel-apis `tool_calls`) are implemented. The rest of the UI context is the worker's headless one even with a client
attached (`createHeadlessExtensionUI`): `setWorkingMessage`, `setWorkingVisible`, `setHiddenThinkingLabel`,
`setToolsExpanded` and `setTheme` change nothing, `getEditorText` is `""`, `getToolsExpanded` `false`,
`getAllThemes` `[]`, and `ui/setTheme` answers `{ success: false, error: "Native worker UI adapter does not switch
themes" }` (`"UI not available"` from a host without UI, `noOpUIContext`).

Declared `providers` join the model registry of the session's profile while its extensions run
(`session-core` `extension_providers`, the `ai` registry's extension layer): `/model`, Ctrl+P, `--list-models` and
model selection list them, and requests go to their base URL with their api, key and headers; a declaration that does
not compose is a load diagnostic `Extension "<path>" error: <message>`. The TUI asks the session for the shortcuts in
force under its effective keybindings (`extensionShortcuts`), checks them before its own bindings, runs a match
through the session (`extensionShortcut` -> `shortcuts/run`), lists them in `/hotkeys` and shows the conflict warnings
as `[Extension issues]`.
