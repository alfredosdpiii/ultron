//! The session actor's side of the extension host (crate::extension_host): requests it serves, the facts it
//! publishes, and the events the TypeScript worker emits to extensions (legacy-extension-adapter.ts):
//!
//! - `session_start` (`reason: "startup"`, or `"fork"` with `previousSessionFile` for a session a client forked to,
//!   once) when the session opens, without waiting (`void runner.emit`);
//! - `before_agent_start` when a run starts (harness `before_run`): the prompt's text, the worker's prompt options
//!   (`cwd`, `selectedTools`, no context files or skills); the messages handlers return go to the model after the run's
//!   prompt, as user messages;
//! - `context` / `context_with_system` over each model request (harness `transform_context`);
//! - `agent_end` (`messages: []`) when a run ends, without the actor waiting;
//! - `session_before_fork` before a client forks the session (only when an extension handles it);
//! - `session_shutdown` (`reason: "quit"`, or `"fork"` with `targetSessionFile` after a client moved to a fork and the
//!   session was not used again) when the session closes, awaited.

use super::{Core, Input, LANE, OpResult, lock};
use crate::durable_store::lock as lock_session;
use crate::extension_host::{Call, Facts, content_to_text};
use crate::lane::now_ms;
use ai::Message;
use extensions::ExtensionHost;
use serde_json::{Map, Value, json};
use session_journal::Write;
use session_journal::session::{BRANCH_TIP, LANE_STATE};
use std::sync::Arc;

/// The fork marker the session a client forked to carries until its first worker reads it
/// (`forkedSessionStart` of session-start.ts).
pub(crate) const SESSION_START: (&str, &str) = ("ultron.session.start", "fork");

impl Core {
    pub(super) fn extension_host(&self) -> Option<Arc<ExtensionHost>> {
        self.runtime.extensions.clone()
    }

    /// The session opened: bind the extension host to it and emit `session_start`. A fork marker is consumed
    /// whether or not extensions are loaded.
    pub(super) fn open_extensions(&mut self) -> Result<(), String> {
        let forked = lock_session(&self.session)
            .value(SESSION_START.0, SESSION_START.1)
            .cloned();
        if forked.is_some() {
            lock_session(&self.session).commit(&[Write::delete(SESSION_START.0, SESSION_START.1)])?;
        }
        let Some(host) = self.extension_host() else {
            return Ok(());
        };
        self.runtime
            .extension_api
            .bind_actor(self.tx.clone(), self.session.clone(), &self.lane.stack);
        self.runtime
            .extension_api
            .bind_models(self.env.clone(), self.flags.clone());
        // A handler's failure reaches the clients as `extension_error` (rpc-mode.ts `onError`).
        let errors = self.tx.clone();
        host.router().on_error(move |error| {
            let _ = errors.send(Input::ExtensionError(error.to_value()));
        });
        self.refresh_extension_facts();
        let mut event = json!({"type": "session_start", "reason": "startup"});
        if let Some(forked) = forked {
            event["reason"] = json!("fork");
            if let Some(previous) = forked.get("previousSessionFile").filter(|p| p.is_string()) {
                event["previousSessionFile"] = previous.clone();
            }
        }
        std::thread::spawn(move || {
            host.router().emit(&event);
        });
        Ok(())
    }

    /// Publish the facts extensions read without asking the actor (model, thinking level, idle, pending messages).
    pub(super) fn refresh_extension_facts(&self) {
        if self.runtime.extensions.is_none() {
            return;
        }
        let api = &self.runtime.extension_api;
        let current = api.facts();
        let pending = {
            let queues = lock(&self.queues);
            !queues.items.is_empty() || !queues.delivered.is_empty()
        };
        let idle = self.run.is_none();
        let same_model =
            current.model["provider"] == self.model.provider.as_str() && current.model["id"] == self.model.id.as_str();
        if same_model && current.thinking == self.thinking && current.idle == idle && current.pending == pending {
            return;
        }
        api.set_facts(Facts {
            model: if same_model {
                current.model
            } else {
                serde_json::to_value(&self.model).unwrap_or(Value::Null)
            },
            thinking: self.thinking.clone(),
            idle,
            pending,
        });
    }

    /// An extension's request that needs the actor.
    pub(super) fn extension_call(&mut self, call: Call) {
        match call {
            Call::Fire(op) => self.op(op, Box::new(|_| {})),
            Call::Await(op, tx) => self.op(
                op,
                Box::new(move |result: OpResult| {
                    let _ = tx.send(result);
                }),
            ),
            Call::WaitIdle(tx) => {
                if self.run.is_some() {
                    self.after_run.push(Input::Extension(Call::WaitIdle(tx)));
                } else {
                    let _ = tx.send(());
                }
            }
            Call::AppendCustom { custom_type, data } => {
                // A custom entry joins the branch at a boundary: after the run in progress.
                if self.run.is_some() {
                    self.after_run
                        .push(Input::Extension(Call::AppendCustom { custom_type, data }));
                    return;
                }
                let _ = self.append_custom(&custom_type, data);
            }
        }
        self.refresh_extension_facts();
    }

    /// `appendCustomEntry`: a `custom` entry at the tip of `main`, announced.
    fn append_custom(&mut self, custom_type: &str, data: Value) -> Result<(), String> {
        let id = session_journal::lock_clock(&lock_session(&self.session).clock()).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), self.tip().map_or(Value::Null, Value::from));
        entry.insert("type".into(), "custom".into());
        entry.insert("customType".into(), custom_type.into());
        if !data.is_null() {
            entry.insert("data".into(), data);
        }
        let lane_state = self.lane_state();
        lock_session(&self.session).commit(&[
            Write::Entry(entry),
            Write::set(BRANCH_TIP, LANE, id.into()),
            Write::set(LANE_STATE, LANE, lane_state),
        ])?;
        self.announce();
        Ok(())
    }

    /// Harness `before_run`: `before_agent_start` with the prompt's text and the worker's prompt options; the
    /// messages it returns are kept for the run `operation`'s requests. The session is in use again: a later
    /// close is an ordinary quit.
    pub(super) fn before_agent_start(&mut self, prompt: &Value, operation: Option<&str>) {
        let Some(host) = self.extension_host() else { return };
        let api = self.runtime.extension_api.clone();
        *lock(&api.shutdown) = json!({"reason": "quit"});
        *lock(&api.injected) = None;
        if !host.router().has_handlers("before_agent_start") {
            return;
        }
        let cwd = self.env.cwd.display().to_string();
        let options = json!({
            "selectedTools": api.active_tools(), "toolSnippets": {}, "toolGuidelines": {}, "promptGuidelines": [],
            "appendSystemPrompt": "", "sections": {}, "cwd": cwd, "contextFiles": [], "skills": [],
        });
        let render = |options: &Value| -> String {
            let mut build = resources::system_prompt::BuildOptions::new(&cwd);
            build.selected_tools = vec!["rlm".into()];
            let force = options.get("forceSystemPrompt").and_then(Value::as_str);
            resources::system_prompt::build_system_prompt(&build, force).unwrap_or_default()
        };
        let text = content_to_text(prompt);
        let outcome = host.router().emit_before_agent_start(&text, None, &options, &render);
        let messages: Vec<Value> = outcome
            .messages
            .iter()
            .map(|message| {
                json!({"role": "user", "content": [{"type": "text", "text": content_to_text(&message["content"])}], "timestamp": now_ms()})
            })
            .collect();
        if let Some(operation) = operation
            && !messages.is_empty()
        {
            *lock(&api.injected) = Some((operation.to_string(), messages));
        }
    }

    /// Harness `transform_context` for a request of the run in progress: the `before_agent_start` messages after
    /// the run's prompt (at `prompt_end` in `messages`), then the extensions' `context` handlers.
    pub(super) fn extension_request(&self, messages: Vec<Message>, prompt_end: Option<usize>) -> Vec<Message> {
        let Some(host) = self.extension_host() else {
            return messages;
        };
        let mut messages = messages;
        let operation = self
            .run
            .as_ref()
            .and_then(|run| run.journal.run().map(|r| r.operation_id().to_string()));
        if let (Some(at), Some(operation)) = (prompt_end, operation)
            && let Some((injected_for, injected)) = lock(&self.runtime.extension_api.injected).as_ref()
            && *injected_for == operation
            && at <= messages.len()
        {
            let extra: Vec<Message> = injected
                .iter()
                .filter_map(|m| serde_json::from_value(m.clone()).ok())
                .collect();
            messages.splice(at..at, extra);
        }
        let router = host.router();
        if !router.has_handlers("context") && !router.has_handlers("context_with_system") {
            return messages;
        }
        let values: Vec<Value> = messages.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        let changed = router.emit_context(&values);
        if changed == values {
            return messages;
        }
        match changed
            .into_iter()
            .map(serde_json::from_value::<Message>)
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(changed) => changed,
            Err(error) => {
                router.emit_error(extensions::ExtensionError {
                    extension_path: "<context>".into(),
                    event: "context".into(),
                    error: format!("Invalid context messages: {error}"),
                });
                messages
            }
        }
    }

    /// The run ended: `agent_end` (`messages: []`), without the actor waiting for its handlers.
    pub(super) fn emit_agent_end(&self) {
        let Some(host) = self.extension_host() else { return };
        if !host.router().has_handlers("agent_end") {
            return;
        }
        std::thread::spawn(move || {
            host.router().emit(&json!({"type": "agent_end", "messages": []}));
        });
    }

    /// A client moved on to the fork `target` (`forked`): `session_shutdown` says so unless the session is used again.
    pub(super) fn extensions_forked(&self, target: &str) {
        *lock(&self.runtime.extension_api.shutdown) = json!({"reason": "fork", "targetSessionFile": target});
    }

    /// The session closes: `session_shutdown`, awaited, before the processes stop.
    pub(super) fn emit_session_shutdown(&self) {
        let Some(host) = self.extension_host() else { return };
        let mut event = lock(&self.runtime.extension_api.shutdown).clone();
        event["type"] = json!("session_shutdown");
        let mut ordered = Map::new();
        ordered.insert("type".into(), json!("session_shutdown"));
        if let Value::Object(fields) = event {
            for (key, value) in fields {
                ordered.entry(key).or_insert(value);
            }
        }
        host.router().emit(&Value::Object(ordered));
        self.runtime.extension_api.unbind_actor();
    }
}

/// `beforeFork(entryId, position)`: `session_before_fork` when an extension handles it; `{cancelled}`.
pub fn before_fork(host: Option<&Arc<ExtensionHost>>, entry_id: &str, position: &str) -> Value {
    let Some(host) = host.filter(|h| h.router().has_handlers("session_before_fork")) else {
        return json!({"cancelled": false});
    };
    let result = host
        .router()
        .emit(&json!({"type": "session_before_fork", "entryId": entry_id, "position": position}));
    json!({"cancelled": result.is_some_and(|r| r["cancel"] == true)})
}
