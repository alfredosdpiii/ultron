//! `ultron --mode rpc`: Pi's JSONL protocol on stdin/stdout (`modes/rpc/rpc-types.ts`) as the native command serves
//! it (`experimental/rpc-native.ts`). Commands are JSON lines with a `type` and an optional `id`; each gets one
//! `{id, type: "response", command, success, data | error}` line, and lane events stream as Pi session events.
//!
//! The command layer ([`handle`]) parses and validates commands and shapes responses; everything a command does
//! to a session goes through [`RpcSession`]. [`remote`] is the session as `rpc-native.ts` reaches it: through this
//! process's server generation, attached to the session worker that runs it.

pub mod client;
pub mod events;
pub mod jsonl;
pub mod remote;

use serde_json::{Map, Value, json};
use std::io::Write;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};

/// The stdout line writer, shared by the command loop and the threads that answer later (bash).
#[derive(Clone)]
pub struct Output(Arc<Mutex<Box<dyn Write + Send>>>);

impl Output {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Output(Arc::new(Mutex::new(writer)))
    }

    /// Write one JSONL record and flush it.
    pub fn emit(&self, value: &Value) {
        let mut writer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writer.write_all(jsonl::serialize_json_line(value).as_bytes());
        let _ = writer.flush();
    }
}

/// What the command loop reacts to: a stdin record, the end of stdin, or an event of the session's own work.
pub enum Input<E> {
    Line(String),
    Eof,
    Event(E),
}

pub use session_core::core::QueueKind;

/// A command that answers after the handler returns (`bash`): it writes its response line itself.
pub struct Reply {
    id: Option<Value>,
    command: String,
    out: Output,
}

impl Reply {
    pub fn send(self, result: Result<Option<Value>, String>) {
        self.out.emit(&match result {
            Ok(data) => success(self.id.as_ref(), &self.command, data),
            Err(message) => failure(self.id.as_ref(), Some(&self.command), &message),
        });
    }
}

/// The session operations behind the RPC commands. Errors are the `error` text of the failed response.
pub trait RpcSession {
    /// The session's own work reporting back (run events, finished background commands).
    type Event: Send + 'static;

    fn deliver(&mut self, event: Self::Event, out: &Output);
    /// Whether work is still in flight that must finish before the process exits.
    fn busy(&self) -> bool;
    /// Stdin ended: stop runs, then [`busy`](Self::busy) drains the rest.
    fn input_ended(&mut self);
    /// Release the session (delete `--no-session` files, stop the kernel).
    fn close(&mut self);

    fn is_running(&self) -> bool;
    /// Start a run with this prompt; `agent_start` is emitted before this returns.
    fn prompt(&mut self, message: String, images: Vec<Value>, out: &Output) -> Result<(), String>;
    fn enqueue(&mut self, kind: QueueKind, message: String, images: Vec<Value>, out: &Output) -> Result<(), String>;
    fn abort(&mut self);
    /// Empty the queues; returns the texts that were queued.
    fn clear_queue(&mut self, out: &Output) -> Value;
    fn new_session(&mut self) -> Result<(), String>;
    fn state(&self) -> Value;
    fn available_models(&self) -> Vec<Value>;
    /// Select a model from the available ones; returns it.
    fn set_model(&mut self, provider: &str, model_id: &str) -> Result<Value, String>;
    /// The next available model, or `None` with fewer than two.
    fn cycle_model(&mut self) -> Result<Option<Value>, String>;
    fn thinking_levels(&self) -> Vec<String>;
    fn set_thinking_level(&mut self, level: &str, out: &Output) -> Result<(), String>;
    fn cycle_thinking_level(&mut self, out: &Output) -> Result<Option<String>, String>;
    fn set_queue_mode(&mut self, kind: QueueKind, mode: &str) -> Result<(), String>;
    fn compact(&mut self, custom_instructions: Option<&str>, out: &Output) -> Result<Value, String>;
    fn set_auto_compaction(&mut self, enabled: bool);
    fn set_auto_retry(&mut self, enabled: bool);
    fn abort_retry(&mut self);
    fn bash(&mut self, command: String, exclude_from_context: bool, reply: Reply) -> Result<(), String>;
    fn abort_bash(&mut self);
    fn session_stats(&self) -> Value;
    fn switch_session(&mut self, selector: &str) -> Result<(), String>;
    /// Fork before a user message; returns its text.
    fn fork(&mut self, entry_id: &str) -> Result<String, String>;
    fn clone_session(&mut self) -> Result<(), String>;
    fn fork_messages(&self) -> Vec<Value>;
    /// Entries after `since` (all without it), and the leaf id.
    fn entries(&self, since: Option<&str>) -> Result<(Vec<Value>, Value), String>;
    fn tree(&self) -> (Value, Value);
    fn last_assistant_text(&self) -> Option<String>;
    fn set_session_name(&mut self, name: &str) -> Result<(), String>;
    fn messages(&self) -> Vec<Value>;
    fn commands(&self) -> Result<Vec<Value>, String>;
}

fn success(id: Option<&Value>, command: &str, data: Option<Value>) -> Value {
    let mut out = Map::new();
    if let Some(id) = id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), "response".into());
    out.insert("command".into(), command.into());
    out.insert("success".into(), true.into());
    if let Some(data) = data {
        out.insert("data".into(), data);
    }
    Value::Object(out)
}

/// The error response; `command` is omitted when the command had no string `type`.
fn failure(id: Option<&Value>, command: Option<&str>, message: &str) -> Value {
    let mut out = Map::new();
    if let Some(id) = id {
        out.insert("id".into(), id.clone());
    }
    out.insert("type".into(), "response".into());
    if let Some(command) = command {
        out.insert("command".into(), command.into());
    }
    out.insert("success".into(), false.into());
    out.insert("error".into(), message.into());
    Value::Object(out)
}

/// `promptRequest`: the message text and any images.
fn prompt_request(command: &Value, kind: &str) -> Result<(String, Vec<Value>), String> {
    let message = command
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{kind} requires a message"))?;
    let images = command
        .get("images")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok((message.to_string(), images))
}

/// `queueMode`.
fn queue_mode(mode: Option<&Value>) -> Result<&str, String> {
    match mode.and_then(Value::as_str) {
        Some(mode @ ("all" | "one-at-a-time")) => Ok(mode),
        _ => Err(format!(
            "Invalid queue mode: {}",
            mode.map_or("undefined".to_string(), js_string)
        )),
    }
}

/// `String(value)` for the values a command field can hold.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Array(_) | Value::Object(_) => {
            if value.is_array() {
                value
                    .as_array()
                    .map(|items| items.iter().map(js_string).collect::<Vec<_>>().join(","))
                    .unwrap_or_default()
            } else {
                "[object Object]".into()
            }
        }
        other => session_file::js::stringify(other),
    }
}

/// One command: `Ok(None)` when the response comes later (or was already written).
fn run_command<S: RpcSession>(
    session: &mut S,
    command: &Value,
    kind: &str,
    out: &Output,
) -> Result<Option<Option<Value>>, String> {
    let str_field = |key: &str| command.get(key).and_then(Value::as_str);
    let done = |data: Option<Value>| Ok(Some(data));
    match kind {
        "prompt" => {
            let (message, images) = prompt_request(command, kind)?;
            if session.is_running() {
                let queue = match str_field("streamingBehavior") {
                    Some("steer") => QueueKind::Steer,
                    Some("followUp") => QueueKind::FollowUp,
                    _ => {
                        return Err("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".into());
                    }
                };
                session.enqueue(queue, message, images, out)?;
                return done(None);
            }
            session.prompt(message, images, out)?;
            done(None)
        }
        "steer" | "follow_up" => {
            let (message, images) = prompt_request(command, kind)?;
            let queue = if kind == "steer" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            session.enqueue(queue, message, images, out)?;
            done(None)
        }
        "abort" => {
            session.abort();
            done(None)
        }
        "clear_queue" => done(Some(session.clear_queue(out))),
        "new_session" => {
            session.new_session()?;
            done(Some(json!({"cancelled": false})))
        }
        "get_state" => done(Some(session.state())),
        "set_model" => {
            let provider = command.get("provider").map_or("undefined".into(), js_string);
            let model_id = command.get("modelId").map_or("undefined".into(), js_string);
            done(Some(session.set_model(&provider, &model_id)?))
        }
        "cycle_model" => done(Some(session.cycle_model()?.unwrap_or(Value::Null))),
        "get_available_models" => done(Some(json!({"models": session.available_models()}))),
        "set_thinking_level" => {
            let level = command.get("level").map_or("undefined".into(), js_string);
            session.set_thinking_level(&level, out)?;
            done(None)
        }
        "cycle_thinking_level" => match session.cycle_thinking_level(out)? {
            Some(level) => done(Some(json!({"level": level}))),
            None => done(Some(Value::Null)),
        },
        "get_available_thinking_levels" => done(Some(json!({"levels": session.thinking_levels()}))),
        "set_steering_mode" | "set_follow_up_mode" => {
            let mode = queue_mode(command.get("mode"))?;
            let queue = if kind == "set_steering_mode" {
                QueueKind::Steer
            } else {
                QueueKind::FollowUp
            };
            session.set_queue_mode(queue, mode)?;
            done(None)
        }
        "compact" => done(Some(session.compact(str_field("customInstructions"), out)?)),
        "set_auto_compaction" => {
            session.set_auto_compaction(command.get("enabled") == Some(&Value::Bool(true)));
            done(None)
        }
        "set_auto_retry" => {
            session.set_auto_retry(command.get("enabled") == Some(&Value::Bool(true)));
            done(None)
        }
        "abort_retry" => {
            session.abort_retry();
            done(None)
        }
        "bash" => {
            let text = str_field("command").ok_or("bash requires a command")?;
            let reply = Reply {
                id: command.get("id").cloned(),
                command: kind.into(),
                out: out.clone(),
            };
            session.bash(
                text.to_string(),
                command.get("excludeFromContext") == Some(&Value::Bool(true)),
                reply,
            )?;
            Ok(None)
        }
        "abort_bash" => {
            session.abort_bash();
            done(None)
        }
        "get_session_stats" => done(Some(session.session_stats())),
        "switch_session" => {
            let selector = str_field("sessionPath").ok_or("switch_session requires sessionPath")?;
            session.switch_session(selector)?;
            done(Some(json!({"cancelled": false})))
        }
        "fork" => {
            let entry_id = str_field("entryId").ok_or("fork requires entryId")?;
            let text = session.fork(entry_id)?;
            done(Some(json!({"text": text, "cancelled": false})))
        }
        "clone" => {
            session.clone_session()?;
            done(Some(json!({"cancelled": false})))
        }
        "get_fork_messages" => done(Some(json!({"messages": session.fork_messages()}))),
        "get_entries" => {
            let (entries, leaf) = session.entries(str_field("since"))?;
            done(Some(json!({"entries": entries, "leafId": leaf})))
        }
        "get_tree" => {
            let (tree, leaf) = session.tree();
            done(Some(json!({"tree": tree, "leafId": leaf})))
        }
        "get_last_assistant_text" => done(Some(json!({"text": session.last_assistant_text()}))),
        "set_session_name" => {
            let name = str_field("name").map(session_file::js::trim).unwrap_or_default();
            if name.is_empty() {
                return Err("Session name cannot be empty".into());
            }
            session.set_session_name(name)?;
            done(None)
        }
        "get_messages" => done(Some(json!({"messages": session.messages()}))),
        "get_commands" => done(Some(json!({"commands": session.commands()?}))),
        // Ultron's inspector and the HTML export need subsystems the Rust build does not have yet.
        "inspect" | "export_html" => Err(format!("{kind} is not implemented in the Rust build yet")),
        other => Err(format!("Unknown command: {other}")),
    }
}

/// `handleLine`: parse one record and answer it. Extension UI responses are accepted and ignored (no extension
/// host runs yet), as unknown ids are.
pub fn handle_line<S: RpcSession>(session: &mut S, line: &str, out: &Output) {
    let command: Value = match serde_json::from_str(line) {
        Ok(command) => command,
        Err(error) => {
            out.emit(&failure(
                None,
                Some("parse"),
                &format!("Failed to parse command: {error}"),
            ));
            return;
        }
    };
    let kind = command.get("type").and_then(Value::as_str);
    if kind == Some("extension_ui_response") {
        return;
    }
    let id = command.get("id");
    let Some(kind) = kind else {
        let shown = command.get("type").map_or("undefined".into(), js_string);
        out.emit(&failure(id, None, &format!("Unknown command: {shown}")));
        return;
    };
    match run_command(session, &command, kind, out) {
        Ok(Some(data)) => out.emit(&success(id, kind, data)),
        Ok(None) => {}
        Err(message) => out.emit(&failure(id, Some(kind), &message)),
    }
}

/// Serve commands until stdin ends, then let in-flight work finish and release the session.
pub fn serve<S: RpcSession>(session: &mut S, inputs: &Receiver<Input<S::Event>>, out: &Output) {
    let mut ended = false;
    while let Ok(input) = inputs.recv() {
        match input {
            Input::Line(line) => {
                if !ended {
                    handle_line(session, &line, out);
                }
            }
            Input::Eof => {
                ended = true;
                session.input_ended();
            }
            Input::Event(event) => session.deliver(event, out),
        }
        if ended && !session.busy() {
            break;
        }
    }
    session.close();
}
