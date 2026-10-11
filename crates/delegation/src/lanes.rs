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

use crate::host::Park;
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
    kernel: Arc<Kernel>,
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
    options: LanesOptions,
}

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
        if kind.starts_with("rlm.lane") || kind.starts_with("rlm.cell") || kind == "rlm.recovery" {
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
        let (sender, receiver) = channel();
        Ok((
            Lanes {
                inner: Arc::new(Inner {
                    state: Mutex::new(State::default()),
                    changed: Condvar::new(),
                    events: Mutex::new(sender),
                    options,
                }),
            },
            receiver,
        ))
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
        let kernel = (self.inner.options.create)(lane)?;
        state.lanes.insert(
            lane.to_string(),
            Lane {
                kernel: Arc::new(kernel),
                busy: None,
            },
        );
        Ok(())
    }

    /// Closes an idle lane and shuts its kernel down.
    pub fn close_lane(&self, lane: &str) -> Result<(), String> {
        let kernel = {
            let mut state = lock(&self.inner.state);
            match state.lanes.get(lane) {
                None => return Err(format!("Unknown REPL lane: {lane}")),
                Some(Lane { busy: Some(cell), .. }) => {
                    return Err(format!("REPL lane {lane} is running {cell}; cancel it first"));
                }
                Some(_) => state.lanes.remove(lane).map(|l| l.kernel),
            }
        };
        if let Some(kernel) = kernel {
            kernel.shutdown();
        }
        Ok(())
    }

    /// Open lanes: name, running cell (or null) and kernel pid (or null before its first cell).
    pub fn list(&self) -> Value {
        let state = lock(&self.inner.state);
        Value::from(
            state
                .lanes
                .iter()
                .map(|(name, lane)| json!({ "lane": name, "cell": lane.busy, "pid": lane.kernel.pid() }))
                .collect::<Vec<_>>(),
        )
    }

    /// The kernel of an open lane.
    pub fn kernel(&self, lane: &str) -> Option<Arc<Kernel>> {
        lock(&self.inner.state).lanes.get(lane).map(|l| l.kernel.clone())
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
            entry.busy = Some(id.clone());
            let kernel = entry.kernel.clone();
            let (waits, kill) = (Signal::new(), Signal::new());
            state.cells.insert(
                id.clone(),
                Cell {
                    lane: lane.to_string(),
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

    /// Waits up to `timeout` for a cell that is not detached to end and takes its outcome.
    pub fn wait_cell(&self, id: &str, timeout: Duration) -> Option<Result<ExecutionResult, String>> {
        self.await_end(id, timeout)
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
    /// `rlm.lane.close()`, `rlm.cells()`, `rlm.cell.cancel()`, `rlm.recovery()`): `rlm.lanes`,
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
        let (cells, kernels): (Vec<String>, Vec<Arc<Kernel>>) = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            (
                state
                    .cells
                    .iter()
                    .filter(|(_, c)| c.outcome.is_none())
                    .map(|(id, _)| id.clone())
                    .collect(),
                state.lanes.values().map(|l| l.kernel.clone()).collect(),
            )
        };
        for cell in cells {
            let _ = self.cancel(&cell, "REPL lanes closed");
        }
        for kernel in kernels {
            kernel.shutdown();
        }
    }
}
