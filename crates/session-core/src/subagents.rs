//! Subagents on the durable task journal. An `rlm.spawn` child runs as a child conversation of the session's durable
//! runtime (`harness::chat::children`: an `ultron.child` task owning its conversation), so its model calls and
//! cells are as durable as the root's: a worker that dies leaves it to the next one, which resumes it (a pinned
//! request is resent, a running cell reports `interrupted`) and reattaches the delegation host's task to it.
//!
//! The delegation host keeps its task journal in the session, where the TypeScript product keeps it
//! (`createSessionTaskStore`: value `ultron.tasks` / `root`), so finished results stay collectable after a restart,
//! and `rlm.results()`, `rlm.collect` and `rlm.list_subagents()` read the same tasks.

use crate::compose::{AgentStores, Stack, SubagentHost, child_setup};
use crate::durable_store::{SharedSession, lock};
use delegation::agents::{LaneReply, LaneRun, LaneRunner};
use delegation::host::{ChildRun, ChildRunner, ChildWaiter, Delegation};
use delegation::task_store::TaskStore;
use durable::{ConversationId, Cursor, ROOT_CONVERSATION_ID, TaskId, TaskQuery};
use harness::chat::children::{CHILD_KIND, ChildInput, child_report, spawn_child};
use harness::chat::{ChatHarness, ModelRef};
use harness::scheduler::{Cancel, Ctx};
use kernel::Signal;
use serde_json::Value;
use session_journal::Write;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

/// Where the TypeScript product keeps the task journal of a session's task host.
pub const TASKS_NAMESPACE: &str = "ultron.tasks";
pub const TASKS_KEY: &str = "root";
/// The lane a subagent task's cells run in (`ultron.rlm-child.<task id>`), which its child task is named after.
pub const LANE_PREFIX: &str = "ultron.rlm-child.";
/// The lane of another root conversation of the session (`ultron.conversation.<id>`).
pub const CONVERSATION_LANE_PREFIX: &str = "ultron.conversation.";

/// A document kept as one session value (`createSessionTaskStore`, `createSessionDefinitionStore`).
pub struct SessionValueStore {
    pub session: SharedSession,
    pub namespace: &'static str,
    pub key: &'static str,
}

impl TaskStore for SessionValueStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(lock(&self.session).value(self.namespace, self.key).cloned())
    }

    fn write(&mut self, document: &Value) -> Result<(), String> {
        lock(&self.session)
            .commit(&[Write::set(self.namespace, self.key, document.clone())])
            .map(drop)
    }
}

/// The usage ledger's document (`ultron.usage` / `root`), which the operation journal also writes: each run counts
/// its model turns there as it commits them. A ledger write keeps those counts: a root's `turns` are the stored
/// ones when they are ahead of the ledger's.
pub struct SessionUsageStore(pub SharedSession);

impl TaskStore for SessionUsageStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(lock(&self.0).value(USAGE_NAMESPACE, USAGE_KEY).cloned())
    }

    fn write(&mut self, document: &Value) -> Result<(), String> {
        let mut session = lock(&self.0);
        let mut merged = document.clone();
        if let Some(stored) = session.value(USAGE_NAMESPACE, USAGE_KEY).cloned() {
            let count = |root: &Value| root["turns"]["count"].as_f64().unwrap_or(0.0);
            for (id, root) in stored["roots"].as_object().into_iter().flatten() {
                match merged["roots"].get(id) {
                    None if merged["roots"].is_object() => merged["roots"][id] = root.clone(),
                    Some(mine) if count(root) > count(mine) => merged["roots"][id]["turns"] = root["turns"].clone(),
                    _ => {}
                }
            }
        }
        session
            .commit(&[Write::set(USAGE_NAMESPACE, USAGE_KEY, merged)])
            .map(drop)
    }
}

const USAGE_NAMESPACE: &str = "ultron.usage";
const USAGE_KEY: &str = "root";

/// The subagent tasks the stored journal holds unfinished (read before the host reopens it, which records them
/// interrupted).
pub fn unfinished(session: &SharedSession) -> Vec<String> {
    let session = lock(session);
    session
        .value(TASKS_NAMESPACE, TASKS_KEY)
        .and_then(|document| document["tasks"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|task| {
            !matches!(
                task["state"].as_str(),
                Some("completed" | "failed" | "cancelled" | "interrupted")
            )
        })
        .filter_map(|task| task["id"].as_str().map(str::to_string))
        .collect()
}

fn lock_map<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Subagents as child conversations of the session's durable runtime.
#[derive(Default)]
pub struct DurableChildren {
    chat: OnceLock<ChatHarness>,
    /// The system prompt of each lane this process started (a worktree child's names its worktree).
    systems: Mutex<HashMap<String, String>>,
}

impl DurableChildren {
    pub fn new() -> Arc<DurableChildren> {
        Arc::new(DurableChildren::default())
    }

    /// The runtime the children run in, once it is open.
    pub fn attach(&self, chat: ChatHarness) {
        let _ = self.chat.set(chat);
    }

    /// The subagent host of a session: its journal in `session`, its children run durably.
    pub fn host(self: &Arc<Self>, session: SharedSession) -> SubagentHost {
        let children = self.clone();
        let lanes = self.clone();
        let value = |namespace, key| {
            Box::new(SessionValueStore {
                session: session.clone(),
                namespace,
                key,
            }) as Box<dyn TaskStore>
        };
        SubagentHost {
            store: value(TASKS_NAMESPACE, TASKS_KEY),
            runner: Box::new(move |stack: Weak<Stack>| -> ChildRunner {
                Arc::new(move |run: ChildRun| children.run(&stack, run))
            }),
            agents: Some(AgentStores {
                // One host per journal: the typed agents' tasks are a document of their own beside the subagents'.
                tasks: value(TASKS_NAMESPACE, "agents"),
                definitions: value("ultron.definitions", "root"),
                usage: Box::new(SessionUsageStore(session.clone())),
                lanes: Box::new(move |stack: Weak<Stack>| -> LaneRunner {
                    Arc::new(move |run: LaneRun| lanes.run_lane(&stack, run))
                }),
            }),
        }
    }

    /// Start `run` as a child conversation and wait for its answer.
    fn run(&self, stack: &Weak<Stack>, run: ChildRun) -> Result<String, String> {
        let chat = self.chat.get().cloned().ok_or("The durable runtime is not open")?;
        let stack = stack.upgrade().ok_or("The session is closed")?;
        let setup = child_setup(&stack, &run);
        lock_map(&self.systems).insert(run.lane_name.clone(), setup.system);
        let input = ChildInput {
            name: run.lane_name.clone(),
            prompt: run.prompt.clone(),
            model: Some(ModelRef::new(&setup.model.provider, &setup.model.id)),
            timeout_ms: Some(run.timeout_ms),
            keep: false,
        };
        let task = chat
            .commit(|tx| spawn_child(tx, ROOT_CONVERSATION_ID, None, &input))
            .map_err(|e| e.to_string())?;
        let _ = chat.resume();
        wait_child(&chat, task, &run.signal)
    }

    /// A typed agent's `rlm` run, as a child conversation on the lane the task host named.
    fn run_lane(&self, stack: &Weak<Stack>, run: LaneRun) -> Result<LaneReply, String> {
        let chat = self.chat.get().cloned().ok_or("The durable runtime is not open")?;
        let stack = stack.upgrade().ok_or("The session is closed")?;
        lock_map(&self.systems).insert(run.lane.clone(), stack.system().to_string());
        let model = match run.model.as_deref().and_then(|spec| spec.split_once('/')) {
            Some((provider, id)) => ModelRef::new(provider, id),
            None => ModelRef::new(&stack.model().provider, &stack.model().id),
        };
        let input = ChildInput {
            name: run.lane.clone(),
            prompt: run.prompt.clone(),
            model: Some(model),
            timeout_ms: Some(run.timeout_ms),
            keep: false,
        };
        let task = chat
            .commit(|tx| spawn_child(tx, ROOT_CONVERSATION_ID, None, &input))
            .map_err(|e| e.to_string())?;
        let _ = chat.resume();
        let text = wait_child(&chat, task, &run.signal)?;
        Ok(LaneReply {
            text,
            ..LaneReply::default()
        })
    }

    /// The lane of a child conversation: the name of the child task that owns it.
    pub fn lane_of(&self, conversation: ConversationId) -> Option<String> {
        let chat = self.chat.get()?;
        chat.commit(|tx| {
            let Some(record) = tx.conversation(conversation)? else {
                return Ok(None);
            };
            // Another root conversation of the session has a REPL lane of its own.
            let Some(owner) = record.owner else {
                return Ok((conversation != ROOT_CONVERSATION_ID)
                    .then(|| format!("{CONVERSATION_LANE_PREFIX}{}", conversation.0)));
            };
            Ok(tx
                .task(owner.task_id)?
                .filter(|task| task.kind == CHILD_KIND)
                .and_then(|task| task.input["name"].as_str().map(str::to_string)))
        })
        .ok()
        .flatten()
    }

    /// The system prompt a child lane was started with in this process.
    pub fn system_of(&self, lane: &str) -> Option<String> {
        lock_map(&self.systems).get(lane).cloned()
    }

    /// Reopen the delegation host's tasks after a restart, reattaching every unfinished child the runtime still
    /// holds. Returns the reattached task ids.
    pub fn restore(&self, delegation: &Delegation, unfinished: &[String]) -> Result<Vec<String>, String> {
        let Some(chat) = self.chat.get().cloned() else {
            return Ok(Vec::new());
        };
        let children = child_tasks(&chat)?;
        delegation.restore(unfinished, |id| {
            let task = *children.get(&format!("{LANE_PREFIX}{id}"))?;
            let chat = chat.clone();
            Some(Box::new(move |signal: Signal| wait_child(&chat, task, &signal)) as ChildWaiter)
        })
    }
}

/// The runtime's child tasks by name.
fn child_tasks(chat: &ChatHarness) -> Result<HashMap<String, TaskId>, String> {
    chat.commit(|tx| {
        let query = TaskQuery {
            kind: Some(CHILD_KIND.to_string()),
            ..TaskQuery::default()
        };
        let mut found = HashMap::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = tx.scan_tasks(&query, 256, cursor.as_ref())?;
            for task in page.items {
                if let Some(name) = task.input["name"].as_str() {
                    found.insert(name.to_string(), task.id);
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(found),
            }
        }
    })
    .map_err(|e| e.to_string())
}

/// Wait for a child task's answer. Cancelling its task (`signal`: a delete, its timeout, an ancestor's cancel, the
/// host closing) aborts the child's work, as the journal records it cancelled.
fn wait_child(chat: &ChatHarness, task: TaskId, signal: &Signal) -> Result<String, String> {
    let cancel = Cancel::new();
    let done = Cancel::new();
    let (watched, stop, target) = (signal.clone(), done.clone(), cancel.clone());
    // The task's signal has no callback: it is watched until the wait ends.
    let watcher = std::thread::spawn(move || {
        while !stop.is_cancelled() {
            if watched.wait(Duration::from_millis(200)) {
                target.abort();
                return;
            }
        }
    });
    let waited = chat.wait_for_task(task, &Ctx::with_signal(&cancel));
    done.abort();
    let _ = watcher.join();
    match waited {
        Ok(record) => {
            let report = child_report(&record).ok_or("The subagent ended without a result")?;
            let result = &report["result"];
            if result["status"] == "succeeded" {
                Ok(match &result["value"] {
                    Value::String(text) => text.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                })
            } else {
                Err(result["error"]
                    .as_str()
                    .map_or_else(|| format!("The subagent ended {}", result["status"]), str::to_string))
            }
        }
        Err(error) => {
            let reason = signal.reason().unwrap_or_else(|| error.to_string());
            let _ = chat.abort_task(task, &Ctx::background());
            Err(reason)
        }
    }
}
