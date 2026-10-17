//! The session runtime of an external root (session-worker.ts with `externalRoot`): what the session actor wires for
//! Ultron's own root lane, wired for a root lane that never runs because Claude Code owns the conversation.
//!
//! - [`LaneRootHost`]: the root's turns open and close root turns of the typed-agent host (work admitted during a
//!   turn is charged to it), reset the guards' per-turn times and the stuck-loop count, and read the turn's budget;
//! - [`ExternalBranches`]: `ctx.*` on the root lane's branch of the session (appended at once: the root never runs,
//!   so no entry ever waits for a turn boundary);
//! - [`wire`]: the budget line of cell results, the roots of shell jobs, and the ends of shell jobs and detached
//!   tool calls as `<runtime_event>` lines for the root's inbox (the dispatcher's `rootSink`);
//! - [`async_pending`]: the `async.pending` inspection.

use kernel_apis::TaskHost;
use kernel_apis::context_control::ContextBranches;
use serde_json::{Map, Value, json};
use session_core::async_events::{RuntimeEvent, runtime_event_text};
use session_core::compose::Stack;
use session_core::durable_store::SharedSession;
use session_core::lane::{Lane, MAIN};
use session_journal::Write;
use session_journal::session::BRANCH_TIP;
use std::sync::{Arc, Mutex, PoisonError};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The root's turns over a session stack.
pub struct LaneRootHost {
    stack: Arc<Stack>,
    /// The usage root of the turn now open (shell jobs and the budget line of root cells are charged to it).
    root: Arc<Mutex<Option<String>>>,
}

impl LaneRootHost {
    pub fn new(stack: Arc<Stack>) -> LaneRootHost {
        LaneRootHost {
            stack,
            root: Arc::default(),
        }
    }

    /// The usage root of the open turn.
    pub fn current_root(&self) -> Option<String> {
        lock(&self.root).clone()
    }
}

impl crate::external_root::RootHost for LaneRootHost {
    fn begin_root_turn(&self, turn: &str) {
        self.stack.agents.begin_root_turn(turn);
        *lock(&self.root) = Some(self.root_id_of_run(turn));
    }

    fn end_root_turn(&self, turn: &str) {
        self.stack.agents.end_root_turn(turn);
        let root = self.root_id_of_run(turn);
        let mut current = lock(&self.root);
        if current.as_deref() == Some(root.as_str()) {
            *current = None;
        }
    }

    fn root_id_of_run(&self, turn: &str) -> String {
        self.stack
            .agents
            .usage_root_for_lane(MAIN, turn)
            .unwrap_or_else(|| format!("turn:{turn}"))
    }

    fn pending_root_notifications(&self) -> u64 {
        pending_root_notifications(&self.stack)
    }

    fn turn_budget_exhausted(&self, root: &str) -> Option<String> {
        self.stack.usage.turn_budget_exhausted(Some(root)).ok().flatten()
    }

    fn run_ended(&self, lane: &str) {
        self.stack.cell_hints().run_ended(lane);
    }

    fn begin_file_turn(&self) {
        self.stack.begin_turn();
    }
}

/// `pendingRootNotifications`: subagents and root-level tasks the root started that have not ended.
pub fn pending_root_notifications(stack: &Stack) -> u64 {
    let subagents = stack.delegation.running(MAIN).len();
    let tasks = stack
        .agents
        .tasks()
        .unwrap_or_default()
        .iter()
        .filter(|task| task["parentId"].is_null() && task["result"].is_null())
        .count();
    (subagents + tasks) as u64
}

/// The root lane's branch of the session, for `ctx.*`.
pub struct ExternalBranches {
    pub session: SharedSession,
}

fn root_only(lane: &str) -> Result<(), String> {
    if lane == MAIN {
        Ok(())
    } else {
        Err(format!("ctx edits the root lane's context only (lane {lane})"))
    }
}

impl ContextBranches for ExternalBranches {
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        root_only(lane)?;
        lock(&self.session).branch_entries(MAIN)
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        root_only(lane)?;
        let mut session = lock(&self.session);
        let id = session_journal::lock_clock(&session.clock()).next_id();
        let tip = session.branch_tip(MAIN).flatten();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
        entry.insert("type".into(), "custom".into());
        entry.insert("customType".into(), custom_type.into());
        entry.insert("data".into(), data.clone());
        session.commit(&[Write::Entry(entry), Write::set(BRANCH_TIP, MAIN, id.clone().into())])?;
        Ok(id)
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        root_only(lane)?;
        Ok(lock(&self.session).branch_tip(MAIN).flatten())
    }
}

/// Wire an external root's lane: `ctx.*` over `session`, the budget line, shell jobs' roots, and the ends of the
/// root's shell jobs and detached tool calls into `inbox` (unless `ULTRON_ASYNC_EVENTS` turned events off).
pub fn wire(lane: &Lane, session: SharedSession, host: &LaneRootHost, inbox: Arc<Mutex<Vec<String>>>) {
    let stack = &lane.stack;
    let context = Arc::new(kernel_apis::ContextControl::new(Arc::new(ExternalBranches { session })));
    let _ = stack.context.set(context);
    // `budgetLine(await usage.remaining(host.usageRootForLane(lane, ...)))`: the root's cells are charged to the turn
    // that is open, another lane's to its task's root.
    let (usage, agents, root) = (stack.usage.clone(), stack.agents.clone(), host.root.clone());
    let explicit_token_cap = usage.limits().max_total_tokens.is_some();
    let _ = stack.budget_note.set(Arc::new(move |lane: &str| {
        let root = if lane == MAIN {
            lock(&root).clone()
        } else {
            agents.usage_root_for_lane(lane, "")
        };
        let remaining = usage.remaining(root.as_deref()).ok()?;
        delegation::usage::budget_line(&remaining, explicit_token_cap)
    }));
    // A root's shell job belongs to the turn that started it.
    let roots = host.root.clone();
    stack.host.set_roots(Arc::new(
        move |lane: &str| if lane == MAIN { lock(&roots).clone() } else { None },
    ));
    if !stack.events.enabled() {
        return;
    }
    let jobs = inbox.clone();
    stack.host.on_job_end(Arc::new(move |end: host::JobEnd| {
        if end.record.lane != MAIN {
            return;
        }
        let Some(event) = end.event() else { return };
        lock(&jobs).push(runtime_event_text(&[RuntimeEvent {
            kind: event["kind"].as_str().unwrap_or("job_done").into(),
            id: end.record.id.clone(),
            status: end.record.status.clone(),
            summary: event["summary"].as_str().unwrap_or_default().into(),
            fetch: event["fetch"].as_str().unwrap_or_default().into(),
            lane: end.record.lane.clone(),
            root_id: end.record.root_id.clone(),
        }]));
    }));
    let tools = inbox;
    stack
        .tool_calls()
        .on_end(Arc::new(move |end: kernel_apis::ToolCallEnd| {
            if end.call["lane"].as_str().unwrap_or(MAIN) != MAIN {
                return;
            }
            let Some((id, status, summary, fetch)) = kernel_apis::tool_calls::tool_event(&end) else {
                return;
            };
            lock(&tools).push(runtime_event_text(&[RuntimeEvent {
                kind: "tool_done".into(),
                id,
                status,
                summary,
                fetch,
                lane: MAIN.into(),
                root_id: end.call["rootId"].as_str().map(str::to_string),
            }]));
        }));
}

/// `async.pending` of a session whose root is driven from outside: root-owned work still running. The root lane
/// never runs and its events go straight to the root's inbox, so `running` and `events` are 0.
pub fn async_pending(stack: &Stack) -> Value {
    if !stack.events.enabled() {
        return json!({"pending": false, "jobs": 0, "tasks": 0, "events": 0});
    }
    let none = |_: Option<&str>| false;
    let jobs = stack.host.running(MAIN, &none) as u64;
    let tools = stack.tool_calls().running(MAIN, &none) as u64;
    let tasks = pending_root_notifications(stack);
    json!({
        "pending": jobs + tools + tasks > 0,
        "jobs": jobs,
        "tools": tools,
        "tasks": tasks,
        "events": 0,
        "running": 0,
    })
}
