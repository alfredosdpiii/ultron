//! Durable task scheduler and task runtime over a `durable::Session`.
//!
//! Port of upstream Pi Durable `packages/durable/src/harness/{scheduler,util,define}.ts` and the task half of
//! `harness.ts` (earendil-works/pi, MIT; pinned revision in `crates/durable/fixtures/storage-conformance.json`), spec
//! §5 (tasks) and §10.1 (failures).
//!
//! Threads, not async: the Session is synchronous with a FIFO ticket line, so upstream's promise chain on the line
//! becomes threads queued on it. One worker thread per Harness runs upstream's two microtask jobs (reconcile and the
//! reservation drain); every invocation (a run or an abort of one task) runs on its own thread; waits block on
//! condition variables and are cancelled through [`Cancel`].
//!
//! `live` mirrors every committed non-terminal task record. The commit listener updates it on the Session line, so
//! code on the line reads exactly the committed state from it. Every task transition is decided and written by one
//! callback serialized on the line: reservation, marks, runtime commits, finalization, and the step before each
//! phase. Handlers and joins run off the line. An invocation ends inside the step that decides its end, so a runtime
//! commit it queued either lands before that decision or is rejected.
//!
//! Lock discipline: the state mutex is never held while waiting for the line, calling user code (handlers, hooks,
//! registry readers, reports), or blocking; code on the line takes it in short sections.
//!
//! Not ported here: agents, hooks, tools, settings, context reads and kept contexts, conversation handles bound to an
//! invocation, `watchDoc` through the runtime, the task-graph replicated state, and the Harness's built-in documents
//! and tasks (generation, tool, compaction).

pub mod cancel;
mod harness;
pub mod registry;
mod runtime;

pub use cancel::{Cancel, Ctx, Linked};
pub use harness::{
    AbortResult, Conversation, FallibleClock, Harness, HarnessOptions, Inspection, Scheduling, TaskInspection,
    TaskInspectionState, safe_now,
};
pub use registry::{Extension, Handler, Migrate, Registry, RegistryReader, RegistrySnapshot, Task, TaskBuilder};
pub use runtime::{NextState, TaskRuntime, aborted_with, completed};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

use durable::{
    AbortReason, CommitChange, CommitPublication, ConversationId, JoinPolicy, Session, SessionError, StorageWrite,
    SubmissionQuery, SubmissionStatus, SubmissionType, TaskId, TaskOutcome, TaskOutcomeError, TaskQuery, TaskRecord,
    TaskState, TaskStatus, Tx, TxScope,
};
use indexmap::{IndexMap, IndexSet};
use serde_json::Value;

use crate::effect_gate::{Gate, GateControl, Refusal, create_gate};
use cancel::{Waiters, lock, wait_any};
use registry::phase_of;

const SCAN_PAGE_SIZE: usize = 256;
const LIVE_STATUSES: [TaskStatus; 4] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Completing,
];

/// A Harness or task-runtime failure. `name()` is the upstream error class name.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessError {
    /// A Session error: `SessionFailed` (with its cause), a storage error, `ReadAfterWrite`, ...
    Session(SessionError),
    /// An abort without a reason (upstream `AbortError`, "This operation was aborted").
    Aborted,
    /// Upstream plain `Error`.
    Error(String),
    /// Upstream `new Error(message, { cause })` with a string cause.
    Caused { message: String, cause: String },
}

impl HarnessError {
    pub fn name(&self) -> &'static str {
        match self {
            HarnessError::Session(error) => error.name(),
            HarnessError::Aborted => "AbortError",
            HarnessError::Error(_) | HarnessError::Caused { .. } => "Error",
        }
    }

    pub fn message(&self) -> String {
        self.to_string()
    }

    /// The first error of a failed Session (`SessionFailed.cause`).
    pub fn session_cause(&self) -> Option<&SessionError> {
        match self {
            HarnessError::Session(error) => error.cause(),
            _ => None,
        }
    }

    pub fn cause(&self) -> Option<&str> {
        match self {
            HarnessError::Caused { cause, .. } => Some(cause),
            _ => None,
        }
    }

    pub fn msg(message: impl Into<String>) -> HarnessError {
        HarnessError::Error(message.into())
    }

    fn into_session(self) -> SessionError {
        match self {
            HarnessError::Session(error) => error,
            other => SessionError::Error(other.to_string()),
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::Session(error) => error.fmt(f),
            HarnessError::Aborted => f.write_str("This operation was aborted"),
            HarnessError::Error(message) | HarnessError::Caused { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<SessionError> for HarnessError {
    fn from(error: SessionError) -> Self {
        HarnessError::Session(error)
    }
}

pub type Result<T> = std::result::Result<T, HarnessError>;

/// Run one commit whose callback fails with a [`HarnessError`]; the callback's own error comes back unchanged.
pub(crate) fn commit_on<T>(
    session: &Session,
    scope: TxScope,
    change: impl FnOnce(&mut Tx<'_>) -> Result<T>,
) -> Result<T> {
    let mut own = None;
    let result = session.commit_with(scope, |tx| match change(tx) {
        Ok(value) => Ok(value),
        Err(error) => {
            let carried = error.clone().into_session();
            own = Some(error);
            Err(carried)
        }
    });
    result.map_err(|error| own.take().unwrap_or(HarnessError::Session(error)))
}

/// What a closed Harness rejects with: `SessionFailed` with the cause when the Session failed.
pub(crate) fn closed_error(session: &Session) -> HarnessError {
    match session.failure() {
        Some(failure) => HarnessError::Session(SessionError::Failed(Box::new(failure))),
        None => HarnessError::msg("Harness is closed"),
    }
}

/// Run user code, turning a panic into an error.
pub(crate) fn contained<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "[panic]".to_owned());
            Err(HarnessError::Error(message))
        }
    }
}

/// Terminal outcomes the scheduler writes without running task code.
#[derive(Clone, Debug, PartialEq)]
pub enum SchedulerOutcome {
    Faulted { message: String },
    Orphaned { reason: String },
}

impl SchedulerOutcome {
    fn outcome(&self) -> TaskOutcome {
        match self {
            SchedulerOutcome::Faulted { message } => TaskOutcome::Faulted {
                error: TaskOutcomeError {
                    message: message.clone(),
                    detail: None,
                },
            },
            SchedulerOutcome::Orphaned { reason } => TaskOutcome::Orphaned { reason: reason.clone() },
        }
    }
}

/// Harness cleanup staged in the commit that makes a scheduler outcome terminal.
pub type SettleOutcome = Arc<dyn Fn(&mut Tx<'_>, &TaskRecord, &SchedulerOutcome) -> Result<()> + Send + Sync>;
/// Withdraw a conversation's queued inputs, for conversation abort and abort cascades.
pub type WithdrawInputs = Arc<dyn Fn(&mut Tx<'_>, ConversationId) -> Result<()> + Send + Sync>;
pub type Report = Arc<dyn Fn(&HarnessError) + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

// ─── Records ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Run,
    Abort,
}

/// The immutable ownership fields of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TaskNode {
    conversation_id: ConversationId,
    owner: Option<TaskId>,
    background: bool,
}

/// Where a walk up the ownership tree continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Up {
    Task(TaskId),
    Conversation(ConversationId),
}

enum Step {
    Task(TaskId, TaskNode),
    Conversation(ConversationId),
    Unknown,
}

/// Candidate records a commit staged; they override committed records in ownership walks.
#[derive(Default)]
struct Overlay {
    tasks: IndexMap<TaskId, TaskRecord>,
    edges: HashMap<ConversationId, Option<TaskId>>,
}

#[derive(Clone, Copy)]
enum Scope {
    Conversation(ConversationId),
    Roots,
}

/// Why a pending task cannot be reserved under a registry snapshot. Derived, never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
}

impl BlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockedReason::MissingTask => "missing_task",
            BlockedReason::TaskTooOld => "task_too_old",
            BlockedReason::MigrationFailed => "migration_failed",
        }
    }
}

enum Fit {
    Ready {
        task: Task,
        migrates: bool,
    },
    Blocked {
        reason: BlockedReason,
        error: Option<HarnessError>,
    },
}

enum Resolution {
    Ready { task: Task, record: Box<TaskRecord> },
    Blocked(BlockedReason),
}

fn node_of(record: &TaskRecord) -> TaskNode {
    TaskNode {
        conversation_id: record.conversation_id,
        owner: record.owner,
        background: record.background,
    }
}

fn parent_of(node: &TaskNode) -> Up {
    match node.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(node.conversation_id),
    }
}

fn record_parent(record: &TaskRecord) -> Up {
    parent_of(&node_of(record))
}

fn outcome_of(record: &TaskRecord) -> Option<&TaskOutcome> {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => Some(outcome),
        _ => None,
    }
}

/// Whether the record holds or ends with an outcome other than `completed`.
fn failed_outcome(record: &TaskRecord) -> bool {
    outcome_of(record).is_some_and(|outcome| !matches!(outcome, TaskOutcome::Completed { .. }))
}

/// A live owner's durable cancellation intent: its abort mark, or a held outcome other than `completed`.
fn cancellation_intent(record: &TaskRecord) -> bool {
    record.state.status() != TaskStatus::Terminal && (record.abort_requested || failed_outcome(record))
}

/// The record with an abort mark: `restart` for an abandonment; without a reason, a request, which replaces `restart`.
fn with_abort_mark(record: &TaskRecord, reason: Option<AbortReason>) -> TaskRecord {
    TaskRecord {
        abort_requested: true,
        abort_reason: reason,
        ..record.clone()
    }
}

/// Replace a live record's state; memos disappear once an outcome is decided.
pub(crate) fn with_state(record: &TaskRecord, state: TaskState) -> TaskRecord {
    let memos = match state {
        TaskState::Terminal { .. } | TaskState::Completing { .. } => None,
        _ => record.memos.clone(),
    };
    TaskRecord {
        state,
        memos,
        ..record.clone()
    }
}

fn checkpoint_of(record: &TaskRecord) -> Option<&Value> {
    match &record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => Some(checkpoint),
        _ => None,
    }
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completing => "completing",
        TaskStatus::Terminal => "terminal",
    }
}

/// Whether a definition can take the task at reservation: same version, or newer with a migration.
fn can_reserve(task: &Task, record: &TaskRecord) -> bool {
    task.version() == record.version || (task.version() > record.version && task.migration().is_some())
}

fn missing_migration(record: &TaskRecord, task: &Task) -> HarnessError {
    HarnessError::Error(format!(
        "Task {} version {} has no migration from {}",
        record.kind,
        task.version(),
        record.version
    ))
}

/// Structural equality of two JSON values; object key order is ignored and numbers compare by value.
pub(crate) fn json_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a == b || a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| json_equal(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| json_equal(value, other)))
        }
        _ => left == right,
    }
}

// ─── Invocations ──────────────────────────────────────────────────────────────

/// One in-memory execution of a task in run or abort mode.
pub(crate) struct Invocation {
    pub(crate) task_id: TaskId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) mode: Mode,
    /// Signal passed to handlers.
    pub(crate) controller: Cancel,
    /// Admission of runtime operations; closed with the ended error when the invocation ends.
    gate: Gate<(), HarnessError>,
    control: GateControl<(), HarnessError>,
    ended: AtomicBool,
    /// Cancelled once the invocation's thread has finished.
    pub(crate) done: Cancel,
}

impl Invocation {
    fn ended_error(&self) -> HarnessError {
        HarnessError::Error(format!("Task {} invocation has ended", self.task_id.0))
    }

    /// Fails once the invocation has ended.
    pub(crate) fn check(&self) -> Result<()> {
        self.gate.admit(|| ()).map_err(|refusal| match refusal {
            Refusal::Closed(error) => error,
            Refusal::Aborting(_) => self.ended_error(),
        })
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

/// What a runtime reads for the phase handler it serves: the phase's snapshot and task.
pub(crate) struct PhaseState {
    pub(crate) task: Task,
    pub(crate) snapshot: Arc<RegistrySnapshot>,
    /// Replacement definition already reported as unable to take over: `Some(None)` for a missing one.
    reported: Option<Option<Task>>,
}

struct Reservation {
    invocation: Arc<Invocation>,
    task: Task,
    snapshot: Arc<RegistrySnapshot>,
}

/// Outcome of the phase that just returned, judged by the next step.
struct PhaseResult {
    checkpoint: Value,
    failure: Option<HarnessError>,
}

/// Step decision: continue with the next phase, end the invocation, or end it by writing `faulted`.
enum Decision {
    Continue,
    End,
    Fault(HarnessError),
}

// ─── State ────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    live: IndexMap<TaskId, TaskRecord>,
    invocations: HashMap<TaskId, Arc<Invocation>>,
    /// Definition whose migration failed per task; retried only once the registry resolves another definition.
    failed_migrations: HashMap<TaskId, (Task, HarnessError)>,
    /// Owner task of each loaded conversation, `None` when ownerless.
    edges: HashMap<ConversationId, Option<TaskId>>,
    /// Tasks that own a loaded conversation; their nodes stay in `settled` once terminal.
    conversation_owners: HashSet<TaskId>,
    /// Ownership fields of terminal tasks that walks pass through.
    settled: HashMap<TaskId, TaskNode>,
    /// `failFast` waiters the next reconcile checks for a failed task in `on`.
    fail_fast_checks: IndexSet<TaskId>,
    /// Live tasks with `abandonOnRestart` found at open; the first reservation pass abort-marks them.
    abandoned: IndexSet<TaskId>,
    reconcile_scheduled: bool,
    cascade_pending: bool,
    enabled: bool,
    closing: bool,
    dirty: bool,
}

impl State {
    fn schedule_reconcile(&mut self) {
        if !self.reconcile_scheduled && !self.closing {
            self.reconcile_scheduled = true;
        }
    }

    fn set_edge(&mut self, conversation_id: ConversationId, owner: Option<TaskId>) {
        self.edges.insert(conversation_id, owner);
        if let Some(owner) = owner {
            self.conversation_owners.insert(owner);
        }
    }

    fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>> {
        if let Some(edge) = overlay.and_then(|overlay| overlay.edges.get(&id)) {
            return Some(*edge);
        }
        self.edges.get(&id).copied()
    }

    fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        overlay
            .and_then(|overlay| overlay.tasks.get(&id))
            .or_else(|| self.live.get(&id))
            .map(node_of)
            .or_else(|| self.settled.get(&id).copied())
    }

    /// Walk up from `start`: owner tasks and conversations, ending at an ownerless root or an edge not loaded yet.
    fn above(&self, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut at = Some(start);
        while let Some(up) = at {
            match up {
                Up::Task(id) => {
                    let Some(node) = self.node(id, overlay) else {
                        steps.push(Step::Unknown);
                        return steps;
                    };
                    steps.push(Step::Task(id, node));
                    at = Some(parent_of(&node));
                }
                Up::Conversation(id) => {
                    steps.push(Step::Conversation(id));
                    match self.edge(id, overlay) {
                        None => {
                            steps.push(Step::Unknown);
                            return steps;
                        }
                        Some(edge) => at = edge.map(Up::Task),
                    }
                }
            }
        }
        steps
    }

    fn chain_known(&self, start: Up, overlay: Option<&Overlay>) -> bool {
        !self
            .above(start, overlay)
            .iter()
            .any(|step| matches!(step, Step::Unknown))
    }

    /// Live records, with the overlay's candidates replacing committed ones; terminal candidates are gone.
    fn live_records<'a>(&'a self, overlay: Option<&'a Overlay>) -> Vec<&'a TaskRecord> {
        let mut records = Vec::new();
        for record in self.live.values() {
            let candidate = overlay
                .and_then(|overlay| overlay.tasks.get(&record.id))
                .unwrap_or(record);
            if candidate.state.status() != TaskStatus::Terminal {
                records.push(candidate);
            }
        }
        if let Some(overlay) = overlay {
            for record in overlay.tasks.values() {
                if !self.live.contains_key(&record.id) && record.state.status() != TaskStatus::Terminal {
                    records.push(record);
                }
            }
        }
        records
    }

    /// Every task with live ordinary owned work, mapped to that work: each live non-background task counts for every
    /// owner task above it up to and including the first background one. Owner chains must be loaded.
    fn owned_live(&self, overlay: Option<&Overlay>) -> HashMap<TaskId, Vec<TaskId>> {
        let mut owned: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        for record in self.live_records(overlay) {
            if record.background {
                continue;
            }
            for step in self.above(record_parent(record), overlay) {
                match step {
                    Step::Unknown => break,
                    Step::Conversation(_) => continue,
                    Step::Task(id, node) => {
                        owned.entry(id).or_default().push(record.id);
                        if node.background {
                            break;
                        }
                    }
                }
            }
        }
        owned
    }

    /// Whether ordinary traversal from `scope` reaches `start`; `None` while an edge is not loaded.
    fn in_scope(&self, start: Up, scope: Scope, cross_background: bool) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(id) => {
                    if matches!(scope, Scope::Conversation(target) if target == id) {
                        return Some(true);
                    }
                }
                Step::Task(_, node) => {
                    if node.background && !cross_background {
                        return Some(false);
                    }
                }
            }
        }
        Some(matches!(scope, Scope::Roots))
    }

    /// The nearest live owner above `start` whose cancellation intent reaches it: one with intent before a
    /// background owner without it. Terminal owners never cascade.
    fn cancelling_owner(&self, start: Up) -> Option<&TaskRecord> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(_) => continue,
                Step::Task(id, node) => {
                    if let Some(live) = self.live.get(&id)
                        && cancellation_intent(live)
                    {
                        return Some(live);
                    }
                    if node.background {
                        return None;
                    }
                }
            }
        }
        None
    }

    fn below_cancelled(&self, start: Up) -> bool {
        self.cancelling_owner(start).is_some()
    }

    /// No live non-background task in the scope; a task whose owner edges are not loaded yet counts as inside.
    fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        let scope = match conversation_id {
            Some(id) => Scope::Conversation(id),
            None => Scope::Roots,
        };
        !self
            .live
            .values()
            .any(|record| !record.background && self.in_scope(record_parent(record), scope, false) != Some(false))
    }

    /// Live tasks a task waits for before its next invocation: its live ordinary owned work when abort-marked, since
    /// abort runs bottom-up, otherwise the live part of the `on` of a wait.
    fn waiting_on(&self, record: &TaskRecord, owned: &HashMap<TaskId, Vec<TaskId>>) -> Vec<TaskId> {
        if record.abort_requested {
            return owned.get(&record.id).cloned().unwrap_or_default();
        }
        match &record.state {
            TaskState::Waiting { on, .. } => on.iter().copied().filter(|id| self.live.contains_key(id)).collect(),
            _ => Vec::new(),
        }
    }

    fn fit(&self, record: &TaskRecord, task: Option<&Task>) -> Fit {
        let Some(task) = task else {
            return Fit::Blocked {
                reason: BlockedReason::MissingTask,
                error: None,
            };
        };
        if task.version() == record.version {
            return Fit::Ready {
                task: task.clone(),
                migrates: false,
            };
        }
        if task.version() < record.version {
            return Fit::Blocked {
                reason: BlockedReason::TaskTooOld,
                error: None,
            };
        }
        if let Some((failed, error)) = self.failed_migrations.get(&record.id)
            && failed.same(task)
        {
            return Fit::Blocked {
                reason: BlockedReason::MigrationFailed,
                error: Some(error.clone()),
            };
        }
        Fit::Ready {
            task: task.clone(),
            migrates: true,
        }
    }
}

fn overlay_of(tx: &Tx<'_>) -> Overlay {
    Overlay {
        tasks: tx
            .staged_tasks()
            .into_iter()
            .map(|record| (record.id, record))
            .collect(),
        edges: tx
            .staged_conversations()
            .into_iter()
            .map(|record| (record.id, record.owner.map(|owner| owner.task_id)))
            .collect(),
    }
}

// ─── Scheduler ────────────────────────────────────────────────────────────────

pub(crate) struct SchedulerOptions {
    pub session: Session,
    pub registry: Arc<dyn RegistryReader>,
    pub now: Clock,
    pub report: Report,
    pub settle_outcome: SettleOutcome,
    pub withdraw_inputs: WithdrawInputs,
}

/// Durable task scheduler of one Harness.
pub(crate) struct Scheduler {
    pub(crate) session: Session,
    registry: Arc<dyn RegistryReader>,
    pub(crate) now: Clock,
    pub(crate) report: Report,
    settle_outcome: SettleOutcome,
    withdraw_inputs: WithdrawInputs,
    state: Mutex<State>,
    /// Wakes the worker; paired with `state`.
    wake: Condvar,
    task_waiters: Waiters<TaskId, TaskRecord>,
    /// Idle waiters by conversation; `None` waits for the whole Harness.
    idle_waiters: Waiters<Option<ConversationId>, ()>,
    /// Cancelled with `SessionFailed` the moment the Session fails.
    failed: Cancel,
    unsubscribe_registry: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Whether the Session's failure has been reported (upstream `SessionImpl.fail` reports it once).
    failure_reported: AtomicBool,
    this: Weak<Scheduler>,
}

impl Scheduler {
    pub(crate) fn new(options: SchedulerOptions) -> Arc<Scheduler> {
        Arc::new_cyclic(|this| Scheduler {
            session: options.session,
            registry: options.registry,
            now: options.now,
            report: options.report,
            settle_outcome: options.settle_outcome,
            withdraw_inputs: options.withdraw_inputs,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            task_waiters: Waiters::new(),
            idle_waiters: Waiters::new(),
            failed: Cancel::new(),
            unsubscribe_registry: Mutex::new(None),
            failure_reported: AtomicBool::new(false),
            this: this.clone(),
        })
    }

    fn st(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn arc(&self) -> Arc<Scheduler> {
        self.this.upgrade().expect("scheduler is alive while used")
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.st().closing
    }

    fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        commit_on(&self.session, TxScope::default(), change)
    }

    /// Load live tasks and change surviving `running` tasks back to `pending`. Dispatches nothing.
    pub(crate) fn open(&self) -> Result<()> {
        let weak = self.this.clone();
        self.session.subscribe_commits(move |publication| {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.observe(publication);
            }
        })?;
        let weak = self.this.clone();
        self.session.subscribe_close(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.seal();
            }
        })?;
        let weak = self.this.clone();
        let unsubscribe = self.registry.subscribe(Box::new(move || {
            if let Some(scheduler) = weak.upgrade() {
                scheduler.kick();
            }
        }));
        *lock(&self.unsubscribe_registry) = Some(unsubscribe);
        if self.is_closing() {
            // A Session that failed while subscribing has already sealed; release what it could not.
            if let Some(unsubscribe) = lock(&self.unsubscribe_registry).take() {
                unsubscribe();
            }
        }
        self.commit(|tx| {
            // Every table read before the first write.
            let mut scans = Vec::new();
            for status in LIVE_STATUSES {
                let query = TaskQuery {
                    status: Some(status),
                    ..TaskQuery::default()
                };
                let mut cursor = None;
                loop {
                    let page = tx.scan_tasks(&query, SCAN_PAGE_SIZE, cursor.as_ref())?;
                    scans.extend(page.items);
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
            let mut writes = Vec::new();
            {
                let mut st = self.st();
                for record in scans {
                    if record.abandon_on_restart {
                        st.abandoned.insert(record.id);
                    }
                    if let TaskState::Running { checkpoint } = &record.state {
                        writes.push(with_state(
                            &record,
                            TaskState::Pending {
                                checkpoint: checkpoint.clone(),
                            },
                        ));
                    }
                    if let TaskState::Waiting {
                        policy: JoinPolicy::FailFast,
                        ..
                    } = &record.state
                    {
                        st.fail_fast_checks.insert(record.id);
                    }
                    st.live.insert(record.id, record);
                }
            }
            for record in writes {
                tx.set_task(record)?;
            }
            Ok(())
        })?;
        // Derive abort marks a crash left unapplied below cancelled owners, and finalize held outcomes.
        {
            let mut st = self.st();
            st.cascade_pending = true;
            st.schedule_reconcile();
        }
        let scheduler = self.arc();
        std::thread::Builder::new()
            .name("durable-scheduler".into())
            .spawn(move || scheduler.work())
            .map_err(|error| HarnessError::Error(error.to_string()))?;
        self.wake.notify_all();
        Ok(())
    }

    /// Enable scheduling. Idempotent; the kick does nothing once closing.
    pub(crate) fn resume(&self) {
        self.st().enabled = true;
        self.kick();
    }

    /// Signal every invocation and wait for them; writes nothing.
    pub(crate) fn join(&self) {
        let invocations: Vec<Arc<Invocation>> = self.st().invocations.values().cloned().collect();
        for invocation in &invocations {
            invocation.controller.abort();
        }
        for invocation in &invocations {
            invocation.done.wait();
        }
    }

    /// Commit the abort mark, or settle a task no registered definition can take as `orphaned` when nothing it owns is
    /// live, then join the run invocation seen on the line. With `keep_restart`, a restart-marked task keeps its mark.
    pub(crate) fn abort(&self, id: TaskId, ctx: &Ctx, keep_restart: bool) -> Result<AbortResult> {
        let (result, run) = self.commit(|tx| {
            ctx.check()?;
            let Some(current) = tx.task(id)? else {
                return Err(HarnessError::Error(format!("Task {} does not exist", id.0)));
            };
            if current.state.status() == TaskStatus::Terminal {
                return Ok((AbortResult::Terminal, None));
            }
            let invocation = self.st().invocations.get(&id).cloned();
            // A restart-marked task has no run invocation; it only waits for its abort.
            if keep_restart && current.abort_reason == Some(AbortReason::Restart) {
                return Ok((AbortResult::Marked, None));
            }
            if invocation.is_none() && current.state.status() != TaskStatus::Completing {
                self.load_scopes(tx, false)?;
                let owned = self.st().owned_live(None).contains_key(&id);
                if !owned {
                    let snapshot = self.registry.snapshot();
                    if let Resolution::Blocked(reason) = self.resolve(&current, &snapshot) {
                        let outcome = SchedulerOutcome::Orphaned {
                            reason: reason.as_str().into(),
                        };
                        self.terminate(tx, &current, &outcome)?;
                        return Ok((AbortResult::Marked, None));
                    }
                }
            }
            if !current.abort_requested || current.abort_reason.is_some() {
                tx.set_task(with_abort_mark(&current, None))?;
            }
            let run = invocation.filter(|invocation| invocation.mode == Mode::Run);
            Ok((AbortResult::Marked, run))
        })?;
        // The commit listener signalled the run; join it. A run that ignores its signal can outlive a failed Session,
        // which ends the wait instead.
        if let Some(run) = run {
            let caller = ctx.signal().cloned().unwrap_or_default();
            let (index, reason) = wait_any(&[&run.done, &self.failed, &caller]);
            if index != 0 {
                return Err(reason);
            }
        }
        Ok(result)
    }

    pub(crate) fn wait_for_task(&self, id: TaskId, ctx: &Ctx) -> Result<TaskRecord> {
        // Check and register on the line so no terminal publication falls between them.
        let slot = self.commit(|tx| {
            if self.is_closing() {
                return Err(closed_error(&self.session));
            }
            if self.st().live.contains_key(&id) {
                return self.task_waiters.add(id, ctx);
            }
            match tx.task(id)? {
                Some(record) => Ok(cancel::Slot::settled(Ok(record))),
                None => Err(HarnessError::Error(format!("Task {} does not exist", id.0))),
            }
        })?;
        slot.wait(ctx)
    }

    /// Resolve when ordinary traversal from the conversation, or from every ownerless conversation, reaches no live
    /// non-background task.
    pub(crate) fn wait_for_idle(&self, conversation_id: Option<ConversationId>, ctx: &Ctx) -> Result<()> {
        let slot = {
            let mut st = self.st();
            if st.closing {
                drop(st);
                return Err(closed_error(&self.session));
            }
            if st.idle(conversation_id) {
                return Ok(());
            }
            st.schedule_reconcile();
            self.wake.notify_all();
            self.idle_waiters.add(conversation_id, ctx)?
        };
        slot.wait(ctx)
    }

    /// `Conversation.abort()`: in one commit, withdraw the queued inputs and mark every live non-background task that
    /// ordinary traversal from the conversation reaches; returns once the scope is idle. With `background`, traversal
    /// crosses background boundaries, and the wait also covers every task it reached.
    pub(crate) fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        let reached = self.commit(|tx| {
            ctx.check()?;
            let queued = self.load_scopes(tx, true)?;
            let scope = Scope::Conversation(conversation_id);
            let (reached, marks, withdraw) = {
                let st = self.st();
                let mut reached = Vec::new();
                let mut marks = Vec::new();
                for record in st.live.values() {
                    if record.background && !background {
                        continue;
                    }
                    if st.in_scope(record_parent(record), scope, background) != Some(true) {
                        continue;
                    }
                    reached.push(record.id);
                    if !record.abort_requested || record.abort_reason.is_some() {
                        marks.push(with_abort_mark(record, None));
                    }
                }
                let withdraw: Vec<ConversationId> = queued
                    .into_iter()
                    .filter(|id| st.in_scope(Up::Conversation(*id), scope, background) == Some(true))
                    .collect();
                (reached, marks, withdraw)
            };
            for mark in marks {
                tx.set_task(mark)?;
            }
            for id in withdraw {
                (self.withdraw_inputs)(tx, id)?;
            }
            Ok(reached)
        })?;
        if background {
            for id in reached {
                self.wait_for_task(id, ctx)?;
            }
        }
        self.wait_for_idle(Some(conversation_id), ctx)
    }

    // ─── Scheduling ───────────────────────────────────────────────────────────

    fn observe(&self, publication: &CommitPublication) {
        let mut signal = Vec::new();
        let mut resolved = Vec::new();
        let mut changed = false;
        {
            let mut st = self.st();
            let mut updated = Vec::new();
            let mut failed = Vec::new();
            for change in &publication.changes {
                let CommitChange::Table(StorageWrite::Task { value: record }) = change else {
                    continue;
                };
                changed = true;
                let previous = st.live.get(&record.id).cloned();
                if failed_outcome(record) && previous.as_ref().is_none_or(|previous| !failed_outcome(previous)) {
                    failed.push(record.id);
                }
                if record.state.status() == TaskStatus::Terminal {
                    st.live.shift_remove(&record.id);
                    st.failed_migrations.remove(&record.id);
                    st.fail_fast_checks.shift_remove(&record.id);
                    if st.conversation_owners.contains(&record.id) {
                        st.settled.insert(record.id, node_of(record));
                    }
                    resolved.push(record.clone());
                    // Its owner may finalize now.
                    st.schedule_reconcile();
                    continue;
                }
                if record.abort_requested && previous.as_ref().is_none_or(|previous| !previous.abort_requested) {
                    st.cascade_pending = true;
                    // Signal a run invocation of the newly marked task; its next step ends it.
                    if let Some(invocation) = st.invocations.get(&record.id)
                        && invocation.mode == Mode::Run
                    {
                        signal.push(invocation.controller.clone());
                    }
                }
                // A request replacing a `restart` mark upgrades the marks of the work below.
                if previous
                    .as_ref()
                    .is_some_and(|previous| previous.abort_reason.is_some())
                    && record.abort_reason.is_none()
                {
                    st.cascade_pending = true;
                }
                let status = record.state.status();
                let previous_status = previous.as_ref().map(|previous| previous.state.status());
                if status == TaskStatus::Completing && previous_status != Some(TaskStatus::Completing) {
                    if cancellation_intent(record) {
                        st.cascade_pending = true;
                    }
                    st.schedule_reconcile();
                }
                if let TaskState::Waiting {
                    policy: JoinPolicy::FailFast,
                    ..
                } = record.state
                    && previous_status != Some(TaskStatus::Waiting)
                {
                    st.fail_fast_checks.insert(record.id);
                    st.schedule_reconcile();
                }
                st.live.insert(record.id, record.clone());
                updated.push(record.clone());
            }
            for id in failed {
                let waiters: Vec<TaskId> = st
                    .live
                    .values()
                    .filter(|record| {
                        matches!(&record.state, TaskState::Waiting { policy: JoinPolicy::FailFast, on, .. } if on.contains(&id))
                    })
                    .map(|record| record.id)
                    .collect();
                for waiter in waiters {
                    st.fail_fast_checks.insert(waiter);
                    st.schedule_reconcile();
                }
            }
            for change in &publication.changes {
                if let CommitChange::Table(StorageWrite::Conversation { value }) = change
                    && !st.edges.contains_key(&value.id)
                {
                    st.set_edge(value.id, value.owner.map(|owner| owner.task_id));
                }
            }
            for change in &publication.changes {
                // A queued input below a cancelled owner is withdrawn, even after its cascade.
                let CommitChange::Table(StorageWrite::Submission { value }) = change else {
                    continue;
                };
                if value.status != SubmissionStatus::Queued || value.kind != SubmissionType::Input {
                    continue;
                }
                let up = Up::Conversation(value.conversation_id);
                if !st.chain_known(up, None) || st.below_cancelled(up) {
                    st.cascade_pending = true;
                }
            }
            for record in &updated {
                // Work created below a cancelled owner, even after its cascade, is aborted too.
                let parent = record_parent(record);
                if !st.chain_known(parent, None) {
                    st.schedule_reconcile();
                } else if !record.background && !record.abort_requested && st.below_cancelled(parent) {
                    st.cascade_pending = true;
                }
            }
            // A cascade that a reservation or step found pending runs with the next commit of any kind.
            if st.cascade_pending {
                st.schedule_reconcile();
            }
        }
        self.wake.notify_all();
        for controller in signal {
            controller.abort();
        }
        for record in resolved {
            self.task_waiters.resolve(&record.id, record.clone());
        }
        if !changed {
            return;
        }
        self.settle_idle();
        self.kick();
    }

    /// Resolve idle waiters whose scope is idle.
    fn settle_idle(&self) {
        let idle: Vec<Option<ConversationId>> = {
            let st = self.st();
            self.idle_waiters
                .keys()
                .into_iter()
                .filter(|conversation_id| st.idle(*conversation_id))
                .collect()
        };
        for conversation_id in idle {
            self.idle_waiters.resolve(&conversation_id, ());
        }
    }

    fn kick(&self) {
        self.st().dirty = true;
        self.wake.notify_all();
    }

    /// The worker: upstream's reconcile and drain microtasks, one at a time.
    fn work(self: Arc<Self>) {
        enum Job {
            Reconcile,
            Reserve,
        }
        loop {
            let job = {
                let mut st = self.st();
                loop {
                    if st.closing {
                        return;
                    }
                    if st.reconcile_scheduled {
                        break Job::Reconcile;
                    }
                    if st.dirty && st.enabled {
                        st.dirty = false;
                        break Job::Reserve;
                    }
                    st = self.wake.wait(st).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            match job {
                Job::Reconcile => self.reconcile(),
                Job::Reserve => match self.reserve() {
                    Ok(reservations) => {
                        for reservation in reservations {
                            self.start(reservation);
                        }
                    }
                    // As in `reconcile`: the reservation commit runs no task code.
                    Err(error) => self.fail_session(error),
                },
            }
        }
    }

    /// One commit that applies what committed records imply: abort marks below live owners with cancellation intent,
    /// `failFast` marks, withdrawn queued inputs below cancelled owners, and the final terminal record of every
    /// `completing` task whose ordinary owned work is gone. The durable records are the intent, so this also repairs
    /// whatever a crash left unapplied. Resolves idle waiters that the loaded edges decide.
    fn reconcile(&self) {
        let (cascade, checks) = {
            let mut st = self.st();
            st.reconcile_scheduled = false;
            let cascade = std::mem::take(&mut st.cascade_pending);
            let checks: Vec<TaskId> = std::mem::take(&mut st.fail_fast_checks).into_iter().collect();
            (cascade, checks)
        };
        let result = self.commit(|tx| {
            if self.is_closing() {
                return Ok(());
            }
            let queued = self.load_scopes(tx, cascade)?;
            // A cascade from an abandoned owner passes its `restart` reason on; any other intent marks, or upgrades a
            // `restart` mark, as a request. Collected first, so a request wins over a `restart` mark of the same pass.
            let mut marks: IndexMap<TaskId, (TaskRecord, Option<AbortReason>)> = IndexMap::new();
            fn mark(
                marks: &mut IndexMap<TaskId, (TaskRecord, Option<AbortReason>)>,
                record: &TaskRecord,
                reason: Option<AbortReason>,
            ) {
                if let Some((_, staged)) = marks.get(&record.id)
                    && staged.is_none()
                {
                    return;
                }
                if record.abort_requested && (record.abort_reason.is_none() || reason.is_some()) {
                    return;
                }
                marks.insert(record.id, (record.clone(), reason));
            }
            let fail_fast: Vec<(TaskId, Vec<TaskId>)> = {
                let st = self.st();
                // Loading edges can reveal a cancelled owner, so marks are derived on every pass.
                for record in st.live.values() {
                    if record.background {
                        continue;
                    }
                    let Some(owner) = st.cancelling_owner(record_parent(record)) else {
                        continue;
                    };
                    // Only an owner abandoned after a restart, and nothing else, passes its reason on.
                    let restart = owner.abort_reason == Some(AbortReason::Restart) && !failed_outcome(owner);
                    mark(&mut marks, record, restart.then_some(AbortReason::Restart));
                }
                checks
                    .iter()
                    .filter_map(|id| match st.live.get(id).map(|record| &record.state) {
                        Some(TaskState::Waiting { on, .. }) => Some((*id, on.clone())),
                        _ => None,
                    })
                    .collect()
            };
            for (_, on) in fail_fast {
                if !self.any_failed(tx, &on)? {
                    continue;
                }
                // Every other live task: the failed one keeps its own outcome.
                let st = self.st();
                for member in &on {
                    if let Some(record) = st.live.get(member)
                        && !failed_outcome(record)
                    {
                        mark(&mut marks, record, None);
                    }
                }
            }
            for (record, reason) in marks.values() {
                tx.set_task(with_abort_mark(record, *reason))?;
            }
            let withdraw: Vec<ConversationId> = {
                let st = self.st();
                queued
                    .into_iter()
                    .filter(|id| st.below_cancelled(Up::Conversation(*id)))
                    .collect()
            };
            for id in withdraw {
                (self.withdraw_inputs)(tx, id)?;
            }
            self.finalize(tx)
        });
        if let Err(error) = result {
            // No extension code runs in this commit: a throw is a storage failure, a host callback, or a bug. None is
            // fixed by running the pass again, so it fails the Session.
            self.fail_session(error);
        }
        self.settle_idle();
    }

    /// Whether any of `ids` holds or ended with an outcome other than `completed`.
    fn any_failed(&self, tx: &mut Tx<'_>, ids: &[TaskId]) -> Result<bool> {
        for id in ids {
            let live = self.st().live.get(id).cloned();
            let record = match live {
                Some(record) => Some(record),
                None => tx.committed_task_record(*id)?,
            };
            if record.is_some_and(|record| failed_outcome(&record)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Write the terminal record of every `completing` task without live ordinary owned work, repeating over the
    /// commit's candidates until nothing changes. A held scheduler outcome gets its Harness cleanup here.
    fn finalize(&self, tx: &mut Tx<'_>) -> Result<()> {
        loop {
            let overlay = overlay_of(tx);
            let done: Vec<TaskRecord> = {
                let st = self.st();
                let owned = st.owned_live(Some(&overlay));
                st.live_records(Some(&overlay))
                    .into_iter()
                    .filter(|record| record.state.status() == TaskStatus::Completing && !owned.contains_key(&record.id))
                    .cloned()
                    .collect()
            };
            if done.is_empty() {
                return Ok(());
            }
            for record in done {
                let TaskState::Completing { outcome } = &record.state else {
                    unreachable!("filtered")
                };
                let outcome = outcome.clone();
                tx.set_task(with_state(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                ))?;
                // Only the scheduler writes `faulted` and `orphaned`; their cleanup waits for this commit.
                let scheduler_outcome = match outcome {
                    TaskOutcome::Faulted { error } => Some(SchedulerOutcome::Faulted { message: error.message }),
                    TaskOutcome::Orphaned { reason } => Some(SchedulerOutcome::Orphaned { reason }),
                    _ => None,
                };
                if let Some(outcome) = scheduler_outcome {
                    (self.settle_outcome)(tx, &record, &outcome)?;
                }
            }
        }
    }

    /// Load the owner chains of every live task and, with `queued`, of every conversation with queued submissions;
    /// returns the latter. Reads committed Storage, so it may run inside a commit callback.
    fn load_scopes(&self, tx: &mut Tx<'_>, queued: bool) -> Result<Vec<ConversationId>> {
        let parents: Vec<Up> = {
            let st = self.st();
            st.live
                .values()
                .map(record_parent)
                .filter(|parent| !st.chain_known(*parent, None))
                .collect()
        };
        for parent in parents {
            self.load_chain(tx, parent, None)?;
        }
        if !queued {
            return Ok(Vec::new());
        }
        let query = SubmissionQuery {
            status: Some(SubmissionStatus::Queued),
            ..SubmissionQuery::default()
        };
        let mut conversations: IndexSet<ConversationId> = IndexSet::new();
        let mut cursor = None;
        loop {
            let page = tx.committed_scan_submissions(&query, SCAN_PAGE_SIZE, cursor.as_ref())?;
            conversations.extend(page.items.iter().map(|submission| submission.conversation_id));
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        for id in &conversations {
            self.load_chain(tx, Up::Conversation(*id), None)?;
        }
        Ok(conversations.into_iter().collect())
    }

    /// Load the owner edges and task nodes from `start` up to its ownerless root.
    fn load_chain(&self, tx: &mut Tx<'_>, start: Up, overlay: Option<&Overlay>) -> Result<()> {
        let mut at = Some(start);
        while let Some(up) = at {
            match up {
                Up::Task(id) => {
                    let known = self.st().node(id, overlay);
                    let node = match known {
                        Some(node) => node,
                        None => {
                            let Some(record) = tx.committed_task_record(id)? else {
                                return Ok(());
                            };
                            let node = node_of(&record);
                            if record.state.status() == TaskStatus::Terminal {
                                self.st().settled.insert(record.id, node);
                            }
                            node
                        }
                    };
                    at = Some(parent_of(&node));
                }
                Up::Conversation(id) => {
                    let known = self.st().edge(id, overlay);
                    let edge = match known {
                        Some(edge) => edge,
                        None => {
                            let record = tx.committed_conversation(id)?;
                            let edge = record.and_then(|record| record.owner).map(|owner| owner.task_id);
                            self.st().set_edge(id, edge);
                            edge
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        Ok(())
    }

    /// Fail the Session for a throw in one of the scheduler's own commits; ignored once closing.
    fn fail_session(&self, error: HarnessError) {
        if !self.is_closing() {
            self.session.fail(error.into_session());
        }
    }

    /// Close listener: runs synchronously once admission is sealed, or when the Session fails (on the failing
    /// thread, which may hold the line: then it only signals). A plain close also joins every invocation here, after
    /// admission is sealed and before Storage closes (upstream `beforeClose`).
    fn seal(&self) {
        {
            let mut st = self.st();
            st.closing = true;
        }
        self.wake.notify_all();
        if let Some(unsubscribe) = lock(&self.unsubscribe_registry).take() {
            unsubscribe();
        }
        let error = closed_error(&self.session);
        self.task_waiters.reject_all(error.clone());
        self.idle_waiters.reject_all(error.clone());
        match self.session.failure() {
            Some(_) => {
                self.failed.cancel(error);
                self.report_failure();
                let invocations: Vec<Arc<Invocation>> = self.st().invocations.values().cloned().collect();
                for invocation in invocations {
                    invocation.controller.abort();
                }
            }
            None => self.join(),
        }
    }

    /// Report the Session's failure once. A failure during a close that already ran the close listeners (a commit
    /// admitted before close failing) is reported when the close finishes.
    pub(crate) fn report_failure(&self) {
        if let Some(failure) = self.session.failure()
            && !self.failure_reported.swap(true, Ordering::SeqCst)
        {
            self.failed.cancel(closed_error(&self.session));
            (self.report)(&HarnessError::Session(failure));
        }
    }

    /// Reserve every eligible task in one commit; orphan abort-marked tasks no definition can take, unless abandoned
    /// after a restart. The first pass after open only abort-marks the abandoned tasks, so later passes see the marks.
    fn reserve(&self) -> Result<Vec<Reservation>> {
        let mut reservations: Vec<Reservation> = Vec::new();
        let mut abandoned: Vec<TaskId> = Vec::new();
        let result = self.commit(|tx| {
            let marks: Vec<TaskRecord> = {
                let st = self.st();
                if !st.enabled || st.closing {
                    return Ok(());
                }
                st.abandoned
                    .iter()
                    .filter_map(|id| st.live.get(id))
                    .filter(|record| !record.abort_requested)
                    .map(|record| with_abort_mark(record, Some(AbortReason::Restart)))
                    .collect()
            };
            for mark in marks {
                tx.set_task(mark)?;
            }
            {
                let st = self.st();
                if !st.abandoned.is_empty() {
                    abandoned = st.abandoned.iter().copied().collect();
                    return Ok(());
                }
            }
            self.load_scopes(tx, false)?;
            let (candidates, owned) = {
                let st = self.st();
                (st.live.values().cloned().collect::<Vec<_>>(), st.owned_live(None))
            };
            // Taken once per pass, and only when some task is a candidate.
            let mut snapshot: Option<Arc<RegistrySnapshot>> = None;
            for record in candidates {
                let mode = if record.abort_requested { Mode::Abort } else { Mode::Run };
                {
                    let mut st = self.st();
                    if st.invocations.contains_key(&record.id) || !st.waiting_on(&record, &owned).is_empty() {
                        continue;
                    }
                    if record.state.status() == TaskStatus::Completing {
                        continue;
                    }
                    // Work below an owner with cancellation intent waits for its cascade mark instead of running a
                    // phase; schedule the cascade, which may not have run yet.
                    if mode == Mode::Run && !record.background && st.below_cancelled(record_parent(&record)) {
                        st.cascade_pending = true;
                        st.schedule_reconcile();
                        continue;
                    }
                }
                let snapshot = snapshot.get_or_insert_with(|| self.registry.snapshot()).clone();
                let (task, resolved) = match self.resolve(&record, &snapshot) {
                    Resolution::Blocked(reason) => {
                        // An abandoned task waits for its definition, so its abort handler can clean up.
                        if mode == Mode::Abort && record.abort_reason.is_none() {
                            let outcome = SchedulerOutcome::Orphaned {
                                reason: reason.as_str().into(),
                            };
                            self.terminate(tx, &record, &outcome)?;
                        }
                        continue;
                    }
                    Resolution::Ready { task, record } => (task, *record),
                };
                if resolved != record || record.state.status() != TaskStatus::Running {
                    let checkpoint = checkpoint_of(&resolved).cloned().unwrap_or(Value::Null);
                    tx.set_task(with_state(&resolved, TaskState::Running { checkpoint }))?;
                }
                // Registered on the line, so marks and later reservations see it and close joins it.
                let invocation = self.create_invocation(&record, mode);
                reservations.push(Reservation {
                    invocation,
                    task,
                    snapshot,
                });
            }
            Ok(())
        });
        if let Err(error) = result {
            let mut st = self.st();
            for reservation in &reservations {
                let invocation = &reservation.invocation;
                if st
                    .invocations
                    .get(&invocation.task_id)
                    .is_some_and(|current| Arc::ptr_eq(current, invocation))
                {
                    st.invocations.remove(&invocation.task_id);
                }
                invocation.done.abort();
            }
            return Err(error);
        }
        // Marked, or already marked or gone: done with them. Reserve again, now seeing the marks.
        let mut st = self.st();
        for id in &abandoned {
            st.abandoned.shift_remove(id);
        }
        if !abandoned.is_empty() {
            st.dirty = true;
        }
        Ok(reservations)
    }

    /// Resolve the record's definition by kind, migrating an older stored version.
    fn resolve(&self, record: &TaskRecord, snapshot: &RegistrySnapshot) -> Resolution {
        let fit = self.st().fit(record, snapshot.task(&record.kind));
        let (task, migrates) = match fit {
            Fit::Blocked { reason, .. } => return Resolution::Blocked(reason),
            Fit::Ready { task, migrates } => (task, migrates),
        };
        if !migrates {
            return Resolution::Ready {
                task,
                record: Box::new(record.clone()),
            };
        }
        let migrated = match task.migration() {
            None => Err(missing_migration(record, &task)),
            Some(migrate) => {
                let checkpoint = checkpoint_of(record).cloned().unwrap_or(Value::Null);
                contained(|| migrate(&record.input, &checkpoint, record.version))
            }
        };
        match migrated {
            Ok((input, checkpoint)) => {
                let state = match &record.state {
                    TaskState::Waiting { on, policy, .. } => TaskState::Waiting {
                        checkpoint,
                        on: on.clone(),
                        policy: *policy,
                    },
                    TaskState::Running { .. } => TaskState::Running { checkpoint },
                    _ => TaskState::Pending { checkpoint },
                };
                Resolution::Ready {
                    record: Box::new(TaskRecord {
                        version: task.version(),
                        input,
                        state,
                        ..record.clone()
                    }),
                    task,
                }
            }
            Err(error) => {
                self.st().failed_migrations.insert(record.id, (task, error.clone()));
                (self.report)(&error);
                Resolution::Blocked(BlockedReason::MigrationFailed)
            }
        }
    }

    /// Scheduling state and every live task with its derived state, read on the line. Runs no task code.
    pub(crate) fn inspect(
        &self,
        tx: &mut Tx<'_>,
        snapshot: &RegistrySnapshot,
    ) -> Result<(Scheduling, Vec<TaskInspection>)> {
        self.load_scopes(tx, false)?;
        let st = self.st();
        let owned = st.owned_live(None);
        let tasks = st
            .live
            .values()
            .map(|record| TaskInspection {
                record: record.clone(),
                state: self.inspect_task(&st, record, snapshot, &owned),
            })
            .collect();
        let scheduling = if st.closing {
            Scheduling::Closing
        } else if st.enabled {
            Scheduling::Running
        } else {
            Scheduling::Paused
        };
        Ok((scheduling, tasks))
    }

    fn inspect_task(
        &self,
        st: &State,
        record: &TaskRecord,
        snapshot: &RegistrySnapshot,
        owned: &HashMap<TaskId, Vec<TaskId>>,
    ) -> TaskInspectionState {
        if st.invocations.contains_key(&record.id) {
            return TaskInspectionState::Running;
        }
        if record.state.status() == TaskStatus::Completing {
            return TaskInspectionState::Completing;
        }
        let on = st.waiting_on(record, owned);
        if !on.is_empty() {
            return TaskInspectionState::Waiting { on };
        }
        match st.fit(record, snapshot.task(&record.kind)) {
            Fit::Blocked { reason, error } => TaskInspectionState::Blocked { reason, error },
            Fit::Ready { task, migrates } => {
                if migrates && task.migration().is_none() {
                    TaskInspectionState::Blocked {
                        reason: BlockedReason::MigrationFailed,
                        error: Some(missing_migration(record, &task)),
                    }
                } else {
                    TaskInspectionState::Ready { migrates }
                }
            }
        }
    }

    fn create_invocation(&self, record: &TaskRecord, mode: Mode) -> Arc<Invocation> {
        let (gate, control) = create_gate();
        let invocation = Arc::new(Invocation {
            task_id: record.id,
            conversation_id: record.conversation_id,
            mode,
            controller: Cancel::new(),
            gate,
            control,
            ended: AtomicBool::new(false),
            done: Cancel::new(),
        });
        self.st().invocations.insert(record.id, invocation.clone());
        invocation
    }

    fn start(&self, reservation: Reservation) {
        let scheduler = self.arc();
        let name = format!("durable-task-{}", reservation.invocation.task_id.0);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let invocation = reservation.invocation.clone();
            let result = match invocation.mode {
                Mode::Run => scheduler.run(reservation),
                Mode::Abort => scheduler.run_abort(reservation),
            };
            if let Err(error) = result {
                (scheduler.report)(&error);
            }
            scheduler.end(&invocation);
            invocation.done.abort();
            scheduler.kick();
        });
        if let Err(error) = spawned {
            (self.report)(&HarnessError::Error(error.to_string()));
        }
    }

    /// Run phase handlers, each preceded by a step that decides on the line whether the invocation continues.
    fn run(&self, reservation: Reservation) -> Result<()> {
        let invocation = reservation.invocation;
        let phase = Arc::new(Mutex::new(PhaseState {
            task: reservation.task,
            snapshot: reservation.snapshot,
            reported: None,
        }));
        let runtime = TaskRuntime::new(self.arc(), invocation.clone(), phase.clone());
        let mut previous: Option<PhaseResult> = None;
        loop {
            let current = self.step(&invocation, |tx, current| {
                self.decide(tx, current, previous.as_ref(), &phase)
            });
            // Close may seal between the decision and dispatch.
            let Some(current) = current else { return Ok(()) };
            if self.is_closing() {
                return Ok(());
            }
            let checkpoint = checkpoint_of(&current).cloned().unwrap_or(Value::Null);
            let task = lock(&phase).task.clone();
            let name = phase_of(&current).unwrap_or_default().to_owned();
            let result = match task.phase(&name) {
                Some(handler) => contained(|| handler(&current, &runtime)),
                None => Err(HarnessError::Error(format!(
                    "Task {} has no phase {}",
                    current.kind, name
                ))),
            };
            previous = Some(PhaseResult {
                checkpoint,
                failure: result.err(),
            });
        }
    }

    /// Precedence rules for a run invocation, on the line. Rules 1 (terminal, `completing`, or `waiting`) and 2
    /// (closing) are applied by `step`.
    fn decide(
        &self,
        tx: &mut Tx<'_>,
        current: &TaskRecord,
        previous: Option<&PhaseResult>,
        phase: &Mutex<PhaseState>,
    ) -> Result<Decision> {
        // 3. abort mark: end; a fresh abort invocation starts once the task's ordinary owned work is gone.
        if current.abort_requested {
            return Ok(Decision::End);
        }
        // An owner's cancellation intent ends it too, before its cascade marks it; reservation then holds it back.
        {
            let mut st = self.st();
            if !current.background && st.below_cancelled(record_parent(current)) {
                st.cascade_pending = true;
                st.schedule_reconcile();
                drop(st);
                self.wake.notify_all();
                return Ok(Decision::End);
            }
        }
        let Some(previous) = previous else {
            return Ok(Decision::Continue);
        };
        // 4. uncaught error.
        if let Some(failure) = &previous.failure {
            return Ok(Decision::Fault(failure.clone()));
        }
        // 6. no durable progress.
        let checkpoint = checkpoint_of(current).cloned().unwrap_or(Value::Null);
        if json_equal(&checkpoint, &previous.checkpoint) {
            let name = previous
                .checkpoint
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(Decision::Fault(HarnessError::Error(format!(
                "Task {} phase {} returned without durable progress",
                current.kind, name
            ))));
        }
        // 5. progress: refresh the snapshot; hand over to a replacement definition that can take the task.
        let snapshot = self.registry.snapshot();
        let next = snapshot.task(&current.kind).cloned();
        let report = {
            let mut phase = lock(phase);
            phase.snapshot = snapshot;
            let replaced = match &next {
                Some(next) => !next.same(&phase.task),
                None => true,
            };
            if !replaced {
                None
            } else if next.as_ref().is_some_and(|next| can_reserve(next, current)) {
                drop(phase);
                tx.set_task(with_state(current, TaskState::Pending { checkpoint }))?;
                return Ok(Decision::End);
            } else {
                let already = phase.reported.as_ref().is_some_and(|reported| match (reported, &next) {
                    (None, None) => true,
                    (Some(reported), Some(next)) => reported.same(next),
                    _ => false,
                });
                if already {
                    None
                } else {
                    phase.reported = Some(next.clone());
                    Some(HarnessError::Caused {
                        message: format!(
                            "Task {} keeps running under its old {} definition",
                            current.id.0, current.kind
                        ),
                        cause: if next.is_none() {
                            "missing_task"
                        } else {
                            "incompatible_task"
                        }
                        .into(),
                    })
                }
            }
        };
        if let Some(error) = report {
            (self.report)(&error);
        }
        Ok(Decision::Continue)
    }

    /// Run the abort handler once; rules 1, 2, and 4 apply, and returning without an outcome faults.
    fn run_abort(&self, reservation: Reservation) -> Result<()> {
        let invocation = reservation.invocation;
        let current = self.st().live.get(&invocation.task_id).cloned();
        let Some(current) = current else { return Ok(()) };
        if self.is_closing() {
            return Ok(());
        }
        let phase = Arc::new(Mutex::new(PhaseState {
            task: reservation.task.clone(),
            snapshot: reservation.snapshot,
            reported: None,
        }));
        let runtime = TaskRuntime::new(self.arc(), invocation.clone(), phase);
        let handler = reservation.task.abort_handler().clone();
        let failure = contained(|| handler(&current, &runtime)).err();
        let message = format!(
            "Abort handler of task {} returned without a terminal outcome",
            invocation.task_id.0
        );
        let fault = failure.unwrap_or(HarnessError::Error(message));
        self.step(&invocation, |_, _| Ok(Decision::Fault(fault)));
        Ok(())
    }

    /// One decision on the Session line. A task that is no longer running or a closing Harness ends the invocation
    /// without a write; otherwise `decide` may stage a write and returns whether the invocation continues. A rejected
    /// step, such as admission after close, also ends the invocation.
    fn step(
        &self,
        invocation: &Arc<Invocation>,
        decide: impl FnOnce(&mut Tx<'_>, &TaskRecord) -> Result<Decision>,
    ) -> Option<TaskRecord> {
        let result = self.commit(|tx| {
            let current = {
                let st = self.st();
                match st.live.get(&invocation.task_id) {
                    Some(found) if found.state.status() == TaskStatus::Running && !st.closing => Some(found.clone()),
                    _ => None,
                }
            };
            let decision = match &current {
                Some(current) => decide(tx, current)?,
                None => Decision::End,
            };
            if let Decision::Continue = decision {
                return Ok(current);
            }
            self.end(invocation);
            if let (Decision::Fault(error), Some(current)) = (decision, &current) {
                let outcome = SchedulerOutcome::Faulted {
                    message: error.message(),
                };
                self.terminate(tx, current, &outcome)?;
            }
            Ok(None)
        });
        match result {
            Ok(current) => current,
            Err(error) => {
                // The step writes the scheduler's own decision; a throw there would otherwise leave the task running,
                // to be reserved and run again.
                self.end(invocation);
                self.fail_session(error);
                None
            }
        }
    }

    /// Write an outcome the scheduler decided. While the task's ordinary owned work is live it holds as `completing`
    /// and its Harness cleanup waits for the final commit; otherwise it is terminal with its cleanup.
    fn terminate(&self, tx: &mut Tx<'_>, record: &TaskRecord, outcome: &SchedulerOutcome) -> Result<()> {
        self.load_scopes(tx, false)?;
        let overlay = overlay_of(tx);
        let held = self.st().owned_live(Some(&overlay)).contains_key(&record.id);
        if held {
            tx.set_task(with_state(
                record,
                TaskState::Completing {
                    outcome: outcome.outcome(),
                },
            ))?;
            return Ok(());
        }
        tx.set_task(with_state(
            record,
            TaskState::Terminal {
                outcome: outcome.outcome(),
            },
        ))?;
        (self.settle_outcome)(tx, record, outcome)
    }

    /// Replace a running task's state with what it committed. A terminal state holds as `completing` while ordinary
    /// owned work is live, judged on the commit's candidates. A wait is validated first.
    pub(crate) fn commit_state(
        &self,
        tx: &mut Tx<'_>,
        invocation: &Invocation,
        current: &TaskRecord,
        next: NextState,
    ) -> Result<()> {
        let state = match next {
            NextState::Running { checkpoint } => TaskState::Running { checkpoint },
            NextState::Waiting { checkpoint, on, policy } => {
                self.validate_wait(tx, invocation, current, &on, policy)?;
                TaskState::Waiting { checkpoint, on, policy }
            }
            NextState::Terminal { outcome } => {
                let overlay = overlay_of(tx);
                self.load_scopes(tx, false)?;
                for record in overlay.tasks.values() {
                    self.load_chain(tx, record_parent(record), Some(&overlay))?;
                }
                if self.st().owned_live(Some(&overlay)).contains_key(&current.id) {
                    tx.set_task(with_state(current, TaskState::Completing { outcome }))?;
                    return Ok(());
                }
                TaskState::Terminal { outcome }
            }
        };
        tx.set_task(with_state(current, state))?;
        Ok(())
    }

    /// A wait names existing tasks other than the waiter and its owners; `failFast` only tasks the waiter owns. An
    /// abort handler cannot wait.
    fn validate_wait(
        &self,
        tx: &mut Tx<'_>,
        invocation: &Invocation,
        current: &TaskRecord,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> Result<()> {
        if invocation.mode == Mode::Abort {
            return Err(HarnessError::Error(format!(
                "Abort handler of task {} cannot wait",
                current.id.0
            )));
        }
        let overlay = overlay_of(tx);
        self.load_chain(tx, record_parent(current), None)?;
        let owners: HashSet<TaskId> = self
            .st()
            .above(record_parent(current), None)
            .into_iter()
            .filter_map(|step| match step {
                Step::Task(id, _) => Some(id),
                _ => None,
            })
            .collect();
        for id in on {
            if *id == current.id || owners.contains(id) {
                return Err(HarnessError::Error(format!(
                    "Task {} cannot wait on itself or its owner {}",
                    current.id.0, id.0
                )));
            }
            let known = overlay
                .tasks
                .get(id)
                .cloned()
                .or_else(|| self.st().live.get(id).cloned());
            let member = match known {
                Some(member) => Some(member),
                None => tx.committed_task_record(*id)?,
            };
            let Some(member) = member else {
                return Err(HarnessError::Error(format!("Task {} does not exist", id.0)));
            };
            if policy == JoinPolicy::FailFast && member.owner != Some(current.id) {
                return Err(HarnessError::Error(format!(
                    "Task {} can wait failFast only on tasks it owns; {} is not one",
                    current.id.0, id.0
                )));
            }
        }
        Ok(())
    }

    /// End an invocation: its runtime operations reject from now on, its signal aborts, and its task is free.
    pub(crate) fn end(&self, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut st = self.st();
            if st
                .invocations
                .get(&invocation.task_id)
                .is_some_and(|current| Arc::ptr_eq(current, invocation))
            {
                st.invocations.remove(&invocation.task_id);
            }
        }
        let error = invocation.ended_error();
        invocation.control.close(error.clone(), "invocation has ended");
        // Pending waits bound to the invocation, such as a waitForTask(), reject with it.
        invocation.controller.cancel(error);
    }

    pub(crate) fn live_record(&self, id: TaskId) -> Option<TaskRecord> {
        self.st().live.get(&id).cloned()
    }

    pub(crate) fn registry(&self) -> &Arc<dyn RegistryReader> {
        &self.registry
    }
}
