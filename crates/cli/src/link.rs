//! A client's link to one session over the native architecture, as `rpc-native.ts` and `client-tui.ts` hold it:
//! this process's own server generation for the instance (crates/daemon), a client connection (crates/client), the
//! session selected (created, forked or opened) and attached, its services subscribed, and its transcript's
//! lane-watch events forwarded to the caller. Operations go to the session worker's `ultron.session-control.op`
//! (the session actor's [`Op`]s); switching sessions re-attaches, as TypeScript clients do.

use crate::Env;
use crate::args::Args;
use crate::print_mode::{Target, server_options};
use client::runtime::{CallError, Connection};
use daemon::server::{RunningServer, start_foreground_server};
use serde_json::{Value, json};
use session_core::core::{Op, OpResult};
use std::sync::Arc;

fn fail(error: CallError) -> String {
    daemon::lost::describe(&error.to_string()).unwrap_or_else(|| error.to_string())
}

/// Where a link forwards the attached session's lane-watch events.
pub type WatchSink = Arc<dyn Fn(Value) + Send + Sync>;

pub struct Link {
    server: Arc<RunningServer>,
    pub connection: Arc<Connection>,
    pub session_id: String,
    subscriptions: Vec<String>,
    transcript: Option<String>,
    events: WatchSink,
    /// Sessions this link created (removed on close under `--no-session`).
    pub created: Vec<String>,
}

impl Link {
    /// Start the server, select `target`'s session and attach to it; transcript events go to `events`.
    pub fn open(parsed: &Args, env: &Env, target: &Target, mode: &str, events: WatchSink) -> Result<Link, String> {
        let options = server_options(parsed, env, mode)?;
        let server = start_foreground_server(&options).map_err(|e| e.to_string())?;
        let connection = match Connection::connect(&server.endpoint_path, &server.server_id) {
            Ok(connection) => connection,
            Err(error) => {
                server.close();
                return Err(fail(error));
            }
        };
        let mut link = Link {
            server,
            connection,
            session_id: String::new(),
            subscriptions: Vec::new(),
            transcript: None,
            events,
            created: Vec::new(),
        };
        let name = parsed.name.as_deref();
        let selected = match target {
            Target::Open(metadata) => {
                if let Some(name) = name {
                    link.management("rename", json!([metadata.id, name]))?;
                }
                Ok(metadata.id.clone())
            }
            Target::New { id } => {
                let mut options = json!({});
                if let Some(id) = id {
                    options["id"] = json!(id);
                }
                if let Some(name) = name {
                    options["name"] = json!(name);
                }
                link.create(options)
            }
            Target::Fork(metadata) => {
                let mut options = json!({"forkFromSessionId": metadata.id});
                if let Some(name) = name {
                    options["name"] = json!(name);
                }
                link.create(options)
            }
            Target::Ephemeral => link.create(json!({})),
        };
        let attached = selected.and_then(|id| link.attach(&id));
        if let Err(error) = attached {
            link.close(false);
            return Err(error);
        }
        Ok(link)
    }

    fn management(&self, member: &str, args: Value) -> Result<Option<Value>, String> {
        self.connection
            .call(self.connection.server_target(), "pi.session-management", member, args)
            .map_err(fail)
    }

    /// Create a session (`pi.session-management.create` options); returns its id.
    pub fn create(&mut self, options: Value) -> Result<String, String> {
        let summary = self.management("create", json!([options]))?.unwrap_or_default();
        let id = summary["sessionId"].as_str().unwrap_or_default().to_string();
        self.created.push(id.clone());
        Ok(id)
    }

    /// The file of a session the server lists.
    pub fn session_file(&self, id: &str) -> Result<Option<String>, String> {
        let listed = self.management("describe", json!([{}]))?.unwrap_or_default();
        Ok(listed
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["sessionId"] == id)
            .and_then(|s| s["sessionFile"].as_str().map(str::to_string)))
    }

    /// Attach to session `id` (replacing the current one): its services subscribed, its transcript followed.
    pub fn attach(&mut self, id: &str) -> Result<(), String> {
        self.unsubscribe();
        let server = self.connection.server_target();
        self.connection
            .call(
                server,
                "pi.presentation-plugins",
                "prepareSession",
                json!([{"sessionId": id, "packagePaths": null}]),
            )
            .map_err(fail)?;
        self.management("attach", json!([id]))?;
        self.connection
            .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
            .map_err(fail)?;
        let session = self.connection.session_target().map_err(fail)?;
        let mut started = Vec::new();
        for service in ["pi.models", "pi.agent-controller", "pi.transcript"] {
            started.push(
                self.connection
                    .start_subscribe(session.clone(), service)
                    .map_err(fail)?,
            );
        }
        for (subscription, request) in &started {
            self.connection.finish_subscribe(subscription, request).map_err(fail)?;
            self.subscriptions.push(subscription.clone());
        }
        // The session's name, as it changes (the Rust-only operations service).
        let names = self
            .connection
            .subscribe(session, daemon::worker::OPS_SERVICE)
            .map_err(fail)?;
        let name_events = self.events.clone();
        self.connection.listen(
            &names,
            Box::new(move |_, value| name_events(json!({"type": "session_name", "name": value["name"]}))),
        );
        self.subscriptions.push(names);
        let transcript = started[2].0.clone();
        let events = self.events.clone();
        self.connection.listen(
            &transcript,
            Box::new(move |_, value| {
                if !value["event"].is_null() {
                    events(value["event"].clone());
                }
            }),
        );
        self.transcript = Some(transcript);
        self.session_id = id.to_string();
        Ok(())
    }

    fn unsubscribe(&mut self) {
        if let Some(transcript) = self.transcript.take() {
            self.connection.unlisten(&transcript);
        }
        for subscription in &self.subscriptions {
            self.connection.unlisten(subscription);
        }
        let Ok(session) = self.connection.session_target() else {
            self.subscriptions.clear();
            return;
        };
        let pending: Vec<String> = self
            .subscriptions
            .drain(..)
            .filter_map(|s| {
                self.connection.forget(&s);
                self.connection.start_unsubscribe(session.clone(), &s).ok()
            })
            .collect();
        for id in pending {
            let _ = self.connection.wait(&id);
        }
    }

    /// The attached session's transcript snapshot (`LaneSnapshot`).
    pub fn snapshot(&self) -> Value {
        self.transcript
            .as_ref()
            .and_then(|t| self.connection.state(t, "state"))
            .map_or(Value::Null, |s| s["snapshot"].clone())
    }

    /// Run a session operation on the attached session's worker.
    pub fn op(&self, op: &Op) -> OpResult {
        let session = self.connection.session_target().map_err(fail)?;
        self.connection
            .call(session, daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]))
            .map_err(fail)
    }

    /// A blocking call of the attached session (`bash`, `compact`, the agent controller's `prompt`).
    pub fn call(&self, service: &str, member: &str, args: Value) -> OpResult {
        let session = self.connection.session_target().map_err(fail)?;
        self.connection.call(session, service, member, args).map_err(fail)
    }

    /// Detach, remove the sessions this link created when `remove_created`, and stop the server.
    pub fn close(mut self, remove_created: bool) {
        self.unsubscribe();
        if remove_created {
            for id in std::mem::take(&mut self.created) {
                let attached = self.connection.attachment().is_some_and(|a| a.session_id == id);
                if self.management("remove", json!([id])).is_ok() && attached {
                    let _ = self.connection.wait_attachment(|a| a.is_none());
                }
            }
        }
        self.connection.disconnect();
        self.server.close();
    }
}
