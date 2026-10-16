//! What the agent hub does to a subagent, as session operations over the kernel API (RLM-first: no host-driven
//! chat, the hub's actions are host requests a REPL cell could make too):
//!
//! - steer: `agent_message.send {message, receiver_role: "child", receiver_id, steer: true}` from the root lane,
//!   the human correction of TypeScript's A42 (ultron-a42-intervention-subtree-stop.test.ts): the family module
//!   checks the receiver is a live direct child, records the delivery, and steers the child's lane with the
//!   untrusted-message envelope;
//! - revive: an `rlm.spawn` subagent a dead worker left `interrupted` (the hub shows it parked) starts again from its
//!   brief (prompt, model, thinking, timeout) as a new subagent of the root;
//! - transcript: the messages of the subagent's durable child conversation (`agents.transcript`).
//!
//! In TypeScript `rlm.spawn` children are tasks of the same native host as typed agents, so the family module
//! resolves and steers them. Here they belong to the delegation host, so [`Family`] gives the family module a task
//! host that lists them beside the typed agents', and steers their lanes (and typed agents' lanes) through
//! [`LaneSteer`].

use crate::subagents::LANE_PREFIX;
use delegation::host::Delegation;
use durable::{ConversationId, Cursor, TaskId, TaskOutcome, TaskQuery, TaskState};
use harness::chat::ChatHarness;
use harness::chat::children::{CHILD_KIND, ChildInput};
use kernel::Signal;
use kernel_apis::{ApiModule, FamilyModule, Request, TaskHost};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

/// Queue a steering message into a lane's running conversation; false when the lane has no live run.
pub type LaneSteer = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// The subagent `lane` runs (`ultron.rlm-child.<id>`), when the delegation host knows it.
fn subagent_of_lane(lane: &str, delegation: &Delegation) -> Option<String> {
    let id = lane.strip_prefix(LANE_PREFIX)?;
    let known = delegation
        .tasks()
        .ok()?
        .iter()
        .any(|task| task["id"].as_str() == Some(id));
    known.then(|| id.to_string())
}

/// The family module (`agent_message.*`) over a task host that knows the session's subagents too.
pub struct Family {
    inner: Arc<FamilyModule>,
    delegation: Delegation,
    steer: Arc<OnceLock<LaneSteer>>,
}

impl Family {
    pub fn new(inner: Arc<FamilyModule>, delegation: Delegation, steer: Arc<OnceLock<LaneSteer>>) -> Family {
        Family {
            inner,
            delegation,
            steer,
        }
    }
}

impl ApiModule for Family {
    fn prefixes(&self) -> &[&str] {
        self.inner.prefixes()
    }

    fn start(&self, host: &dyn TaskHost) -> Result<(), String> {
        let tasks = SessionTasks {
            host,
            delegation: &self.delegation,
            steer: self.steer.get(),
        };
        self.inner.start(&tasks)
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        let tasks = SessionTasks {
            host: request.host,
            delegation: &self.delegation,
            steer: self.steer.get(),
        };
        let caller = request
            .caller_task
            .map(str::to_string)
            .or_else(|| subagent_of_lane(request.lane, &self.delegation));
        self.inner.handle(&Request {
            caller_task: caller.as_deref(),
            host: &tasks,
            ..*request
        })
    }
}

/// The typed-agent host with the delegation host's subagents beside its tasks.
struct SessionTasks<'a> {
    host: &'a dyn TaskHost,
    delegation: &'a Delegation,
    steer: Option<&'a LaneSteer>,
}

impl SessionTasks<'_> {
    fn subagent(&self, task_id: &str) -> Option<Value> {
        self.delegation
            .tasks()
            .ok()?
            .into_iter()
            .find(|task| task["id"].as_str() == Some(task_id))
    }
}

impl TaskHost for SessionTasks<'_> {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        self.host
            .caller_task_id(lane)
            .or_else(|| subagent_of_lane(lane, self.delegation))
    }

    fn task_lane(&self, task_id: &str) -> Option<String> {
        self.host
            .task_lane(task_id)
            .or_else(|| self.subagent(task_id).map(|_| format!("{LANE_PREFIX}{task_id}")))
    }

    fn strategy(&self, definition: &str) -> Result<String, String> {
        self.host.strategy(definition)
    }

    fn tasks(&self) -> Result<Vec<Value>, String> {
        let mut tasks = self.host.tasks()?;
        for task in self.delegation.tasks()? {
            if !tasks.iter().any(|known| known["id"] == task["id"]) {
                tasks.push(task);
            }
        }
        Ok(tasks)
    }

    fn spawn(&self, request: &Value, parent: Option<&str>, signal: &Signal) -> Result<Value, String> {
        self.host.spawn(request, parent, signal)
    }

    fn result(&self, task_id: &str) -> Result<Value, String> {
        self.host.result(task_id)
    }

    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
        self.host.cancel(task_id, reason)
    }

    /// A subagent's lane, or a typed agent's the typed-agent host does not steer itself.
    fn steer(&self, task_id: &str, message: &str, signal: &Signal) -> Result<bool, String> {
        if self.subagent(task_id).is_none() && self.host.steer(task_id, message, signal)? {
            return Ok(true);
        }
        let lane = self.task_lane(task_id);
        Ok(match (lane, self.steer) {
            (Some(lane), Some(steer)) => steer(&lane, message),
            _ => false,
        })
    }

    fn usage(&self) -> Result<Option<Value>, String> {
        self.host.usage()
    }

    fn now(&self) -> i64 {
        self.host.now()
    }

    fn pin_lane(&self, lane: &str, holder: &str) -> bool {
        self.host.pin_lane(lane, holder)
    }

    fn unpin_lane(&self, lane: &str, holder: &str) {
        self.host.unpin_lane(lane, holder)
    }
}

/// The durable child task that runs `lane` (finished or not): its id, input and state.
pub fn child_task(chat: &ChatHarness, lane: &str) -> Option<(TaskId, ChildInput, TaskState)> {
    chat.commit(|tx| {
        let query = TaskQuery {
            kind: Some(CHILD_KIND.to_string()),
            ..TaskQuery::default()
        };
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = tx.scan_tasks(&query, 256, cursor.as_ref())?;
            for task in page.items {
                if task.input["name"].as_str() == Some(lane)
                    && let Ok(input) = serde_json::from_value::<ChildInput>(task.input.clone())
                {
                    return Ok(Some((task.id, input, task.state)));
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(None),
            }
        }
    })
    .ok()
    .flatten()
}

/// The conversation a child task owns: the checkpoint's while it runs, the receipt's once it answered, else the
/// conversation created for it.
pub fn child_conversation(chat: &ChatHarness, task: TaskId, state: &TaskState) -> Option<ConversationId> {
    let from = |value: &Value| value["conversation"].as_u64().map(ConversationId);
    let known = match state {
        TaskState::Running { checkpoint }
        | TaskState::Pending { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => from(checkpoint),
        TaskState::Terminal {
            outcome: TaskOutcome::Completed { result },
        } => from(result),
        _ => None,
    };
    known.or_else(|| {
        chat.commit(|tx| {
            let mut cursor: Option<Cursor> = None;
            loop {
                let page =
                    tx.committed_scan_conversations(&durable::ConversationQuery::default(), 256, cursor.as_ref())?;
                if let Some(found) = page
                    .items
                    .iter()
                    .find(|c| c.owner.as_ref().is_some_and(|owner| owner.task_id == task))
                {
                    return Ok(Some(found.id));
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => return Ok(None),
                }
            }
        })
        .ok()
        .flatten()
    })
}
