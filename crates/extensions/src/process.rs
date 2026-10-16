//! Extension processes: spawn from a manifest, `initialize` handshake, registrations, request routing in both
//! directions, crash isolation and restart.
//!
//! A process that exits fails only its own in-flight requests (the router reports them as that extension's errors
//! and moves on). The next use restarts it, up to the manifest's `maxRestarts`; after that the extension is disabled
//! for the session and every use fails at once.

use crate::manifest::{Diagnostic, Manifest};
use crate::router::{Endpoint, Router};
use crate::rpc::{CancelToken, NotificationHandler, Peer, RequestHandler, RpcError, method_not_found};
use crate::ui::{UiUpdate, no_ui_answer, parse_update, prompt_kind};
use mcp::{StdioOptions, StdioTransport};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// The protocol revision this host speaks (`initialize.protocolVersion`).
pub const PROTOCOL_VERSION: &str = "2026-10-10";
/// How long `shutdown` may take before the process is stopped.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the rest of the product provides to extensions. Everything has a default so a host only implements what it
/// supports; unknown methods answer "Method not found".
pub trait HostApi: Send + Sync {
    /// An extension's request to the host (`host/*`, `session/*`, `agent/*`, `ui/*` prompts, ...).
    fn request(&self, extension: &str, method: &str, params: &Value, cancel: &CancelToken) -> Result<Value, RpcError> {
        let _ = (extension, params, cancel);
        no_ui_answer(method).ok_or_else(|| method_not_found(method))
    }
    /// A declarative UI update.
    fn ui(&self, extension: &str, update: UiUpdate) {
        let _ = (extension, update);
    }
    /// `log` notifications and protocol problems (malformed UI updates, ...).
    fn log(&self, extension: &str, level: &str, message: &str) {
        let _ = (extension, level, message);
    }
    /// The process exited or failed to start; `restarts` is how many restarts it has used.
    fn exited(&self, extension: &str, reason: &str) {
        let _ = (extension, reason);
    }
    /// Whether a UI serves the session right now (the worker's `hasUI`: an interactive client polls the extension UI,
    /// or the one it was started for has not arrived yet). Only consulted when the session has a UI at all.
    fn has_ui(&self) -> bool {
        true
    }
}

/// A host with no UI and no host operations (print mode, tests).
pub struct NoHost;
impl HostApi for NoHost {}

/// Session facts sent in `initialize`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub cwd: String,
    /// "interactive", "print", "json" or "rpc".
    pub mode: String,
    pub has_ui: bool,
    /// Values for registered flags given on the command line.
    pub flags: Map<String, Value>,
}

/// `registerFlag(name, { type, default?, description? })`.
#[derive(Debug, Clone, PartialEq)]
pub struct FlagSpec {
    pub name: String,
    /// "boolean" or "string".
    pub kind: String,
    pub default: Option<Value>,
    pub description: Option<String>,
}

/// `registerProvider(name, config)` without code: the declarative provider configuration (`baseUrl`, `apiKey`,
/// `api`, `headers`, `authHeader`, `models`), the shape of a models.json provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSpec {
    pub name: String,
    pub config: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// What an extension registered: from `initialize`'s `capabilities`, replaced whole by `registrations/changed`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Registrations {
    pub events: Vec<String>,
    pub functions: Vec<FunctionSpec>,
    pub commands: Vec<(String, Option<String>)>,
    pub shortcuts: Vec<(String, Option<String>)>,
    pub flags: Vec<FlagSpec>,
    pub providers: Vec<ProviderSpec>,
    pub bus: Vec<String>,
}

/// JavaScript's `typeof` for a JSON value.
fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Null | Value::Array(_) | Value::Object(_) => "object",
    }
}

/// `registerFlag`'s checks: a name, a type of "boolean" or "string", and a default of that type
/// (`Invalid default for flag "<name>": expected <type>, got <typeof default>`).
fn parse_flags(value: Option<&Value>) -> Result<Vec<FlagSpec>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let items = value.as_array().ok_or("flags must be an array")?;
    items
        .iter()
        .map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or("each of flags needs a \"name\"")?;
            let kind = match item.get("type").and_then(Value::as_str) {
                Some(kind @ ("boolean" | "string")) => kind.to_string(),
                _ => return Err(format!("flag \"{name}\" needs a \"type\" of \"boolean\" or \"string\"")),
            };
            let default = item.get("default").cloned();
            if let Some(default) = &default
                && type_of(default) != kind
            {
                return Err(format!(
                    "Invalid default for flag \"{name}\": expected {kind}, got {}",
                    type_of(default)
                ));
            }
            Ok(FlagSpec {
                name: name.to_string(),
                kind,
                default,
                description: item.get("description").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

fn parse_providers(value: Option<&Value>) -> Result<Vec<ProviderSpec>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let items = value.as_array().ok_or("providers must be an array")?;
    items
        .iter()
        .map(|item| {
            let mut config = item.as_object().cloned().ok_or("each provider must be an object")?;
            let name = config
                .remove("name")
                .and_then(|n| n.as_str().map(str::to_string))
                .filter(|n| !n.is_empty())
                .ok_or("each provider needs a \"name\"")?;
            Ok(ProviderSpec { name, config })
        })
        .collect()
}

fn strings(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{what} must be strings"))
            })
            .collect(),
        Some(_) => Err(format!("{what} must be an array")),
    }
}

fn named(value: Option<&Value>, what: &str, key: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let items = value.as_array().ok_or_else(|| format!("{what} must be an array"))?;
    items
        .iter()
        .map(|item| {
            let name = item
                .get(key)
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| format!("each of {what} needs a \"{key}\""))?;
            Ok((
                name.to_string(),
                item.get("description").and_then(Value::as_str).map(str::to_string),
            ))
        })
        .collect()
}

impl Registrations {
    pub fn parse(capabilities: &Value) -> Result<Registrations, String> {
        if !capabilities.is_object() {
            return Err("capabilities must be an object".into());
        }
        let functions = match capabilities.get("functions") {
            None => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|f| {
                    let name = f
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|n| is_identifier(n))
                        .ok_or("each function needs a \"name\" that is a Python identifier")?;
                    Ok(FunctionSpec {
                        name: name.to_string(),
                        description: f
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        parameters: f
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({ "type": "object" })),
                    })
                })
                .collect::<Result<_, String>>()?,
            Some(_) => return Err("functions must be an array".into()),
        };
        Ok(Registrations {
            events: strings(capabilities.get("events"), "events")?,
            functions,
            commands: named(capabilities.get("commands"), "commands", "name")?,
            shortcuts: named(capabilities.get("shortcuts"), "shortcuts", "key")?,
            flags: parse_flags(capabilities.get("flags"))?,
            providers: parse_providers(capabilities.get("providers"))?,
            bus: strings(capabilities.get("bus"), "bus")?,
        })
    }
}

/// Python identifiers for `ext.<extension>.<function>`.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Nesting depth of blocking UI prompts and the outer prompt's (kind, title).
type PromptDepth = (usize, Option<(String, Option<String>)>);

/// State shared by every extension of one host: the host API, session facts, the bus and the UI prompt depth.
struct Shared {
    host: Arc<dyn HostApi>,
    session: SessionInfo,
    processes: Mutex<Vec<Weak<ExtensionProcess>>>,
    /// `withUIPrompt`: nesting depth and the outer prompt.
    prompt: Mutex<PromptDepth>,
    /// `runtime.flagValues`: one value per flag name for the whole session (the first registered default, or the
    /// command line's value).
    flag_values: Mutex<Map<String, Value>>,
}

impl Shared {
    /// The session has a UI and it is served now.
    fn has_ui(&self) -> bool {
        self.session.has_ui && self.host.has_ui()
    }

    /// `registerFlag` after load: a default fills a flag that has no value yet.
    fn register_defaults(&self, flags: &[FlagSpec]) {
        let mut values = lock(&self.flag_values);
        for flag in flags {
            if let Some(default) = &flag.default
                && !values.contains_key(&flag.name)
            {
                values.insert(flag.name.clone(), default.clone());
            }
        }
    }

    fn processes(&self) -> Vec<Arc<ExtensionProcess>> {
        lock(&self.processes).iter().filter_map(Weak::upgrade).collect()
    }

    /// `void this.emit(event)`: delivered without waiting, to running subscribers only.
    fn emit_detached(&self, event: Value) {
        let kind = event["type"].as_str().unwrap_or_default().to_string();
        for process in self.processes() {
            if process.subscribes(&kind)
                && let Some(peer) = process.live_peer()
            {
                let _ = peer.notify("notifications/event", event.clone());
            }
        }
    }

    fn prompt_event(kind: &str, prompt_kind: &str, title: Option<&str>) -> Value {
        let mut event = json!({ "type": kind, "reason": "ui_prompt", "kind": prompt_kind });
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            event["title"] = json!(title);
        }
        event
    }

    fn prompt_start(&self, kind: &str, title: Option<&str>) {
        let outer = {
            let mut prompt = lock(&self.prompt);
            prompt.0 += 1;
            if prompt.0 == 1 {
                prompt.1 = Some((kind.to_string(), title.map(str::to_string)));
            }
            prompt.0 == 1
        };
        if outer {
            self.emit_detached(Self::prompt_event("ui_prompt_start", kind, title));
        }
    }

    fn prompt_end(&self, kind: &str, title: Option<&str>) {
        let finished = {
            let mut prompt = lock(&self.prompt);
            prompt.0 = prompt.0.saturating_sub(1);
            if prompt.0 > 0 {
                return;
            }
            prompt.1.take()
        };
        let (kind, title) = finished.unwrap_or((kind.to_string(), title.map(str::to_string)));
        self.emit_detached(Self::prompt_event("ui_prompt_end", &kind, title.as_deref()));
    }

    fn broadcast(&self, channel: &str, data: &Value) {
        for process in self.processes() {
            if process.registrations().bus.iter().any(|c| c == channel)
                && let Some(peer) = process.live_peer()
            {
                let _ = peer.notify("bus/message", json!({ "channel": channel, "data": data }));
            }
        }
    }
}

struct ProcState {
    peer: Option<Peer>,
    registrations: Registrations,
    info: Value,
    /// Processes started so far (the first start plus restarts).
    starts: u32,
    disabled: Option<String>,
}

pub struct ExtensionProcess {
    manifest: Manifest,
    display_path: String,
    shared: Arc<Shared>,
    me: Weak<ExtensionProcess>,
    state: Mutex<ProcState>,
    /// Serializes (re)starts so concurrent callers start one process.
    starting: Mutex<()>,
}

impl ExtensionProcess {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    pub fn registrations(&self) -> Registrations {
        lock(&self.state).registrations.clone()
    }

    /// `extensionInfo` from the last handshake.
    pub fn info(&self) -> Value {
        lock(&self.state).info.clone()
    }

    pub fn restarts(&self) -> u32 {
        lock(&self.state).starts.saturating_sub(1)
    }

    pub fn disabled(&self) -> Option<String> {
        lock(&self.state).disabled.clone()
    }

    pub fn pid(&self) -> Option<u32> {
        let _ = self.live_peer()?;
        lock(&self.state)
            .info
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
    }

    fn live_peer(&self) -> Option<Peer> {
        lock(&self.state).peer.clone().filter(|p| !p.is_closed())
    }

    fn request_handler(&self) -> RequestHandler {
        let shared = Arc::downgrade(&self.shared);
        let name = self.manifest.name.clone();
        let me = self.me.clone();
        Arc::new(move |method, params, cancel| {
            let Some(shared) = shared.upgrade() else {
                return Err(RpcError::Closed("host stopped".into()));
            };
            let params = params.cloned().unwrap_or(Value::Null);
            if method == "flags/get" {
                // `getFlag`: only a flag this extension registered has a value for it.
                let flag = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let registered = me
                    .upgrade()
                    .is_some_and(|p| p.registrations().flags.iter().any(|f| f.name == flag));
                return Ok(match registered {
                    true => lock(&shared.flag_values).get(flag).cloned().unwrap_or(Value::Null),
                    false => Value::Null,
                });
            }
            let Some(kind) = prompt_kind(method) else {
                return shared.host.request(&name, method, &params, cancel);
            };
            let title = params.get("title").and_then(Value::as_str).map(str::to_string);
            shared.prompt_start(kind, title.as_deref());
            let answer = if shared.has_ui() {
                shared.host.request(&name, method, &params, cancel)
            } else {
                Ok(no_ui_answer(method).unwrap_or_default())
            };
            shared.prompt_end(kind, title.as_deref());
            answer
        })
    }

    fn notification_handler(&self) -> NotificationHandler {
        let me = self.me.clone();
        Arc::new(move |method, params| {
            let Some(process) = me.upgrade() else { return };
            let params = params.cloned().unwrap_or(Value::Null);
            let host = &process.shared.host;
            let name = &process.manifest.name;
            match method {
                "registrations/changed" => match Registrations::parse(&params) {
                    Ok(registrations) => {
                        process.shared.register_defaults(&registrations.flags);
                        lock(&process.state).registrations = registrations;
                    }
                    Err(error) => host.log(name, "error", &format!("registrations/changed rejected: {error}")),
                },
                "bus/emit" => {
                    let channel = params.get("channel").and_then(Value::as_str).unwrap_or_default();
                    process
                        .shared
                        .broadcast(channel, params.get("data").unwrap_or(&Value::Null));
                }
                "log" => {
                    let level = params.get("level").and_then(Value::as_str).unwrap_or("info");
                    host.log(
                        name,
                        level,
                        params.get("message").and_then(Value::as_str).unwrap_or_default(),
                    );
                }
                _ => match parse_update(method, &params) {
                    Ok(Some(update)) => {
                        if process.shared.has_ui() {
                            host.ui(name, update);
                        }
                    }
                    Ok(None) => host.log(name, "warning", &format!("unknown notification {method}")),
                    Err(error) => host.log(name, "error", &format!("{method}: {error}")),
                },
            }
        })
    }

    /// Spawns the process and runs `initialize`.
    fn spawn(&self, restart: u32) -> Result<(Peer, Registrations, Value), String> {
        let manifest = &self.manifest;
        let transport = Arc::new(StdioTransport::new(StdioOptions {
            command: manifest.command.clone(),
            args: manifest.args.clone(),
            cwd: Some(manifest.dir.clone()),
            env: manifest.env.clone(),
            ..StdioOptions::default()
        }));
        let peer = Peer::start(transport.clone(), self.request_handler(), self.notification_handler())
            .map_err(|e| e.to_string())?;
        let session = &self.shared.session;
        // The first start gets the command line's values; a restart the session's values of the flags it registered.
        let flags = if restart == 0 {
            session.flags.clone()
        } else {
            let values = lock(&self.shared.flag_values);
            self.registrations()
                .flags
                .iter()
                .filter_map(|f| values.get(&f.name).map(|v| (f.name.clone(), v.clone())))
                .collect()
        };
        let has_ui = self.shared.has_ui();
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "hostInfo": { "name": "ultron", "version": env!("CARGO_PKG_VERSION") },
            "session": { "cwd": session.cwd, "mode": session.mode, "hasUI": has_ui },
            "flags": flags,
            "restart": restart,
            "capabilities": {
                "ui": { "status": true, "panels": true, "prompts": has_ui },
                "events": EVENTS.as_slice(),
            },
        });
        let handshake = peer
            .request("initialize", params, Some(manifest.startup_timeout), None)
            .map_err(|e| match e {
                RpcError::Closed(_) => {
                    let stderr = transport.stderr();
                    let stderr = stderr.trim();
                    if stderr.is_empty() {
                        "process exited during initialize".to_string()
                    } else {
                        format!("process exited during initialize: {stderr}")
                    }
                }
                other => format!("initialize failed: {other}"),
            })
            .and_then(|result| {
                let version = result
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if version != PROTOCOL_VERSION {
                    return Err(format!(
                        "unsupported protocol version \"{version}\" (host speaks {PROTOCOL_VERSION})"
                    ));
                }
                let mut info = result.get("extensionInfo").cloned().unwrap_or(Value::Null);
                if !info.get("name").is_some_and(Value::is_string) {
                    return Err("initialize result needs extensionInfo.name".into());
                }
                let registrations = Registrations::parse(result.get("capabilities").unwrap_or(&json!({})))?;
                if let Some(unknown) = registrations.events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
                    return Err(format!("unknown event \"{unknown}\""));
                }
                info["pid"] = json!(transport.pid());
                Ok((registrations, info))
            });
        match handshake {
            Ok((registrations, info)) => {
                let _ = peer.notify("notifications/initialized", json!({}));
                let host = self.shared.host.clone();
                let name = self.manifest.name.clone();
                peer.on_close(move |reason| host.exited(&name, reason));
                Ok((peer, registrations, info))
            }
            Err(error) => {
                peer.close("initialize failed");
                Err(error)
            }
        }
    }

    /// The running process, restarting a dead one while restarts remain.
    fn peer(&self) -> Result<Peer, RpcError> {
        if let Some(peer) = self.live_peer() {
            return Ok(peer);
        }
        let _starting = lock(&self.starting);
        if let Some(peer) = self.live_peer() {
            return Ok(peer);
        }
        let starts = {
            let state = lock(&self.state);
            if let Some(reason) = &state.disabled {
                return Err(RpcError::Closed(reason.clone()));
            }
            state.starts
        };
        let restarts = starts.saturating_sub(1);
        if starts > 0 && restarts >= self.manifest.max_restarts {
            let reason = format!(
                "Extension {} is disabled: it exited after {} restart(s)",
                self.manifest.name, restarts
            );
            lock(&self.state).disabled = Some(reason.clone());
            return Err(RpcError::Closed(reason));
        }
        lock(&self.state).starts += 1;
        match self.spawn(starts) {
            Ok((peer, registrations, info)) => {
                let mut state = lock(&self.state);
                state.peer = Some(peer.clone());
                state.registrations = registrations;
                state.info = info;
                Ok(peer)
            }
            Err(error) => {
                let reason = format!("Extension {} failed to start: {error}", self.manifest.name);
                self.shared.host.exited(&self.manifest.name, &reason);
                Err(RpcError::Closed(reason))
            }
        }
    }

    /// Calls a registered kernel-API function (`functions/call`).
    pub fn call_function(
        &self,
        name: &str,
        arguments: Value,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, RpcError> {
        self.peer()?.request(
            "functions/call",
            json!({ "name": name, "arguments": arguments }),
            timeout,
            cancel,
        )
    }

    /// Runs a registered slash command (`commands/run`); commands may prompt the user, so there is no timeout.
    pub fn run_command(&self, name: &str, args: &str, cancel: Option<&CancelToken>) -> Result<Value, RpcError> {
        self.peer()?
            .request("commands/run", json!({ "name": name, "args": args }), None, cancel)
    }

    /// Argument completions for a command (`commands/complete`); `null` when the extension has none.
    pub fn complete_command(&self, name: &str, prefix: &str, timeout: Duration) -> Result<Value, RpcError> {
        self.peer()?.request(
            "commands/complete",
            json!({ "name": name, "prefix": prefix }),
            Some(timeout),
            None,
        )
    }

    /// Runs a registered shortcut (`shortcuts/run`).
    pub fn run_shortcut(&self, key: &str) -> Result<Value, RpcError> {
        self.peer()?.request("shortcuts/run", json!({ "key": key }), None, None)
    }

    /// Asks the process to shut down, then stops it.
    pub fn shutdown(&self) {
        let peer = lock(&self.state).peer.take();
        if let Some(peer) = peer {
            if !peer.is_closed() {
                let _ = peer.request("shutdown", json!({}), Some(SHUTDOWN_TIMEOUT), None);
            }
            peer.close("extension host shut down");
        }
        lock(&self.state).disabled = Some(format!("Extension {} was shut down", self.manifest.name));
    }
}

impl Drop for ExtensionProcess {
    fn drop(&mut self) {
        if let Some(peer) = lock(&self.state).peer.take() {
            peer.close("extension host dropped");
        }
    }
}

impl ExtensionProcess {
    /// One `event` request; `cancel` gives it up (`notifications/cancelled`).
    fn deliver(&self, event: &Value, cancel: Option<&CancelToken>) -> Result<Option<Value>, String> {
        let kind = event["type"].as_str().unwrap_or_default();
        let peer = self.peer().map_err(|e| e.to_string())?;
        match peer.request("event", event.clone(), Some(self.manifest.event_timeout), cancel) {
            Ok(Value::Null) => Ok(None),
            Ok(value) => Ok(Some(value)),
            Err(RpcError::Timeout(ms)) => Err(format!(
                "Extension {} did not answer {kind} within {ms}ms",
                self.manifest.name
            )),
            Err(RpcError::Closed(reason)) => Err(format!("Extension {} exited: {reason}", self.manifest.name)),
            Err(other) => Err(other.to_string()),
        }
    }
}

impl Endpoint for ExtensionProcess {
    fn path(&self) -> &str {
        &self.display_path
    }

    /// A disabled extension is reported once (the dispatch that disabled it) and then skipped.
    fn subscribes(&self, event_type: &str) -> bool {
        let state = lock(&self.state);
        state.disabled.is_none() && state.registrations.events.iter().any(|e| e == event_type)
    }

    fn dispatch(&self, event: &Value) -> Result<Option<Value>, String> {
        self.deliver(event, None)
    }

    fn dispatch_cancellable(&self, event: &Value, cancel: &CancelToken) -> Result<Option<Value>, String> {
        self.deliver(event, Some(cancel))
    }

    fn commands(&self) -> Vec<(String, Option<String>)> {
        lock(&self.state).registrations.commands.clone()
    }
}

/// Every event type the host can deliver (`ExtensionEvent["type"]` in types.ts).
pub const EVENTS: [&str; 41] = [
    "project_trust",
    "resources_discover",
    "session_start",
    "session_info_changed",
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_compact",
    "session_compact_failed",
    "session_shutdown",
    "session_before_tree",
    "session_tree",
    "context",
    "context_with_system",
    "cache_warming_decision",
    "before_provider_request",
    "before_provider_headers",
    "after_provider_response",
    "before_agent_start",
    "agent_start",
    "agent_end",
    "agent_before_settle",
    "agent_settled",
    "ui_prompt_start",
    "ui_prompt_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "model_select",
    "thinking_level_select",
    "tool_call",
    "tool_result",
    "user_bash",
    "input",
    "before_file_write",
    "after_cell_changes",
];

/// All extension processes of one session, in load order, and the router over them.
pub struct ExtensionHost {
    processes: Vec<Arc<ExtensionProcess>>,
    router: Router,
    pub diagnostics: Vec<Diagnostic>,
    /// Problems with the command line's extension flags (`applyExtensionFlags`): unknown options, a string flag
    /// without a value.
    pub flag_diagnostics: Vec<String>,
    shared: Arc<Shared>,
}

/// `applyExtensionFlags` (agent-session-services.ts) over `SessionInfo.flags`, the command line's unknown options: a
/// registered boolean flag becomes `true`, a string flag takes its string value; anything else is reported.
fn apply_cli_flags(shared: &Shared, processes: &[Arc<ExtensionProcess>]) -> Vec<String> {
    let mut registered: Vec<(String, String)> = Vec::new();
    for process in processes {
        for flag in process.registrations().flags {
            match registered.iter_mut().find(|(n, _)| *n == flag.name) {
                Some(slot) => slot.1 = flag.kind,
                None => registered.push((flag.name, flag.kind)),
            }
        }
    }
    let mut diagnostics = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for (name, value) in &shared.session.flags {
        let Some((_, kind)) = registered.iter().find(|(n, _)| n == name) else {
            unknown.push(name.clone());
            continue;
        };
        let mut values = lock(&shared.flag_values);
        match (kind.as_str(), value) {
            ("boolean", _) => {
                values.insert(name.clone(), Value::Bool(true));
            }
            (_, Value::String(_)) => {
                values.insert(name.clone(), value.clone());
            }
            _ => diagnostics.push(format!("Extension flag \"--{name}\" requires a value")),
        }
    }
    if !unknown.is_empty() {
        diagnostics.push(format!(
            "Unknown option{}: {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown.iter().map(|n| format!("--{n}")).collect::<Vec<_>>().join(", ")
        ));
    }
    diagnostics
}

impl ExtensionHost {
    /// Starts every manifest (concurrently) and keeps those whose handshake succeeds, in manifest order. Failures
    /// become diagnostics, like `LoadExtensionsResult.errors`.
    pub fn load(manifests: Vec<Manifest>, session: SessionInfo, host: Arc<dyn HostApi>) -> ExtensionHost {
        let shared = Arc::new(Shared {
            host,
            session,
            processes: Mutex::new(Vec::new()),
            prompt: Mutex::new((0, None)),
            flag_values: Mutex::new(Map::new()),
        });
        let candidates: Vec<Arc<ExtensionProcess>> = manifests
            .into_iter()
            .map(|manifest| {
                let display_path = manifest.path.to_string_lossy().into_owned();
                Arc::new_cyclic(|me| ExtensionProcess {
                    manifest,
                    display_path,
                    shared: shared.clone(),
                    me: me.clone(),
                    state: Mutex::new(ProcState {
                        peer: None,
                        registrations: Registrations::default(),
                        info: Value::Null,
                        starts: 0,
                        disabled: None,
                    }),
                    starting: Mutex::new(()),
                })
            })
            .collect();
        let started: Vec<Result<(), String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = candidates
                .iter()
                .map(|p| scope.spawn(move || p.peer().map(drop).map_err(|e| e.to_string())))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("start panicked".into())))
                .collect()
        });
        let mut processes = Vec::new();
        let mut diagnostics = Vec::new();
        for (process, outcome) in candidates.into_iter().zip(started) {
            match outcome {
                Ok(()) => processes.push(process),
                Err(message) => diagnostics.push(Diagnostic {
                    path: process.manifest.path.clone(),
                    message,
                }),
            }
        }
        *lock(&shared.processes) = processes.iter().map(Arc::downgrade).collect();
        // Defaults commit in load order (the first registration of a name wins), then the command line's values.
        for process in &processes {
            shared.register_defaults(&process.registrations().flags);
        }
        let flag_diagnostics = apply_cli_flags(&shared, &processes);
        let endpoints: Vec<Arc<dyn Endpoint>> = processes.iter().map(|p| p.clone() as Arc<dyn Endpoint>).collect();
        ExtensionHost {
            router: Router::new(endpoints),
            processes,
            diagnostics,
            flag_diagnostics,
            shared,
        }
    }

    /// `getFlags`: every registered flag, the first registration of a name winning, with its extension's path.
    pub fn flags(&self) -> Vec<(FlagSpec, String)> {
        let mut out: Vec<(FlagSpec, String)> = Vec::new();
        for process in &self.processes {
            for flag in process.registrations().flags {
                if !out.iter().any(|(f, _)| f.name == flag.name) {
                    out.push((flag, process.display_path.clone()));
                }
            }
        }
        out
    }

    /// `getFlagValues`: the session's flag values.
    pub fn flag_values(&self) -> Map<String, Value> {
        lock(&self.shared.flag_values).clone()
    }

    /// `setFlagValue`.
    pub fn set_flag_value(&self, name: &str, value: Value) {
        lock(&self.shared.flag_values).insert(name.to_string(), value);
    }

    /// Providers the extensions declared (`registerProvider` without code) with the extension's path, every
    /// declaration in load order: the model registry applies them in turn, a later declaration of a name merging over
    /// an earlier one as `registerProvider` does.
    pub fn providers(&self) -> Vec<(ProviderSpec, String)> {
        self.processes
            .iter()
            .flat_map(|process| {
                process
                    .registrations()
                    .providers
                    .into_iter()
                    .map(|provider| (provider, process.display_path.clone()))
            })
            .collect()
    }

    pub fn router(&self) -> &Router {
        &self.router
    }

    /// A host message on the extension event bus (`pi.events.emit(channel, data)` from the host side, e.g.
    /// `ultron:context_edit`): every extension subscribed to `channel` gets `bus/message`.
    pub fn publish(&self, channel: &str, data: &Value) {
        self.shared.broadcast(channel, data);
    }

    pub fn processes(&self) -> &[Arc<ExtensionProcess>] {
        &self.processes
    }

    pub fn process(&self, name: &str) -> Option<&Arc<ExtensionProcess>> {
        self.processes.iter().find(|p| p.name() == name)
    }

    /// Kernel-API functions as `(extension, function)`, reached from Python as `ext.<extension>.<function>(...)`.
    pub fn functions(&self) -> Vec<(String, FunctionSpec)> {
        self.processes
            .iter()
            .flat_map(|p| {
                p.registrations()
                    .functions
                    .into_iter()
                    .map(|f| (p.name().to_string(), f))
            })
            .collect()
    }

    pub fn call_function(
        &self,
        extension: &str,
        name: &str,
        arguments: Value,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, RpcError> {
        let process = self
            .process(extension)
            .ok_or_else(|| RpcError::Other(format!("No extension named {extension}")))?;
        if !process.registrations().functions.iter().any(|f| f.name == name) {
            return Err(RpcError::Other(format!("Extension {extension} has no function {name}")));
        }
        process.call_function(name, arguments, timeout, cancel)
    }

    /// `getShortcuts`: the extension shortcuts the TUI binds given its resolved keybindings (action, keys), and the
    /// conflict warnings ([`crate::shortcuts::resolve`]).
    pub fn shortcuts(&self, keybindings: &[(String, Vec<String>)]) -> crate::shortcuts::Shortcuts {
        let extensions: Vec<crate::shortcuts::Registered> = self
            .processes
            .iter()
            .map(|p| (p.display_path.clone(), p.registrations().shortcuts))
            .collect();
        crate::shortcuts::resolve(&extensions, keybindings)
    }

    /// Runs the shortcut bound to `key` (`shortcuts/run` to the extension that owns it under `keybindings`).
    pub fn run_shortcut(&self, key: &str, keybindings: &[(String, Vec<String>)]) -> Result<Value, RpcError> {
        let resolved = self.shortcuts(keybindings);
        let shortcut = resolved
            .get(key)
            .ok_or_else(|| RpcError::Other(format!("No extension shortcut {key}")))?;
        let process = self
            .processes
            .iter()
            .find(|p| p.display_path == shortcut.extension_path)
            .ok_or_else(|| RpcError::Other(format!("No extension shortcut {key}")))?;
        process.run_shortcut(&shortcut.key)
    }

    /// Runs `/invocation_name args` (names from `Router::registered_commands`).
    pub fn run_command(
        &self,
        invocation_name: &str,
        args: &str,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, RpcError> {
        let command = self
            .router
            .registered_commands()
            .into_iter()
            .find(|c| c.invocation_name == invocation_name)
            .ok_or_else(|| RpcError::Other(format!("Unknown command /{invocation_name}")))?;
        let process = self
            .processes
            .iter()
            .find(|p| p.display_path == command.extension_path)
            .ok_or_else(|| RpcError::Other(format!("Unknown command /{invocation_name}")))?;
        process.run_command(&command.name, args, cancel)
    }

    /// Shuts every process down (`session_shutdown` should be emitted first).
    pub fn shutdown(&self) {
        std::thread::scope(|scope| {
            for process in &self.processes {
                scope.spawn(move || process.shutdown());
            }
        });
    }
}
