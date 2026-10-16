//! The session's shared world for the REPL (`swarm_api.py`): `swarm.*` (who holds what, durable peer messages),
//! `board.*` (the task board), `notes.*` (shared notes), and persistent children (`rlm.spawn(..., keep=True)`,
//! `rlm.ask`). They are served by `harness::chat::host_ops` over the session's durable runtime, as the calling lane's
//! conversation: `main` is the root conversation, a subagent lane its child conversation, another root conversation
//! its own. Ordinary subagents stay with the delegation host; `rlm.collect`, `rlm.results`, `rlm.cancel` and
//! `rlm.delete_subagent` come here only when every child they name is a persistent one.

use crate::subagents::{CONVERSATION_LANE_PREFIX, DurableChildren};
use durable::{ConversationId, ROOT_CONVERSATION_ID, TaskId};
use harness::chat::CellCall;
use harness::chat::children::CHILD_KIND;
use harness::chat::host_ops::HostOps;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Requests that always come here.
const SHARED: [&str; 13] = [
    "swarm.register",
    "swarm.agents",
    "swarm.route",
    "swarm.send",
    "swarm.inbox",
    "board.add",
    "board.claim",
    "board.update",
    "board.list",
    "notes.read",
    "notes.append",
    "notes.write",
    "rlm.ask",
];

/// Child requests that come here when every child they name is persistent.
const CHILD_REQUESTS: [&str; 4] = ["rlm.collect", "rlm.results", "rlm.cancel", "rlm.delete_subagent"];

pub struct SwarmOps {
    children: Arc<DurableChildren>,
}

impl SwarmOps {
    pub fn new(children: Arc<DurableChildren>) -> SwarmOps {
        SwarmOps { children }
    }

    /// The conversation `lane` works in.
    fn conversation(&self, lane: &str) -> Option<ConversationId> {
        if lane == crate::lane::MAIN {
            return Some(ROOT_CONVERSATION_ID);
        }
        if let Some(id) = lane.strip_prefix(CONVERSATION_LANE_PREFIX) {
            return id.parse().ok().map(ConversationId);
        }
        self.children.conversation_of_lane(lane)
    }

    /// Whether `selector` names a persistent child of the durable runtime.
    fn persistent(&self, selector: &Value) -> bool {
        let id = match selector {
            Value::String(text) => text.parse::<u64>().ok(),
            Value::Number(number) => number.as_u64(),
            _ => None,
        };
        let Some(chat) = self.children.chat() else { return false };
        id.and_then(|id| chat.get_task(TaskId(id)).ok().flatten())
            .is_some_and(|task| task.kind == CHILD_KIND && task.input["keep"] == true)
    }

    /// Whether `kind` with `payload` is served here.
    fn serves(&self, kind: &str, payload: &Map<String, Value>) -> bool {
        if SHARED.contains(&kind) {
            return true;
        }
        if kind == "rlm.spawn" {
            let keep = |v: Option<&Value>| v == Some(&Value::Bool(true));
            return keep(payload.get("keep")) || keep(payload.get("kwargs").and_then(|k| k.get("keep")));
        }
        if CHILD_REQUESTS.contains(&kind) {
            let named: Vec<&Value> = match (payload.get("selectors"), payload.get("selector").or(payload.get("id"))) {
                (Some(Value::Array(list)), _) => list.iter().collect(),
                (_, Some(one)) => vec![one],
                _ => Vec::new(),
            };
            return !named.is_empty() && named.iter().all(|s| self.persistent(s));
        }
        false
    }

    /// Serve `kind` for `lane`; `None` when it is not served here.
    pub fn handle(
        &self,
        lane: &str,
        kind: &str,
        payload: &Map<String, Value>,
        signal: &Signal,
    ) -> Option<Result<Value, String>> {
        if !self.serves(kind, payload) {
            return None;
        }
        let Some(chat) = self.children.chat() else {
            return Some(Err("The durable runtime is not open".into()));
        };
        let Some(conversation) = self.conversation(lane) else {
            return Some(Err(format!("The lane {lane} has no conversation")));
        };
        let payload = payload.clone();
        let call = CellCall {
            conversation_id: conversation,
            task_id: TaskId(0),
            call_id: String::new(),
            name: "rlm".into(),
            arguments: json!({"lane": lane}),
        };
        Some(HostOps::new(chat).handler(call, None).handle(kind, payload, signal))
    }
}
