//! A root's REPL lanes: named kernels, each running one cell at a time on a thread of its own, so a cell that
//! outlives its tool call never blocks the turn. New versus `native-host.ts`, where the root has one lane and a
//! waiting cell holds it:
//!
//! - A root opens more lanes ([`Lanes::open`], or `rlm.lane.open` from a cell); each has its own Python state.
//! - [`Lanes::run`] returns when the cell ends, when its tool-call budget passes ([`CellOutcome::Running`]: the
//!   caller then [`Lanes::detach`]es or [`Lanes::cancel`]s it), or as soon as the cell parks on a host-side wait
//!   (`rlm.collect` / `rlm.merge` waiting on children): such a cell is detached at once, so the wait happens in the
//!   host, not in the turn.
//! - A detached cell's end is pushed to the [`CellEnd`] channel (no polling).
//! - Cancelling a cell first aborts its host waits (a parked `rlm.collect` raises in Python and the cell ends with
//!   its kernel and variables intact); only a cell still running after the grace period has its kernel killed (its
//!   Python state is lost and the lane's next cell starts a fresh kernel).
//! - Live kernels are bounded by a [`KernelPool`] (`kernel-pool.ts`, as the TS session's rlm tool uses it): a cell
//!   leases its lane's kernel, an idle unpinned kernel is snapshotted and evicted to make room (or after its idle
//!   TTL), and the lane's next cell starts a kernel that restores that snapshot. When every live kernel is running or
//!   pinned, opening or running another lane is refused with the pool's capacity error. `rlm.pool` reports it.

use crate::host::Park;
use crate::kernel_pool::{KernelPool, KernelPoolOptions, Lease, PoolableKernel};
use kernel::{ExecutionResult, HostHandler, Kernel, Signal};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Creates a lane's kernel (not yet started) for a lane name.
pub type LaneFactory = Arc<dyn Fn(&str) -> Result<Kernel, String> + Send + Sync>;
/// Makes the handler for one cell's host requests other than the lane API (normally
/// `Delegation::cell_handler(lane, signal, park)`): `signal` is aborted when the cell ends or is cancelled, and the
/// handler calls `park(true)` / `park(false)` around a host-side wait.
pub type LaneHandlers = Arc<dyn Fn(&str, Signal, Park) -> Arc<dyn HostHandler> + Send + Sync>;

pub struct LanesOptions {
    pub create: LaneFactory,
    pub handlers: LaneHandlers,
    /// Most lanes open at once (the root's "main" included).
    pub max_lanes: usize,
    /// How long a cancelled cell may take to end after its host waits are aborted before its kernel is killed.
    pub cancel_grace: Duration,
    /// Bounds on live kernels; `None`: every open lane keeps its kernel (at most `max_lanes`), none is evicted.
    pub pool: Option<PoolLimits>,
}

/// Where a lane's snapshot goes before its kernel is evicted (the lane kernel's own snapshot path, so the next
/// kernel restores it at startup); `None` evicts without one.
pub type SnapshotPathFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The session's kernel pool (`createUltronRlmTool`: `maxLive` 16, `maxPinned` half of it, 30 min idle TTL).
#[derive(Clone)]
pub struct PoolLimits {
    pub max_live: usize,
    pub max_pinned: Option<usize>,
    pub idle_ttl: Duration,
    pub snapshot_path: Option<SnapshotPathFn>,
    /// The pool's clock in milliseconds since the epoch (none: the system clock), for idle times.
    pub now: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
}

/// How often the session sweeps idle kernels (`setInterval(() => kernels.sweep(), 60_000)`).
pub const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

impl PoolLimits {
    /// The TS defaults for `max_live` kernels: half of them pinnable, a 30 minute idle TTL.
    pub fn new(max_live: usize, snapshot_path: Option<SnapshotPathFn>) -> PoolLimits {
        PoolLimits {
            max_live,
            max_pinned: Some(max_live / 2),
            idle_ttl: Duration::from_secs(30 * 60),
            snapshot_path,
            now: None,
        }
    }
}

/// A lane's kernel in the pool. A kernel that never started has nothing to save: its eviction leaves the lane's
/// snapshot as it is (writing one would replace the state the kernel would have restored with an empty namespace).
struct LaneKernel(Arc<Kernel>);

impl PoolableKernel for LaneKernel {
    fn snapshot(&mut self, path: &str) -> Result<Vec<String>, String> {
        if self.0.pid().is_none() {
            return Ok(Vec::new());
        }
        let reply = self
            .0
            .snapshot(Some(std::path::Path::new(path)))
            .map_err(|error| error.to_string())?;
        crate::kernel_pool::snapshot_outcome(&serde_json::to_value(&reply).map_err(|error| error.to_string())?)
    }

    fn shutdown(&mut self) {
        self.0.shutdown();
    }
}

/// How [`Lanes::run`] returned.
#[derive(Debug, Clone, PartialEq)]
pub enum CellOutcome {
    /// The cell ended within its budget (or its kernel failed: the error text).
    Done(Result<ExecutionResult, String>),
    /// The cell still runs. `parked`: it waits on the host and was detached already; otherwise its budget passed
    /// and the caller decides ([`Lanes::detach`] or [`Lanes::cancel`]).
    Running { cell: String, parked: bool },
}

/// A detached cell ended.
#[derive(Debug, Clone, PartialEq)]
pub struct CellEnd {
    pub cell: String,
    pub lane: String,
    pub outcome: Result<ExecutionResult, String>,
}

struct Cell {
    lane: String,
    /// The lane kernel's lease, released when the cell ends.
    lease: Option<Lease>,
    /// Aborted when the cell ends or is cancelled: ends its host waits.
    waits: Signal,
    /// Aborted to kill the kernel when a cancelled cell does not end.
    kill: Signal,
    parked: usize,
    detached: bool,
    cancelled: bool,
    outcome: Option<Result<ExecutionResult, String>>,
}

struct Lane {
    busy: Option<String>,
}

#[derive(Default)]
struct State {
    lanes: BTreeMap<String, Lane>,
    cells: HashMap<String, Cell>,
    next_cell: u64,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    events: Mutex<Sender<CellEnd>>,
    /// Live kernels (taken after `state`, never the other way round).
    pool: Mutex<KernelPool<LaneKernel>>,
    options: LanesOptions,
    /// Told the type of every host request a cell's kernel makes (the session's runtime counters).
    observer: Mutex<Option<RequestObserver>>,
    /// Adds to what `rlm.pool` reports (the session's file-hook guard statistics).
    pool_extra: Mutex<Option<PoolExtra>>,
}

/// Sees the type of each host request a cell makes, before it is answered.
pub type RequestObserver = Arc<dyn Fn(&str) + Send + Sync>;
/// Completes an `rlm.pool` report.
pub type PoolExtra = Arc<dyn Fn(&mut Value) + Send + Sync>;

/// A root's lanes; clones share them.
#[derive(Clone)]
pub struct Lanes {
    inner: Arc<Inner>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The per-cell host handler: lane API requests go to the lanes, the rest to the cell's handler.
struct CellHandler {
    lanes: Lanes,
    cell: String,
    inner: Arc<dyn HostHandler>,
}

impl HostHandler for CellHandler {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let observer = self
            .lanes
            .inner
            .observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(observer) = observer {
            observer(kind);
        }
        if kind.starts_with("rlm.lane") || kind.starts_with("rlm.cell") || kind == "rlm.recovery" || kind == "rlm.pool"
        {
            return self.lanes.request(kind, payload, Some(&self.cell));
        }
        self.inner.handle(kind, payload, signal)
    }
}

impl Lanes {
    pub fn new(options: LanesOptions) -> Result<(Self, Receiver<CellEnd>), String> {
        if options.max_lanes < 1 {
            return Err("max_lanes must be a positive integer".into());
        }
        let limits = options.pool.clone();
        let snapshot_path = limits.as_ref().and_then(|l| l.snapshot_path.clone());
        let pool = KernelPool::new(KernelPoolOptions {
            // Kernels are created by `Lanes` (fallibly) and handed to the pool; this one is never called.
            create: Box::new(|lane: &str| -> LaneKernel { unreachable!("lane {lane} kernels are created by Lanes") }),
            max_live: limits.as_ref().map_or(options.max_lanes, |l| l.max_live),
            idle_ttl_ms: limits.as_ref().map_or(f64::MAX, |l| l.idle_ttl.as_secs_f64() * 1000.0),
            max_pinned: limits.as_ref().and_then(|l| l.max_pinned),
            snapshot_path: snapshot_path
                .map(|path| Box::new(move |lane: &str| path(lane)) as crate::kernel_pool::SnapshotPath),
            now: match limits.as_ref().and_then(|l| l.now.clone()) {
                Some(now) => Box::new(move || now()),
                None => Box::new(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0)
                }),
            },
        })?;
        let (sender, receiver) = channel();
        Ok((
            Lanes {
                inner: Arc::new(Inner {
                    state: Mutex::new(State::default()),
                    changed: Condvar::new(),
                    events: Mutex::new(sender),
                    pool: Mutex::new(pool),
                    options,
                    observer: Mutex::new(None),
                    pool_extra: Mutex::new(None),
                }),
            },
            receiver,
        ))
    }

    fn pool(&self) -> MutexGuard<'_, KernelPool<LaneKernel>> {
        self.inner.pool.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lease `lane`'s kernel, starting a new one (which restores the lane's snapshot) when it has none live. Idle
    /// kernels past their TTL go first.
    fn lease(&self, lane: &str) -> Result<(Lease, Arc<Kernel>), String> {
        let mut pool = self.pool();
        pool.sweep();
        let create = self.inner.options.create.clone();
        let lease = pool.acquire_with(lane, || create(lane).map(|kernel| LaneKernel(Arc::new(kernel))))?;
        let kernel = pool
            .kernel(lease)
            .map(|k| k.0.clone())
            .ok_or("leased kernel vanished")?;
        Ok((lease, kernel))
    }

    /// `kernels.sweep()`: snapshot and evict every idle unpinned kernel past its TTL; the lanes it evicted.
    pub fn sweep(&self) -> Vec<String> {
        if lock(&self.inner.state).closed {
            return Vec::new();
        }
        self.pool()
            .sweep()
            .iter()
            .filter(|record| record["evicted"] == true)
            .filter_map(|record| record["lane"].as_str().map(str::to_string))
            .collect()
    }

    /// Sweep idle kernels on every tick until `ticks` closes (the session's sweeper: [`Lanes::sweep_every`]).
    /// Each sweep's evicted lanes go to `swept`, when given.
    pub fn sweep_on(&self, ticks: Receiver<()>, swept: Option<Sender<Vec<String>>>) -> std::thread::JoinHandle<()> {
        let lanes = self.clone();
        std::thread::spawn(move || {
            while ticks.recv().is_ok() {
                let evicted = lanes.sweep();
                if let Some(swept) = &swept {
                    let _ = swept.send(evicted);
                }
            }
        })
    }

    /// Sweep idle kernels every `interval` until the session stops (the TS session's 60 s `kernels.sweep()`):
    /// `stopped(interval)` waits up to `interval` and says whether the session stopped.
    pub fn sweep_every(&self, interval: Duration, stopped: impl Fn(Duration) -> bool + Send + 'static) {
        let (tick, ticks) = channel();
        let _ = self.sweep_on(ticks, None);
        std::thread::spawn(move || {
            while !stopped(interval) {
                if tick.send(()).is_err() {
                    return;
                }
            }
        });
    }

    /// Whether a kernel for `lane` could be admitted now: it has one, there is room, or an idle unpinned kernel
    /// can make room. Expired idle kernels go first, as with a lease.
    fn check_room(&self, lane: &str) -> Result<(), String> {
        let mut pool = self.pool();
        pool.sweep();
        let stats = pool.stats();
        let lanes = stats["lanes"].as_array().cloned().unwrap_or_default();
        let live = stats["live"].as_u64().unwrap_or(0) as usize;
        let max = stats["maxLive"].as_u64().unwrap_or(0) as usize;
        let has = lanes.iter().any(|l| l["lane"] == lane);
        let evictable = lanes
            .iter()
            .any(|l| l["running"].as_u64() == Some(0) && l["pinnedBy"].as_array().is_none_or(Vec::is_empty));
        if has || live < max || evictable {
            Ok(())
        } else {
            Err(crate::kernel_pool::capacity_error(live, max))
        }
    }

    /// Keeps `lane`'s kernel live for `holder` (a retained instance); false when the pool has no pin left.
    pub fn pin(&self, lane: &str, holder: &str) -> bool {
        lock(&self.inner.state).closed.then_some(()).is_none() && self.pool().pin(lane, holder).is_ok()
    }

    pub fn unpin(&self, lane: &str, holder: &str) {
        self.pool().unpin(lane, holder);
    }

    /// Every host request a cell makes is shown to `observer` first.
    pub fn observe_requests(&self, observer: RequestObserver) {
        *self.inner.observer.lock().unwrap_or_else(PoisonError::into_inner) = Some(observer);
    }

    /// `rlm.pool` reports, inspected or asked by a cell, are completed by `extra`.
    pub fn extend_pool_stats(&self, extra: PoolExtra) {
        *self.inner.pool_extra.lock().unwrap_or_else(PoisonError::into_inner) = Some(extra);
    }

    /// `rlm.pool`: live kernels, their leases, pins and idle time, and how many evictions happened, plus what
    /// [`Lanes::extend_pool_stats`] adds.
    pub fn pool_stats(&self) -> Value {
        let mut stats = self.kernel_pool_stats();
        let extra = self
            .inner
            .pool_extra
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(extra) = extra {
            extra(&mut stats);
        }
        stats
    }

    fn kernel_pool_stats(&self) -> Value {
        let mut pool = self.pool();
        let mut stats = pool.stats();
        stats["evictions"] = json!(pool.evictions().iter().filter(|r| r["evicted"] == true).count());
        // Each live kernel's process-tree memory (session-worker.ts `poolStats`), and the tree cap.
        let (mut total, mut measured, mut cap) = (0u64, false, None);
        if let Some(lanes) = stats["lanes"].as_array_mut() {
            for lane in lanes {
                let name = lane["lane"].as_str().unwrap_or_default().to_string();
                let usage = pool.live(&name).and_then(|kernel| kernel.0.memory_usage());
                if let Some((bytes, lane_cap)) = usage {
                    lane["memoryBytes"] = json!(bytes);
                    total += bytes;
                    measured = true;
                    cap = lane_cap.or(cap);
                }
            }
        }
        if measured {
            stats["memoryBytes"] = json!(total);
        }
        if let Some(cap) = cap {
            stats["memoryCapBytes"] = json!(cap);
        }
        stats
    }

    /// Opens a lane (idempotent); its kernel starts with its first cell.
    pub fn open(&self, lane: &str) -> Result<(), String> {
        if lane.trim().is_empty() {
            return Err("lane must be a nonempty string".into());
        }
        let mut state = lock(&self.inner.state);
        if state.closed {
            return Err("REPL lanes are closed".into());
        }
        if state.lanes.contains_key(lane) {
            return Ok(());
        }
        if state.lanes.len() >= self.inner.options.max_lanes {
            return Err(format!(
                "REPL lane limit reached: {} of {} lanes are open; close one first",
                state.lanes.len(),
                self.inner.options.max_lanes
            ));
        }
        // The lane's kernel is admitted with its first cell, as the TS rlm tool admits a kernel only for a cell
        // (`kernels.use`): an open lane is not a live kernel (`rlm.pool` shows 0/16 at idle). Past the pool's capacity
        // (every live kernel running or pinned) the lane is refused now.
        self.check_room(lane)?;
        state.lanes.insert(lane.to_string(), Lane { busy: None });
        Ok(())
    }

    /// Closes an idle lane and shuts its kernel down.
    pub fn close_lane(&self, lane: &str) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        match state.lanes.get(lane) {
            None => return Err(format!("Unknown REPL lane: {lane}")),
            Some(Lane { busy: Some(cell), .. }) => {
                return Err(format!("REPL lane {lane} is running {cell}; cancel it first"));
            }
            Some(_) => state.lanes.remove(lane),
        };
        // A closed lane's kernel goes, pinned or not (its continuity checkpoint is already written).
        self.pool().discard(lane);
        Ok(())
    }

    /// Open lanes: name, running cell (or null) and kernel pid (or null before its first cell).
    pub fn list(&self) -> Value {
        let state = lock(&self.inner.state);
        let mut pool = self.pool();
        Value::from(
            state
                .lanes
                .iter()
                .map(|(name, lane)| {
                    let pid = pool.live(name).and_then(|kernel| kernel.0.pid());
                    json!({ "lane": name, "cell": lane.busy, "pid": pid })
                })
                .collect::<Vec<_>>(),
        )
    }

    /// The live kernel of an open lane (none while it is evicted).
    pub fn kernel(&self, lane: &str) -> Option<Arc<Kernel>> {
        if !lock(&self.inner.state).lanes.contains_key(lane) {
            return None;
        }
        self.pool().live(lane).map(|kernel| kernel.0.clone())
    }

    /// Runs a cell in `lane`, returning within `budget` (see [`CellOutcome`]). A lane still running a cell refuses.
    pub fn run(&self, lane: &str, code: &str, budget: Duration) -> Result<CellOutcome, String> {
        let (id, kernel, waits, kill) = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err("REPL lanes are closed".into());
            }
            state.next_cell += 1;
            let id = format!("cell-{}", state.next_cell);
            let entry = state
                .lanes
                .get_mut(lane)
                .ok_or_else(|| format!("Unknown REPL lane: {lane}"))?;
            if let Some(cell) = &entry.busy {
                return Err(format!(
                    "REPL lane {lane} is still running {cell}: cancel it, wait for its end, or run in another lane"
                ));
            }
            // The lane's kernel for this cell: its live one, or a new one restoring its snapshot. Past the pool's
            // capacity the cell is refused before it is admitted.
            let (lease, kernel) = self.lease(lane)?;
            if let Some(entry) = state.lanes.get_mut(lane) {
                entry.busy = Some(id.clone());
            }
            let (waits, kill) = (Signal::new(), Signal::new());
            state.cells.insert(
                id.clone(),
                Cell {
                    lane: lane.to_string(),
                    lease: Some(lease),
                    waits: waits.clone(),
                    kill: kill.clone(),
                    parked: 0,
                    detached: false,
                    cancelled: false,
                    outcome: None,
                },
            );
            (id, kernel, waits, kill)
        };
        let handler: Arc<dyn HostHandler> = Arc::new(CellHandler {
            lanes: self.clone(),
            cell: id.clone(),
            inner: (self.inner.options.handlers)(lane, waits.clone(), {
                let (lanes, cell) = (self.clone(), id.clone());
                Arc::new(move |on| lanes.park(&cell, if on { 1 } else { -1 }))
            }),
        });
        let (lanes, code, cell) = (self.clone(), code.to_string(), id.clone());
        std::thread::spawn(move || {
            let outcome = kernel
                .execute(&code, Some(&kill), Some(handler))
                .map_err(|e| e.message().to_string());
            lanes.ended(&cell, outcome);
        });
        let deadline = Instant::now() + budget;
        let mut state = lock(&self.inner.state);
        loop {
            let cell = state.cells.get_mut(&id).ok_or("cell vanished")?;
            if let Some(outcome) = &cell.outcome {
                let outcome = outcome.clone();
                state.cells.remove(&id);
                return Ok(CellOutcome::Done(outcome));
            }
            if cell.parked > 0 {
                cell.detached = true;
                return Ok(CellOutcome::Running { cell: id, parked: true });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(CellOutcome::Running {
                    cell: id,
                    parked: false,
                });
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn park(&self, cell: &str, delta: isize) {
        let mut state = lock(&self.inner.state);
        if let Some(cell) = state.cells.get_mut(cell) {
            cell.parked = cell.parked.saturating_add_signed(delta);
        }
        drop(state);
        self.inner.changed.notify_all();
    }

    fn ended(&self, id: &str, outcome: Result<ExecutionResult, String>) {
        let event = {
            let mut state = lock(&self.inner.state);
            let Some(cell) = state.cells.get_mut(id) else {
                return;
            };
            cell.waits.abort("cell ended");
            cell.outcome = Some(outcome.clone());
            if let Some(lease) = cell.lease.take() {
                self.pool().release(lease);
            }
            let lane = cell.lane.clone();
            let event = (cell.detached && !cell.cancelled).then(|| CellEnd {
                cell: id.to_string(),
                lane: lane.clone(),
                outcome,
            });
            if event.is_some() {
                state.cells.remove(id);
            }
            if let Some(entry) = state.lanes.get_mut(&lane)
                && entry.busy.as_deref() == Some(id)
            {
                entry.busy = None;
            }
            event
        };
        self.inner.changed.notify_all();
        if let Some(event) = event {
            let _ = self
                .inner
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(event);
        }
    }

    /// Detaches a running cell: its end arrives on the [`CellEnd`] channel. A cell that already ended returns its
    /// outcome instead.
    pub fn detach(&self, id: &str) -> Result<Option<Result<ExecutionResult, String>>, String> {
        let mut state = lock(&self.inner.state);
        let cell = state.cells.get_mut(id).ok_or_else(|| format!("Unknown cell: {id}"))?;
        if let Some(outcome) = cell.outcome.clone() {
            state.cells.remove(id);
            return Ok(Some(outcome));
        }
        cell.detached = true;
        Ok(None)
    }

    /// Cancels a running cell and returns its outcome: its host waits end first (the cell raises and its kernel
    /// keeps its state); a cell still running after the grace period has its kernel killed.
    pub fn cancel(&self, id: &str, reason: &str) -> Result<Result<ExecutionResult, String>, String> {
        let (waits, kill) = {
            let mut state = lock(&self.inner.state);
            let cell = state.cells.get_mut(id).ok_or_else(|| format!("Unknown cell: {id}"))?;
            cell.cancelled = true;
            (cell.waits.clone(), cell.kill.clone())
        };
        waits.abort(reason);
        if let Some(outcome) = self.await_end(id, self.inner.options.cancel_grace) {
            return Ok(outcome);
        }
        kill.abort(reason);
        self.await_end(id, Duration::from_secs(10))
            .ok_or_else(|| format!("cell {id} did not end after its kernel was killed"))
    }

    fn await_end(&self, id: &str, timeout: Duration) -> Option<Result<ExecutionResult, String>> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.inner.state);
        loop {
            let outcome = state.cells.get(id)?.outcome.clone();
            if let Some(outcome) = outcome {
                state.cells.remove(id);
                return Some(outcome);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The lane API a cell reaches through `rlm.host_request` (and the runtime's `rlm.lanes()`, `rlm.lane.open()`,
    /// `rlm.lane.close()`, `rlm.cells()`, `rlm.cell.cancel()`, `rlm.recovery()`), plus `rlm.pool`: `rlm.lanes`,
    /// `rlm.lane.open {lane}`, `rlm.lane.close {lane}`, `rlm.cells`, `rlm.cell.cancel {cell}` and `rlm.recovery`
    /// (the requesting cell's lane kernel's latest [`kernel::ContinuityReport`], or null). `own` is the requesting
    /// cell, which cannot cancel itself.
    pub fn request(&self, kind: &str, payload: Map<String, Value>, own: Option<&str>) -> Result<Value, String> {
        let only = |allowed: &[&str]| match payload.keys().find(|k| !allowed.contains(&k.as_str())) {
            Some(key) => Err(format!("Unknown payload field: {key}")),
            None => Ok(()),
        };
        let text = |name: &str| match payload.get(name).and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
            _ => Err(format!("{name} must be a nonempty string")),
        };
        match kind {
            "rlm.lanes" => {
                only(&[])?;
                Ok(json!({ "lanes": self.list() }))
            }
            "rlm.lane.open" => {
                only(&["lane"])?;
                self.open(&text("lane")?)?;
                Ok(json!({ "opened": true }))
            }
            "rlm.lane.close" => {
                only(&["lane"])?;
                self.close_lane(&text("lane")?)?;
                Ok(json!({ "closed": true }))
            }
            "rlm.cells" => {
                only(&[])?;
                let state = lock(&self.inner.state);
                let mut cells: Vec<Value> = state
                    .cells
                    .iter()
                    .filter(|(_, c)| c.outcome.is_none())
                    .map(
                        |(id, c)| json!({ "cell": id, "lane": c.lane, "parked": c.parked > 0, "detached": c.detached }),
                    )
                    .collect();
                cells.sort_by(|a, b| a["cell"].as_str().cmp(&b["cell"].as_str()));
                Ok(json!({ "cells": cells }))
            }
            "rlm.cell.cancel" => {
                only(&["cell"])?;
                let cell = text("cell")?;
                if own == Some(cell.as_str()) {
                    return Err("a cell cannot cancel itself".into());
                }
                let outcome = self.cancel(&cell, "cancelled from another cell")?;
                Ok(match outcome {
                    Ok(result) => json!({ "cancelled": true, "status": result.status }),
                    Err(error) => json!({ "cancelled": true, "status": "error", "error": error }),
                })
            }
            "rlm.pool" => {
                only(&[])?;
                Ok(self.pool_stats())
            }
            "rlm.recovery" => {
                only(&[])?;
                let lane = own
                    .and_then(|cell| lock(&self.inner.state).cells.get(cell).map(|c| c.lane.clone()))
                    .ok_or("rlm.recovery: the request is not from a running cell")?;
                let report = self.kernel(&lane).and_then(|kernel| kernel.last_recovery());
                serde_json::to_value(report).map_err(|e| e.to_string())
            }
            other => Err(format!("Unknown host request: {other}")),
        }
    }

    /// Cancels every running cell and shuts every lane down.
    pub fn close(&self) {
        let cells: Vec<String> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state
                .cells
                .iter()
                .filter(|(_, c)| c.outcome.is_none())
                .map(|(id, _)| id.clone())
                .collect()
        };
        for cell in cells {
            let _ = self.cancel(&cell, "REPL lanes closed");
        }
        // Every kernel goes, the idle ones after their snapshot.
        self.pool().close();
    }
}
