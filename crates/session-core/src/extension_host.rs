//! The session's side of the extension protocol: what an extension process may ask the host (`session/*`, `host/*`,
//! `agent/*`, `model/*`, `thinking/*`, `tools/list`, `commands/list`, `ui/*` dialogs) and where its declarative UI
//! goes (crates/extensions/PROTOCOL.md section 4). TypeScript serves the same requests through the worker's
//! `LegacyExtensionAdapter.bind` (legacy-extension-adapter.ts) and its UI through `ExtensionUIBridge.createContext`.
//!
//! The extension processes start before the session actor exists, so the actor, the session and the stack are bound
//! once they are open. Requests never wait for the actor where TypeScript does not wait either: reads come from the
//! session file and a snapshot of the actor's facts, and TypeScript's fire-and-forget actions (`void lane.abort()`,
//! `void lane.followUp(...)`, ...) are queued to the actor and answered at once. So an extension handler that the
//! actor waits for (`before_agent_start`, `context`) can still use them. Only `model/set`, `session/navigateTree`
//! and `agent/waitForIdle` wait for the actor, as TypeScript awaits them.
//!
//! `ctx.sessionManager` is crate::extension_session, `ctx.modelRegistry` and `ctx.scopedModels` are
//! crate::extension_registry. The active tool list (`getActiveTools`/`setActiveTools`) is the adapter's: the REPL
//! stays pinned and the extension functions (REPL tools) are filtered out, so the model's tool list is `[rlm]`
//! whatever an extension activates.

use crate::compose::{Stack, SystemPrompt};
use crate::core::{Input, Op, OpResult, QueueKind};
use crate::durable_store::{SharedSession, lock as lock_session};
use crate::{ModelFlags, RunEnv};
use extensions::rpc::{CancelToken, RpcError, method_not_found};
use extensions::ui::{NoticeLevel, Placement, UiUpdate, no_ui_answer};
use extensions::{ExtensionHost, HostApi};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

/// The client UI extensions reach (the worker's extension UI bridge, `ExtensionUIBridge`): dialogs and
/// notifications in Pi's RPC wire form (`extension_ui_request` without `type` and `id`).
pub trait ExtensionUi: Send + Sync {
    /// A client polls (or the one the worker was started for has not arrived yet): the session has a UI.
    fn serving(&self) -> bool;
    /// A dialog; the client's answer (`{value}`, `{confirmed}`), `None` when cancelled, timed out or unserved.
    fn dialog(
        &self,
        request: Map<String, Value>,
        timeout: Option<Duration>,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Value>;
    /// A notification (dropped when nobody serves).
    fn emit(&self, request: Map<String, Value>);
}

/// What the actor does for an extension (core::extension_ops).
pub enum Call {
    /// An operation whose answer nobody waits for (TypeScript's `void lane.x()`).
    Fire(Op),
    /// An operation answered on the channel.
    Await(Op, Sender<OpResult>),
    /// `appendEntry(customType, data)`: a custom entry at the tip (after the run in progress).
    AppendCustom { custom_type: String, data: Value },
    /// `waitForIdle`: answered once no run is in progress.
    WaitIdle(Sender<()>),
}

/// The actor's facts extensions read without asking it (`ctx.model`, `ctx.thinkingLevel`, `ctx.isIdle()`,
/// `ctx.hasPendingMessages()`), refreshed whenever the actor reports progress.
#[derive(Clone, Debug, PartialEq)]
pub struct Facts {
    pub model: Value,
    pub thinking: String,
    pub idle: bool,
    pub pending: bool,
}

impl Default for Facts {
    fn default() -> Self {
        Facts {
            model: Value::Null,
            thinking: "medium".into(),
            idle: true,
            pending: false,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What `host/shutdown` does (`ctx.shutdown()`); the TypeScript worker binds none, so by default nothing.
pub type ShutdownAction = Arc<dyn Fn() + Send + Sync>;

/// The session's [`HostApi`].
pub struct SessionHost {
    cwd: PathBuf,
    agent_dir: PathBuf,
    home: PathBuf,
    mode: String,
    project_trusted: bool,
    ui: Option<Arc<dyn ExtensionUi>>,
    on_shutdown: Option<ShutdownAction>,
    actor: Mutex<Option<Sender<Input>>>,
    session: OnceLock<SharedSession>,
    stack: OnceLock<Weak<Stack>>,
    extensions: OnceLock<Weak<ExtensionHost>>,
    system: OnceLock<SystemPrompt>,
    facts: Mutex<Facts>,
    /// The `session_shutdown` this session will report: `{reason: "quit"}`, or `{reason: "fork", targetSessionFile}`
    /// after a client moved on to a fork (until the session is used again).
    pub(crate) shutdown: Mutex<Value>,
    /// `before_agent_start` messages of the run in progress: (operation id, user messages).
    pub(crate) injected: Mutex<Option<(String, Vec<Value>)>>,
    /// The session's environment and model flags (the model registry extensions read), once the session opened.
    models: OnceLock<(RunEnv, ModelFlags)>,
    /// `getActiveTools`: the REPL, plus whatever an extension activated that is not one of its REPL tools.
    active_tools: Mutex<Vec<String>>,
}

/// The tool no extension can deactivate: without the REPL the model has nothing to work with.
const PINNED_TOOLS: [&str; 1] = ["rlm"];

/// The worker's headless `setTheme` (legacy-extension-adapter.ts `createHeadlessExtensionUI`).
pub const SET_THEME_ERROR: &str = "Native worker UI adapter does not switch themes";

impl SessionHost {
    pub fn new(
        cwd: PathBuf,
        agent_dir: PathBuf,
        home: PathBuf,
        mode: &str,
        project_trusted: bool,
        ui: Option<Arc<dyn ExtensionUi>>,
        on_shutdown: Option<ShutdownAction>,
    ) -> Self {
        SessionHost {
            cwd,
            agent_dir,
            home,
            mode: mode.to_string(),
            project_trusted,
            ui,
            on_shutdown,
            actor: Mutex::new(None),
            session: OnceLock::new(),
            stack: OnceLock::new(),
            extensions: OnceLock::new(),
            system: OnceLock::new(),
            facts: Mutex::new(Facts::default()),
            shutdown: Mutex::new(json!({"reason": "quit"})),
            injected: Mutex::new(None),
            models: OnceLock::new(),
            active_tools: Mutex::new(PINNED_TOOLS.iter().map(|t| t.to_string()).collect()),
        }
    }

    /// The session's environment and model flags, for `models/*` and `scopedModels`.
    pub(crate) fn bind_models(&self, env: RunEnv, flags: ModelFlags) {
        let _ = self.models.set((env, flags));
    }

    /// `getActiveTools`.
    pub fn active_tools(&self) -> Vec<String> {
        lock(&self.active_tools).clone()
    }

    /// `setActiveTools(names)` as the worker filters it: the session's REPL tools (extension functions) stay out
    /// of the model's tool list, and the REPL stays in front.
    pub fn set_active_tools(&self, names: &[String]) {
        let registered: Vec<String> = self
            .extensions
            .get()
            .and_then(Weak::upgrade)
            .map(|host| host.functions().into_iter().map(|(_, f)| f.name).collect())
            .unwrap_or_default();
        let next: Vec<String> = names.iter().filter(|n| !registered.contains(n)).cloned().collect();
        let mut active: Vec<String> = PINNED_TOOLS
            .iter()
            .map(|t| t.to_string())
            .filter(|t| !next.contains(t))
            .collect();
        active.extend(next);
        *lock(&self.active_tools) = active;
    }

    /// Whether the session can show extension UI at all (Pi's `hasUI` modes: the TUI and RPC with a bridge).
    pub fn ui_capable(&self) -> bool {
        self.ui.is_some() && matches!(self.mode.as_str(), "interactive" | "tui" | "rpc")
    }

    pub(crate) fn bind_extensions(&self, host: &Arc<ExtensionHost>, system: SystemPrompt) {
        let _ = self.extensions.set(Arc::downgrade(host));
        let _ = self.system.set(system);
    }

    pub(crate) fn bind_actor(&self, actor: Sender<Input>, session: SharedSession, stack: &Arc<Stack>) {
        *lock(&self.actor) = Some(actor);
        let _ = self.session.set(session);
        let _ = self.stack.set(Arc::downgrade(stack));
    }

    /// The actor stopped: later requests that need it fail at once.
    pub(crate) fn unbind_actor(&self) {
        *lock(&self.actor) = None;
    }

    pub(crate) fn set_facts(&self, facts: Facts) {
        let mut current = lock(&self.facts);
        if *current != facts {
            *current = facts;
        }
    }

    pub fn facts(&self) -> Facts {
        lock(&self.facts).clone()
    }

    fn send(&self, call: Call) -> Result<(), RpcError> {
        lock(&self.actor)
            .as_ref()
            .ok_or_else(|| RpcError::Other("The session is not open".into()))?
            .send(Input::Extension(call))
            .map_err(|_| RpcError::Other("The session is closed".into()))
    }

    fn fire(&self, op: Op) -> Result<Value, RpcError> {
        self.send(Call::Fire(op))?;
        Ok(Value::Null)
    }

    fn await_op(&self, op: Op, cancel: &CancelToken) -> Result<OpResult, RpcError> {
        let (tx, rx) = channel();
        self.send(Call::Await(op, tx))?;
        wait(&rx, cancel)
    }

    fn session(&self) -> Result<&SharedSession, RpcError> {
        self.session
            .get()
            .ok_or_else(|| RpcError::Other("The session is not open".into()))
    }

    /// `host/context` (`ExtensionContext` facts).
    fn context(&self) -> Value {
        let facts = self.facts();
        json!({
            "cwd": self.cwd.display().to_string(),
            "mode": self.mode,
            "hasUI": self.has_ui() && self.ui_capable(),
            "model": facts.model,
            "thinkingLevel": facts.thinking,
            "isIdle": facts.idle,
            "projectTrusted": self.project_trusted,
            "hasPendingMessages": facts.pending,
            // The worker reports no context usage (`getContextUsage: () => undefined`).
            "contextUsage": null,
            "systemPrompt": self.system.get().map(SystemPrompt::current),
            "scopedModels": self
                .models
                .get()
                .map_or_else(|| json!([]), |(env, flags)| crate::extension_registry::scoped_models(env, flags)),
            // The command context's `getSystemPromptOptions` (the worker's `{ cwd }`).
            "systemPromptOptions": {"cwd": self.cwd.display().to_string()},
        })
    }

    /// `getAllTools`: the extension functions with their Pi `SourceInfo`.
    fn tools(&self) -> Value {
        let Some(host) = self.extensions.get().and_then(Weak::upgrade) else {
            return json!([]);
        };
        let settings = config::settings::SettingsManager::create(&self.cwd, &self.agent_dir, self.project_trusted);
        let paths: Vec<(String, PathBuf)> = host
            .processes()
            .iter()
            .map(|p| (p.name().to_string(), p.manifest().path.clone()))
            .collect();
        Value::Array(
            host.functions()
                .into_iter()
                .map(|(extension, f)| {
                    let mut tool = json!({"name": f.name, "description": f.description, "parameters": f.parameters});
                    if let Some(guidelines) = &f.prompt_guidelines {
                        tool["promptGuidelines"] = json!(guidelines);
                    }
                    if let Some((_, path)) = paths.iter().find(|(name, _)| *name == extension)
                        && let Some(info) = crate::extension_commands::source_info(
                            path,
                            &settings,
                            &self.cwd,
                            &self.agent_dir,
                            &self.home,
                        )
                    {
                        tool["sourceInfo"] = info;
                    }
                    tool
                })
                .collect(),
        )
    }

    fn dialog(&self, method: &str, params: &Value, cancel: &CancelToken) -> Value {
        let default = no_ui_answer(method).unwrap_or(Value::Null);
        let Some(ui) = &self.ui else { return default };
        let text = |key: &str| params.get(key).cloned().unwrap_or(Value::Null);
        let timeout = params
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
        let (kind, fields): (&str, Vec<(&str, Value)>) = match method {
            "ui/select" => (
                "select",
                vec![
                    ("title", text("title")),
                    ("options", text("options")),
                    ("timeout", json!(timeout.map(|t| t.as_millis() as u64))),
                ],
            ),
            "ui/confirm" => (
                "confirm",
                vec![
                    ("title", text("title")),
                    ("message", text("message")),
                    ("timeout", json!(timeout.map(|t| t.as_millis() as u64))),
                ],
            ),
            "ui/input" => (
                "input",
                vec![
                    ("title", text("title")),
                    ("placeholder", text("placeholder")),
                    ("timeout", json!(timeout.map(|t| t.as_millis() as u64))),
                ],
            ),
            // Pi's RPC editor dialog has no timeout.
            _ => ("editor", vec![("title", text("title")), ("prefill", text("prefill"))]),
        };
        let mut request = Map::new();
        request.insert("method".into(), json!(kind));
        // Undefined fields are left out, as in Pi's JSONL output.
        for (key, value) in fields.into_iter().filter(|(_, v)| !v.is_null()) {
            request.insert(key.into(), value);
        }
        let timeout = if kind == "editor" { None } else { timeout };
        let Some(answer) = ui.dialog(request, timeout, &|| cancel.is_cancelled()) else {
            return default;
        };
        match kind {
            "confirm" => answer
                .get("confirmed")
                .cloned()
                .filter(Value::is_boolean)
                .unwrap_or(json!(false)),
            _ => answer
                .get("value")
                .cloned()
                .filter(Value::is_string)
                .unwrap_or(Value::Null),
        }
    }
}

fn wait<T>(rx: &std::sync::mpsc::Receiver<T>, cancel: &CancelToken) -> Result<T, RpcError> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => return Ok(value),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RpcError::Other("The session is closed".into()));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(reason) = cancel.reason() {
                    return Err(RpcError::Cancelled(reason));
                }
            }
        }
    }
}

/// `contentToText` (legacy-extension-adapter.ts): a string, or the text parts of a content list joined by newlines.
pub fn content_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map.get("content").map(content_to_text).unwrap_or_default(),
        _ => String::new(),
    }
}

fn field<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::remote(-32602, format!("\"{key}\" must be a string")))
}

impl HostApi for SessionHost {
    fn request(&self, _extension: &str, method: &str, params: &Value, cancel: &CancelToken) -> Result<Value, RpcError> {
        match method {
            "ui/select" | "ui/confirm" | "ui/input" | "ui/editor" => Ok(self.dialog(method, params, cancel)),
            // The worker's headless fallback answers the rest of the UI context.
            "ui/getEditorText" | "ui/getToolsExpanded" | "ui/getAllThemes" => {
                Ok(no_ui_answer(method).unwrap_or_default())
            }
            "ui/setTheme" => Ok(json!({"success": false, "error": SET_THEME_ERROR})),
            "tools/active" => Ok(json!(self.active_tools())),
            "tools/setActive" => {
                let names: Vec<String> = params
                    .get("names")
                    .and_then(Value::as_array)
                    .and_then(|names| names.iter().map(|n| n.as_str().map(str::to_string)).collect())
                    .ok_or_else(|| RpcError::remote(-32602, "\"names\" must be an array of strings"))?;
                self.set_active_tools(&names);
                Ok(Value::Null)
            }
            _ if method.starts_with("models/") => {
                let (env, flags) = self
                    .models
                    .get()
                    .ok_or_else(|| RpcError::Other("The session is not open".into()))?;
                match crate::extension_registry::request(env, flags, method, params) {
                    Some(answer) => answer.map_err(|e| RpcError::remote(-32602, e)),
                    None => Err(method_not_found(method)),
                }
            }
            "host/context" => Ok(self.context()),
            "host/exec" => extensions::exec::exec(params, &self.cwd, cancel),
            // The worker binds no shutdown handler (`this.#onShutdown?.()`); an embedder may.
            "host/shutdown" => {
                if let Some(action) = &self.on_shutdown {
                    action();
                }
                Ok(Value::Null)
            }
            "thinking/get" => Ok(json!(self.facts().thinking)),
            "tools/list" => Ok(self.tools()),
            "commands/list" => Ok(self
                .stack
                .get()
                .and_then(Weak::upgrade)
                .map_or_else(|| json!([]), |stack| Value::Array(stack.commands()))),
            "session/getName"
            | "session/entries"
            | "session/branch"
            | "session/leaf"
            | "session/leafEntry"
            | "session/entry"
            | "session/label"
            | "session/tree"
            | "session/contextEntries"
            | "session/file"
            | "session/id"
            | "session/dir"
            | "session/cwd"
            | "session/header" => {
                let session = lock_session(self.session()?);
                match crate::extension_session::request(&session, method, params) {
                    Some(answer) => answer.map_err(|e| RpcError::remote(-32602, e)),
                    None => Err(method_not_found(method)),
                }
            }
            // `sendMessage: () => {}` in the worker.
            "session/sendMessage" => Ok(Value::Null),
            "session/sendUserMessage" => {
                let text = content_to_text(params.get("content").unwrap_or(&Value::Null));
                let queue = match params.get("deliverAs").and_then(Value::as_str) {
                    Some("steer") => QueueKind::Steer,
                    _ => QueueKind::FollowUp,
                };
                self.fire(Op::Prompt {
                    message: text,
                    images: Vec::new(),
                    queue: Some(queue),
                })
            }
            "session/appendEntry" => {
                let custom_type = field(params, "customType")?.to_string();
                let data = params.get("data").cloned().unwrap_or(Value::Null);
                self.send(Call::AppendCustom { custom_type, data })?;
                Ok(Value::Null)
            }
            "session/setName" => self.fire(Op::SetName {
                name: field(params, "name")?.to_string(),
            }),
            "session/setLabel" => self.fire(Op::SetLabel {
                entry_id: field(params, "entryId")?.to_string(),
                label: params.get("label").and_then(Value::as_str).map(str::to_string),
            }),
            "session/navigateTree" => {
                let options = params.get("options").cloned().unwrap_or(Value::Null);
                let text = |key: &str| options.get(key).and_then(Value::as_str).map(str::to_string);
                let op = Op::Navigate {
                    target_id: params.get("targetId").and_then(Value::as_str).map(str::to_string),
                    summarize: options["summarize"] == true,
                    instructions: text("customInstructions"),
                    label: text("label"),
                };
                match self.await_op(op, cancel)? {
                    Ok(_) => Ok(json!({"cancelled": false})),
                    Err(error) => Err(RpcError::Other(error)),
                }
            }
            // Sessions belong to the server: the worker's command context answers these without acting.
            "session/new" | "session/fork" | "session/switch" => Ok(json!({"cancelled": false})),
            "session/reload" => Ok(Value::Null),
            "agent/abort" => self.fire(Op::Abort),
            "agent/compact" => self.fire(Op::Compact {
                instructions: params
                    .get("customInstructions")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            "agent/waitForIdle" => {
                let (tx, rx) = channel();
                self.send(Call::WaitIdle(tx))?;
                wait(&rx, cancel).map(|()| Value::Null)
            }
            "thinking/set" => self.fire(Op::SetThinking {
                level: field(params, "level")?.to_string(),
            }),
            "model/set" => {
                let op = Op::SetModel {
                    provider: field(params, "provider")?.to_string(),
                    model_id: field(params, "id")?.to_string(),
                };
                Ok(json!(self.await_op(op, cancel)?.is_ok()))
            }
            other => Err(method_not_found(other)),
        }
    }

    /// `ExtensionUIBridge.createContext`: notifications, status, widgets, title and editor text reach the client;
    /// the rest of the UI context is the worker's headless one.
    fn ui(&self, _extension: &str, update: UiUpdate) {
        let Some(ui) = &self.ui else { return };
        let request: Vec<(&str, Value)> = match update {
            UiUpdate::Notify { message, level } => vec![
                ("method", json!("notify")),
                ("message", json!(message)),
                (
                    "notifyType",
                    json!(match level {
                        NoticeLevel::Info => "info",
                        NoticeLevel::Warning => "warning",
                        NoticeLevel::Error => "error",
                    }),
                ),
            ],
            UiUpdate::Status { key, text } => vec![
                ("method", json!("setStatus")),
                ("statusKey", json!(key)),
                ("statusText", json!(text)),
            ],
            UiUpdate::Panel { key, lines, placement } => vec![
                ("method", json!("setWidget")),
                ("widgetKey", json!(key)),
                ("widgetLines", json!(lines)),
                (
                    "widgetPlacement",
                    json!(match placement {
                        Placement::AboveEditor => "aboveEditor",
                        Placement::BelowEditor => "belowEditor",
                    }),
                ),
            ],
            UiUpdate::Title(title) => vec![("method", json!("setTitle")), ("title", json!(title))],
            UiUpdate::EditorText(text) | UiUpdate::PasteToEditor(text) => {
                vec![("method", json!("set_editor_text")), ("text", json!(text))]
            }
            _ => return,
        };
        // Undefined fields are left out, as in Pi's JSONL output.
        let request: Map<String, Value> = request
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        ui.emit(request);
    }

    /// The worker runner's `hasUI`: an interactive client serves the extension UI.
    fn has_ui(&self) -> bool {
        self.ui.as_ref().is_some_and(|ui| ui.serving())
    }
}
