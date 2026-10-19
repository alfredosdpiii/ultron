//! The session's shared world for the REPL (`swarm_api.py`): `swarm.*` (who holds what, durable peer messages),
//! `board.*` (the task board), `notes.*` (shared notes), and persistent children (`rlm.spawn(..., keep=True)`,
//! `rlm.ask`). They are served by `harness::chat::host_ops` over the session's durable runtime, as the calling lane's
//! conversation: `main` is the root conversation, a subagent lane its child conversation, another root conversation
//! its own. Ordinary subagents stay with the delegation host; `rlm.collect`, `rlm.results`, `rlm.cancel` and
//! `rlm.delete_subagent` come here only when every child they name is a persistent one.

use crate::compose::Stack;
use crate::kept_contracts::KeptContracts;
use crate::subagents::{CONVERSATION_LANE_PREFIX, DurableChildren};
use durable::{ConversationId, ROOT_CONVERSATION_ID, TaskId};
use harness::chat::CellCall;
use harness::chat::children::CHILD_KIND;
use harness::chat::host_ops::HostOps;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Weak};

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
    /// Contracts of persistent children (crate::kept_contracts), once the session's stack is known.
    contracts: Option<KeptContracts>,
}

impl SwarmOps {
    pub fn new(children: Arc<DurableChildren>) -> SwarmOps {
        SwarmOps {
            children,
            contracts: None,
        }
    }

    /// Persistent children may be spawned with `role=` / `contract=` / `hop=`: each of their answers is checked.
    pub fn with_contracts(mut self, stack: Weak<Stack>) -> SwarmOps {
        self.contracts = Some(KeptContracts::new(self.children.clone(), stack));
        self
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
        // A kept child's verdict, when it was spawned with a contract (others go to the delegation host).
        if kind == "rlm.finish" {
            return self.contracts.as_ref()?.finish(lane, payload);
        }
        if !self.serves(kind, payload) {
            return None;
        }
        let serve = |kind: &str, payload: &Map<String, Value>| self.serve(lane, kind, payload, signal);
        let Some(contracts) = &self.contracts else {
            return Some(serve(kind, payload));
        };
        Some(match kind {
            "rlm.spawn" => contracts.spawn(payload, &serve).unwrap_or_else(|| serve(kind, payload)),
            "rlm.ask" => {
                let child = payload.get("child").map(|c| match c {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                });
                if let Some(child) = &child {
                    contracts.before_ask(child);
                }
                serve(kind, payload).map(|answer| match &child {
                    Some(child) => contracts.ask_answer(child, answer, lane, signal, &serve),
                    None => answer,
                })
            }
            "rlm.collect" | "rlm.results" => {
                serve(kind, payload).map(|answer| contracts.reported(answer, lane, signal, &serve))
            }
            _ => serve(kind, payload),
        })
    }

    /// Serve `kind` for `lane` from the durable runtime's host operations.
    fn serve(&self, lane: &str, kind: &str, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let chat = self.children.chat().ok_or("The durable runtime is not open")?;
        let conversation = self
            .conversation(lane)
            .ok_or_else(|| format!("The lane {lane} has no conversation"))?;
        let call = CellCall {
            conversation_id: conversation,
            task_id: TaskId(0),
            call_id: String::new(),
            name: "rlm".into(),
            arguments: json!({"lane": lane}),
        };
        HostOps::new(chat)
            .handler(call, None)
            .handle(kind, payload.clone(), signal)
    }
}
