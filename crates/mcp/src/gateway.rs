//! The `mcp` gateway (`extensions/mcp/index.ts`): the one extension tool through which the REPL's `mcp` namespace
//! (runtime.py `tools_api.Mcp`) reaches the servers of `mcp.json`. MCP tools are never model tools: the model calls
//! `rlm`, a cell calls `await mcp.call(...)`, the kernel sends a `tools.call` host request for `mcp`, and the host
//! runs [`Gateway::execute`] on the call's thread.
//!
//! Servers come from `<agentDir>/mcp.json` and, in trusted projects, `<cwd>/.pi/mcp.json`. Each connects on first
//! use (`lifecycle: "eager"` or `"keep-alive"`: in the background when the session starts), is reused by every
//! later call, reconnects after it drops, and closes with the session. Values a server's env or headers resolve to
//! are handed to the session's output masker.
//!
//! A gateway error is a result, not a failure: its text says what to do next and `details.error` names it, which
//! the REPL raises as `McpError`. Only a cancelled call fails.

use crate::CancelToken;
use crate::config::{LoadedConfig, ServerEntry, load_config, tool_exposure};
use crate::connection::{Connection, ConnectionEnv, ServerState};
use crate::results::{convert_mcp_result, is_mcp_app_resource, listed, save_to_temp_file, text_of, to_model_content};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// The gateway tool's name: the REPL's `mcp` namespace calls it.
pub const MCP_GATEWAY_TOOL: &str = "mcp";
pub const MCP_GATEWAY_LABEL: &str = "MCP";

/// Ultron's version, sent as `clientInfo.version` (`VERSION` of config.ts: packages/coding-agent/package.json).
pub fn client_version() -> String {
    let package: Value =
        serde_json::from_str(include_str!("../../../packages/coding-agent/package.json")).unwrap_or(Value::Null);
    package["version"].as_str().unwrap_or("0.0.0").to_string()
}

/// The gateway tool's JSON schema (`GATEWAY_PARAMETERS`).
pub fn gateway_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tool": { "type": "string", "description": "MCP tool to call" },
            "args": {
                "anyOf": [{ "type": "string" }, { "type": "object", "additionalProperties": true }],
                "description": "Tool arguments as a JSON object, or as a JSON string encoding one",
            },
            "connect": { "type": "string", "description": "Server to connect (or reconnect)" },
            "describe": { "type": "string", "description": "Tool to describe" },
            "instructions": { "type": "string", "description": "Server whose usage instructions to return" },
            "search": { "type": "string", "description": "Words to find tools by" },
            "limit": { "type": "number" },
            "server": { "type": "string", "description": "Server name: lists its tools, or narrows the other modes" },
            "resources": { "description": "true (or a server name) lists resources" },
            "read": { "type": "string", "description": "URI of a resource to read; needs server" },
        },
    })
}

/// `gatewayDescription`: the worker reads the server names from its `Servers:` line for the runtime guide.
pub fn gateway_description(names: &[String]) -> String {
    let mut parts = vec![
        "MCP gateway: server status, tool listing, description and search, resources, and MCP tool calls.".to_string(),
    ];
    if !names.is_empty() {
        parts.push(format!("Servers: {}", names.join(", ")));
    }
    parts.push(
        "From Python use the mcp namespace: await mcp.servers(), mcp.tools(\"server\"), mcp.describe(\"tool\"), mcp.search(\"query\"), mcp.call(\"tool\", key=value).".into(),
    );
    parts.join("\n\n")
}

/// `mcpServerNames` of session-worker.ts: the names on the description's `Servers:` line.
pub fn server_names_of(description: &str) -> Vec<String> {
    description
        .lines()
        .find_map(|line| line.strip_prefix("Servers: "))
        .map(|names| {
            names
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// What the REPL learns about the gateway tool.
#[derive(Clone, Debug, PartialEq)]
pub struct GatewayTool {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Default)]
pub struct GatewayOptions {
    pub agent_dir: PathBuf,
    /// Whether the project's `.pi/mcp.json` may be read.
    pub project_trusted: bool,
    /// Servers to use instead of the `mcp.json` files.
    pub servers: Option<Vec<ServerEntry>>,
    pub env: ConnectionEnv,
    /// Opens the sign-in URL (default: the platform browser).
    pub open_url: Option<UrlOpener>,
}

const COMMAND_ACTIONS: [&str; 7] = ["connect", "disconnect", "login", "logout", "enable", "disable", "tools"];
const COMMAND_USAGE: &str =
    "Usage: /mcp [status | reload | connect | disconnect | login | logout | enable | disable | tools <server>]";

/// What `/mcp` needs from the client it runs in (`ctx.ui` and `ctx.hasUI` of the extension API).
pub trait CommandUi: Sync {
    /// Whether dialogs can be shown (the menu needs them).
    fn has_ui(&self) -> bool;
    fn notify(&self, message: &str, level: &str);
    /// A choice among `options`; `None` when dismissed.
    fn select(&self, title: &str, options: &[String]) -> Option<String>;
    /// A line of text; `None` when dismissed or when `cancel` ends the dialog.
    fn input(&self, title: &str, placeholder: &str, cancel: &CancelToken) -> Option<String>;
}

/// Notices of a client without dialogs.
#[derive(Default)]
struct Recorded {
    notices: Mutex<Vec<Notice>>,
}

impl CommandUi for Recorded {
    fn has_ui(&self) -> bool {
        false
    }

    fn notify(&self, message: &str, level: &str) {
        lock(&self.notices).push((message.into(), level.into()));
    }

    fn select(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }

    fn input(&self, _: &str, _: &str, _: &CancelToken) -> Option<String> {
        None
    }
}

/// The sign-in prompt over the command's UI (`signInPrompt` of index.ts).
struct Prompt<'a> {
    name: &'a str,
    ui: &'a dyn CommandUi,
    open_url: Option<UrlOpener>,
    hint: String,
}

impl crate::oauth::signin::SignInPrompt for Prompt<'_> {
    fn show_authorization_url(&self, url: &str) {
        match &self.open_url {
            Some(open) => open(url),
            None => open_browser(url),
        }
        self.ui.notify(
            &format!("Sign in to MCP server \"{}\" in your browser:\n{url}", self.name),
            "info",
        );
    }

    /// As in /login: a stray Enter on the empty prompt does not end the sign-in; Esc does.
    fn prompt_for_redirect_url(&self, cancel: &CancelToken) -> Option<String> {
        while !cancel.is_cancelled() {
            let answer = self.ui.input(
                &format!(
                    "MCP sign-in for {}: paste the redirect URL, or wait for the browser. {}",
                    self.name, self.hint
                ),
                "http://127.0.0.1/callback?code=...",
                cancel,
            )?;
            if !answer.trim().is_empty() {
                return Some(answer);
            }
        }
        None
    }
}

/// `openBrowser`'s launcher for `platform` (Node's `process.platform`): `open` on macOS, `rundll32
/// url.dll,FileProtocolHandler` on Windows (never `cmd /c start`, which would re-parse the URL), else `xdg-open`.
pub fn browser_command(platform: &str, target: &str) -> (String, Vec<String>) {
    match platform {
        "darwin" => ("open".into(), vec![target.into()]),
        "win32" => (
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), target.into()],
        ),
        _ => ("xdg-open".into(), vec![target.into()]),
    }
}

/// Node's `process.platform` for this build.
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Opens a URL in the platform browser (`openBrowser`): best effort, never through a shell. The URL stays on screen
/// to open by hand when the launcher is missing.
pub fn open_browser(target: &str) {
    let (command, args) = browser_command(node_platform(), target);
    spawn_detached(&command, &args);
}

/// Starts `command` detached (its own process group, no stdio, as `spawn(..., {detached: true, stdio: "ignore"})`)
/// and reaps it in the background; a launcher that fails to start is ignored.
pub fn spawn_detached(command: &str, args: &[String]) {
    let mut launcher = std::process::Command::new(command);
    launcher
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut launcher, 0);
    if let Ok(mut child) = launcher.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub type UrlOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// A notice for the user (`ctx.ui.notify`): `(message, level)`.
pub type Notice = (String, String);
pub type NoticeSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

struct Server {
    entry: ServerEntry,
    connection: Option<Arc<Connection>>,
}

#[derive(Default)]
struct State {
    servers: Vec<Server>,
    config_errors: Vec<String>,
    closed: bool,
    tool: Option<GatewayTool>,
    eager: Vec<JoinHandle<()>>,
}

pub struct Gateway {
    options: GatewayOptions,
    state: Mutex<State>,
}

/// An error the gateway reports in `details.error`; `code: None` is any other failure (`error`).
struct GatewayError {
    code: Option<&'static str>,
    message: String,
    details: Map<String, Value>,
}

impl GatewayError {
    fn new(code: &'static str, message: String) -> Self {
        Self {
            code: Some(code),
            message,
            details: Map::new(),
        }
    }

    fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    fn other(message: String) -> Self {
        Self {
            code: None,
            message,
            details: Map::new(),
        }
    }
}

type Outcome = Result<Value, GatewayError>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn text(value: impl Into<String>, details: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.into() }], "details": details })
}

/// The name the REPL lists a tool under: `<server>_<tool>`, unique across servers.
fn display_name(server: &str, tool: &str) -> String {
    format!("{server}_{tool}")
}

fn tool_name(tool: &Value) -> &str {
    tool["name"].as_str().unwrap_or_default()
}

/// A string parameter that is present and non-empty (JavaScript truthiness).
fn param<'a>(params: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

/// `JSON.stringify(value, null, 2)`.
fn pretty(value: Option<&Value>) -> String {
    value.map_or_else(
        || "undefined".into(),
        |v| serde_json::to_string_pretty(v).unwrap_or_default(),
    )
}

/// Primary weight of a character under the root collation `localeCompare` uses: whitespace, punctuation and
/// symbols (in CLDR order), digits, then letters regardless of case.
fn collation_weight(c: char) -> (u8, u32) {
    const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if c.is_whitespace() {
        (0, 0)
    } else if let Some(at) = PUNCTUATION.find(c) {
        (1, at as u32)
    } else if c.is_ascii_digit() {
        (2, c as u32)
    } else if c.is_alphabetic() {
        (3, c.to_lowercase().next().unwrap_or(c) as u32)
    } else {
        (1, 100 + c as u32)
    }
}

/// `a.localeCompare(b)` for tool names: primary weights first, then lowercase before uppercase.
fn locale_compare(a: &str, b: &str) -> Ordering {
    let primary = |s: &str| s.chars().map(collation_weight).collect::<Vec<_>>();
    primary(a)
        .cmp(&primary(b))
        .then_with(|| {
            let case = |s: &str| s.chars().map(|c| c.is_uppercase()).collect::<Vec<_>>();
            case(a).cmp(&case(b))
        })
        .then_with(|| a.cmp(b))
}

/// Score of `tool` for the query words: name matches count more than description matches.
fn search_score(words: &[String], server: &ServerEntry, tool: &Value) -> u32 {
    let name = format!("{} {}", server.name, tool_name(tool)).to_lowercase();
    let description = format!(
        "{} {} {}",
        tool["title"].as_str().unwrap_or_default(),
        tool["description"].as_str().unwrap_or_default(),
        server
            .config
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_lowercase();
    let mut score = 0;
    for word in words {
        if name.contains(word.as_str()) {
            score += 2;
        } else if description.contains(word.as_str()) {
            score += 1;
        } else {
            return 0;
        }
    }
    score
}

fn status_of(entry: &ServerEntry, connection: Option<&Arc<Connection>>) -> String {
    if !entry.enabled() {
        return "disabled".into();
    }
    match connection.map(|c| c.state()) {
        None => "not connected".into(),
        Some(ServerState::NeedsAuth) => "needs sign-in".into(),
        Some(state) => state.as_str().into(),
    }
}

/// Tools the REPL can reach: `"exposure": "hidden"` (per server or per tool) removes them.
fn visible_tools(entry: &ServerEntry, connection: Option<&Arc<Connection>>) -> Vec<Value> {
    connection
        .map(|c| c.tools())
        .unwrap_or_default()
        .into_iter()
        .filter(|tool| tool_exposure(&entry.config, tool_name(tool)) != "hidden")
        .collect()
}

impl Gateway {
    /// The gateway for one session. Servers are known before the session starts: a call may arrive first.
    pub fn new(mut options: GatewayOptions) -> Arc<Self> {
        // `credentials ??= new McpOAuthCredentialStore()`: the profile's mcp-auth.json, read only when used.
        if options.env.credentials.is_none() {
            options.env.credentials = Some(Arc::new(crate::oauth::store::CredentialStore::new(
                &options.agent_dir,
                options.env.secrets.clone(),
            )));
        }
        let gateway = Arc::new(Self {
            options,
            state: Mutex::new(State::default()),
        });
        let loaded = gateway.load();
        {
            let mut state = lock(&gateway.state);
            state.servers = loaded
                .servers
                .into_iter()
                .map(|entry| Server {
                    entry,
                    connection: None,
                })
                .collect();
        }
        gateway.ensure_tool();
        gateway
    }

    fn load(&self) -> LoadedConfig {
        match &self.options.servers {
            Some(servers) => LoadedConfig {
                servers: servers.clone(),
                ..LoadedConfig::default()
            },
            None => load_config(
                &self.options.agent_dir,
                &self.options.env.cwd,
                self.options.project_trusted,
            ),
        }
    }

    /// The gateway is registered once there is a server to reach; a profile without MCP servers has no `mcp` tool.
    fn ensure_tool(&self) {
        let mut state = lock(&self.state);
        if state.tool.is_some() || state.servers.is_empty() {
            return;
        }
        let names: Vec<String> = self
            .load()
            .servers
            .into_iter()
            .filter(ServerEntry::enabled)
            .map(|e| e.name)
            .collect();
        state.tool = Some(GatewayTool {
            name: MCP_GATEWAY_TOOL.into(),
            label: MCP_GATEWAY_LABEL.into(),
            description: gateway_description(&names),
            parameters: gateway_parameters(),
        });
    }

    /// The `mcp` tool, once registered.
    pub fn tool(&self) -> Option<GatewayTool> {
        lock(&self.state).tool.clone()
    }

    /// Read the configuration again. Connections of servers whose entry did not change are kept.
    pub fn reload(&self) {
        let loaded = self.load();
        let dropped: Vec<Arc<Connection>> = {
            let mut state = lock(&self.state);
            state.config_errors = loaded.errors;
            let mut previous = std::mem::take(&mut state.servers);
            for entry in loaded.servers {
                let kept = previous
                    .iter()
                    .position(|s| {
                        s.entry.name == entry.name
                            && serde_json::to_string(&s.entry.config).ok() == serde_json::to_string(&entry.config).ok()
                    })
                    .map(|at| previous.remove(at).connection);
                state.servers.push(Server {
                    entry,
                    connection: kept.flatten(),
                });
            }
            previous.into_iter().filter_map(|s| s.connection).collect()
        };
        for connection in dropped {
            connection.close();
        }
        self.ensure_tool();
    }

    /// `session_start`: reload, then connect `eager` and `keep-alive` servers in the background. Returns the
    /// notices to show (configuration errors); `notify` later receives sign-in prompts of eager servers.
    pub fn start(self: &Arc<Self>, notify: Option<NoticeSink>) -> Vec<Notice> {
        lock(&self.state).closed = false;
        self.reload();
        let (errors, eager): (Vec<String>, Vec<String>) = {
            let state = lock(&self.state);
            let eager = state
                .servers
                .iter()
                .filter(|s| {
                    s.entry.enabled()
                        && matches!(
                            s.entry.config.get("lifecycle").and_then(Value::as_str),
                            Some("eager" | "keep-alive")
                        )
                })
                .map(|s| s.entry.name.clone())
                .collect();
            (state.config_errors.clone(), eager)
        };
        let mut notices = Vec::new();
        if !errors.is_empty() {
            notices.push((format!("MCP configuration:\n{}", errors.join("\n")), "warning".into()));
        }
        // Nothing here waits for a server.
        for name in eager {
            let gateway = self.clone();
            let notify = notify.clone();
            let handle = std::thread::spawn(move || {
                let Ok(connection) = gateway.connection_named(&name) else {
                    return;
                };
                if let Err(error) = gateway.connect(&connection)
                    && error.code == Some("auth_required")
                    && !lock(&gateway.state).closed
                    && let Some(notify) = &notify
                {
                    notify(&error.message, "warning");
                }
            });
            lock(&self.state).eager.push(handle);
        }
        notices
    }

    /// Wait for the background connects [`Gateway::start`] began.
    pub fn settle(&self) {
        let handles = std::mem::take(&mut lock(&self.state).eager);
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// `session_shutdown`: close every connection (stdio servers get their stdin closed, then signals).
    pub fn close(&self) {
        let connections: Vec<Arc<Connection>> = {
            let mut state = lock(&self.state);
            state.closed = true;
            state.servers.iter().filter_map(|s| s.connection.clone()).collect()
        };
        for connection in connections {
            connection.close();
        }
    }

    /// The enabled servers' names, in configuration order.
    pub fn server_names(&self) -> Vec<String> {
        lock(&self.state)
            .servers
            .iter()
            .filter(|s| s.entry.enabled())
            .map(|s| s.entry.name.clone())
            .collect()
    }

    /// Run the gateway with the tool's params. `Err` only when `cancel` stopped the call.
    pub fn execute(&self, params: &Map<String, Value>, cancel: Option<&CancelToken>) -> Result<Value, String> {
        match self.gateway(params, cancel) {
            Ok(result) => Ok(result),
            Err(error) => {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(error.message);
                }
                let message = match error.code {
                    Some(_) => error.message,
                    None => format!("Error: {}", error.message),
                };
                let mut details = error.details;
                details.insert("error".into(), error.code.unwrap_or("error").into());
                Ok(text(message, Value::Object(details)))
            }
        }
    }

    fn entry_named(&self, name: &str) -> Result<ServerEntry, GatewayError> {
        let state = lock(&self.state);
        match state.servers.iter().find(|s| s.entry.name == name) {
            Some(server) => Ok(server.entry.clone()),
            None => {
                let names: Vec<&str> = state.servers.iter().map(|s| s.entry.name.as_str()).collect();
                let names = if names.is_empty() {
                    "(none configured)".to_string()
                } else {
                    names.join(", ")
                };
                Err(GatewayError::new(
                    "server_not_found",
                    format!("No MCP server \"{name}\". Servers: {names}"),
                ))
            }
        }
    }

    /// The server's connection (created when needed). Disabled servers have none.
    fn connection_named(&self, name: &str) -> Result<Arc<Connection>, GatewayError> {
        let entry = self.entry_named(name)?;
        if !entry.enabled() {
            return Err(GatewayError::new(
                "server_disabled",
                format!("MCP server \"{name}\" is disabled. Enable it with /mcp."),
            ));
        }
        let mut state = lock(&self.state);
        let Some(server) = state.servers.iter_mut().find(|s| s.entry.name == name) else {
            return Err(GatewayError::new(
                "server_not_found",
                format!("No MCP server \"{name}\""),
            ));
        };
        Ok(server
            .connection
            .get_or_insert_with(|| Connection::new(server.entry.clone(), self.options.env.clone()))
            .clone())
    }

    /// The connection, connected. Failures are gateway errors that say what to do next.
    fn connect(&self, connection: &Arc<Connection>) -> Result<(), GatewayError> {
        connection.get_client().map(drop).map_err(|error| {
            let name = connection.name().to_string();
            if connection.state() == ServerState::NeedsAuth {
                GatewayError::new("auth_required", error).with("server", name)
            } else {
                GatewayError::new(
                    "connect_failed",
                    format!("MCP server \"{name}\" could not be connected: {error}"),
                )
                .with("server", name)
            }
        })
    }

    fn connected(&self, name: &str) -> Result<Arc<Connection>, GatewayError> {
        let connection = self.connection_named(name)?;
        self.connect(&connection)?;
        Ok(connection)
    }

    fn enabled_servers(&self, only: Option<&str>) -> Result<Vec<ServerEntry>, GatewayError> {
        match only {
            Some(name) => Ok(vec![self.entry_named(name)?]),
            None => Ok(lock(&self.state)
                .servers
                .iter()
                .filter(|s| s.entry.enabled())
                .map(|s| s.entry.clone())
                .collect()),
        }
    }

    /// Find a tool by the name the REPL lists (`<server>_<tool>`), by Pi's `mcp__<server>__<tool>`, or by the
    /// server's own name for it. The named or prefixed servers connect first, the rest only when the name is
    /// still unknown.
    fn find_tool(&self, name: &str, only: Option<&str>) -> Result<(Arc<Connection>, Value), GatewayError> {
        let candidates = self.enabled_servers(only)?;
        let matches = |server: &str, tool: &str| {
            let pi_name: String = format!("mcp__{server}__{tool}")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
                .collect();
            tool == name || display_name(server, tool) == name || pi_name == name
        };
        let is_prefixed = |server: &ServerEntry| {
            name.starts_with(&format!("{}_", server.name))
                || name.starts_with(&format!("mcp__{}__", server.name.replace('-', "_")))
        };
        let (prefixed, rest): (Vec<ServerEntry>, Vec<ServerEntry>) = candidates.into_iter().partition(is_prefixed);
        let prefixed_count = prefixed.len();
        let mut failure = None;
        for (index, entry) in prefixed.into_iter().chain(rest).enumerate() {
            let connection = match self.connected(&entry.name) {
                Ok(connection) => connection,
                Err(error) => {
                    // A server that is down does not hide a tool another server has; the named server's error is kept.
                    if (only.is_some() || index < prefixed_count) && failure.is_none() {
                        failure = Some(error);
                    }
                    continue;
                }
            };
            if let Some(tool) = visible_tools(&entry, Some(&connection))
                .into_iter()
                .find(|tool| matches(&entry.name, tool_name(tool)))
            {
                return Ok((connection, tool));
            }
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
        let on = only.map(|s| format!(" on server \"{s}\"")).unwrap_or_default();
        Err(GatewayError::new(
            "tool_not_found",
            format!(
                "Tool \"{name}\" not found{on}. List tools with await mcp.tools(\"server\") or await mcp.search(\"words\")."
            ),
        )
        .with("requestedTool", name))
    }

    /// `status`: every server's state, tool count and last error, then the configuration errors.
    pub fn status(&self) -> Value {
        let state = lock(&self.state);
        let list: Vec<Value> = state
            .servers
            .iter()
            .map(|server| {
                let connection = server.connection.as_ref();
                let entry = &server.entry;
                let tool_count = match connection {
                    Some(c) if c.state() == ServerState::Connected => json!(visible_tools(entry, connection).len()),
                    _ => Value::Null,
                };
                let mut item = json!({
                    "name": entry.name,
                    "status": status_of(entry, connection),
                    "toolCount": tool_count,
                    "transport": if entry.config.contains_key("url") { "http" } else { "stdio" },
                    // Servers given in code are not saved anywhere (`scope ?? "session"`).
                    "scope": if self.options.servers.is_some() { "session" } else { entry.scope.as_str() },
                });
                if !entry.enabled() {
                    item["disabled"] = json!(true);
                }
                if let Some(description) = entry
                    .config
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    item["description"] = json!(description);
                }
                if let Some(error) = connection.and_then(|c| c.error()).filter(|e| !e.is_empty()) {
                    item["error"] = json!(error);
                }
                item
            })
            .collect();
        let connected = list.iter().filter(|s| s["status"] == "connected").count();
        let mut lines = vec![format!("MCP: {connected}/{} servers connected", list.len())];
        for server in &list {
            let count = server["toolCount"]
                .as_u64()
                .map(|n| format!(" ({n} tools)"))
                .unwrap_or_default();
            let error = server["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default();
            lines.push(format!(
                "{}: {}{count}{error}",
                server["name"].as_str().unwrap_or_default(),
                server["status"].as_str().unwrap_or_default()
            ));
        }
        lines.extend(state.config_errors.iter().cloned());
        let mut details = json!({ "mode": "status", "servers": list });
        if !state.config_errors.is_empty() {
            details["configErrors"] = json!(state.config_errors);
        }
        text(lines.join("\n"), details)
    }

    fn gateway(&self, params: &Map<String, Value>, cancel: Option<&CancelToken>) -> Outcome {
        let server_param = param(params, "server");
        if let Some(tool) = param(params, "tool") {
            let args = parse_args(params.get("args"))?;
            let (connection, found) = self.find_tool(tool, server_param)?;
            self.connect(&connection)?;
            let original = tool_name(&found).to_string();
            let result = connection
                .call_tool(&original, &args, cancel)
                .map_err(GatewayError::other)?;
            let converted = convert_mcp_result(
                connection.name(),
                &original,
                &result,
                connection.has_listed_resources(),
                &save_to_temp_file,
            );
            let mut details = json!({ "mode": "call", "server": connection.name(), "tool": original });
            if let Some(structured) = result.get("structuredContent") {
                details["structuredContent"] = structured.clone();
            }
            if converted["isError"] == true {
                details["error"] = json!("tool_error");
            }
            return Ok(json!({ "content": converted["content"], "details": details }));
        }
        if let Some(name) = param(params, "connect") {
            let connection = self.connection_named(name)?;
            if connection.state() == ServerState::Connected {
                connection.reconnect().map_err(GatewayError::other)?;
            } else {
                self.connect(&connection)?;
            }
            let entry = self.entry_named(name)?;
            let count = visible_tools(&entry, Some(&connection)).len();
            return Ok(text(
                format!("Connected to {name} ({count} tools)."),
                json!({ "mode": "connect", "server": name }),
            ));
        }
        if let Some(name) = param(params, "describe") {
            let (connection, tool) = self.find_tool(name, server_param)?;
            let server = connection.name();
            let original = tool_name(&tool);
            let description = tool["description"].as_str().unwrap_or_default();
            let mut described = json!({
                "name": display_name(server, original),
                "originalName": original,
                "description": description,
            });
            if let Some(schema) = tool.get("inputSchema") {
                described["inputSchema"] = schema.clone();
            }
            if let Some(schema) = tool.get("outputSchema") {
                described["outputSchema"] = schema.clone();
            }
            return Ok(text(
                format!(
                    "{}\nServer: {server}\n\n{description}\n\nParameters (JSON schema):\n{}",
                    display_name(server, original),
                    pretty(tool.get("inputSchema"))
                ),
                json!({ "mode": "describe", "tool": described, "server": server }),
            ));
        }
        if let Some(search) = params.get("search").filter(|v| !v.is_null()) {
            let words: Vec<String> = search
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .split_whitespace()
                .map(String::from)
                .collect();
            let mut matches: Vec<(u32, String, String, String)> = Vec::new();
            for entry in self.enabled_servers(server_param)? {
                let Ok(connection) = self.connected(&entry.name) else {
                    continue;
                };
                for tool in visible_tools(&entry, Some(&connection)) {
                    let score = if words.is_empty() {
                        1
                    } else {
                        search_score(&words, &entry, &tool)
                    };
                    if score > 0 {
                        matches.push((
                            score,
                            entry.name.clone(),
                            display_name(&entry.name, tool_name(&tool)),
                            first_line(tool["description"].as_str().unwrap_or_default()).to_string(),
                        ));
                    }
                }
            }
            matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_compare(&a.2, &b.2)));
            let limit = params
                .get("limit")
                .and_then(Value::as_f64)
                .filter(|l| *l > 0.0)
                .map_or(20, |l| l.trunc() as usize);
            let limited: Vec<&(u32, String, String, String)> = matches.iter().take(limit).collect();
            let mut lines = vec![format!("Found {} tools", matches.len())];
            lines.extend(limited.iter().map(|m| format!("{}: {}", m.2, m.3)));
            let listed: Vec<Value> = limited
                .iter()
                .map(|m| json!({ "server": m.1, "tool": m.2, "score": m.0, "description": m.3 }))
                .collect();
            return Ok(text(
                lines.join("\n"),
                json!({ "mode": "search", "matches": listed, "count": matches.len() }),
            ));
        }
        if let Some(name) = param(params, "instructions") {
            self.entry_named(name)?;
            let connection = self.connected(name)?;
            let instructions = connection
                .instructions()
                .unwrap_or_else(|| format!("MCP server \"{name}\" has no instructions."));
            return Ok(text(instructions, json!({ "mode": "instructions", "server": name })));
        }
        if let Some(uri) = param(params, "read") {
            let Some(name) = server_param else {
                return Err(GatewayError::new(
                    "invalid_args",
                    "Reading a resource needs the server name".into(),
                ));
            };
            self.entry_named(name)?;
            let connection = self.connected(name)?;
            let result = connection.read_resource(uri, cancel).map_err(GatewayError::other)?;
            let blocks: Vec<Value> = result["contents"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|resource| json!({ "type": "resource", "resource": resource }))
                .collect();
            let content = to_model_content(name, &blocks, false, &save_to_temp_file);
            return Ok(json!({ "content": content, "details": { "mode": "read", "server": name, "uri": uri } }));
        }
        if let Some(resources) = params.get("resources").filter(|v| truthy(v)) {
            let only = resources.as_str().or(server_param);
            let (mut found, mut templates, mut errors) = (Vec::new(), Vec::new(), Vec::new());
            for entry in self.enabled_servers(only)? {
                let listing = self.connected(&entry.name).and_then(|connection| {
                    let resources = connection.all_resources(cancel).map_err(GatewayError::other)?;
                    let templates = connection.all_resource_templates(cancel).map_err(GatewayError::other)?;
                    Ok((resources, templates))
                });
                match listing {
                    Ok((resources, list)) => {
                        found.extend(
                            resources
                                .iter()
                                .filter(|r| !is_mcp_app_resource(r))
                                .map(|r| listed(&entry.name, r)),
                        );
                        templates.extend(
                            list.iter()
                                .filter(|t| !is_mcp_app_resource(t))
                                .map(|t| listed(&entry.name, t)),
                        );
                    }
                    Err(error) => errors.push(json!({ "server": entry.name, "error": error.message })),
                }
            }
            let mut details = json!({ "mode": "resources", "resources": found, "resourceTemplates": templates });
            if !errors.is_empty() {
                details["errors"] = json!(errors);
            }
            return Ok(text(
                format!("{} resources, {} resource templates", found.len(), templates.len()),
                details,
            ));
        }
        if let Some(name) = server_param {
            let entry = self.entry_named(name)?;
            let connection = self.connected(name)?;
            let tools: Vec<String> = visible_tools(&entry, Some(&connection))
                .iter()
                .map(|t| display_name(name, tool_name(t)))
                .collect();
            return Ok(text(
                format!("{name} ({} tools)\n{}", tools.len(), tools.join("\n")),
                json!({ "mode": "list", "server": name, "tools": tools, "count": tools.len() }),
            ));
        }
        Ok(self.status())
    }

    /// `/mcp [status | reload | <action> <server>]` without a UI: the last notice it shows, `(message, level)`.
    pub fn command(&self, args: &str) -> Notice {
        let ui = Recorded::default();
        self.run_command(args, &ui);
        ui.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .unwrap_or_default()
    }

    /// The `/mcp` handler: status, reload, or an action on a server; without arguments in a client with dialogs, a
    /// menu (pick a server, then what to do with it). Every outcome is a notice.
    pub fn run_command(&self, args: &str, ui: &dyn CommandUi) {
        let words: Vec<&str> = args.split_whitespace().collect();
        let status = |gateway: &Self| Self::text_of(&gateway.status());
        let outcome = match words.as_slice() {
            [] => {
                let picked = if ui.has_ui() { self.menu(ui) } else { Ok(None) };
                picked.map(|text| Some(text.unwrap_or_else(|| status(self))))
            }
            ["status"] => Ok(Some(status(self))),
            ["reload"] => {
                self.reload();
                Ok(Some(status(self)))
            }
            [action, name] if COMMAND_ACTIONS.contains(action) => self
                .entry_named(name)
                .and_then(|_| self.action(action, name, ui))
                .map(Some),
            _ => return ui.notify(COMMAND_USAGE, "warning"),
        };
        match outcome {
            Ok(Some(text)) => ui.notify(&text, "info"),
            Ok(None) => {}
            Err(error) => ui.notify(&format!("MCP: {}", error.message), "error"),
        }
    }

    /// The status word `/mcp` shows for a server.
    fn status_named(&self, name: &str) -> String {
        let state = lock(&self.state);
        state
            .servers
            .iter()
            .find(|s| s.entry.name == name)
            .map(|s| status_of(&s.entry, s.connection.as_ref()))
            .unwrap_or_default()
    }

    /// `/mcp` without arguments in a client with dialogs.
    fn menu(&self, ui: &dyn CommandUi) -> Result<Option<String>, GatewayError> {
        let servers: Vec<(String, bool)> = lock(&self.state)
            .servers
            .iter()
            .map(|s| (s.entry.name.clone(), s.entry.config.contains_key("url")))
            .collect();
        if servers.is_empty() {
            return Ok(None);
        }
        let labels: Vec<String> = servers
            .iter()
            .map(|(name, _)| format!("{name} \u{2014} {}", self.status_named(name)))
            .collect();
        let Some(picked) = ui.select("MCP servers", &labels) else {
            return Ok(None);
        };
        let Some(index) = labels.iter().position(|l| *l == picked) else {
            return Ok(None);
        };
        let (name, http) = &servers[index];
        let state = self.status_named(name);
        let mut choices: Vec<(&str, &str)> = Vec::new();
        if state == "disabled" {
            choices.push(("Enable", "enable"));
        }
        if state != "disabled" && state != "connected" {
            choices.push(("Connect", "connect"));
        }
        if state == "connected" {
            choices.extend([
                ("Show tools", "tools"),
                ("Reconnect", "connect"),
                ("Disconnect", "disconnect"),
            ]);
        }
        if state != "disabled" && *http {
            choices.extend([("Sign in", "login"), ("Sign out", "logout")]);
        }
        if state != "disabled" {
            choices.push(("Disable", "disable"));
        }
        let options: Vec<String> = choices.iter().map(|(label, _)| label.to_string()).collect();
        let action = ui.select(&format!("{name} ({state})"), &options);
        match choices.iter().find(|(label, _)| Some(*label) == action.as_deref()) {
            Some((_, action)) => self.action(action, name, ui).map(Some),
            None => Ok(None),
        }
    }

    fn action(&self, action: &str, name: &str, ui: &dyn CommandUi) -> Result<String, GatewayError> {
        let entry = self.entry_named(name)?;
        match action {
            "connect" => {
                let mut params = Map::new();
                params.insert("connect".into(), json!(name));
                Ok(Self::text_of(&self.gateway(&params, None)?))
            }
            "disconnect" => Ok(self.disconnect(name)),
            "login" => self.sign_in(&entry, ui),
            "logout" => {
                let connection = self.connection_named(name)?;
                let removed = match (connection.oauth_url(), &self.options.env.credentials) {
                    (Some(url), Some(credentials)) => credentials
                        .remove(name, &url)
                        .map_err(|e| GatewayError::other(e.to_string()))?,
                    _ => false,
                };
                connection.sign_out();
                Ok(if removed {
                    format!("Signed out of \"{name}\".")
                } else {
                    format!("No stored credentials for \"{name}\".")
                })
            }
            "enable" | "disable" => {
                let enabled = action == "enable";
                if self.options.servers.is_none() {
                    crate::edit::update_server_enabled(Path::new(&entry.source), name, enabled)
                        .map_err(GatewayError::other)?;
                }
                if !enabled {
                    self.disconnect(name);
                }
                if let Some(server) = lock(&self.state).servers.iter_mut().find(|s| s.entry.name == name) {
                    if enabled {
                        server.entry.config.shift_remove("enabled");
                    } else {
                        server.entry.config.insert("enabled".into(), json!(false));
                    }
                }
                let verb = if enabled { "Enabled" } else { "Disabled" };
                let place = match self.options.servers {
                    None => format!(" in {}", entry.source),
                    Some(_) => " for this session".into(),
                };
                Ok(format!("{verb} \"{name}\"{place}."))
            }
            _ => {
                let connection = self.connected(name)?;
                let tools = visible_tools(&entry, Some(&connection));
                let mut lines = vec![format!("{name}: {} tools", tools.len())];
                lines.extend(tools.iter().map(|tool| {
                    format!(
                        "  {}: {}",
                        display_name(name, tool_name(tool)),
                        first_line(tool["description"].as_str().unwrap_or_default())
                    )
                }));
                Ok(lines.join("\n"))
            }
        }
    }

    /// `/mcp login`: the browser sign-in of an OAuth server, then a reconnect with the new tokens.
    fn sign_in(&self, entry: &ServerEntry, ui: &dyn CommandUi) -> Result<String, GatewayError> {
        let name = entry.name.as_str();
        let connection = self.connection_named(name)?;
        let Some(url) = connection.oauth_url() else {
            let provider = entry
                .config
                .get("url")
                .and_then(|_| entry.config.get("auth")?.get("provider")?.as_str());
            return Ok(match provider {
                Some(provider) => format!("MCP server \"{name}\" uses the {provider} login: run /login {provider}."),
                None => format!("MCP server \"{name}\" does not use OAuth sign-in."),
            });
        };
        // A first connection attempt finds out what the server asks for (the challenge), or that it needs nothing.
        let _ = connection.get_client();
        let credentials = self
            .options
            .env
            .credentials
            .clone()
            .ok_or_else(|| GatewayError::other("MCP OAuth credentials are not available".into()))?;
        let settings = connection.oauth_settings().map_err(GatewayError::other)?;
        let prompt = Prompt {
            name,
            ui,
            open_url: self.options.open_url.clone(),
            hint: crate::oauth::signin::unreachable_callback_hint(crate::oauth::signin::browser_reach(
                &|key| std::env::var(key).ok(),
                &|path| Path::new(path).exists(),
            )),
        };
        match crate::oauth::signin::sign_in(
            &url,
            &credentials.for_server(name, &url),
            &settings,
            connection.challenge().as_ref(),
            &prompt,
        ) {
            Ok(()) => {}
            Err(crate::oauth::OAuthError::Cancelled) => return Ok(format!("Sign-in to \"{name}\" cancelled.")),
            Err(error) => return Err(GatewayError::other(error.to_string())),
        }
        connection.reconnect().map_err(GatewayError::other)?;
        Ok(format!(
            "Signed in to \"{name}\" ({} tools).",
            visible_tools(entry, Some(&connection)).len()
        ))
    }

    fn disconnect(&self, name: &str) -> String {
        let connection = lock(&self.state)
            .servers
            .iter_mut()
            .find(|s| s.entry.name == name)
            .and_then(|s| s.connection.take());
        if let Some(connection) = connection {
            connection.close();
        }
        format!("Disconnected \"{name}\". The next call connects it again.")
    }

    /// The text of a gateway result (`textOf`).
    pub fn text_of(result: &Value) -> String {
        text_of(result["content"].as_array().map_or(&[], Vec::as_slice))
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

/// `parseArgs`: an object, a JSON string encoding one, or nothing.
fn parse_args(args: Option<&Value>) -> Result<Map<String, Value>, GatewayError> {
    let parsed = match args {
        None => return Ok(Map::new()),
        Some(Value::String(s)) if s.is_empty() => return Ok(Map::new()),
        Some(Value::String(s)) => crate::js_json::parse(s).map_err(GatewayError::other)?,
        Some(other) => other.clone(),
    };
    match parsed {
        Value::Object(map) => Ok(map),
        _ => Err(GatewayError::new(
            "invalid_args",
            "Invalid args: expected a JSON object".into(),
        )),
    }
}
