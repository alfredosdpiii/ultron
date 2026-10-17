//! Event fan-out with the ordering and veto rules of `core/extensions/runner.ts`.
//!
//! Extensions are visited in load order; each subscribed extension receives the event once (an extension process runs
//! its own handlers). A handler that fails, times out or whose process died is reported through the error listeners
//! and the dispatch moves on, except where `runner.ts` lets the error escape (`before_file_write`, `user_bash`), which
//! return `Err`.
//!
//! Only the emitters a product path calls are here; `runner.ts`'s `tool_call`, `tool_result`, `message_end`,
//! `input`, `before_provider_*`, `cache_warming_decision`, `resources_discover` and boundary emitters are not, since
//! the native worker never emits them (the model's one tool is `rlm`).
//!
//! A result member that is present (even `null`) counts as TypeScript's `!== undefined`; an omitted member is
//! `undefined`.

use serde_json::{Value, json};
use session_file::current_system_message;
use session_file::js::truthy;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::rpc::CancelToken;

mod cancellable;
pub use cancellable::Cancelled;

/// One loaded extension as the router sees it.
pub trait Endpoint: Send + Sync {
    /// The path errors are reported under (`ExtensionError.extensionPath`).
    fn path(&self) -> &str;
    /// Whether the extension subscribed to this event type.
    fn subscribes(&self, event_type: &str) -> bool;
    /// Delivers one event and waits for the handler's result: `Ok(None)` is `undefined`/`null`, `Err` is the
    /// handler's error message (a thrown error, a timeout, or a dead process).
    fn dispatch(&self, event: &Value) -> Result<Option<Value>, String>;
    /// As [`Endpoint::dispatch`] for an event that carries TypeScript's `signal`: when `cancel` fires the handler is
    /// told (`notifications/cancelled`) and the dispatch ends at once (`Err`, `cancel` cancelled). An endpoint
    /// without a way to stop its handler just dispatches.
    fn dispatch_cancellable(&self, event: &Value, cancel: &CancelToken) -> Result<Option<Value>, String> {
        let _ = cancel;
        self.dispatch(event)
    }
    /// Slash commands the extension registered, in registration order: (name, description).
    fn commands(&self) -> Vec<(String, Option<String>)> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionError {
    pub extension_path: String,
    pub event: String,
    pub error: String,
}

impl ExtensionError {
    pub fn to_value(&self) -> Value {
        json!({ "extensionPath": self.extension_path, "event": self.event, "error": self.error })
    }
}

type ErrorListener = Arc<dyn Fn(&ExtensionError) + Send + Sync>;

const SESSION_BEFORE_EVENTS: [&str; 4] = [
    "session_before_switch",
    "session_before_fork",
    "session_before_compact",
    "session_before_tree",
];

const USER_BASH_INVALID: &str = "Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object";
const CONTEXT_SYSTEM_DROPPED: &str = "Handler removed the leading system message; the request has no prompt or initial tool declarations. Keep it at index 0 or replace a dropped prefix with getCurrentSystemMessage().";

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStartOutcome {
    pub messages: Vec<Value>,
    /// The forced system prompt (`systemPromptOptions.forceSystemPrompt`), when a handler replaced it.
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub name: String,
    pub invocation_name: String,
    pub description: Option<String>,
    pub extension_path: String,
}

#[derive(Default)]
pub struct Router {
    endpoints: Vec<Arc<dyn Endpoint>>,
    listeners: Mutex<Vec<ErrorListener>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn event_type(event: &Value) -> &str {
    event.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn is_user_bash_result(value: &Value) -> bool {
    let Some(candidate) = value.as_object() else {
        return false;
    };
    let has_operations = candidate.contains_key("operations");
    let has_result = candidate.contains_key("result");
    if has_operations == has_result {
        return false;
    }
    // `operations` carries an `exec` function: it cannot cross a process boundary.
    if has_operations {
        return false;
    }
    let Some(result) = candidate.get("result").and_then(Value::as_object) else {
        return false;
    };
    result.get("output").is_some_and(Value::is_string)
        && result.contains_key("exitCode")
        && result.get("exitCode").is_some_and(|c| c.is_null() || c.is_number())
        && result.get("cancelled").is_some_and(Value::is_boolean)
        && result.get("truncated").is_some_and(Value::is_boolean)
        && result
            .get("fullOutputPath")
            .is_none_or(|p| p.is_null() || p.is_string())
}

impl Router {
    pub fn new(endpoints: Vec<Arc<dyn Endpoint>>) -> Self {
        Router {
            endpoints,
            listeners: Mutex::new(Vec::new()),
        }
    }

    pub fn endpoints(&self) -> &[Arc<dyn Endpoint>] {
        &self.endpoints
    }

    pub fn on_error(&self, listener: impl Fn(&ExtensionError) + Send + Sync + 'static) {
        lock(&self.listeners).push(Arc::new(listener));
    }

    pub fn emit_error(&self, error: ExtensionError) {
        let listeners = lock(&self.listeners).clone();
        for listener in listeners {
            listener(&error);
        }
    }

    fn report(&self, endpoint: &dyn Endpoint, event: &str, error: String) {
        self.emit_error(ExtensionError {
            extension_path: endpoint.path().to_string(),
            event: event.to_string(),
            error,
        });
    }

    /// Endpoints subscribed to `event_type`, in load order (`snapshotEventHandlers`).
    fn subscribers(&self, event_type: &str) -> Vec<Arc<dyn Endpoint>> {
        self.endpoints
            .iter()
            .filter(|e| e.subscribes(event_type))
            .cloned()
            .collect()
    }

    pub fn has_handlers(&self, event_type: &str) -> bool {
        self.endpoints.iter().any(|e| e.subscribes(event_type))
    }

    /// `emit`: observe-only events, plus the cancellable `session_before_*` events where the first result with a
    /// truthy `cancel` stops the dispatch and the last truthy result is returned.
    pub fn emit(&self, event: &Value) -> Option<Value> {
        self.dispatch_all(event, None).ok().flatten()
    }

    /// `emitProjectTrustEvent`: the first `yes`/`no` wins, `undecided` falls through. Errors are returned, not
    /// reported, because this runs before the session's error listeners exist.
    pub fn emit_project_trust(&self, event: &Value) -> (Option<Value>, Vec<ExtensionError>) {
        let mut errors = Vec::new();
        for endpoint in self.subscribers("project_trust") {
            let outcome = endpoint.dispatch(event).and_then(|value| match value {
                Some(value) if value.is_object() => Ok(value),
                _ => Err("project_trust handler returned no decision".to_string()),
            });
            match outcome {
                Ok(value) if value.get("trusted") == Some(&json!("undecided")) => {}
                Ok(value) => return (Some(value), errors),
                Err(error) => errors.push(ExtensionError {
                    extension_path: endpoint.path().to_string(),
                    event: "project_trust".into(),
                    error,
                }),
            }
        }
        (None, errors)
    }

    /// `emitBeforeFileWrite`: the first block wins (default reason "blocked by an extension"); other messages are
    /// joined. A handler error is not caught.
    pub fn emit_before_file_write(&self, event: &Value) -> Result<Option<Value>, ExtensionError> {
        let mut messages: Vec<String> = Vec::new();
        for endpoint in self.subscribers("before_file_write") {
            let value = endpoint.dispatch(event).map_err(|error| ExtensionError {
                extension_path: endpoint.path().to_string(),
                event: "before_file_write".into(),
                error,
            })?;
            let value = value.unwrap_or_default();
            if truthy(value.get("block")) {
                let reason = match value.get("reason") {
                    Some(reason) if !reason.is_null() => reason.clone(),
                    _ => json!("blocked by an extension"),
                };
                return Ok(Some(json!({ "block": true, "reason": reason })));
            }
            if truthy(value.get("message")) {
                messages.push(js_string(&value["message"]));
            }
        }
        Ok((!messages.is_empty()).then(|| json!({ "message": messages.join("\n") })))
    }

    /// `emitAfterCellChanges`: every handler runs; messages are joined.
    pub fn emit_after_cell_changes(&self, event: &Value) -> Option<Value> {
        let mut messages: Vec<String> = Vec::new();
        for endpoint in self.subscribers("after_cell_changes") {
            match endpoint.dispatch(event) {
                Ok(value) => {
                    if let Some(message) = value
                        .as_ref()
                        .and_then(|v| v.get("message"))
                        .filter(|m| truthy(Some(m)))
                    {
                        messages.push(js_string(message));
                    }
                }
                Err(error) => self.report(&*endpoint, "after_cell_changes", error),
            }
        }
        (!messages.is_empty()).then(|| json!({ "message": messages.join("\n") }))
    }

    /// `emitUserBash`: the first result replaces local execution. An invalid result or a handler error is reported
    /// and also returned as the error.
    pub fn emit_user_bash(&self, event: &Value) -> Result<Option<Value>, ExtensionError> {
        for endpoint in self.subscribers("user_bash") {
            let outcome = endpoint.dispatch(event).and_then(|value| match value {
                None => Ok(None),
                Some(value) if is_user_bash_result(&value) => Ok(Some(value)),
                Some(_) => Err(USER_BASH_INVALID.to_string()),
            });
            match outcome {
                Ok(None) => continue,
                Ok(Some(value)) => return Ok(Some(value)),
                Err(error) => {
                    let error = ExtensionError {
                        extension_path: endpoint.path().to_string(),
                        event: "user_bash".into(),
                        error,
                    };
                    self.emit_error(error.clone());
                    return Err(error);
                }
            }
        }
        Ok(None)
    }

    /// `emitBeforeAgentStart`: messages collect; a `systemPrompt` result forces the prompt for later handlers and
    /// the turn. `render` turns the options (with `forceSystemPrompt`) into the prompt handlers see.
    pub fn emit_before_agent_start(
        &self,
        prompt: &str,
        images: Option<&Value>,
        options: &Value,
        render: &dyn Fn(&Value) -> String,
    ) -> AgentStartOutcome {
        let mut options = options.clone();
        let mut messages = Vec::new();
        let mut system_prompt = None;
        for endpoint in self.subscribers("before_agent_start") {
            let mut event = json!({ "type": "before_agent_start", "prompt": prompt });
            if let Some(images) = images {
                event["images"] = images.clone();
            }
            event["systemPrompt"] = json!(render(&options));
            event["systemPromptOptions"] = options.clone();
            match endpoint.dispatch(&event) {
                Ok(Some(value)) if truthy(Some(&value)) => {
                    if let Some(message) = value.get("message").filter(|m| truthy(Some(m))) {
                        messages.push(message.clone());
                    }
                    if let Some(forced) = value.get("systemPrompt") {
                        let forced = js_string(forced);
                        if let Some(map) = options.as_object_mut() {
                            map.insert("forceSystemPrompt".into(), json!(forced));
                        }
                        system_prompt = Some(forced);
                    }
                }
                Ok(_) => {}
                Err(error) => self.report(&*endpoint, "before_agent_start", error),
            }
        }
        AgentStartOutcome {
            messages,
            system_prompt,
        }
    }

    /// `emitContext`: `context` handlers see the conversation without system messages and a changed conversation
    /// gets the replayed system message back in front; `context_with_system` handlers then see everything and their
    /// output is used as returned.
    pub fn emit_context(&self, messages: &[Value]) -> Vec<Value> {
        let mut current: Vec<Value> = messages.to_vec();
        for endpoint in self.subscribers("context") {
            let visible: Vec<Value> = current.iter().filter(|m| m["role"] != "system").cloned().collect();
            let event = json!({ "type": "context", "messages": visible });
            match endpoint.dispatch(&event) {
                Ok(value) => {
                    let Some(Value::Array(returned)) = value.as_ref().and_then(|v| v.get("messages")).cloned() else {
                        continue;
                    };
                    if returned == visible {
                        continue;
                    }
                    current = match current_system_message(&current) {
                        Some(head) => std::iter::once(head).chain(returned).collect(),
                        None => returned,
                    };
                }
                Err(error) => self.report(&*endpoint, "context", error),
            }
        }
        for endpoint in self.subscribers("context_with_system") {
            let had_system = current.first().is_some_and(|m| m["role"] == "system");
            let event = json!({ "type": "context_with_system", "messages": current });
            match endpoint.dispatch(&event) {
                Ok(value) => {
                    if let Some(Value::Array(returned)) = value.as_ref().and_then(|v| v.get("messages")) {
                        current = returned.clone();
                    }
                    if had_system && !current.first().is_some_and(|m| m["role"] == "system") {
                        self.report(&*endpoint, "context_with_system", CONTEXT_SYSTEM_DROPPED.into());
                    }
                }
                Err(error) => self.report(&*endpoint, "context_with_system", error),
            }
        }
        current
    }

    /// `getRegisteredCommands`: every extension's commands in load order; a name registered more than once is
    /// invoked as `name:1`, `name:2`, ..., skipping invocation names already taken.
    pub fn registered_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands: Vec<(String, Option<String>, String)> = Vec::new();
        for endpoint in &self.endpoints {
            for (name, description) in endpoint.commands() {
                commands.push((name, description, endpoint.path().to_string()));
            }
        }
        let count = |name: &str| commands.iter().filter(|(n, _, _)| n == name).count();
        let mut seen: Vec<(String, usize)> = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for (name, description, path) in &commands {
            let occurrence = match seen.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => {
                    entry.1 += 1;
                    entry.1
                }
                None => {
                    seen.push((name.clone(), 1));
                    1
                }
            };
            let mut invocation = if count(name) > 1 {
                format!("{name}:{occurrence}")
            } else {
                name.clone()
            };
            if taken.contains(&invocation) {
                let mut suffix = occurrence;
                loop {
                    suffix += 1;
                    invocation = format!("{name}:{suffix}");
                    if !taken.contains(&invocation) {
                        break;
                    }
                }
            }
            taken.push(invocation.clone());
            out.push(ResolvedCommand {
                name: name.clone(),
                invocation_name: invocation,
                description: description.clone(),
                extension_path: path.clone(),
            });
        }
        out
    }
}

/// `String(value)` for the strings handlers return.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}
