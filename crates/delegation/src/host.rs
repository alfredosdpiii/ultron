//! The `rlm.spawn` / `rlm.collect` / `rlm.finish` / `rlm.list_subagents` / `rlm.delete_subagent` host requests of
//! `native-host.ts`, for subagents run by a [`ChildRunner`] (its `externalChild` path): each child runs on a thread of
//! its own with its own lane name, so its host requests (its `rlm.finish`, nested spawns) arrive through
//! [`Delegation::lane_handler`] like any kernel's.
//!
//! Waits are host-side. With [`CollectMode::Host`] a cell's `rlm.collect` never waits (it fails at once naming the
//! running children) and the turn scheduler waits between turns ([`Delegation::running`], [`Delegation::wait`], the
//! [`DetachedEnd`] channel). With [`CollectMode::Wait`] (TS parity) the wait blocks only the request's host thread,
//! ends when the cell is cancelled, and reports itself through [`CellContext::park`] so the lane detaches the cell.
//! A detached child's end is pushed to the [`DetachedEnd`] channel as soon as its result is durable.

use crate::task_store::{MemoryStore, TaskJournal, TaskStore, task_fingerprint};
use crate::verdict::{MAX_VERDICT_REJECTIONS, validate_verdict};
use crate::workspace::{
    DEFAULT_MAX_SNAPSHOT_ENTRIES, WorkspaceSnapshot, check_files, diff_snapshots, snapshot_workspace,
};
use crate::worktrees::{self as wt, WorktreeRecord, WorktreeSetup};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const CHILD: &str = "rlm-child@1";
const DEFAULT_TIMEOUT_MS: f64 = 30.0 * 60.0 * 1000.0;
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// Default ceiling on `rlm.spawn` nesting (`ULTRON_SPAWN_DEPTH`).
pub const DEFAULT_SPAWN_DEPTH: u32 = 3;

/// `spawnDepthLimit`: `ULTRON_SPAWN_DEPTH` as a nonnegative integer, else the default; 0 means no ceiling.
pub fn spawn_depth_limit(raw: Option<&str>) -> u32 {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.fract() == 0.0 && *v >= 0.0 && *v <= 9_007_199_254_740_991.0)
        .map_or(DEFAULT_SPAWN_DEPTH, |v| v.min(f64::from(u32::MAX)) as u32)
}

/// One subagent run (`NativeExternalChildRun`).
pub struct ChildRun {
    pub task_id: String,
    /// The lane the child acts as: its host requests go through `host.lane_handler(&lane_name)`.
    pub lane_name: String,
    pub prompt: String,
    /// Nesting levels above the child (1 for a child of the root) and how many more it may create.
    pub level: u32,
    pub allowance: u32,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub timeout_ms: u64,
    /// A worktree child's (or its descendant's) working directory and extra environment; `None` / empty: the host's.
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Aborted when the child is cancelled (delete, timeout, ancestor cancelled, host closed).
    pub signal: Signal,
    pub host: Delegation,
}

/// Runs a child to its final reply text, or fails with an error message.
pub type ChildRunner = Arc<dyn Fn(ChildRun) -> Result<String, String> + Send + Sync>;

/// A detached task reached its durable terminal result (`DetachedTaskEnd`).
#[derive(Clone, Debug, PartialEq)]
pub struct DetachedEnd {
    pub task: Value,
    pub kind: &'static str,
    pub owner_lane: String,
    /// A request was waiting on the result when it ended.
    pub awaited: bool,
    /// The kernel call that returns the full result.
    pub fetch: String,
}

/// Called with `true` when a request starts waiting host-side on children and `false` when that wait ends: how a
/// lane learns its cell is parked (see `lanes.rs`).
pub type Park = Arc<dyn Fn(bool) + Send + Sync>;

/// What a request knows about the cell that made it.
#[derive(Clone, Default)]
pub struct CellContext {
    /// Aborted when the cell ends or is cancelled: ends a waiting `rlm.collect` with an error.
    pub cancel: Option<Signal>,
    pub park: Option<Park>,
}

/// How `rlm.collect` treats children that are still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectMode {
    /// As `native-host.ts`: the request waits (on its own host thread, cancelled with its cell) for every child.
    Wait,
    /// The cell never waits: with every selected child ended it returns their results; otherwise it fails at once,
    /// naming the running children, and the host waits instead (their ends arrive as `child_done` events).
    Host,
}

pub struct DelegationOptions {
    pub store: Box<dyn TaskStore>,
    pub collect: CollectMode,
    pub runner: ChildRunner,
    /// See [`spawn_depth_limit`].
    pub spawn_depth: u32,
    /// Model for spawns that name none.
    pub child_model: Option<String>,
    pub now: Box<dyn Fn() -> f64 + Send + Sync>,
    /// The working directory subagents share: snapshotted when a child starts and ends to check its verdict's
    /// `changed_files`, and the tree worktree children branch from and merge into.
    pub workspace: Option<PathBuf>,
    /// Names worktree branches (`ultron/<session>/<child>`).
    pub session_id: String,
    /// Default worktree setup (the `worktrees` setting); a spawn's `worktree_setup=` overrides it key by key.
    pub worktree_setup: Option<WorktreeSetup>,
    /// `ULTRON_KEEP_WORKTREES`: keep worktrees and branches after merges and on close.
    pub keep_worktrees: bool,
}

impl DelegationOptions {
    /// Defaults: in-memory journal, waiting collect, depth 3, no workspace, session "session", a zero clock.
    pub fn new(runner: ChildRunner) -> Self {
        DelegationOptions {
            store: Box::new(MemoryStore::default()),
            collect: CollectMode::Wait,
            runner,
            spawn_depth: DEFAULT_SPAWN_DEPTH,
            child_model: None,
            now: Box::new(|| 0.0),
            workspace: None,
            session_id: "session".into(),
            worktree_setup: None,
            keep_worktrees: false,
        }
    }
}

struct Task {
    id: String,
    parent_id: Option<String>,
    record: Value,
    outcome: Option<Result<Value, String>>,
    spawn_depth: u32,
    lane_name: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    verdict: Option<Value>,
    verdict_rejections: u32,
    verdict_problems: Vec<String>,
    verdict_invalid: bool,
    waiters: usize,
    signal: Signal,
    /// A terminal result is being finalized (worktree commit) before it is committed.
    finishing: bool,
    worktree: Option<WorktreeRecord>,
    /// The worktree's files when the child started (after setup), and the parent's tree then.
    worktree_baseline: Option<WorkspaceSnapshot>,
    outside_before: Option<WorkspaceSnapshot>,
    /// The child's working directory when it started (its verdict check's "before").
    before: Option<WorkspaceSnapshot>,
    merge_status: Option<String>,
}

struct State {
    journal: TaskJournal,
    tasks: Vec<Task>,
    lane_tasks: HashMap<String, String>,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    runner: ChildRunner,
    collect: CollectMode,
    events: Mutex<Sender<DetachedEnd>>,
    spawn_depth: u32,
    child_model: Option<String>,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    workspace: Option<PathBuf>,
    session_id: String,
    worktree_setup: Option<WorktreeSetup>,
    keep_worktrees: bool,
    /// `rlm.merge` calls run one at a time: each writes into a working tree.
    merging: Mutex<()>,
}

/// `worktreeChanges`: Git's changes in a worktree minus files unchanged since the child started (its setup command's
/// output); `None` lets the commit take every change.
fn worktree_changes(record: &WorktreeRecord, baseline: Option<&WorkspaceSnapshot>) -> Option<Vec<String>> {
    let baseline = baseline.filter(|b| b.complete)?;
    let now = snapshot_workspace(&baseline.root, DEFAULT_MAX_SNAPSHOT_ENTRIES, true);
    if !now.complete {
        return None;
    }
    let status = wt::list_dirty(Path::new(&record.path))?;
    let prefix = wt::relative_inside(Path::new(&record.path), &baseline.root).unwrap_or_default();
    Some(
        status
            .into_iter()
            .filter(|path| {
                let inside = if prefix.is_empty() {
                    Some(path.as_str())
                } else {
                    path.strip_prefix(&format!("{prefix}/"))
                };
                // Outside the pictured directory nothing is known: keep it.
                inside.is_none_or(|inside| baseline.entries.get(inside) != now.entries.get(inside))
            })
            .collect(),
    )
}

/// The delegation host; clones share it.
#[derive(Clone)]
pub struct Delegation {
    inner: Arc<Inner>,
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

fn nonempty_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}

fn safe_integer(value: &Value) -> Option<i64> {
    value
        .as_f64()
        .filter(|v| v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0)
        .map(|v| v as i64)
}

/// `worktreeSetupOption`: a spawn's `worktree_setup=`, checked.
fn check_worktree_setup(value: Option<&Value>) -> Result<Option<WorktreeSetup>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let setup = value.as_object().ok_or("agent input must be a JSON object")?;
    fields(setup, &["link", "copy", "command", "timeout_ms"])?;
    for name in ["link", "copy"] {
        let Some(item) = setup.get(name).filter(|v| !v.is_null()) else {
            continue;
        };
        let ok = item
            .as_array()
            .is_some_and(|paths| paths.iter().all(|p| p.as_str().is_some_and(|s| !s.trim().is_empty())));
        if !ok {
            return Err(format!("worktree_setup.{name} must be a list of relative paths"));
        }
    }
    if setup.get("command").is_some_and(|c| !c.is_null() && !c.is_string()) {
        return Err("worktree_setup.command must be a string".into());
    }
    if setup
        .get("timeout_ms")
        .is_some_and(|t| safe_integer(t).is_none_or(|n| n < 1))
    {
        return Err("worktree_setup.timeout_ms must be a positive integer".into());
    }
    let paths = |name: &str| {
        setup
            .get(name)
            .filter(|v| !v.is_null())
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
    };
    Ok(Some(WorktreeSetup {
        link: paths("link"),
        copy: paths("copy"),
        command: setup
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(String::from),
        timeout_ms: setup.get("timeout_ms").and_then(Value::as_u64),
    }))
}

/// `worktreeMode`: off, on, or "auto" (falls back to the shared tree outside Git).
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorktreeMode {
    Off,
    On,
    Auto,
}

/// The setting's setup, overridden key by key by the spawn's.
fn merged_setup(default: Option<&WorktreeSetup>, spawn: Option<&WorktreeSetup>) -> WorktreeSetup {
    let base = default.cloned().unwrap_or_default();
    let Some(spawn) = spawn else {
        return base;
    };
    WorktreeSetup {
        link: spawn.link.clone().or(base.link),
        copy: spawn.copy.clone().or(base.copy),
        command: spawn.command.clone().or(base.command),
        timeout_ms: spawn.timeout_ms.or(base.timeout_ms),
    }
}

fn public_task(task: &Task) -> Value {
    let mut out = json!({ "id": task.id, "definition": task.record["definition"], "state": task.record["state"] });
    if let Some(parent) = &task.parent_id {
        out["parentId"] = json!(parent);
    }
    if let Some(Ok(result)) = &task.outcome {
        out["result"] = result.clone();
    }
    out
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    fn is_child(&self, id: &str) -> bool {
        self.task(id).is_some_and(|t| t.record["definition"] == CHILD)
    }
    /// The chain of task ids from `id` up through its ancestors.
    fn line(&self, id: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = id.map(String::from);
        while let Some(id) = current {
            current = self.task(&id).and_then(|t| t.parent_id.clone());
            out.push(id);
        }
        out
    }
    /// The nearest worktree of a task and its ancestors: a child without its own works in its parent's.
    fn worktree_of(&self, id: Option<&str>) -> Option<&WorktreeRecord> {
        self.line(id)
            .iter()
            .find_map(|id| self.task(id).and_then(|t| t.worktree.as_ref()))
    }
    /// The directory a task works in: its (or an ancestor's) worktree, else the shared workspace.
    fn workspace_of(&self, id: Option<&str>, shared: Option<&Path>) -> Option<PathBuf> {
        self.worktree_of(id)
            .map(|w| PathBuf::from(&w.cwd))
            .or_else(|| shared.map(Path::to_path_buf))
    }
    /// A child lane sees only its own task and descendants; the root sees everything.
    fn visible(&self, id: &str, parent: Option<&str>) -> bool {
        parent.is_none_or(|parent| self.line(Some(id)).iter().any(|a| a == parent))
    }
}

impl Delegation {
    pub fn new(options: DelegationOptions) -> (Self, Receiver<DetachedEnd>) {
        let (sender, receiver) = channel();
        let host = Delegation {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    journal: TaskJournal::new(options.store),
                    tasks: Vec::new(),
                    lane_tasks: HashMap::new(),
                    closed: false,
                }),
                changed: Condvar::new(),
                runner: options.runner,
                collect: options.collect,
                events: Mutex::new(sender),
                spawn_depth: options.spawn_depth,
                child_model: options.child_model,
                now: options.now,
                workspace: options.workspace,
                session_id: options.session_id,
                worktree_setup: options.worktree_setup,
                keep_worktrees: options.keep_worktrees,
                merging: Mutex::new(()),
            }),
        };
        (host, receiver)
    }

    /// The kernel host handler for `lane` ("main" for the root; a child's `lane_name`).
    pub fn lane_handler(&self, lane: &str) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        move |kind: &str, payload: Map<String, Value>, signal: &Signal| {
            let cell = CellContext {
                cancel: Some(signal.clone()),
                park: None,
            };
            host.handle_with(kind, Value::Object(payload), &lane, &cell)
        }
    }

    /// The host handler for one cell of `lane`: its waits end with `signal` (instead of the kernel's request signal)
    /// and report themselves through `park`.
    pub fn cell_handler(&self, lane: &str, signal: Signal, park: Park) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        let cell = CellContext {
            cancel: Some(signal),
            park: Some(park),
        };
        move |kind: &str, payload: Map<String, Value>, _signal: &Signal| {
            host.handle_with(kind, Value::Object(payload), &lane, &cell)
        }
    }

    /// Serve one host request from `lane`.
    pub fn handle(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        self.handle_with(kind, payload, lane, &CellContext::default())
    }

    /// Serve one host request from `lane`, made by `cell`.
    pub fn handle_with(&self, kind: &str, payload: Value, lane: &str, cell: &CellContext) -> Result<Value, String> {
        let cancel = cell.cancel.as_ref();
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        let parent = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err("Ultron task host is closed".into());
            }
            state.journal.list()?;
            state.lane_tasks.get(lane).cloned()
        };
        let parent = parent.as_deref();
        match kind {
            "rlm.spawn" => self.spawn(&payload, parent),
            "rlm.finish" => self.finish_request(payload, parent),
            "rlm.list_subagents" => {
                fields(&payload, &[])?;
                let state = lock(&self.inner.state);
                let subagents: Vec<Value> = state
                    .tasks
                    .iter()
                    .filter(|t| t.record["definition"] == CHILD && Some(t.id.as_str()) != parent)
                    .filter(|t| state.visible(&t.id, parent))
                    .map(public_task)
                    .collect();
                Ok(json!({ "subagents": subagents }))
            }
            "rlm.collect" | "rlm.results" => {
                let ids = self.selected(&payload, parent)?;
                let waits = kind == "rlm.collect" && self.inner.collect == CollectMode::Wait;
                if !waits {
                    let running = self.unfinished(&ids);
                    if kind == "rlm.collect" && !running.is_empty() {
                        return Err(format!(
                            "rlm.collect: {} subagent(s) still running ({}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again.",
                            running.len(),
                            running.join(", ")
                        ));
                    }
                    let mut out = self.collected(self.wait_until(&ids, Some(Duration::ZERO), None))?;
                    if kind == "rlm.results" {
                        out["running"] = json!(running);
                    }
                    return Ok(out);
                }
                let park = cell.park.as_ref().filter(|_| !self.unfinished(&ids).is_empty());
                if let Some(park) = park {
                    park(true);
                }
                let outcomes = self.wait_until(&ids, None, cancel);
                if let Some(park) = park {
                    park(false);
                }
                if outcomes.len() < ids.len() {
                    return Err(cancel
                        .and_then(Signal::reason)
                        .map_or("rlm.collect was cancelled".into(), |r| {
                            format!("rlm.collect was cancelled: {r}")
                        }));
                }
                self.collected(outcomes)
            }
            "rlm.delete_subagent" => {
                fields(&payload, &["selector"])?;
                let selector = nonempty_string(payload.get("selector"), "selector")?;
                let known = {
                    let state = lock(&self.inner.state);
                    state.is_child(&selector) && state.visible(&selector, parent)
                };
                if !known {
                    return Err("Unknown RLM child".into());
                }
                let result = self.cancel(&selector, "RLM child deleted")?;
                Ok(json!({ "deleted": result["status"] == "cancelled" }))
            }
            "rlm.merge" => self.merge_request(&payload, parent, cell),
            "rlm.worktrees.list" => {
                fields(&payload, &["all"])?;
                let shared = self.inner.workspace.as_deref();
                let from = lock(&self.inner.state)
                    .workspace_of(parent, shared)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_default();
                let state = lock(&self.inner.state);
                let worktrees: Vec<Value> = wt::list_records(&from)
                    .into_iter()
                    .filter(|r| payload.get("all") == Some(&Value::Bool(true)) || r.session_id == self.inner.session_id)
                    .map(|r| {
                        let running = r
                            .task_id
                            .as_deref()
                            .and_then(|t| state.task(t))
                            .is_some_and(|t| t.outcome.is_none());
                        json!({ "id": r.id, "task": r.task_id, "name": r.name, "branch": r.branch, "path": r.path,
                                "state": r.state, "commit": r.commit.clone().flatten(), "running": running,
                                "session": r.session_id })
                    })
                    .collect();
                Ok(json!({ "worktrees": worktrees }))
            }
            "rlm.worktrees.cleanup" => {
                fields(&payload, &["branches"])?;
                let workspace = lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
                let Some(workspace) = workspace else {
                    return Ok(json!({ "removed": [], "kept_branches": [], "running": [] }));
                };
                let running = |record: &WorktreeRecord| {
                    let state = lock(&self.inner.state);
                    record
                        .task_id
                        .as_deref()
                        .and_then(|t| state.task(t))
                        .is_some_and(|t| t.outcome.is_none())
                };
                let branches = payload.get("branches") == Some(&Value::Bool(true));
                let session = self.inner.session_id.clone();
                let records: Vec<WorktreeRecord> = wt::list_records(&workspace)
                    .into_iter()
                    .filter(|r| r.session_id == session)
                    .collect();
                let select = |r: &WorktreeRecord| r.session_id == session && !running(r);
                let own = wt::prune_worktrees(
                    &workspace,
                    &wt::PruneOptions {
                        select: Some(&select),
                        force: true,
                        delete_branches: branches,
                    },
                );
                let orphans = wt::prune_worktrees(
                    &workspace,
                    &wt::PruneOptions {
                        select: None,
                        force: false,
                        delete_branches: branches,
                    },
                );
                let removed = [own.removed, orphans.removed].concat();
                let kept = [own.kept_branches, orphans.kept_branches].concat();
                let still: Vec<String> = records
                    .iter()
                    .filter(|r| running(r))
                    .map(|r| r.branch.clone())
                    .collect();
                Ok(json!({ "removed": removed, "kept_branches": kept, "running": still }))
            }
            other => Err(format!("Unknown host request: {other}")),
        }
    }

    /// `rlm.merge`: the children end first (a host-side wait, cancelled with the cell), then each child's branch is
    /// merged, in the given order and one merge call at a time, into the caller's working tree.
    fn merge_request(
        &self,
        payload: &Map<String, Value>,
        parent: Option<&str>,
        cell: &CellContext,
    ) -> Result<Value, String> {
        fields(payload, &["selectors", "on_conflict", "include_failed"])?;
        let on_conflict = match payload.get("on_conflict") {
            None => "stop",
            Some(Value::String(s)) if ["stop", "markers", "skip"].contains(&s.as_str()) => s.as_str(),
            Some(_) => return Err("on_conflict must be \"stop\", \"skip\" or \"markers\"".into()),
        };
        let selectors = match payload.get("selectors").and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .map(|s| nonempty_string(Some(s), "selector"))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let children: Vec<String> = {
            let state = lock(&self.inner.state);
            if selectors.is_empty() {
                // Default: the caller's own worktree children, in the order they were spawned.
                state
                    .tasks
                    .iter()
                    .filter(|t| t.record["definition"] == CHILD && t.parent_id.as_deref() == parent)
                    .filter(|t| {
                        t.worktree.is_some() || matches!(&t.outcome, Some(Ok(result)) if !result["worktree"].is_null())
                    })
                    .map(|t| t.id.clone())
                    .collect()
            } else {
                for id in &selectors {
                    if !state.is_child(id) || Some(id.as_str()) == parent || !state.visible(id, parent) {
                        return Err(format!("Unknown RLM child: {id}"));
                    }
                }
                selectors.clone()
            }
        };
        if (1..children.len()).any(|i| children[..i].contains(&children[i])) {
            return Err("rlm.merge: a child is listed twice".into());
        }
        // Every child ends first.
        let running = self.unfinished(&children);
        if let Some(park) = cell.park.as_ref().filter(|_| !running.is_empty()) {
            park(true);
        }
        let ended = self.wait_until(&children, None, cell.cancel.as_ref());
        if let Some(park) = cell.park.as_ref().filter(|_| !running.is_empty()) {
            park(false);
        }
        if ended.len() < children.len() {
            return Err(cell
                .cancel
                .as_ref()
                .and_then(Signal::reason)
                .map_or("rlm.merge was cancelled".into(), |r| {
                    format!("rlm.merge was cancelled: {r}")
                }));
        }
        let target = lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let Some(target) = target else {
            return Err("rlm.merge needs a working directory, and this session has none".into());
        };
        let _serial = self.inner.merging.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(self.merge_children(
            &children,
            &target,
            on_conflict,
            payload.get("include_failed") == Some(&Value::Bool(true)),
        ))
    }

    /// `mergeChildren`: the first conflict stops the rest (`skip` goes on); nothing of a conflicting child is written
    /// unless `markers`. A merged or empty child's worktree and branch are removed; a conflicting one is kept.
    fn merge_children(&self, children: &[String], target: &Path, on_conflict: &str, include_failed: bool) -> Value {
        let mut results: Vec<Value> = Vec::new();
        let mut stopped_at: Option<String> = None;
        for id in children {
            let (result, record, merge_status) = {
                let state = lock(&self.inner.state);
                let task = state.task(id);
                (
                    task.and_then(|t| t.outcome.clone()).and_then(Result::ok),
                    task.and_then(|t| t.worktree.clone()),
                    task.and_then(|t| t.merge_status.clone()),
                )
            };
            let result = result.unwrap_or(Value::Null);
            let info = &result["worktree"];
            let name = record
                .as_ref()
                .map(|r| r.name.clone())
                .or_else(|| {
                    info["branch"]
                        .as_str()
                        .and_then(|b| b.rsplit('/').next())
                        .map(String::from)
                })
                .unwrap_or_else(|| id.clone());
            let mut base = json!({ "id": id, "name": name });
            if info.is_object() {
                base["branch"] = info["branch"].clone();
            }
            let with = |extra: Value| {
                let mut item = base.clone();
                if let (Some(item), Value::Object(extra)) = (item.as_object_mut(), extra) {
                    item.extend(extra);
                }
                item
            };
            if let Some(at) = &stopped_at {
                results.push(with(
                    json!({ "status": "not_attempted", "reason": format!("stopped at the conflict of {at}") }),
                ));
                continue;
            }
            if !info.is_object() {
                results.push(with(json!({ "status": "no_worktree",
                    "reason": "it ran in the shared tree: its changes are already there" })));
                continue;
            }
            if merge_status.as_deref() == Some("merged") {
                results.push(with(
                    json!({ "status": "merged", "files": info["changed_files"], "note": "already merged" }),
                ));
                continue;
            }
            let verdict = &result["verdict"];
            let passed = result["status"] == "succeeded" && (!verdict.is_object() || verdict["status"] == "passed");
            if !passed && !include_failed {
                let status = result["status"].as_str().unwrap_or("unknown");
                let verdict_note = verdict["status"]
                    .as_str()
                    .map_or(String::new(), |v| format!(", verdict {v}"));
                let mut item = with(json!({ "status": "skipped",
                    "reason": format!("the child did not succeed ({status}{verdict_note}); include_failed=True merges it anyway") }));
                if !info["commit"].is_null() {
                    item["commit"] = info["commit"].clone();
                }
                results.push(item);
                continue;
            }
            if let Some(error) = info["error"].as_str() {
                results.push(with(json!({ "status": "failed", "reason": error })));
                stopped_at = (on_conflict != "skip").then(|| id.clone());
                continue;
            }
            let source = wt::MergeSource {
                name: name.clone(),
                branch: info["branch"].as_str().unwrap_or_default().to_string(),
                base: info["base"].as_str().unwrap_or_default().to_string(),
                commit: info["commit"].as_str().map(String::from),
            };
            let outcome = match wt::merge_child_worktree(target, &source, on_conflict == "markers") {
                Ok(outcome) => outcome,
                Err(error) => {
                    results.push(with(json!({ "status": "failed", "reason": error.message })));
                    if on_conflict != "skip" {
                        stopped_at = Some(id.clone());
                    }
                    continue;
                }
            };
            let record = record.or_else(|| {
                wt::list_records(target)
                    .into_iter()
                    .find(|r| r.task_id.as_deref() == Some(id))
            });
            let status = outcome.status();
            {
                let mut state = lock(&self.inner.state);
                if let Some(task) = state.task_mut(id) {
                    task.merge_status = Some(status.to_string());
                }
            }
            match outcome {
                wt::MergeOutcome::Conflict {
                    files,
                    conflicts,
                    markers,
                    message,
                } => {
                    if let Some(mut record) = record {
                        record.state = "conflict".into();
                        let _ = wt::save_record(&record);
                        self.store_record(id, record);
                    }
                    let text = if markers {
                        format!(
                            "conflict markers written in {}: resolve them, or restore those files",
                            files.join(", ")
                        )
                    } else {
                        format!(
                            "nothing of {name} was written: {}. Its branch {} keeps the work (worktree {})",
                            if message.is_empty() {
                                "the merge conflicts"
                            } else {
                                message.as_str()
                            },
                            source.branch,
                            info["path"].as_str().unwrap_or_default()
                        )
                    };
                    results.push(with(json!({ "status": "conflict", "files": files,
                        "conflicts": conflicts.iter().map(wt::ConflictFile::to_json).collect::<Vec<_>>(),
                        "markers": markers, "message": text, "commit": info["commit"] })));
                    if on_conflict != "skip" {
                        stopped_at = Some(id.clone());
                    }
                }
                other => {
                    // Merged or empty: its worktree and branch have done their job.
                    if let Some(mut record) = record {
                        if !self.inner.keep_worktrees {
                            wt::remove_child_worktree(&record, true);
                        } else {
                            record.state = "merged".into();
                            let _ = wt::save_record(&record);
                            self.store_record(id, record);
                        }
                    }
                    results.push(match other {
                        wt::MergeOutcome::Merged { files, diffstat } => {
                            with(json!({ "status": "merged", "files": files, "diffstat": diffstat }))
                        }
                        _ => with(json!({ "status": "empty" })),
                    });
                }
            }
        }
        let ok = results
            .iter()
            .all(|item| ["merged", "empty", "no_worktree"].contains(&item["status"].as_str().unwrap_or("")));
        let merged: Vec<Value> = results
            .iter()
            .filter(|item| item["status"] == "merged")
            .map(|item| item["id"].clone())
            .collect();
        let mut out = json!({ "ok": ok, "merged": merged });
        if let Some(at) = stopped_at {
            out["stopped_at"] = json!(at);
        }
        out["results"] = Value::from(results);
        out["note"] = json!(
            "Merged changes are uncommitted modifications in your working tree (new files untracked); review and commit them yourself."
        );
        out
    }

    fn store_record(&self, id: &str, record: WorktreeRecord) {
        let mut state = lock(&self.inner.state);
        if let Some(task) = state.task_mut(id)
            && task.worktree.is_some()
        {
            task.worktree = Some(record);
        }
    }

    fn spawn(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        fields(payload, &["prompt", "kwargs"])?;
        let limit = self.inner.spawn_depth;
        let (depth, allowance) = {
            let state = lock(&self.inner.state);
            let children: Vec<String> = state.line(parent).into_iter().filter(|id| state.is_child(id)).collect();
            let allowance = children
                .first()
                .and_then(|id| state.task(id))
                .map_or(i64::MAX, |spawner| i64::from(spawner.spawn_depth));
            (children.len() as i64, allowance)
        };
        if limit > 0 && depth >= i64::from(limit) {
            return Err(format!(
                "rlm.spawn refused: subagents nest at most {limit} level(s) deep (ULTRON_SPAWN_DEPTH). Do this part yourself: narrow with code, read the candidates, and use rlm.map for passages you cannot settle."
            ));
        }
        if allowance < 1 {
            return Err("rlm.spawn refused: this subagent was started with depth=0, so it does its brief itself. A parent that wants nested delegation passes rlm.spawn(brief, name=..., depth=N).".into());
        }
        let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
        let empty = Map::new();
        let kwargs = match payload.get("kwargs") {
            None => &empty,
            Some(Value::Object(map)) => map,
            Some(_) => return Err("agent input must be a JSON object".into()),
        };
        fields(
            kwargs,
            &[
                "name",
                "model",
                "thinking",
                "timeout_ms",
                "depth",
                "worktree",
                "worktree_setup",
                "fork",
            ],
        )?;
        match kwargs.get("fork") {
            None => {}
            Some(Value::Bool(true)) => {
                return Err("rlm.spawn fork=True is not available when subagents run as external processes".into());
            }
            Some(Value::Bool(false)) => {}
            Some(_) => return Err("fork must be True or False".into()),
        }
        let name = nonempty_string(kwargs.get("name"), "name")?;
        let mode = match kwargs.get("worktree") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => WorktreeMode::Off,
            Some(Value::Bool(true)) => WorktreeMode::On,
            Some(Value::String(s)) if s == "auto" => WorktreeMode::Auto,
            Some(_) => {
                return Err(
                    "worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
                        .into(),
                );
            }
        };
        let setup = check_worktree_setup(kwargs.get("worktree_setup"))?;
        if setup.is_some() && mode == WorktreeMode::Off {
            return Err("worktree_setup needs worktree=True".into());
        }
        let child_depth = match kwargs.get("depth") {
            None => 0,
            Some(value) => safe_integer(value)
                .filter(|n| *n >= 0)
                .ok_or("depth must be a non-negative integer")?,
        };
        let max_child_depth = (allowance.saturating_sub(1)).min(if limit > 0 {
            i64::from(limit) - depth - 1
        } else {
            i64::MAX
        });
        if child_depth > max_child_depth {
            return Err(format!(
                "rlm.spawn depth={child_depth} is too deep here: at most {max_child_depth} (ULTRON_SPAWN_DEPTH={limit} levels in all)."
            ));
        }
        let model = match kwargs.get("model") {
            Some(Value::String(model)) => Some(model.clone()),
            _ => self.inner.child_model.clone(),
        };
        let timeout = match kwargs.get("timeout_ms") {
            Some(value @ Value::Number(_)) => value.clone(),
            _ => json!(DEFAULT_TIMEOUT_MS as i64),
        };
        let timeout_ms = safe_integer(&timeout)
            .filter(|n| (1..=3_600_000).contains(n))
            .ok_or("timeout_ms must be an integer between 1 and 3600000")? as u64;
        // The worktree branches from the spawner's tree as it is now, before the child is admitted.
        let worktree = self.new_worktree(mode, &name, setup.as_ref(), parent)?;
        let thinking = match kwargs.get("thinking") {
            None => None,
            Some(value) => match value.as_str().filter(|t| THINKING_LEVELS.contains(t)) {
                Some(level) => Some(level.to_string()),
                None => {
                    if let Some(record) = &worktree {
                        wt::remove_child_worktree(record, true);
                    }
                    return Err(format!("thinking must be one of {}", THINKING_LEVELS.join(", ")));
                }
            },
        };
        let fingerprint = task_fingerprint(&json!({
            "definition": CHILD,
            "input": { "prompt": prompt },
            "model": model,
            "timeout_ms": timeout_ms,
        }));
        let signal = Signal::new();
        let (task_id, level) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err("Ultron task host is closed".into());
            }
            let admitted = state
                .journal
                .admit(CHILD, &fingerprint, &crate::task_store::uuid_v4(), parent);
            let (record, _) = match admitted {
                Ok(admitted) => admitted,
                Err(error) => {
                    if let Some(record) = &worktree {
                        wt::remove_child_worktree(record, true);
                    }
                    return Err(error);
                }
            };
            let id = record["id"].as_str().unwrap_or_default().to_string();
            state.tasks.push(Task {
                id: id.clone(),
                parent_id: parent.map(String::from),
                record,
                outcome: None,
                spawn_depth: child_depth as u32,
                lane_name: None,
                started_at: Some((self.inner.now)()),
                ended_at: None,
                verdict: None,
                verdict_rejections: 0,
                verdict_problems: Vec::new(),
                verdict_invalid: false,
                waiters: 0,
                signal: signal.clone(),
                finishing: false,
                worktree: worktree.clone().map(|mut record| {
                    record.task_id = Some(id.clone());
                    let _ = wt::save_record(&record);
                    record
                }),
                worktree_baseline: None,
                outside_before: None,
                before: None,
                merge_status: None,
            });
            let level = state.line(Some(&id)).iter().filter(|a| state.is_child(a)).count() as u32;
            let running = state.journal.transition(&id, "running", None)?;
            if let Some(task) = state.task_mut(&id) {
                task.record = running;
            }
            (id, level)
        };
        let lane_name = format!("ultron.rlm-child.{task_id}");
        let (cwd, env, brief) = {
            let state = lock(&self.inner.state);
            let inherited = state.worktree_of(Some(&task_id));
            let note = worktree.as_ref().map(wt::worktree_brief_note);
            (
                inherited.map(|w| w.cwd.clone()),
                inherited.map(wt::worktree_env).unwrap_or_default(),
                match note {
                    Some(note) => format!("{prompt}\n\n{note}"),
                    None => prompt,
                },
            )
        };
        let run = ChildRun {
            task_id: task_id.clone(),
            lane_name: lane_name.clone(),
            prompt: brief,
            level,
            allowance: child_depth as u32,
            model: model.clone(),
            thinking,
            timeout_ms,
            cwd,
            env,
            signal: signal.clone(),
            host: self.clone(),
        };
        {
            let mut state = lock(&self.inner.state);
            state.lane_tasks.insert(lane_name.clone(), task_id.clone());
            if let Some(task) = state.task_mut(&task_id) {
                task.lane_name = Some(lane_name);
            }
        }
        let host = self.clone();
        std::thread::spawn(move || host.run_child(run));
        let host = self.clone();
        let (id, deadline) = (task_id.clone(), signal);
        std::thread::spawn(move || {
            if !deadline.wait(Duration::from_millis(timeout_ms)) {
                let _ = host.cancel(&id, &format!("Ultron task exceeded {timeout_ms}ms timeout"));
            }
        });
        Ok(json!({
            "rlm_child_id": task_id,
            "name": name,
            "session_dir": "",
            "model": model.unwrap_or_default(),
            "timeout_ms": timeout_ms,
            "parent_branch_anchor": "",
            "worktree": worktree.map_or(Value::Null, |w| json!({ "branch": w.branch, "path": w.path, "cwd": w.cwd })),
        }))
    }

    /// `newWorktree`: a private worktree for a child of `parent` (`worktree=True`), branched from the spawner's tree.
    fn new_worktree(
        &self,
        mode: WorktreeMode,
        name: &str,
        setup: Option<&WorktreeSetup>,
        parent: Option<&str>,
    ) -> Result<Option<WorktreeRecord>, String> {
        if mode == WorktreeMode::Off {
            return Ok(None);
        }
        let from = lock(&self.inner.state).workspace_of(parent, self.inner.workspace.as_deref());
        let Some(from) = from else {
            if mode == WorktreeMode::Auto {
                return Ok(None);
            }
            return Err("worktree=True needs a working directory, and this session has none".into());
        };
        let setup = merged_setup(self.inner.worktree_setup.as_ref(), setup);
        let created = wt::create_child_worktree(&wt::CreateWorktreeOptions {
            cwd: &from,
            session_id: &self.inner.session_id,
            name,
            setup: Some(&setup),
            now: (self.inner.now)(),
        });
        match created {
            Ok(record) => Ok(Some(record)),
            Err(error) if mode == WorktreeMode::Auto && error.fallback => Ok(None),
            Err(error) => Err(format!("worktree=True: {}", error.message)),
        }
    }

    fn run_child(&self, run: ChildRun) {
        let id = run.task_id.clone();
        let signal = run.signal.clone();
        let reply = match self.child_start(&id, &signal) {
            Ok(()) => (self.inner.runner)(run),
            Err(error) => Err(error),
        };
        if signal.is_aborted() {
            return;
        }
        let result = match reply {
            Ok(text) => self.child_result(&id, &text),
            Err(error) => Err(error),
        };
        let result =
            result.unwrap_or_else(|error| json!({ "status": "failed", "error": error, "verification": "unverified" }));
        let _ = self.end(&id, result);
    }

    /// `childStart`: a worktree child's setup command runs, then its working directory is pictured (plus the parent's
    /// tree for a worktree child, to see writes it makes outside its worktree).
    fn child_start(&self, id: &str, signal: &Signal) -> Result<(), String> {
        let (own, workspace, parent_workspace) = {
            let state = lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let shared = self.inner.workspace.as_deref();
            (
                task.worktree.clone(),
                state.workspace_of(Some(id), shared),
                state.workspace_of(task.parent_id.as_deref(), shared),
            )
        };
        if let Some(record) = &own {
            wt::run_setup_command(record, Some(signal)).map_err(|e| e.message)?;
        }
        let before = workspace.map(|w| snapshot_workspace(&w, DEFAULT_MAX_SNAPSHOT_ENTRIES, true));
        let outside = match (&own, &before) {
            (Some(_), Some(_)) => parent_workspace.map(|w| snapshot_workspace(&w, DEFAULT_MAX_SNAPSHOT_ENTRIES, true)),
            _ => None,
        };
        let mut state = lock(&self.inner.state);
        if let Some(task) = state.task_mut(id) {
            if own.is_some() {
                task.worktree_baseline = before.clone();
                task.outside_before = outside;
            }
            task.before = before;
        }
        Ok(())
    }

    /// `childResult`: the reply, the verdict (null without one), and the check of its declared files against the
    /// files that changed in its working directory while it ran.
    fn child_result(&self, id: &str, text: &str) -> Result<Value, String> {
        let (verdict, invalid, problems, before, workspace) = {
            let state = lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            (
                task.verdict.clone(),
                task.verdict_invalid,
                task.verdict_problems.clone(),
                task.before.clone(),
                state.workspace_of(Some(id), self.inner.workspace.as_deref()),
            )
        };
        if text.trim().is_empty() && verdict.is_none() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if text.trim().is_empty() {
            verdict.as_ref().map_or(Value::Null, |v| v["summary"].clone())
        } else {
            json!(text)
        };
        let after = match (&before, &workspace) {
            (Some(_), Some(w)) => Some(snapshot_workspace(w, DEFAULT_MAX_SNAPSHOT_ENTRIES, true)),
            _ => None,
        };
        let (concurrent, explained) = {
            let state = lock(&self.inner.state);
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            let others = self.concurrent_with(&state, task);
            let explained: Vec<String> = others
                .iter()
                .filter_map(|other| state.task(other))
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => None,
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (others, explained)
        };
        let mut check = match (&before, &after) {
            (Some(before), Some(after)) => {
                let declared: Vec<String> = verdict
                    .as_ref()
                    .and_then(|v| v["changed_files"].as_array())
                    .map(|files| files.iter().filter_map(|f| f.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let diff = diff_snapshots(before, after)?;
                let mut check = check_files(&declared, &before.root, before, after, &diff, &explained, &concurrent);
                if verdict.is_none() {
                    check["outcome"] = json!(if invalid { "invalid" } else { "unchecked" });
                    check["reason"] = json!("the subagent ended without a verdict (rlm.finish)");
                }
                check
            }
            _ => json!({
                "outcome": if invalid { "invalid" } else { "unchecked" },
                "unobserved": [], "unreported": [], "unlisted": [],
                "concurrent": concurrent,
                "reason": "no workspace to compare",
            }),
        };
        if invalid {
            check["problems"] = json!(problems);
        }
        let outside = self.outside_writes(id);
        if !outside.is_empty() {
            check["outside"] = json!(outside);
        }
        let nothing_checked = verdict.is_none() && !invalid && before.is_none();
        let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                                 "verdict": verdict.clone().unwrap_or(Value::Null) });
        if !nothing_checked {
            result["check"] = check;
        }
        if verdict.is_none() {
            result["unverified"] = json!(true);
        }
        Ok(result)
    }

    /// `outsideWrites`: files in the parent's tree that changed while a worktree child ran, minus what other tasks
    /// working there declared.
    fn outside_writes(&self, id: &str) -> Vec<String> {
        let (root, before, explained) = {
            let state = lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return Vec::new();
            };
            let (Some(_), Some(before)) = (&task.worktree, &task.outside_before) else {
                return Vec::new();
            };
            let shared = self.inner.workspace.as_deref();
            let target = state.workspace_of(task.parent_id.as_deref(), shared);
            let explained: Vec<String> = state
                .tasks
                .iter()
                .filter(|other| other.id != id && state.workspace_of(Some(&other.id), shared) == target)
                .filter_map(|other| match &other.outcome {
                    Some(Ok(result)) => result["verdict"]["changed_files"].as_array().cloned(),
                    _ => other
                        .verdict
                        .as_ref()
                        .and_then(|v| v["changed_files"].as_array().cloned()),
                })
                .flatten()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
            (before.root.clone(), before.clone(), explained)
        };
        let after = snapshot_workspace(&root, DEFAULT_MAX_SNAPSHOT_ENTRIES, true);
        diff_snapshots(&before, &after)
            .map(|diff| {
                diff.changed
                    .into_iter()
                    .filter(|path| !explained.contains(path))
                    .take(100)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Other started tasks whose runs overlapped this one in the same working directory, outside its own line
    /// (ancestors and descendants).
    fn concurrent_with(&self, state: &State, task: &Task) -> Vec<String> {
        let start = task.started_at.unwrap_or(0.0);
        let end = (self.inner.now)();
        let ancestors = state.line(task.parent_id.as_deref());
        let shared = self.inner.workspace.as_deref();
        let workspace = state.workspace_of(Some(&task.id), shared);
        state
            .tasks
            .iter()
            .filter(|other| other.id != task.id && other.lane_name.is_some())
            .filter(|other| state.workspace_of(Some(&other.id), shared) == workspace)
            .filter(|other| {
                other.started_at.is_some_and(|s| s <= end) && other.ended_at.unwrap_or(f64::INFINITY) >= start
            })
            .filter(|other| !ancestors.contains(&other.id))
            .filter(|other| !state.line(other.parent_id.as_deref()).contains(&task.id))
            .map(|other| other.id.clone())
            .collect()
    }

    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    fn end(&self, id: &str, result: Value) -> Result<Value, String> {
        {
            let mut state = lock(&self.inner.state);
            loop {
                let task = state.task(id).ok_or("Unknown Ultron task")?;
                if let Some(outcome) = &task.outcome {
                    return outcome.clone();
                }
                if !task.finishing {
                    break;
                }
                state = self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.finishing = true;
            // Only the first party to end a task aborts its work, and before finalizing it, as cancel() does in TS.
            if result["status"] == "cancelled" {
                task.signal.abort(result["error"].as_str().unwrap_or("cancelled"));
            }
        }
        // A worktree child's work is committed on its branch before its result is published.
        let result = self.finalize_worktree(id, result);
        let event = {
            let mut state = lock(&self.inner.state);
            let terminal = match result["status"].as_str() {
                Some("succeeded") => "completed",
                Some(other) => other,
                None => "failed",
            }
            .to_string();
            let committed = state.journal.transition(id, &terminal, Some(result));
            let now = (self.inner.now)();
            let closed = state.closed;
            let owner_lane = {
                let parent = state.task(id).and_then(|t| t.parent_id.clone());
                parent
                    .and_then(|p| state.task(&p).and_then(|t| t.lane_name.clone()))
                    .unwrap_or_else(|| "main".into())
            };
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.ended_at.get_or_insert(now);
            task.signal.abort("ended");
            let outcome = committed.map(|record| {
                task.record = record.clone();
                record["result"].clone()
            });
            task.outcome = Some(outcome.clone());
            self.inner.changed.notify_all();
            match outcome {
                Ok(_) if !closed => Some(DetachedEnd {
                    task: task.record.clone(),
                    kind: "child_done",
                    owner_lane,
                    awaited: task.waiters > 0,
                    fetch: format!("await rlm.collect([\"{id}\"])"),
                }),
                Ok(_) => None,
                Err(error) => return Err(error),
            }
        };
        if let Some(event) = event {
            let result = event.task["result"].clone();
            let _ = self
                .inner
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(event);
            return Ok(result);
        }
        let state = lock(&self.inner.state);
        state
            .task(id)
            .and_then(|t| t.outcome.clone())
            .unwrap_or(Err("Unknown Ultron task".into()))
    }

    /// `finalizeWorktree`: commit what a worktree child changed on its branch and add the worktree to its result. A
    /// child that changed nothing has its worktree and branch removed at once. Never fails the task: a Git problem is
    /// reported in `worktree.error` and the worktree is kept.
    fn finalize_worktree(&self, id: &str, mut result: Value) -> Value {
        let (record, verdict, baseline) = {
            let state = lock(&self.inner.state);
            let Some(task) = state.task(id) else {
                return result;
            };
            (
                task.worktree.clone().filter(|w| w.state == "active"),
                task.verdict.clone(),
                task.worktree_baseline.clone(),
            )
        };
        let Some(mut record) = record else {
            return result;
        };
        let status = verdict
            .as_ref()
            .and_then(|v| v["status"].as_str())
            .or(result["status"].as_str())
            .unwrap_or("")
            .to_string();
        let summary = match verdict.as_ref().and_then(|v| v["summary"].as_str()) {
            Some(summary) => summary.to_string(),
            None => match (&result["value"], &result["error"]) {
                (Value::String(value), _) => value.clone(),
                (_, Value::String(error)) => error.clone(),
                _ => String::new(),
            },
        };
        let paths = worktree_changes(&record, baseline.as_ref());
        let message = wt::commit_message(&summary, &record.name, Some(id), &status);
        let keep = self.inner.keep_worktrees;
        let mut empty = false;
        let info = match wt::commit_child_worktree(&record, &message, paths.as_deref()) {
            Ok(commit) => {
                record.state = if commit.commit.is_some() && status != "passed" && status != "succeeded" {
                    "failed"
                } else {
                    "ended"
                }
                .into();
                record.commit = Some(commit.commit.clone());
                empty = commit.commit.is_none();
                if empty && !keep {
                    wt::remove_child_worktree(&record, true);
                } else {
                    let _ = wt::save_record(&record);
                }
                let mut info = json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": commit.commit, "changed_files": commit.changed_files,
                    "diffstat": commit.diffstat });
                if empty && !keep {
                    info["removed"] = json!(true);
                }
                info
            }
            Err(error) => {
                record.state = "failed".into();
                let _ = wt::save_record(&record);
                json!({ "task": id, "branch": record.branch, "path": record.path, "repo": record.repo,
                    "base": record.base, "commit": null, "changed_files": [], "diffstat": "",
                    "error": format!("committing the worktree failed: {}", error.message) })
            }
        };
        let mut state = lock(&self.inner.state);
        if let Some(task) = state.task_mut(id) {
            task.worktree = Some(record);
            if empty {
                task.merge_status = Some("empty".into());
            }
        }
        result["worktree"] = info;
        result
    }

    /// Cancel a task and, after it, its unfinished descendants.
    pub fn cancel(&self, id: &str, reason: &str) -> Result<Value, String> {
        let own = self.end(
            id,
            json!({ "status": "cancelled", "error": reason, "verification": "unverified" }),
        );
        let children: Vec<String> = {
            let state = lock(&self.inner.state);
            state
                .tasks
                .iter()
                .filter(|t| t.parent_id.as_deref() == Some(id) && t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for child in children {
            let _ = self.cancel(&child, &format!("Ancestor {id} cancelled: {reason}"));
        }
        own
    }

    /// The children a collect selects: visible to the caller, not the caller itself, in admission order.
    fn selected(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<Vec<String>, String> {
        fields(payload, &["selectors", "timeout_ms"])?;
        let selectors = match payload.get("selectors").and_then(Value::as_array) {
            Some(items) => items
                .iter()
                .map(|s| nonempty_string(Some(s), "selector"))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let state = lock(&self.inner.state);
        Ok(state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && Some(t.id.as_str()) != parent)
            .filter(|t| state.visible(&t.id, parent))
            .filter(|t| selectors.is_empty() || selectors.contains(&t.id))
            .map(|t| t.id.clone())
            .collect())
    }

    fn unfinished(&self, ids: &[String]) -> Vec<String> {
        let state = lock(&self.inner.state);
        ids.iter()
            .filter(|id| state.task(id).is_some_and(|t| t.outcome.is_none()))
            .cloned()
            .collect()
    }

    fn collected(&self, outcomes: Vec<(String, Result<Value, String>)>) -> Result<Value, String> {
        let results = outcomes
            .into_iter()
            .map(|(id, outcome)| outcome.map(|result| json!({ "id": id, "result": result })))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "results": results }))
    }

    /// Unfinished children started from `lane` ("main" for the root), in admission order: what a turn scheduler
    /// waits for (with [`Delegation::wait`] or the [`DetachedEnd`] channel) after the model ends its turn.
    pub fn running(&self, lane: &str) -> Vec<String> {
        let state = lock(&self.inner.state);
        let parent = state.lane_tasks.get(lane).cloned();
        state
            .tasks
            .iter()
            .filter(|t| t.record["definition"] == CHILD && t.outcome.is_none() && t.parent_id == parent)
            .map(|t| t.id.clone())
            .collect()
    }

    /// Host-side wait for tasks' results (in the given order), without holding any kernel. `timeout` bounds the
    /// whole wait; a task still running when it passes is left out.
    pub fn wait(&self, ids: &[String], timeout: Option<Duration>) -> Vec<(String, Result<Value, String>)> {
        self.wait_until(ids, timeout, None)
    }

    /// [`Delegation::wait`] that also ends when `cancel` is aborted.
    pub fn wait_until(
        &self,
        ids: &[String],
        timeout: Option<Duration>,
        cancel: Option<&Signal>,
    ) -> Vec<(String, Result<Value, String>)> {
        if let Some(cancel) = cancel.filter(|c| !c.is_aborted()) {
            // Wakes this wait when the signal is aborted; every cell signal is aborted when its cell ends.
            let (cancel, inner) = (cancel.clone(), self.inner.clone());
            std::thread::spawn(move || {
                while !cancel.wait(Duration::from_secs(3600)) {}
                let _state = lock(&inner.state);
                inner.changed.notify_all();
            });
        }
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let mut state = lock(&self.inner.state);
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters += 1;
            }
        }
        loop {
            let pending = ids.iter().any(|id| state.task(id).is_some_and(|t| t.outcome.is_none()));
            let left = deadline.map(|d| d.saturating_duration_since(std::time::Instant::now()));
            if !pending || left.is_some_and(|l| l.is_zero()) || cancel.is_some_and(Signal::is_aborted) {
                break;
            }
            state = match left {
                Some(left) => {
                    self.inner
                        .changed
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner),
            };
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(task) = state.task_mut(id) {
                task.waiters -= 1;
                if let Some(outcome) = &task.outcome {
                    out.push((id.clone(), outcome.clone()));
                }
            }
        }
        out
    }

    /// Requests now waiting on a task's result.
    pub fn waiters(&self, id: &str) -> usize {
        lock(&self.inner.state).task(id).map_or(0, |t| t.waiters)
    }

    /// The journal's tasks, in admission order.
    pub fn tasks(&self) -> Result<Vec<Value>, String> {
        lock(&self.inner.state).journal.list()
    }

    fn finish_request(&self, mut payload: Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        let mut state = lock(&self.inner.state);
        let id = parent
            .filter(|id| state.is_child(id))
            .ok_or("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply")?
            .to_string();
        let worktree = state.worktree_of(Some(&id)).cloned();
        let workspace = state
            .workspace_of(Some(&id), self.inner.workspace.as_deref())
            .map(|w| w.display().to_string());
        let task = state.task_mut(&id).ok_or("Unknown Ultron task")?;
        if task.outcome.is_some() || task.finishing {
            return Err("rlm.finish: this subagent has already ended".into());
        }
        if task.verdict_invalid {
            return Err("rlm.finish: no attempts left; end your turn, your reply is returned unverified".into());
        }
        // A worktree child may name files by their path in its parent's checkout: they mean its own copies.
        if let (Some(worktree), Some(Value::Array(files))) = (&worktree, payload.get_mut("changed_files")) {
            for file in files.iter_mut() {
                if let Value::String(path) = file {
                    *path = wt::in_worktree(path, worktree);
                }
            }
        }
        match validate_verdict(&payload, workspace.as_deref()) {
            Ok(verdict) => {
                let status = verdict["status"].clone();
                task.verdict = Some(verdict);
                Ok(json!({ "recorded": true, "status": status,
                    "next": "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran." }))
            }
            Err(problems) => {
                task.verdict_rejections += 1;
                let list = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
                task.verdict_problems = problems;
                let left = MAX_VERDICT_REJECTIONS + 1 - task.verdict_rejections.min(MAX_VERDICT_REJECTIONS + 1);
                if left > 0 || task.verdict.is_some() {
                    let tail = if task.verdict.is_some() {
                        "; your earlier verdict stands until then".to_string()
                    } else {
                        format!(" ({left} attempt{} left)", if left == 1 { "" } else { "s" })
                    };
                    return Err(format!(
                        "rlm.finish rejected:\n{list}\nFix these and call rlm.finish again{tail}."
                    ));
                }
                task.verdict_invalid = true;
                Err(format!(
                    "rlm.finish rejected:\n{list}\nNo attempts left: end your turn; your reply is returned unverified, with these problems."
                ))
            }
        }
    }

    /// Stop accepting requests and cancel every unfinished task.
    pub fn close(&self) {
        let open: Vec<String> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state
                .tasks
                .iter()
                .filter(|t| t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for id in open {
            let _ = self.cancel(&id, "Ultron task host closed");
        }
        // This session's finished, unmerged children: the directory goes, the branch keeps the work. Conflicted and
        // failed ones stay for inspection until a later session prunes them.
        if let Some(workspace) = self.inner.workspace.as_deref().filter(|_| !self.inner.keep_worktrees) {
            let (session, pid) = (self.inner.session_id.clone(), std::process::id());
            let select = |r: &WorktreeRecord| r.session_id == session && r.owner.pid == pid && r.state == "ended";
            wt::prune_worktrees(
                workspace,
                &wt::PruneOptions {
                    select: Some(&select),
                    force: true,
                    delete_branches: false,
                },
            );
        }
    }
}
