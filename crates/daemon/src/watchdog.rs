//! A session worker's view of its run's progress: which run is in progress, when it last made progress (any lane
//! event: a stream frame, a tool start or end, a committed entry, a queue change), and whether it is stuck — no
//! progress for longer than the stuck threshold (`ULTRON_STUCK_AFTER_MS`, default ten minutes). A stuck run is
//! reported (the worker's `health` state and its status to the server) and can be aborted through the session
//! operations service (`abortStuck`).

use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

pub const STUCK_AFTER_ENV: &str = "ULTRON_STUCK_AFTER_MS";
pub const DEFAULT_STUCK_AFTER: Duration = Duration::from_secs(600);

/// The stuck threshold from the environment (`0` turns detection off).
pub fn stuck_after(var: &dyn Fn(&str) -> Option<String>) -> Option<Duration> {
    match var(STUCK_AFTER_ENV).and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(DEFAULT_STUCK_AFTER),
    }
}

#[derive(Default)]
struct State {
    run: Option<String>,
    started_at: i64,
    last_progress: i64,
    stuck: bool,
}

/// What an event changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    None,
    Started(String),
    Ended(String),
    /// A stuck run made progress again.
    Unstuck,
}

#[derive(Default)]
pub struct Progress {
    state: Mutex<State>,
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fold one lane-watch event at `now` (milliseconds).
    pub fn event(&self, event: &Value, now: i64) -> Change {
        let mut state = self.lock();
        let run = event["runId"].as_str().map(str::to_string);
        match event["type"].as_str() {
            Some("run_start") => {
                let id = run.unwrap_or_default();
                *state = State {
                    run: Some(id.clone()),
                    started_at: now,
                    last_progress: now,
                    stuck: false,
                };
                Change::Started(id)
            }
            Some("run_end" | "run_suspend") if state.run.is_some() && (run.is_none() || run == state.run) => {
                let id = state.run.take().unwrap_or_default();
                state.stuck = false;
                Change::Ended(id)
            }
            // Name changes and usage totals are not the run's own progress.
            Some("session_name" | "usage") | None => Change::None,
            Some(_) if state.run.is_some() => {
                state.last_progress = now;
                if std::mem::take(&mut state.stuck) {
                    Change::Unstuck
                } else {
                    Change::None
                }
            }
            Some(_) => Change::None,
        }
    }

    pub fn busy(&self) -> bool {
        self.lock().run.is_some()
    }

    pub fn run(&self) -> Option<String> {
        self.lock().run.clone()
    }

    /// Mark the run stuck when it made no progress for `after`; returns true only when it just became stuck.
    pub fn check(&self, now: i64, after: Option<Duration>) -> bool {
        let Some(after) = after else { return false };
        let mut state = self.lock();
        if state.run.is_none() || state.stuck || now - state.last_progress < after.as_millis() as i64 {
            return false;
        }
        state.stuck = true;
        true
    }

    /// `{running, runId, startedAt, lastProgressAt, idleMs, stuck, stuckAfterMs}` at `now`.
    pub fn health(&self, now: i64, after: Option<Duration>) -> Value {
        let state = self.lock();
        let after = after.map(|a| a.as_millis() as u64);
        match &state.run {
            None => json!({"running": false, "runId": null, "startedAt": null, "lastProgressAt": null,
                "idleMs": 0, "stuck": false, "stuckAfterMs": after}),
            Some(run) => json!({"running": true, "runId": run, "startedAt": state.started_at,
                "lastProgressAt": state.last_progress, "idleMs": (now - state.last_progress).max(0),
                "stuck": state.stuck, "stuckAfterMs": after}),
        }
    }
}
