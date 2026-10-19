//! `ultron acp`: an Agent Client Protocol (v1, <https://agentclientprotocol.com>) agent on stdio, for editors that
//! speak ACP (Zed, CodeCompanion.nvim, avante.nvim). JSON-RPC 2.0, one message per line; stdout carries only those
//! lines, everything else goes to stderr.
//!
//! It is a front end of the same session path as `--mode rpc` ([`crate::rpc::remote`]): each working directory a
//! client opens sessions in gets one [`Remote`] (this process's server generation, attached to a session worker),
//! every ACP request is an [`RpcSession`] operation, and the RPC event stream it writes is translated into
//! `session/update`s ([`translate`]). The model still has one tool, `rlm`; each cell is reported as an ACP tool call.
//!
//! - `initialize`: protocol version 1; `loadSession` (not with `--no-session`), image and embedded-context prompts,
//!   `session/list`; no auth methods (Ultron's own credentials apply). The client's `fs`/`terminal` capabilities are
//!   not used: the kernel does its own file and process IO.
//! - `session/new`, `session/load` (replays the stored conversation as `session/update`s first), `session/list`,
//!   `session/prompt` (answered with its `stopReason` when the run ends), `session/cancel`,
//!   `session/set_config_option` (model, thinking level). MCP servers a client passes are not used (the session's MCP
//!   gateway reads `mcp.json`); a note goes to stderr.
//! - No `session/request_permission`: there is no per-tool approval, and the worker's dialogs (`/mcp`'s, which a
//!   prompt does not raise) are answered as dismissed; its notices go to stderr.

pub mod changes;
pub mod translate;

use crate::Env;
use crate::args::Args;
use crate::print_mode::Target;
use crate::rpc::remote::{Event, Remote};
use crate::rpc::{Output, RpcSession};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use translate::{Action, TurnEnd};

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC error. A missing session is an internal error whose `data.details` reads `Session not found: <id>`:
/// avante.nvim recovers (opens a new session) on exactly that.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn session_not_found(id: &str) -> Self {
        let message = format!("Session not found: {id}");
        RpcError {
            code: INTERNAL_ERROR,
            data: Some(json!({"details": message})),
            message,
        }
    }

    fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        error
    }
}

/// What the loop reacts to.
pub enum Input {
    Line(String),
    Eof,
    /// An event of the session front end `slot`.
    Remote(usize, Event),
    Signal(i32),
}

/// A prompt turn in progress.
struct Turn {
    request: Value,
    session_id: String,
    cancelled: bool,
}

/// The sessions of one working directory: one front end, attached to one of them at a time.
struct Slot {
    cwd: PathBuf,
    remote: Remote<'static>,
    capture: Arc<Mutex<Vec<u8>>>,
    out: Output,
    translator: translate::Translator,
    tracker: changes::Tracker,
    attached: String,
    turn: Option<Turn>,
}

/// A byte sink the front end writes its RPC event lines to.
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Server {
    parsed: &'static Args,
    env: Env,
    envs: HashMap<PathBuf, &'static Env>,
    out: Output,
    tx: Sender<Input>,
    slots: Vec<Slot>,
    /// Session id -> its slot.
    sessions: HashMap<String, usize>,
}

type Reply = Result<Value, RpcError>;

fn params_str<'v>(params: &'v Value, key: &str) -> Result<&'v str, RpcError> {
    params[key]
        .as_str()
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, format!("Invalid params: {key} must be a string")))
}

impl Server {
    pub fn new(parsed: Args, env: Env, out: Output, tx: Sender<Input>) -> Self {
        Server {
            parsed: Box::leak(Box::new(parsed)),
            env,
            envs: HashMap::new(),
            out,
            tx,
            slots: Vec::new(),
            sessions: HashMap::new(),
        }
    }

    fn send(&self, message: Value) {
        self.out.emit(&message);
    }

    fn respond(&self, id: &Value, reply: Reply) {
        self.send(match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error.to_json()}),
        });
    }

    fn notify(&self, session_id: &str, update: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": update}}));
    }

    /// One line from the client.
    pub fn handle_line(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                self.respond(
                    &Value::Null,
                    Err(RpcError::new(PARSE_ERROR, format!("Parse error: {error}"))),
                );
                return;
            }
        };
        let Some(method) = message["method"].as_str() else {
            // This agent sends no requests, so a response answers nothing.
            if message.get("id").is_none() {
                self.respond(&Value::Null, Err(RpcError::new(INVALID_REQUEST, "Invalid request")));
            }
            return;
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
            self.notification(method, &params);
            return;
        };
        match method {
            "session/prompt" => {
                if let Err(error) = self.prompt(&id, &params) {
                    self.respond(&id, Err(error));
                }
            }
            "session/load" => {
                let reply = self.load(&params);
                self.respond(&id, reply);
            }
            _ => {
                let reply = self.request(method, &params);
                self.respond(&id, reply);
            }
        }
    }

    fn notification(&mut self, method: &str, params: &Value) {
        if method == "session/cancel"
            && let Some(id) = params["sessionId"].as_str()
        {
            self.cancel(id);
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Reply {
        match method {
            "initialize" => {
                if !params["protocolVersion"].is_u64() {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        "Invalid params: protocolVersion must be a number",
                    ));
                }
                Ok(json!({
                    "protocolVersion": translate::PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": !self.parsed.no_session,
                        "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                        "mcpCapabilities": {"http": false, "sse": false},
                        "sessionCapabilities": {"list": {}},
                    },
                    "authMethods": [],
                    "agentInfo": {"name": "ultron", "title": "Ultron", "version": crate::version()},
                }))
            }
            "authenticate" => Ok(json!({})),
            "session/new" => self.new_session(params),
            "session/list" => self.list(params),
            "session/set_config_option" => self.set_config_option(params),
            other => Err(RpcError::new(METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        }
    }

    /// The environment of sessions in `cwd`.
    fn env_for(&mut self, cwd: &Path) -> &'static Env {
        if let Some(env) = self.envs.get(cwd) {
            return env;
        }
        let mut env = self.env.clone();
        env.cwd = cwd.to_path_buf();
        let env: &'static Env = Box::leak(Box::new(env));
        self.envs.insert(cwd.to_path_buf(), env);
        env
    }

    /// The session's `cwd` param: an existing absolute directory.
    fn cwd(params: &Value) -> Result<PathBuf, RpcError> {
        let cwd = PathBuf::from(params_str(params, "cwd")?);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!(
                    "Invalid params: cwd must be an existing absolute directory: {}",
                    cwd.display()
                ),
            ));
        }
        Ok(config::paths::normalize(&cwd))
    }

    fn note_mcp_servers(params: &Value) {
        let count = params["mcpServers"].as_array().map_or(0, Vec::len);
        if count > 0 {
            eprintln!(
                "ultron acp: {count} MCP server(s) from the client are not used; add them to Ultron's mcp.json (the session's MCP gateway reads it)"
            );
        }
    }

    /// Open `target` in `cwd`: on that directory's front end (switched to it) or a new one. Returns the slot and the
    /// session id.
    fn open(&mut self, cwd: &Path, target: Target) -> Result<(usize, String), RpcError> {
        let internal = |e: String| RpcError::new(INTERNAL_ERROR, e);
        if let Some(index) = self.slots.iter().position(|s| s.cwd == cwd) {
            let slot = &mut self.slots[index];
            if slot.turn.is_some() {
                return Err(RpcError::new(
                    INVALID_REQUEST,
                    "A prompt is running in another session of this directory; cancel it or wait for its end",
                ));
            }
            match &target {
                Target::Open(metadata) => slot.remote.switch_session(&metadata.id).map_err(internal)?,
                _ => slot.remote.new_session().map_err(internal)?,
            }
            let id = slot.remote.state()["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            slot.attached = id.clone();
            return Ok((index, id));
        }
        let env = self.env_for(cwd);
        let index = self.slots.len();
        let (remote_tx, remote_rx) = channel();
        let forward = self.tx.clone();
        std::thread::spawn(move || {
            for input in remote_rx {
                if let crate::rpc::Input::Event(event) = input
                    && forward.send(Input::Remote(index, event)).is_err()
                {
                    return;
                }
            }
        });
        let remote = Remote::open(self.parsed, env, target, remote_tx).map_err(internal)?;
        let id = remote.state()["sessionId"].as_str().unwrap_or_default().to_string();
        let capture = Arc::new(Mutex::new(Vec::new()));
        self.slots.push(Slot {
            cwd: cwd.to_path_buf(),
            remote,
            out: Output::new(Box::new(Capture(capture.clone()))),
            capture,
            translator: translate::Translator::default(),
            tracker: changes::Tracker::default(),
            attached: id.clone(),
            turn: None,
        });
        Ok((index, id))
    }

    fn config_options(&self, index: usize) -> Value {
        let remote = &self.slots[index].remote;
        let state = remote.state();
        let model = &state["model"];
        let current = format!(
            "{}/{}",
            model["provider"].as_str().unwrap_or_default(),
            model["id"].as_str().unwrap_or_default()
        );
        let mut models: Vec<Value> = remote
            .available_models()
            .iter()
            .map(|m| {
                let value = format!(
                    "{}/{}",
                    m["provider"].as_str().unwrap_or_default(),
                    m["id"].as_str().unwrap_or_default()
                );
                json!({"value": value, "name": m["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&value)})
            })
            .collect();
        if !models.iter().any(|m| m["value"] == current.as_str()) {
            models.insert(
                0,
                json!({"value": current, "name": model["name"].as_str().unwrap_or(&current)}),
            );
        }
        let levels: Vec<Value> = remote
            .thinking_levels()
            .iter()
            .map(|level| json!({"value": level, "name": level}))
            .collect();
        let mut options = vec![
            json!({"id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": current, "options": models}),
        ];
        if let Some(level) = state["thinkingLevel"].as_str()
            && !levels.is_empty()
        {
            options.push(
                json!({"id": "thought_level", "name": "Thinking", "category": "thought_level",
                "type": "select", "currentValue": level, "options": levels}),
            );
        }
        Value::Array(options)
    }

    fn new_session(&mut self, params: &Value) -> Reply {
        let cwd = Self::cwd(params)?;
        Self::note_mcp_servers(params);
        let target = if self.parsed.no_session {
            Target::Ephemeral
        } else {
            Target::New { id: None }
        };
        let (index, id) = self.open(&cwd, target)?;
        self.sessions.insert(id.clone(), index);
        Ok(json!({"sessionId": id, "configOptions": self.config_options(index)}))
    }

    fn load(&mut self, params: &Value) -> Reply {
        let id = params_str(params, "sessionId")?.to_string();
        let cwd = Self::cwd(params)?;
        Self::note_mcp_servers(params);
        if self.parsed.no_session {
            return Err(RpcError::new(
                METHOD_NOT_FOUND,
                "session/load is not available with --no-session",
            ));
        }
        let env = self.env_for(&cwd);
        let dir = crate::session::session_directory(self.parsed.session_dir.as_deref(), env);
        let repo = session_journal::SessionRepo::new(&dir, session_journal::SystemClock::shared());
        let metadata = crate::session_by_id(&repo, &id)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, e))?
            .ok_or_else(|| RpcError::session_not_found(&id))?;
        let (index, attached) = self.open(&cwd, Target::Open(metadata))?;
        self.sessions.insert(attached.clone(), index);
        for update in translate::replay(&self.slots[index].remote.messages()) {
            self.notify(&attached, update);
        }
        Ok(json!({"configOptions": self.config_options(index)}))
    }

    fn list(&mut self, params: &Value) -> Reply {
        let cwd = match params["cwd"].as_str() {
            Some(_) => Self::cwd(params)?,
            None => self.env.cwd.clone(),
        };
        let env = self.env_for(&cwd);
        let dir = crate::session::session_directory(self.parsed.session_dir.as_deref(), env);
        let cwd_text = cwd.display().to_string();
        let sessions: Vec<Value> = session_journal::names::list_sessions(&dir, &cwd_text)
            .into_iter()
            .map(|s| {
                let title = s
                    .name
                    .clone()
                    .unwrap_or_else(|| s.first_message.chars().take(80).collect());
                json!({"sessionId": s.metadata.id, "cwd": s.metadata.cwd, "title": title,
                    "updatedAt": session_file::js::iso_string(s.metadata.modified_at as i64)})
            })
            .collect();
        Ok(json!({"sessions": sessions}))
    }

    /// The slot of a session this process opened, attached to it.
    fn slot_of(&mut self, session_id: &str) -> Result<usize, RpcError> {
        let index = *self
            .sessions
            .get(session_id)
            .ok_or_else(|| RpcError::session_not_found(session_id))?;
        let slot = &mut self.slots[index];
        if slot.attached != session_id {
            if slot.turn.is_some() {
                return Err(RpcError::new(
                    INVALID_REQUEST,
                    "A prompt is running in another session of this directory; cancel it or wait for its end",
                ));
            }
            slot.remote
                .switch_session(session_id)
                .map_err(|e| RpcError::new(INTERNAL_ERROR, e))?;
            slot.attached = session_id.to_string();
        }
        Ok(index)
    }

    fn set_config_option(&mut self, params: &Value) -> Reply {
        let index = self.slot_of(params_str(params, "sessionId")?)?;
        let value = params_str(params, "value")?;
        let invalid = |e: String| RpcError::new(INVALID_PARAMS, e);
        let slot = &mut self.slots[index];
        match params_str(params, "configId")? {
            "model" => {
                let (provider, model) = value
                    .split_once('/')
                    .ok_or_else(|| invalid(format!("Unknown model: {value}")))?;
                slot.remote.set_model(provider, model).map_err(invalid)?;
            }
            "thought_level" => slot.remote.set_thinking_level(value, &slot.out).map_err(invalid)?,
            other => return Err(invalid(format!("Unknown config option: {other}"))),
        }
        self.drain(index);
        Ok(json!({"configOptions": self.config_options(index)}))
    }

    fn prompt(&mut self, id: &Value, params: &Value) -> Result<(), RpcError> {
        let session_id = params_str(params, "sessionId")?.to_string();
        let blocks = params["prompt"]
            .as_array()
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, "Invalid params: prompt must be an array"))?;
        let message = translate::user_message(blocks).map_err(|e| RpcError::new(INVALID_PARAMS, e))?;
        let index = self.slot_of(&session_id)?;
        let slot = &mut self.slots[index];
        if slot.turn.is_some() || slot.remote.is_running() {
            return Err(RpcError::new(
                INVALID_REQUEST,
                "A prompt is already running in this session",
            ));
        }
        slot.tracker.begin(&slot.cwd);
        slot.remote
            .prompt(message.text, message.images, &slot.out)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, e))?;
        slot.turn = Some(Turn {
            request: id.clone(),
            session_id,
            cancelled: false,
        });
        Ok(())
    }

    fn cancel(&mut self, session_id: &str) {
        let Some(index) = self.sessions.get(session_id).copied() else {
            return;
        };
        let slot = &mut self.slots[index];
        match &mut slot.turn {
            Some(turn) if turn.session_id == session_id => turn.cancelled = true,
            _ => return,
        }
        slot.remote.abort();
    }

    /// An event of slot `index`'s session.
    pub fn deliver(&mut self, index: usize, event: Event) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.remote.deliver(event, &slot.out);
        }
        self.drain(index);
    }

    /// Translate what slot `index` wrote.
    fn drain(&mut self, index: usize) {
        let bytes = std::mem::take(&mut *self.slots[index].capture.lock().unwrap_or_else(PoisonError::into_inner));
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Ok(line) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let actions = self.slots[index].translator.translate(&line);
            for action in actions {
                self.act(index, action);
            }
        }
    }

    fn act(&mut self, index: usize, action: Action) {
        let session = self.slots[index].turn.as_ref().map(|t| t.session_id.clone());
        match action {
            Action::Update(update) => {
                let slot = &mut self.slots[index];
                // Outside a turn only a rename is news to the client.
                let to = match (&session, &slot.turn) {
                    (Some(session), Some(_)) => Some(session.clone()),
                    _ => (update["sessionUpdate"] == "session_info_update").then(|| slot.attached.clone()),
                };
                if let Some(to) = to {
                    self.notify(&to, update);
                }
            }
            Action::CellStarted => {
                let slot = &mut self.slots[index];
                slot.tracker.begin(&slot.cwd);
            }
            Action::CellEnded {
                id,
                args,
                result,
                is_error,
            } => {
                let slot = &mut self.slots[index];
                let changes = slot.tracker.cell_ended(&slot.cwd);
                if let Some(session) = session {
                    self.notify(
                        &session,
                        translate::tool_update(&id, &args, &result, Some((is_error, &changes))),
                    );
                }
            }
            Action::RunEnded(messages) => {
                let slot = &mut self.slots[index];
                slot.tracker.end();
                let Some(turn) = slot.turn.take() else { return };
                let window = slot.remote.state()["model"]["contextWindow"].as_u64();
                if let Some(usage) = translate::usage_update(&messages, window) {
                    self.notify(&turn.session_id, usage);
                }
                let reply = match translate::turn_end(&messages, turn.cancelled) {
                    TurnEnd::Stop(reason) => Ok(json!({"stopReason": reason})),
                    TurnEnd::Failed(message) => Err(RpcError::new(INTERNAL_ERROR, message)),
                };
                self.respond(&turn.request, reply);
            }
            Action::ExtensionUi(request) => self.extension_ui(index, &request),
        }
    }

    /// A dialog or notice of the worker (`/mcp`'s, the MCP gateway's): a notice goes to stderr; a dialog is answered
    /// as dismissed so the worker does not wait.
    fn extension_ui(&mut self, index: usize, request: &Value) {
        let method = request["method"].as_str().unwrap_or_default();
        if method == "notify" {
            eprintln!("ultron acp: {}", request["message"].as_str().unwrap_or_default());
            return;
        }
        if let Some(id) = request["id"].as_str()
            && ["confirm", "select", "input", "editor"].contains(&method)
        {
            self.slots[index]
                .remote
                .extension_ui_response(id, json!({"cancelled": true}));
        }
    }

    /// Release every session (a running turn keeps running in its worker, detached, as when `--mode rpc` ends).
    pub fn close(&mut self) {
        for slot in &mut self.slots {
            slot.remote.close();
        }
    }
}

/// Serve ACP over `input`/`output` until the input ends; returns the exit code.
pub fn run(
    parsed: Args,
    env: &Env,
    input: Box<dyn std::io::Read + Send>,
    output: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (tx, rx): (Sender<Input>, Receiver<Input>) = channel();
    let signal_tx = Mutex::new(tx.clone());
    daemon::signals::set_handler(Arc::new(move |signal| {
        let code = daemon::signals::exit_code(signal);
        let _ = signal_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(Input::Signal(code));
        std::thread::sleep(crate::print_mode::PROMPT_ABORT_WAIT);
        std::process::exit(code);
    }));
    let lines = tx.clone();
    std::thread::spawn(move || {
        for line in crate::rpc::jsonl::JsonlLines::new(input) {
            match line {
                Ok(line) => {
                    if lines.send(Input::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = lines.send(Input::Eof);
    });
    let mut server = Server::new(parsed, env.clone(), Output::new(output), tx);
    let mut code = 0;
    while let Ok(input) = rx.recv() {
        match input {
            Input::Line(line) => server.handle_line(&line),
            Input::Remote(index, event) => server.deliver(index, event),
            Input::Eof => break,
            Input::Signal(signal) => {
                code = signal;
                break;
            }
        }
    }
    server.close();
    Ok(code)
}
