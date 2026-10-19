//! Port of upstream `test/session-support.ts` (ControlledStorage) and `test/task-support.ts` for the scheduler
//! tests: a storage that records commits, holds commits at a gate, injects one commit failure, simulates a crash,
//! and reopens. It runs over a MemoryStorage kept across reopen, or over one that reopens by replaying its
//! acknowledged commits, the way the product's journal-backed storage reopens (`replay_support`).

#![allow(dead_code)]

use durable::TaskOptions;
use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, MemoryStorage, Page, Seq, Storage, StorageError,
    StorageWrite, StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery,
    TaskRecord,
};
use harness::scheduler::{
    Cancel, Conversation, Ctx, Extension, Harness, HarnessError, HarnessOptions, NextState, Registry, Task, TaskRuntime,
};
use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[path = "../../../durable/tests/replay_support/mod.rs"]
mod replay;

use replay::ReplayLog;

type Result<T> = std::result::Result<T, StorageError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Memory,
    /// Reopens by replaying the acknowledged commits into a fresh working copy.
    Replay,
}

pub const BACKENDS: [Backend; 2] = [Backend::Memory, Backend::Replay];

/// A hang guard, never a timing expectation: a loaded machine must slow a test down, never fail it.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// A gate that holds calls until released and reports when the first held call arrives.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl Gate {
    pub fn wait_entered(&self) {
        let (state, changed) = &*self.0;
        let state = state.lock().unwrap();
        let (state, _) = changed.wait_timeout_while(state, HANG_GUARD, |state| !state.0).unwrap();
        assert!(state.0, "gate never entered");
    }

    pub fn entered(&self) -> bool {
        self.0.0.lock().unwrap().0
    }

    pub fn release(&self) {
        let (state, changed) = &*self.0;
        state.lock().unwrap().1 = true;
        changed.notify_all();
    }

    fn pass(&self) {
        let (state, changed) = &*self.0;
        let mut state = state.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    }
}

type CommitCondition = Box<dyn Fn(&[StorageWrite]) -> bool + Send>;
type CommitFilter = Box<dyn FnMut(&[StorageWrite]) -> Option<StorageError> + Send>;

#[derive(Default)]
struct Control {
    commits: Vec<Vec<StorageWrite>>,
    commit_gate: Option<Gate>,
    commit_failure: Option<StorageError>,
    filter: Option<CommitFilter>,
    mint_failure: Option<StorageError>,
    task_read_failure: Option<StorageError>,
    close_failure: Option<StorageError>,
    conditional_gate: Option<(CommitCondition, Gate)>,
    closed: bool,
    generation: u64,
}

struct Shared {
    backend: Backend,
    log: ReplayLog,
    inner: Mutex<Box<dyn Storage + Send>>,
    control: Mutex<Control>,
}

/// A Storage handle over the shared controlled backend.
#[derive(Clone)]
pub struct Controlled {
    shared: Arc<Shared>,
    generation: u64,
}

fn open(backend: Backend, log: &ReplayLog) -> Box<dyn Storage + Send> {
    match backend {
        Backend::Memory => Box::new(MemoryStorage::new()),
        Backend::Replay => Box::new(log.open()),
    }
}

impl Controlled {
    pub fn new(backend: Backend) -> Controlled {
        let log = ReplayLog::default();
        let inner = open(backend, &log);
        Controlled {
            shared: Arc::new(Shared {
                backend,
                log,
                inner: Mutex::new(inner),
                control: Mutex::new(Control::default()),
            }),
            generation: 0,
        }
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.shared
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn inner(&self) -> MutexGuard<'_, Box<dyn Storage + Send>> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn open_check(&self) -> Result<()> {
        let control = self.control();
        if control.closed || control.generation != self.generation {
            Err(StorageError::Failed("Storage is closed".into()))
        } else {
            Ok(())
        }
    }

    pub fn backend(&self) -> Backend {
        self.shared.backend
    }

    pub fn commits(&self) -> Vec<Vec<StorageWrite>> {
        self.control().commits.clone()
    }

    pub fn commit_count(&self) -> usize {
        self.control().commits.len()
    }

    pub fn hold_commits(&self) -> Gate {
        let gate = Gate::default();
        self.control().commit_gate = Some(gate.clone());
        gate
    }

    /// Hold the first commit for which `when` holds, until the gate is released.
    pub fn hold_commits_when(&self, when: impl Fn(&[StorageWrite]) -> bool + Send + 'static) -> Gate {
        let gate = Gate::default();
        self.control().conditional_gate = Some((Box::new(when), gate.clone()));
        gate
    }

    pub fn fail_next_mint(&self, error: StorageError) {
        self.control().mint_failure = Some(error);
    }

    pub fn fail_next_task_read(&self, error: StorageError) {
        self.control().task_read_failure = Some(error);
    }

    /// Every close fails with `error` after closing the backend.
    pub fn fail_close(&self, error: StorageError) {
        self.control().close_failure = Some(error);
    }

    pub fn fail_next_commit(&self, error: StorageError) {
        self.control().commit_failure = Some(error);
    }

    /// Fail the first commit for which `filter` returns an error.
    pub fn fail_commit_when(&self, filter: impl FnMut(&[StorageWrite]) -> Option<StorageError> + Send + 'static) {
        self.control().filter = Some(Box::new(filter));
    }

    /// Reopen after close, as a fresh process would reopen the same database, keeping every committed record.
    pub fn reopen(&self) -> Controlled {
        if self.shared.backend != Backend::Memory {
            *self.inner() = open(self.shared.backend, &self.shared.log);
        }
        let mut control = self.control();
        control.closed = false;
        Controlled {
            shared: self.shared.clone(),
            generation: control.generation,
        }
    }

    /// Simulate a crash: the held commit never reaches storage, every handle opened so far is dead, and a fresh
    /// handle reopens what was committed.
    pub fn crash(&self) -> Controlled {
        let gate = {
            let mut control = self.control();
            control.generation += 1;
            control.closed = false;
            control.commit_gate.take()
        };
        if let Some(gate) = gate {
            gate.release();
        }
        self.reopen()
    }

    fn read<T>(&self, call: impl FnOnce(&dyn Storage) -> Result<T>) -> Result<T> {
        self.open_check()?;
        call(self.inner().as_ref())
    }
}

impl Storage for Controlled {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        self.open_check()?;
        let gate = {
            let mut control = self.control();
            control.commits.push(writes.clone());
            let conditional = match &control.conditional_gate {
                Some((when, _)) if when(&writes) => control.conditional_gate.take().map(|(_, gate)| gate),
                _ => None,
            };
            conditional.or_else(|| control.commit_gate.clone())
        };
        if let Some(gate) = gate {
            gate.pass();
            let mut control = self.control();
            if control
                .commit_gate
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(&held.0, &gate.0))
            {
                control.commit_gate = None;
            }
        }
        // A crash while held: the commit never lands.
        self.open_check()?;
        let failure = {
            let mut control = self.control();
            let filtered = match control.filter.as_mut() {
                Some(filter) => filter(&writes),
                None => None,
            };
            if filtered.is_some() {
                control.filter = None;
            }
            filtered.or_else(|| control.commit_failure.take())
        };
        if let Some(failure) = failure {
            return Err(failure);
        }
        self.inner().commit(writes)
    }

    fn mint_id(&mut self) -> Result<u64> {
        self.open_check()?;
        if let Some(failure) = self.control().mint_failure.take() {
            return Err(failure);
        }
        self.inner().mint_id()
    }

    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.read(|storage| storage.conversation(id))
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.read(|storage| storage.scan_conversations(query, limit, cursor))
    }

    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.read(|storage| storage.entry(id))
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.read(|storage| storage.visible_entry(conversation_id, id))
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        self.read(|storage| storage.find_latest_head_marker(conversation_id, at_or_before))
    }

    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.read(|storage| storage.scan_entries(query, limit, cursor))
    }

    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        if let Some(failure) = self.control().task_read_failure.take() {
            return Err(failure);
        }
        self.read(|storage| storage.task(id))
    }

    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.read(|storage| storage.scan_tasks(query, limit, cursor))
    }

    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.read(|storage| storage.submission(id))
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.read(|storage| storage.scan_submissions(query, limit, cursor))
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.read(|storage| storage.submission_by_request(conversation_id, request_id))
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        self.read(|storage| storage.find_document(address, at))
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.read(|storage| storage.document(id, at))
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        self.read(|storage| storage.scan_documents(query, limit, cursor))
    }

    fn close(&mut self) -> Result<()> {
        if self.control().generation != self.generation {
            return Ok(());
        }
        self.open_check()?;
        self.control().closed = true;
        if self.shared.backend != Backend::Memory {
            self.inner().close()?;
        }
        match self.control().close_failure.clone() {
            Some(failure) => Err(failure),
            None => Ok(()),
        }
    }
}

// ─── task-support.ts ───────────────────────────────────────────────────────────

/// A thread's result, filled in by a task handler.
pub type Pending<T> = Arc<Mutex<Option<std::thread::JoinHandle<std::result::Result<T, HarnessError>>>>>;

pub type Reports = Arc<Mutex<Vec<HarnessError>>>;

pub struct Opened {
    pub harness: Harness,
    pub registry: Registry,
    pub reports: Reports,
}

pub fn bg() -> Ctx {
    Ctx::background()
}

/// Options for [`open_tasks`].
#[derive(Default)]
pub struct OpenOptions {
    pub registry: Option<Registry>,
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

/// Open a Harness whose registry holds `tasks`; reports are collected.
pub fn open_tasks(
    storage: Controlled,
    tasks: Vec<Task>,
    options: OpenOptions,
) -> std::result::Result<Opened, HarnessError> {
    let registry = options.registry.unwrap_or_default();
    if !tasks.is_empty() {
        registry.install(Extension::tasks("tasks", tasks)).unwrap();
    }
    let reports: Reports = Arc::default();
    let sink = reports.clone();
    let mut harness_options = HarnessOptions::new(Arc::new(registry.clone()));
    harness_options.on_report = Some(Arc::new(move |error| sink.lock().unwrap().push(error.clone())));
    if let Some(now) = options.now {
        harness_options.now = Some(Arc::new(move || Ok(now())));
    }
    let harness = Harness::open(storage, harness_options)?;
    Ok(Opened {
        harness,
        registry,
        reports,
    })
}

pub fn reports(opened: &Opened) -> Vec<HarnessError> {
    opened.reports.lock().unwrap().clone()
}

/// A short real-time settle, only before checking that something did NOT happen: a slow machine can make such a
/// check pass wrongly, never fail. Never use it to wait for something; wait for a condition (`eventually`,
/// `line_reaches`, a gate or latch).
pub fn flush() {
    std::thread::sleep(Duration::from_millis(15));
}

/// Poll until `check` holds. The deadline only guards against hangs: a loaded machine must slow a test down, never
/// fail it.
pub fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("Condition was not reached");
}

/// Wait until `jobs` calls are admitted to the Session line (the held one included): the handshake for "this call has
/// queued behind the held commit". The line is FIFO, so a job admitted now runs after every job counted.
pub fn line_reaches(harness: &Harness, jobs: u64) {
    eventually(|| harness.line_len() >= jobs);
}

/// Wait until a close has sealed admission (`resume` refuses), for a Harness that is already resumed.
pub fn sealed(harness: &Harness) {
    eventually(|| harness.resume().is_err());
}

/// A one-shot latch (upstream `deferred()`): `resolve` once, `wait` blocks until then.
#[derive(Clone, Default)]
pub struct Deferred(Cancel);

impl Deferred {
    pub fn new() -> Deferred {
        Deferred::default()
    }

    pub fn resolve(&self) {
        self.0.abort();
    }

    pub fn wait(&self) {
        self.0.wait();
    }

    pub fn is_resolved(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Block until the runtime's signal aborts; returns its reason as an error (upstream `aborted(signal)`).
pub fn aborted(runtime: &TaskRuntime) -> std::result::Result<(), HarnessError> {
    Err(runtime.signal().wait())
}

/// Next state that completes a task with `result`.
pub fn completed(result: Value) -> Option<NextState> {
    Some(harness::scheduler::completed(result))
}

pub fn aborted_with(reason: &str) -> Option<NextState> {
    Some(harness::scheduler::aborted_with(reason))
}

pub fn running(checkpoint: Value) -> Option<NextState> {
    Some(NextState::Running { checkpoint })
}

pub fn step() -> Value {
    json!({ "phase": "run" })
}

/// The JSON of a value, for `toMatchObject`-style comparisons.
pub fn js<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// Whether `actual` contains every field of `expected` (recursively; arrays compared element-wise).
pub fn matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| matches(found, value))),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, e)| matches(a, e))
        }
        _ => actual == expected,
    }
}

#[track_caller]
pub fn assert_matches(actual: &Value, expected: Value) {
    assert!(matches(actual, &expected), "{actual:#} does not match {expected:#}");
}

/// Run `f` on a thread; the handle joins with its result.
pub fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> std::thread::JoinHandle<T> {
    std::thread::spawn(f)
}

/// Whether a thread has finished after pending work flushes.
pub fn settled<T>(handle: &std::thread::JoinHandle<T>) -> bool {
    flush();
    handle.is_finished()
}

// ─── harness-tasks.test.ts helpers ─────────────────────────────────────────────

pub type RunFn = Arc<dyn Fn(&durable::TaskRecord, &TaskRuntime) -> std::result::Result<(), HarnessError> + Send + Sync>;

/// A one-phase task; the default abort handler settles `aborted` with reason "test".
pub fn one_step(
    name: &str,
    run: impl Fn(&durable::TaskRecord, &TaskRuntime) -> std::result::Result<(), HarnessError> + Send + Sync + 'static,
) -> Task {
    one_step_abort(name, run, |_, runtime| {
        runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("test")))
    })
}

pub fn one_step_abort(
    name: &str,
    run: impl Fn(&durable::TaskRecord, &TaskRuntime) -> std::result::Result<(), HarnessError> + Send + Sync + 'static,
    abort: impl Fn(&durable::TaskRecord, &TaskRuntime) -> std::result::Result<(), HarnessError> + Send + Sync + 'static,
) -> Task {
    let run: RunFn = Arc::new(run);
    Task::define(name, 1, |_| step())
        .phase("run", move |task, runtime| run(task, runtime))
        .abort(abort)
        .build()
}

/// A one-phase task that waits for `gate` and completes with null.
pub fn gated(name: &str, gate: Deferred) -> Task {
    one_step(name, move |_, runtime| {
        gate.wait();
        runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
    })
}

pub fn start(conversation: &Conversation, task: &Task, background: bool) -> TaskId {
    conversation
        .commit(|tx| {
            Ok(tx.create_task(
                task.definition(),
                Value::Null,
                TaskOptions {
                    background,
                    ..TaskOptions::default()
                },
            )?)
        })
        .unwrap()
}

pub fn open_root(backend: Backend, tasks: Vec<Task>, options: OpenOptions) -> (Opened, Conversation, Controlled) {
    let storage = Controlled::new(backend);
    let opened = open_tasks(storage.clone(), tasks, options).unwrap();
    let root = opened.harness.root().unwrap();
    (opened, root, storage)
}

pub fn outcome(record: &durable::TaskRecord) -> Value {
    js(&record.state)["outcome"].clone()
}

pub fn each_backend(case: impl Fn(Backend)) {
    for backend in BACKENDS {
        case(backend);
    }
}
