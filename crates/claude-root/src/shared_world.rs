//! The session's shared world (`swarm.*`, `board.*`, `notes.*`) for the subagents of `ultron claude`.
//!
//! Problem: the shared world lives in the root server's durable runtime (crate::durable_apis), served per
//! conversation; an in-memory `--children ultron` subagent has no conversation there, and a Claude Code subagent runs
//! on its own `ultron mcp --child` server in another process. Both were refused, so a subagent could not take a board
//! task, leave a note or tell the root (or a persistent child) anything before its answer.
//!
//! Trace: a Claude Code subagent's cell runs `await board.add('fix lexer')`; its server's layer (crate::unavailable)
//! sends `{"op": "shared", "token": <its one-time token>, "kind": "board.add", "payload": {...}}` over the parent's
//! control socket (the connection it already holds for `child.finish`); the parent finds the subagent's lane by the
//! token and its `rlm.spawn(name=...)` by the lane, and serves the request as that agent
//! ([`crate::durable_apis::DurableApis::serve_as`]): the task is `by` the subagent's name. A parent that is itself a
//! subagent's server forwards it the same way with the name, up to the root. An in-memory subagent's request is
//! served by the root server's own layer the same way, without the socket.
//!
//! Not forwarded: `rlm.spawn(keep=True)` and `rlm.ask` (crate::durable_apis::subagent_refusal), and schedules (the
//! root's only, as in a native session).

use crate::durable_apis::DurableApis;
use serde_json::{Map, Value};
use session_core::subagents::LANE_PREFIX;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Sends a shared-world request to the parent server: `(name, kind, payload)`; `name` is the requesting subagent's,
/// `None` for this server's own root (the parent knows it by its token).
pub type Upstream = Arc<dyn Fn(Option<&str>, &str, &Map<String, Value>) -> Result<Value, String> + Send + Sync>;

/// The control op a subagent's server forwards shared-world requests with.
pub const OP: &str = "shared";

/// Where a server's subagents reach the shared world: the root's durable runtime, or the parent server.
#[derive(Default)]
pub struct SharedWorld {
    /// Each subagent's `rlm.spawn(name=...)`, by task id (from the spawn's answer).
    names: Mutex<HashMap<String, String>>,
    /// A root server's durable runtime.
    durable: OnceLock<Arc<DurableApis>>,
    /// A subagent server's way to its parent.
    upstream: Option<Upstream>,
}

impl SharedWorld {
    pub fn root() -> Arc<SharedWorld> {
        Arc::new(SharedWorld::default())
    }

    pub fn forwarding(upstream: Upstream) -> Arc<SharedWorld> {
        Arc::new(SharedWorld {
            upstream: Some(upstream),
            ..SharedWorld::default()
        })
    }

    /// The root's durable runtime, once it is open.
    pub fn attach(&self, durable: Arc<DurableApis>) {
        let _ = self.durable.set(durable);
    }

    /// Remember the name of the subagent an `rlm.spawn` answered with.
    pub fn spawned(&self, answer: &Value) {
        if let (Some(id), Some(name)) = (answer["rlm_child_id"].as_str(), answer["name"].as_str()) {
            self.names
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.to_string(), name.to_string());
        }
    }

    /// The name the subagent working in `lane` goes by: its spawn's name, else `subagent-<task id>`.
    pub fn name_of_lane(&self, lane: &str) -> String {
        let Some(id) = lane.strip_prefix(LANE_PREFIX) else {
            return lane.to_string();
        };
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("subagent-{id}"))
    }

    /// Serve `kind` for the subagent called `name` (`None`: this subagent server's own root).
    pub fn serve(&self, name: Option<&str>, kind: &str, payload: &Map<String, Value>) -> Result<Value, String> {
        if !crate::durable_apis::SHARED_RECORDS.contains(&kind) {
            return Err(format!("{kind} is not one of the session's shared records"));
        }
        if let Some(durable) = self.durable.get() {
            return durable.serve_as(name.unwrap_or("root"), kind, payload);
        }
        match &self.upstream {
            Some(upstream) => upstream(name, kind, payload),
            None => Err(format!("{kind}: the session's shared world is not open yet")),
        }
    }
}
