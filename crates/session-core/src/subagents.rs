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
use delegation::host::{ChildRun, ChildRunner, ChildWaiter, Reattached};
use delegation::task_store::TaskStore;
use durable::{ConversationId, Cursor, ROOT_CONVERSATION_ID, TaskId, TaskQuery};
use harness::chat::children::{CHILD_KIND, ChildInput, child_report, spawn_child};
use harness::chat::{ChatHarness, ModelRef};
use harness::scheduler::{Cancel, Ctx};
use kernel::Signal;
use serde_json::Value;
use session_journal::Write;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

/// Where the TypeScript product keeps the task journal of a session's task host.
pub const TASKS_NAMESPACE: &str = "ultron.tasks";
pub const TASKS_KEY: &str = "root";
/// The lane a subagent task's cells run in (`ultron.rlm-child.<task id>`), which its child task is named after.
pub const LANE_PREFIX: &str = "ultron.rlm-child.";
/// The lane of another root conversation of the session (`ultron.conversation.<id>`).
pub const CONVERSATION_LANE_PREFIX: &str = "ultron.conversation.";
/// The `rlm.spawn(name=...)` of each subagent, a session value per task id (Rust-only: the task journal and the
/// child task keep no name), so a revived subagent keeps its name.
pub const NAMES_NAMESPACE: &str = "ultron.subagent-names";

/// The contract (`role`, `contract`, `hop`) of each subagent spawned with one, a session value per task id as
/// `delegation::host::Delegation::contract_of` describes it (Rust-only, like the names): a child reattached after a
/// restart is still held to it. Sessions without these values reattach their children unchecked, as before.
pub const CONTRACTS_NAMESPACE: &str = "ultron.subagent-contracts";

/// The name subagent `task_id` was spawned with, if this session recorded it.
pub fn subagent_name(session: &SharedSession, task_id: &str) -> Option<String> {
    lock(session)
        .value(NAMES_NAMESPACE, task_id)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A document kept as one session value (`createSessionTaskStore`, `createSessionDefinitionStore`).
pub struct SessionValueStore {
    pub session: SharedSession,
    pub namespace: &'static str,
    pub key: &'static str,
    /// Set once the worker hands the session to a successor: the journal is the successor's from then on, so
    /// what this worker's hosts would still record (a waiter's "Harness is closed" as the closing runtime fails its
    /// wait) is dropped, and the successor finds its tasks unfinished and reattaches them.
    pub handed_over: Option<Arc<AtomicBool>>,
}

impl TaskStore for SessionValueStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(lock(&self.session).value(self.namespace, self.key).cloned())
    }

    fn write(&mut self, document: &Value) -> Result<(), String> {
        if self.handed_over.as_ref().is_some_and(|h| h.load(Ordering::SeqCst)) {
            return Ok(());
        }
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
    /// The usage root of the root turn in progress: a subagent it starts is charged to it.
    root: Mutex<Option<String>>,
    /// The worker handed the session over (`SessionValueStore::handed_over`).
    handed_over: Arc<AtomicBool>,
    /// The session whose transcript is the root lane's history (a forked child of the root starts on it).
    session: OnceLock<SharedSession>,
}

impl DurableChildren {
    pub fn new() -> Arc<DurableChildren> {
        Arc::new(DurableChildren::default())
    }

    /// The worker hands the session over: from now on its task journals are the successor's. Called before the
    /// runtime closes, which ends every wait on it.
    pub fn hand_over(&self) {
        self.handed_over.store(true, Ordering::SeqCst);
    }

    /// The runtime the children run in, once it is open.
    pub fn attach(&self, chat: ChatHarness) {
        let _ = self.chat.set(chat);
    }

    /// The runtime the children run in, once it is open.
    pub fn chat(&self) -> Option<ChatHarness> {
        self.chat.get().cloned()
    }

    /// The subagent host of a session: its journal in `session`, its children run durably.
    pub fn host(self: &Arc<Self>, session: SharedSession) -> SubagentHost {
        let _ = self.session.set(session.clone());
        let children = self.clone();
        let lanes = self.clone();
        let value = |namespace, key| {
            Box::new(SessionValueStore {
                session: session.clone(),
                namespace,
                key,
                handed_over: Some(self.handed_over.clone()),
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
                frames: Arc::new(crate::frames::SessionTraceStore(session.clone())),
            }),
        }
    }

    /// Start `run` as a child conversation and wait for its answer.
    fn run(&self, stack: &Weak<Stack>, run: ChildRun) -> Result<String, String> {
        let chat = self.chat.get().cloned().ok_or("The durable runtime is not open")?;
        let stack = stack.upgrade().ok_or("The session is closed")?;
        let setup = child_setup(&stack, &run);
        lock_map(&self.systems).insert(run.lane_name.clone(), setup.system);
        if let Some(session) = self.session.get()
            && !self.handed_over.load(Ordering::SeqCst)
        {
            let mut writes = vec![Write::set(NAMES_NAMESPACE, &run.task_id, run.name.clone().into())];
            // Kept before the child conversation exists, so a worker that dies after starting it leaves both.
            if let Some(contract) = run.host.contract_of(&run.task_id) {
                writes.push(Write::set(CONTRACTS_NAMESPACE, &run.task_id, contract));
            }
            lock(session).commit(&writes)?;
        }
        let input = ChildInput {
            name: run.lane_name.clone(),
            prompt: run.prompt.clone(),
            model: Some(ModelRef::new(&setup.model.provider, &setup.model.id)),
            timeout_ms: Some(run.timeout_ms),
            keep: false,
            thinking: run.thinking.clone(),
            cwd: run.cwd.clone(),
            env: run.env.clone(),
            agent: false,
            usage_root: lock_map(&self.root).clone(),
            fork: run.fork_at.as_deref().and_then(parse_fork_anchor),
            history: match run
                .fork_at
                .as_deref()
                .and_then(|anchor| anchor.strip_prefix(ROOT_ANCHOR))
            {
                Some(entry) => self.root_history(entry)?,
                None => Vec::new(),
            },
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
        // A reused lane (a retained instance) continues its conversation.
        if run.reuse
            && let Some(conversation) = crate::lane_reuse::lane_conversation(&chat, &run.lane)?
        {
            return crate::lane_reuse::continue_lane(&chat, conversation, &run);
        }
        lock_map(&self.systems).insert(run.lane.clone(), stack.system());
        let model = match run.model.as_deref().and_then(|spec| spec.split_once('/')) {
            Some((provider, id)) => ModelRef::new(provider, id),
            None => ModelRef::new(&stack.model().provider, &stack.model().id),
        };
        let input = ChildInput {
            name: run.lane.clone(),
            prompt: run.prompt.clone(),
            model: Some(model),
            timeout_ms: Some(run.timeout_ms),
            agent: true,
            ..ChildInput::default()
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

    /// `rlm.spawn(fork=True)` of `lane`: where its forked child starts (`forkPoint` over the lane's conversation),
    /// none while it has no finished conversation. The root's history is the session transcript (its runtime
    /// conversation starts afresh every turn): `root:<transcript entry>`; another conversation's is in the runtime:
    /// `<conversation>:<entry>`.
    pub fn fork_point(&self, lane: &str) -> Result<Option<String>, String> {
        if lane == crate::lane::MAIN {
            let entries = self.root_context()?;
            let messages: Vec<(String, Vec<Value>)> = entries
                .iter()
                .map(|entry| {
                    let id = entry["id"].as_str().unwrap_or_default().to_string();
                    (id, session_journal::context::entry_context_messages(entry))
                })
                .collect();
            let at = delegation::fork::fork_point(messages.iter().map(|(id, m)| (id.clone(), m.as_slice())));
            return Ok(at.map(|entry| format!("{ROOT_ANCHOR}{entry}")));
        }
        let chat = self.chat.get().cloned().ok_or("The durable runtime is not open")?;
        let conversation = match lane.strip_prefix(CONVERSATION_LANE_PREFIX) {
            Some(id) => id.parse().ok().map(ConversationId),
            None => self.conversation_of_lane(lane),
        };
        let conversation =
            conversation.ok_or_else(|| format!("rlm.spawn fork=True: lane {lane} has no conversation"))?;
        let handle = chat
            .conversation(conversation)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("rlm.spawn fork=True: lane {lane} has no conversation"))?;
        let entries = handle.entries().map_err(|e| e.to_string())?;
        // From the conversation's newest context start: what came before a reset is not its context.
        let start = entries
            .iter()
            .rposition(|e| e.kind == harness::chat::entries::RESET)
            .map_or(0, |i| i + 1);
        let at = delegation::fork::fork_point(
            entries[start..]
                .iter()
                .map(|entry| (entry.id.0.to_string(), entry.model.as_deref().unwrap_or_default())),
        );
        Ok(at.map(|entry| format!("{}:{entry}", conversation.0)))
    }

    /// The root lane's context entries (from its newest compaction), as the next root request would build them.
    fn root_context(&self) -> Result<Vec<Value>, String> {
        let session = self.session.get().ok_or("The session is not open")?;
        let session = lock(session);
        Ok(session_journal::context::build_context_entries(
            &session.branch_entries(crate::lane::MAIN)?,
        ))
    }

    /// The root's history up to its transcript entry `entry`, as model messages for a forked child.
    fn root_history(&self, entry: &str) -> Result<Vec<Value>, String> {
        let entries = self.root_context()?;
        let end = entries
            .iter()
            .position(|e| e["id"] == entry)
            .ok_or("rlm.spawn fork=True: the fork point is no longer in the root's context")?;
        let applied = session_journal::context::apply_context_edits(&entries[..=end]);
        let messages: Vec<Value> = applied
            .iter()
            .flat_map(session_journal::context::entry_context_messages)
            .collect();
        Ok(crate::lane::to_llm(&messages)
            .into_iter()
            .filter(|message| !matches!(message, ai::Message::System(_)))
            .filter_map(|message| serde_json::to_value(message).ok())
            .collect())
    }

    /// [`Self::fork_point`] as the delegation host's hook.
    pub fn fork_point_hook(self: &Arc<Self>) -> delegation::fork::ForkPointHook {
        let children = Arc::downgrade(self);
        Arc::new(move |lane: &str| match children.upgrade() {
            Some(children) => children.fork_point(lane),
            None => Err("The session is closed".into()),
        })
    }

    /// The usage root of the root turn now running (`None`: the ledger's default root).
    pub fn set_root(&self, root: Option<String>) {
        *lock_map(&self.root) = root;
    }

    /// The usage root of the root turn running now (or last).
    pub fn current_root(&self) -> Option<String> {
        lock_map(&self.root).clone()
    }

    /// The input of the unfinished child task that runs `lane`, and its conversation once started.
    fn child_of_lane(&self, lane: &str) -> Option<(ChildInput, Option<ConversationId>)> {
        let chat = self.chat.get()?;
        chat.commit(|tx| {
            let query = TaskQuery {
                kind: Some(CHILD_KIND.to_string()),
                ..TaskQuery::default()
            };
            let mut cursor: Option<Cursor> = None;
            loop {
                let page = tx.scan_tasks(&query, 256, cursor.as_ref())?;
                for task in page.items {
                    if task.input["name"].as_str() != Some(lane) {
                        continue;
                    }
                    let conversation = match &task.state {
                        durable::TaskState::Running { checkpoint }
                        | durable::TaskState::Pending { checkpoint }
                        | durable::TaskState::Waiting { checkpoint, .. } => checkpoint["conversation"].as_u64(),
                        _ => None,
                    };
                    let input = serde_json::from_value::<ChildInput>(task.input.clone()).ok();
                    return Ok(input.map(|input| (input, conversation.map(ConversationId))));
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

    /// The conversation a child lane runs in, while its task is unfinished.
    pub fn conversation_of_lane(&self, lane: &str) -> Option<ConversationId> {
        self.child_of_lane(lane)?.1
    }

    /// The usage root of work started on a child lane: its task's (a typed agent's reservation, a subagent's root
    /// turn), or the agents host's for a lane it runs.
    pub fn usage_root_of_lane(&self, lane: &str, agents: &delegation::agents::Agents) -> Option<String> {
        match self.child_of_lane(lane) {
            Some((input, _)) if !input.agent => input.usage_root,
            _ => agents.usage_root_for_lane(lane, ""),
        }
    }

    /// The usage root a model turn of `conversation` (not the root conversation) is charged to, as
    /// `usageRootForLane` charges every lane's responses: a typed agent's lane to its task's root, a subagent to the
    /// root turn that started it, another root conversation to the ledger's default root.
    pub fn usage_root(&self, conversation: ConversationId, agents: &delegation::agents::Agents) -> Option<String> {
        let chat = self.chat.get()?;
        let input = chat
            .commit(|tx| {
                let Some(owner) = tx.conversation(conversation)?.and_then(|record| record.owner) else {
                    return Ok(None);
                };
                Ok(tx
                    .task(owner.task_id)?
                    .filter(|task| task.kind == CHILD_KIND)
                    .and_then(|task| serde_json::from_value::<ChildInput>(task.input).ok()))
            })
            .ok()
            .flatten()?;
        if input.agent {
            return agents.usage_root_for_lane(&input.name, "");
        }
        input.usage_root
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
    /// holds: its lane gets back its working directory (a worktree child's), environment and system prompt before
    /// the runtime resumes it. Returns the reattached task ids.
    ///
    /// A typed agent's run (`agents.*`) is not reattached: as in TypeScript (`NativeRlmHost` reopens its journal
    /// with every unfinished task `interrupted`, and its in-process lanes are gone), the agent task reads
    /// `interrupted`, so the runtime's child conversation that ran it is aborted rather than left spending tokens
    /// for a result nobody reads. So is any other unfinished subagent conversation the journal no longer runs.
    pub fn restore(&self, stack: &Stack, unfinished: &[String]) -> Result<Vec<String>, String> {
        let Some(chat) = self.chat.get().cloned() else {
            return Ok(Vec::new());
        };
        let children = child_tasks(&chat)?;
        let reattached = stack.delegation.restore(unfinished, |id| {
            let (task, input, _) = children.get(&format!("{LANE_PREFIX}{id}"))?.clone();
            if input.agent || input.keep {
                return None;
            }
            let run = self.restore_lane(stack, id, &input);
            let chat = chat.clone();
            let (lanes, lane) = (stack.lanes.clone(), input.name.clone());
            let contract = self
                .session
                .get()
                .and_then(|session| lock(session).value(CONTRACTS_NAMESPACE, id).cloned());
            Some(Reattached {
                waiter: Box::new(move |signal: Signal| {
                    let reply = wait_child(&chat, task, &signal);
                    // The lane closes when its run ends, as a fresh child's does.
                    let _ = lanes.close_lane(&lane);
                    reply
                }) as ChildWaiter,
                contract,
                run: Some(run),
            })
        })?;
        for (name, (task, input, terminal)) in &children {
            let ours = !terminal
                && (input.agent
                    || name
                        .strip_prefix(LANE_PREFIX)
                        .is_some_and(|id| !reattached.iter().any(|r| r == id)));
            if ours && !input.keep {
                let _ = chat.abort_task(*task, &Ctx::background());
            }
        }
        Ok(reattached)
    }

    /// The lane setup a reattached child had in the worker that started it: its worktree (working directory and
    /// environment), thinking level and system prompt naming that directory. Returns the run, which a contract's
    /// retry hop repeats.
    fn restore_lane(&self, stack: &Stack, id: &str, input: &ChildInput) -> ChildRun {
        let run = ChildRun {
            task_id: id.to_string(),
            name: self
                .session
                .get()
                .and_then(|session| subagent_name(session, id))
                .unwrap_or_default(),
            lane_name: input.name.clone(),
            prompt: input.prompt.clone(),
            level: 1,
            allowance: 0,
            model: input.model.as_ref().map(|m| format!("{}/{}", m.provider, m.model_id)),
            thinking: input.thinking.clone(),
            timeout_ms: input.timeout_ms.unwrap_or_default(),
            cwd: input.cwd.clone(),
            env: input.env.clone(),
            fork_at: None,
            signal: Signal::new(),
            host: stack.delegation.clone(),
        };
        let setup = child_setup(stack, &run);
        lock_map(&self.systems).insert(input.name.clone(), setup.system);
        run
    }
}

/// A fork anchor (`<conversation>:<entry>`, from [`DurableChildren::fork_point`]) as the child's fork.
fn parse_fork_anchor(anchor: &str) -> Option<harness::chat::children::ChildFork> {
    let (conversation, entry) = anchor.split_once(':')?;
    Some(harness::chat::children::ChildFork {
        conversation: ConversationId(conversation.parse().ok()?),
        entry: durable::EntryId(entry.parse().ok()?),
    })
}

/// The fork anchor prefix of the root lane's transcript entries.
const ROOT_ANCHOR: &str = "root:";

/// The runtime's child tasks by name, with their input and whether they ended.
fn child_tasks(chat: &ChatHarness) -> Result<HashMap<String, (TaskId, ChildInput, bool)>, String> {
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
                let terminal = task.state.status() == durable::TaskStatus::Terminal;
                if let Ok(input) = serde_json::from_value::<ChildInput>(task.input.clone()) {
                    found.insert(input.name.clone(), (task.id, input, terminal));
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
