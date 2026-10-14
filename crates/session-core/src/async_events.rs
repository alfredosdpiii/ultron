//! Completion events instead of polling: a port of `packages/coding-agent/src/ultron/async-events.ts`.
//!
//! When detached work ends (a shell job started with `yield_after`, an `rlm.spawn` child, an `agents.*` task, a
//! detached cell), the host delivers one short `ultron-runtime-event` custom message to the lane that owns the work:
//!
//! ```text
//! <runtime_event kind="job_done" id="job-1a2b" status="completed" summary="exit 0; 12 passed" fetch="await rlm.job('job-1a2b')" />
//! ```
//!
//! Delivery ([`Dispatcher`]):
//! - completions within the coalescing window (500 ms) of the first one become one message;
//! - a lane in the middle of a run gets the message as a steer, placed at the next turn boundary; a steer that a run
//!   did not consume (it ended first) is taken back and delivered again as below;
//! - an idle root lane is re-invoked: the message starts a run charged to the root that started the work, so the
//!   per-root turn, token and cost limits keep counting across the chain;
//! - otherwise (a child lane, or a root that must not be re-invoked) the message waits as next-run input. Events are
//!   only ever appended, which keeps the provider's prompt-cache prefix valid.
//!
//! Loop guards: only root-owned work re-invokes the root; a root whose turn the user aborted never restarts work (its
//! jobs are cancelled and their ends are not announced); a refused root (a spent budget) or more than `max_runs`
//! automatic runs in one chain turn re-invocation into quiet delivery. `ULTRON_ASYNC_EVENTS=off` disables events.
//!
//! The dispatcher is one thread: publications, run ends and drains are commands it handles in order, and each lane's
//! batch is delivered after its coalescing deadline, so delivery decisions never race each other.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub const RUNTIME_EVENT_MESSAGE_TYPE: &str = "ultron-runtime-event";
pub const DEFAULT_COALESCE: Duration = Duration::from_millis(500);
pub const DEFAULT_MAX_EVENT_RUNS: usize = 16;
/// How long a batch that could not be delivered (the lane was compacting, a steer failed) waits before it is tried
/// again.
pub const RETRY_AFTER: Duration = Duration::from_millis(1000);

/// One completion.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeEvent {
    /// `job_done`, `tool_done`, `child_done`, `task_done` (and the Rust `cell_done`).
    pub kind: String,
    pub id: String,
    pub status: String,
    pub summary: String,
    /// The kernel call that returns the full result.
    pub fetch: String,
    /// The lane that owns the work and receives the event.
    pub lane: String,
    /// The usage root the work was started under.
    pub root_id: Option<String>,
}

/// `asyncEventsEnabled`: on unless `off`, `0`, `false` or `no`.
pub fn async_events_enabled(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") => true,
        Some(value) => !["off", "0", "false", "no"].contains(&value.to_lowercase().as_str()),
    }
}

/// `maxEventRunsFromEnv`: a non-negative integer, else [`DEFAULT_MAX_EVENT_RUNS`].
pub fn max_event_runs_from_env(value: Option<&str>) -> usize {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_EVENT_RUNS)
}

/// `runtimeEventText`: one `<runtime_event>` line per completion.
pub fn runtime_event_text(events: &[RuntimeEvent]) -> String {
    events
        .iter()
        .map(|e| {
            let summary: String = e.summary.chars().take(4 * 240).collect();
            crate::compose::runtime_event_line(&e.kind, &e.id, &e.status, &summary, &e.fetch)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `runtimeEventMessage`: the events as one Pi custom message.
pub fn runtime_event_message(events: &[RuntimeEvent], now: u64) -> Value {
    json!({
        "role": "custom",
        "customType": RUNTIME_EVENT_MESSAGE_TYPE,
        "content": runtime_event_text(events),
        "display": true,
        "details": {"events": events.iter().map(|e| json!({"kind": e.kind, "id": e.id, "status": e.status})).collect::<Vec<_>>()},
        "timestamp": now,
    })
}

/// What a lane is doing (`inspectExecution`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Execution {
    Idle,
    Run,
    /// Compaction or navigation: delivery waits for it to settle.
    Other,
}

/// The outcome of starting a run with an event message (`accept`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// The run started (its operation id), charged to the given root.
    Run(String),
    /// The lane is running after all (`LaneBusy`).
    Busy,
    Refused(String),
}

/// What taking back a steered message found (`cancelQueued`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancelled {
    Cancelled,
    AlreadyConsumed,
    NotFound,
}

/// The lanes events are delivered to, as the dispatcher uses the harness's lanes.
pub trait EventLanes: Send + Sync {
    fn execution(&self, lane: &str) -> Execution;
    /// Start a run of `lane` with `message`, charged to `root` (`continueRootTurn`).
    fn accept(&self, lane: &str, message: &Value, root: &str) -> Accepted;
    /// Queue `message` for the lane's running run; its queued entry id.
    fn steer(&self, lane: &str, message: &Value) -> Result<String, String>;
    /// Keep `message` for the lane's next run.
    fn next_run(&self, lane: &str, message: &Value) -> Result<String, String>;
    fn cancel_queued(&self, lane: &str, entry: &str) -> Result<Cancelled, String>;
}

/// A delivery decision, for tests and diagnostics (`onDelivery`).
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    pub lane: String,
    /// `run`, `steer` or `nextRun`.
    pub mode: &'static str,
    pub events: Vec<RuntimeEvent>,
    pub reason: Option<String>,
}

pub type RootRefusal = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type RootCallback = Arc<dyn Fn(&str) + Send + Sync>;
pub type DeliveryCallback = Arc<dyn Fn(&Delivery) + Send + Sync>;
pub type RootOfRun = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// `AsyncEventOptions`.
#[derive(Clone)]
pub struct DispatcherOptions {
    pub enabled: bool,
    pub coalesce: Duration,
    /// Automatic runs one chain (a root and the runs its events started) may start.
    pub max_runs: usize,
    /// The usage root of a root-lane run (its own, or the root it continues).
    pub root_of_run: RootOfRun,
    /// Why the root may not be re-invoked (a spent budget), or `None` when it may.
    pub refuse: Option<RootRefusal>,
    /// A root turn was aborted (Esc): stop its jobs.
    pub on_root_aborted: Option<RootCallback>,
    pub on_delivery: Option<DeliveryCallback>,
    pub retry_after: Duration,
}

impl DispatcherOptions {
    pub fn new(root_of_run: RootOfRun) -> DispatcherOptions {
        DispatcherOptions {
            enabled: true,
            coalesce: DEFAULT_COALESCE,
            max_runs: DEFAULT_MAX_EVENT_RUNS,
            root_of_run,
            refuse: None,
            on_root_aborted: None,
            on_delivery: None,
            retry_after: RETRY_AFTER,
        }
    }
}

enum Command {
    Publish(RuntimeEvent),
    RunEnded {
        lane: String,
        run_id: String,
        aborted: bool,
    },
    Drain(Sender<()>),
    Close,
}

#[derive(Default)]
struct Counts {
    pending: HashMap<String, usize>,
    steered: HashMap<String, usize>,
    in_flight: usize,
    aborted: HashSet<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `AsyncEventDispatcher`.
pub struct Dispatcher {
    tx: Mutex<Option<Sender<Command>>>,
    counts: Arc<Mutex<Counts>>,
    enabled: bool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Dispatcher {
    pub fn new(options: DispatcherOptions, lanes: Arc<dyn EventLanes>) -> Arc<Dispatcher> {
        let (tx, rx) = channel();
        let counts = Arc::new(Mutex::new(Counts::default()));
        let enabled = options.enabled;
        let mut worker = Worker {
            options,
            lanes,
            counts: counts.clone(),
            pending: HashMap::new(),
            retries: Vec::new(),
            steered: Vec::new(),
            chain_runs: HashMap::new(),
            now: now_ms,
        };
        let thread = std::thread::spawn(move || worker.run(&rx));
        Arc::new(Dispatcher {
            tx: Mutex::new(Some(tx)),
            counts,
            enabled,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn send(&self, command: Command) -> bool {
        lock(&self.tx).as_ref().is_some_and(|tx| tx.send(command).is_ok())
    }

    /// A completion: delivered after the coalescing window with the others of its lane.
    pub fn publish(&self, event: RuntimeEvent) {
        if !self.enabled {
            return;
        }
        *lock(&self.counts).pending.entry(event.lane.clone()).or_default() += 1;
        if !self.send(Command::Publish(event.clone())) {
            let mut counts = lock(&self.counts);
            if let Some(n) = counts.pending.get_mut(&event.lane) {
                *n = n.saturating_sub(1);
            }
        }
    }

    /// The harness's `run_end` of `lane`: an aborted root turn's root never restarts work, and steers the run did
    /// not consume are taken back.
    pub fn run_ended(&self, lane: &str, run_id: &str, aborted: bool) {
        self.send(Command::RunEnded {
            lane: lane.into(),
            run_id: run_id.into(),
            aborted,
        });
    }

    /// Deliver now, bypassing the coalescing window (tests, shutdown); returns once delivered.
    pub fn drain(&self) {
        let (ack, done) = channel();
        if self.send(Command::Drain(ack)) {
            let _ = done.recv();
        }
    }

    /// Events for `lane` waiting to be delivered, or steered into a run that has not consumed them yet.
    pub fn pending_for(&self, lane: &str) -> usize {
        let counts = lock(&self.counts);
        counts.pending.get(lane).copied().unwrap_or(0)
            + counts.steered.get(lane).copied().unwrap_or(0)
            + counts.in_flight
    }

    /// Whether a root turn was aborted (its work never restarts the model).
    pub fn root_aborted(&self, root: Option<&str>) -> bool {
        root.is_some_and(|root| lock(&self.counts).aborted.contains(root))
    }

    /// Stop delivering (pending events are dropped). The dispatcher thread ends by itself once its current
    /// delivery returns; this never waits for it, as a delivery may be waiting for the caller (the session actor).
    pub fn close(&self) {
        if let Some(tx) = lock(&self.tx).take() {
            let _ = tx.send(Command::Close);
        }
        lock(&self.thread).take();
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.close();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

struct Steered {
    lane: String,
    entry: String,
    events: Vec<RuntimeEvent>,
}

struct Worker {
    options: DispatcherOptions,
    lanes: Arc<dyn EventLanes>,
    counts: Arc<Mutex<Counts>>,
    /// Per lane: its batch and when it is delivered.
    pending: HashMap<String, (Vec<RuntimeEvent>, Instant)>,
    retries: Vec<(Instant, Vec<RuntimeEvent>)>,
    steered: Vec<Steered>,
    chain_runs: HashMap<String, usize>,
    now: fn() -> u64,
}

impl Worker {
    fn run(&mut self, rx: &Receiver<Command>) {
        loop {
            let next = self
                .pending
                .values()
                .map(|(_, at)| *at)
                .chain(self.retries.iter().map(|(at, _)| *at))
                .min();
            let received = match next {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(Command::Publish(event)) => {
                    let coalesce = self.options.coalesce;
                    self.pending
                        .entry(event.lane.clone())
                        .or_insert_with(|| (Vec::new(), Instant::now() + coalesce))
                        .0
                        .push(event);
                }
                Ok(Command::RunEnded { lane, run_id, aborted }) => {
                    if lane == crate::lane::MAIN && aborted {
                        let root = (self.options.root_of_run)(&run_id);
                        lock(&self.counts).aborted.insert(root.clone());
                        if let Some(callback) = &self.options.on_root_aborted {
                            callback(&root);
                        }
                    }
                    if self.steered.iter().any(|s| s.lane == lane) {
                        self.reclaim(&lane, aborted);
                    }
                }
                Ok(Command::Drain(ack)) => {
                    let lanes: Vec<String> = self.pending.keys().cloned().collect();
                    for lane in lanes {
                        self.flush(&lane);
                    }
                    let _ = ack.send(());
                }
                Ok(Command::Close) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            let due: Vec<String> = self
                .pending
                .iter()
                .filter(|(_, (_, at))| *at <= now)
                .map(|(lane, _)| lane.clone())
                .collect();
            for lane in due {
                self.flush(&lane);
            }
            let (retry, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.retries)
                .into_iter()
                .partition(|(at, _)| *at <= now);
            self.retries = waiting;
            for (_, events) in retry {
                // Counted as pending since `retry_later`.
                for event in events {
                    let coalesce = self.options.coalesce;
                    self.pending
                        .entry(event.lane.clone())
                        .or_insert_with(|| (Vec::new(), Instant::now() + coalesce))
                        .0
                        .push(event);
                }
            }
        }
    }

    fn flush(&mut self, lane: &str) {
        let Some((events, _)) = self.pending.remove(lane) else {
            return;
        };
        {
            let mut counts = lock(&self.counts);
            if let Some(n) = counts.pending.get_mut(lane) {
                *n = n.saturating_sub(events.len());
            }
            counts.in_flight += 1;
        }
        self.deliver(lane, events);
        lock(&self.counts).in_flight -= 1;
    }

    fn message(&self, events: &[RuntimeEvent]) -> Value {
        runtime_event_message(events, (self.now)())
    }

    fn delivered(&self, lane: &str, mode: &'static str, events: &[RuntimeEvent], reason: Option<String>) {
        if let Some(callback) = &self.options.on_delivery {
            callback(&Delivery {
                lane: lane.into(),
                mode,
                events: events.to_vec(),
                reason,
            });
        }
    }

    fn deliver(&mut self, lane: &str, events: Vec<RuntimeEvent>) {
        if events.is_empty() {
            return;
        }
        match self.lanes.execution(lane) {
            Execution::Other => return self.retry_later(events),
            Execution::Run => return self.steer(lane, events),
            Execution::Idle => {}
        }
        let message = self.message(&events);
        let reason = self.refusal(lane, &events);
        if reason.is_none() {
            let root = chain_root(&events).unwrap_or_default();
            match self.lanes.accept(lane, &message, &root) {
                Accepted::Run(_) => {
                    *self.chain_runs.entry(root).or_default() += 1;
                    self.delivered(lane, "run", &events, None);
                    return;
                }
                Accepted::Busy => return self.steer(lane, events),
                Accepted::Refused(_) => {}
            }
        }
        if self.lanes.next_run(lane, &message).is_ok() {
            self.delivered(lane, "nextRun", &events, reason);
        }
    }

    /// Why an idle lane's events must not start a run; `None` when they may.
    fn refusal(&self, lane: &str, events: &[RuntimeEvent]) -> Option<String> {
        if lane != crate::lane::MAIN {
            return Some("child lanes are never re-invoked".into());
        }
        let Some(root) = chain_root(events) else {
            return Some("no root turn owns this work".into());
        };
        if lock(&self.counts).aborted.contains(&root) {
            return Some("the root turn was aborted".into());
        }
        let runs = self.chain_runs.get(&root).copied().unwrap_or(0);
        let max = self.options.max_runs;
        if runs >= max {
            return Some(format!("{runs} of {max} automatic runs used for {root}"));
        }
        self.options.refuse.as_ref().and_then(|refuse| refuse(&root))
    }

    fn steer(&mut self, lane: &str, events: Vec<RuntimeEvent>) {
        let message = self.message(&events);
        let entry = match self.lanes.steer(lane, &message) {
            Ok(entry) => entry,
            Err(_) => return self.retry_later(events),
        };
        self.delivered(lane, "steer", &events, None);
        *lock(&self.counts).steered.entry(lane.to_string()).or_default() += 1;
        self.steered.push(Steered {
            lane: lane.into(),
            entry,
            events,
        });
        // The run may have ended between the check and the steer: then nothing will consume it.
        if self.lanes.execution(lane) == Execution::Idle {
            self.reclaim(lane, false);
        }
    }

    /// Deliver `events` again after `retry_after`. They count as pending meanwhile: uncounted, the session saw no
    /// work waiting and an unattached worker could retire with them undelivered.
    fn retry_later(&mut self, events: Vec<RuntimeEvent>) {
        {
            let mut counts = lock(&self.counts);
            for event in &events {
                *counts.pending.entry(event.lane.clone()).or_default() += 1;
            }
        }
        self.retries.push((Instant::now() + self.options.retry_after, events));
    }

    fn forget_steer(&mut self, entry: &str) {
        if let Some(index) = self.steered.iter().position(|s| s.entry == entry) {
            let steered = self.steered.remove(index);
            if let Some(n) = lock(&self.counts).steered.get_mut(&steered.lane) {
                *n = n.saturating_sub(1);
            }
        }
    }

    /// Steered events a finished run did not consume are taken back and delivered again. After an abort, steers are
    /// dropped with the queue; completions of tasks (not of the jobs the abort cancelled) are kept for the lane's
    /// next run.
    fn reclaim(&mut self, lane: &str, aborted: bool) {
        let entries: Vec<(String, Vec<RuntimeEvent>)> = self
            .steered
            .iter()
            .filter(|s| s.lane == lane)
            .map(|s| (s.entry.clone(), s.events.clone()))
            .collect();
        for (entry, events) in entries {
            match self.lanes.cancel_queued(lane, &entry) {
                Err(_) | Ok(Cancelled::AlreadyConsumed) => self.forget_steer(&entry),
                Ok(outcome) if outcome == Cancelled::Cancelled || aborted => {
                    self.forget_steer(&entry);
                    let kept: Vec<RuntimeEvent> = events
                        .into_iter()
                        .filter(|e| !((e.kind == "job_done" || e.kind == "tool_done") && e.status == "cancelled"))
                        .collect();
                    if !kept.is_empty() {
                        self.deliver(lane, kept);
                    }
                }
                Ok(_) => {}
            }
        }
    }
}

/// The root a batch belongs to: the newest event's that names one.
fn chain_root(events: &[RuntimeEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| e.root_id.clone())
}
