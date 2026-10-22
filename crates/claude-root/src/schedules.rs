//! Scheduled prompts under `ultron claude` (`schedule.every`, `schedule.at`; kernel_apis::prompt_schedule).
//!
//! In a native session the Ultron server's timer fires a due schedule and the session actor starts a turn with its
//! prompt (daemon::prompt_schedules). Claude Code's session has no Ultron server, so this server keeps the timer:
//! one thread sleeps until the earliest due time (on the schedules' own clock; a change to the schedules wakes it),
//! records the run (`fire_with`: missed slots collapse into one run, the next slot is after now) and puts the prompt
//! in the root's inbox as a `<scheduled_prompt>` line. While Claude Code is idle the wait-events hook
//! (crate::hooks::WAIT_EVENTS) wakes the model with it, which starts a turn; during a turn it arrives with the next
//! cell result, or wakes the model when the turn ends.
//!
//! The schedules fire only while Claude Code runs (this server is the process it starts for its session). They are
//! kept as session values of their own namespace, [`NAMESPACE`], not `ultron.schedule`: the Ultron server's timer
//! reads only that one from the session journals (daemon::prompt_schedules::scan_journal), so it never fires a Claude
//! Code session's schedule as a native turn on Claude's session. `claude --resume` reopens the same Ultron session,
//! and its server's timer finds the schedules again: the slots missed while Claude Code was down collapse into one
//! run (`fire_with`), as for a timer that slept through them.

use kernel_apis::TickClock;
use kernel_apis::artifacts::KeyedDocuments;
use kernel_apis::prompt_schedule::{Listener, ScheduleModule};
use serde_json::Value;
use session_core::durable_store::{SharedSession, lock};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

/// A delivery that failed is tried again after this (daemon::prompt_schedules::RETRY_MS).
pub const RETRY_MS: i64 = daemon::prompt_schedules::RETRY_MS;

/// Where a Claude Code session's schedules are kept (session values; never `ultron.schedule`).
pub const NAMESPACE: &str = "ultron.claude.schedule";

/// The schedules of `session` as keyed values of [`NAMESPACE`].
pub struct SessionSchedules(pub SharedSession);

impl KeyedDocuments for SessionSchedules {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(lock(&self.0).value(NAMESPACE, key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        lock(&self.0)
            .commit(&[session_journal::Write::set(NAMESPACE, key, value.clone())])
            .map(drop)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(lock(&self.0)
            .state()
            .scan_values(NAMESPACE, prefix)
            .into_iter()
            .map(|stored| (stored.key.clone(), stored.value.clone()))
            .collect())
    }
}

/// `schedule.*` of the root of `session` (whose id is `session_id`), kept in the session; `listener` hears every
/// change.
pub fn module(
    session: &SharedSession,
    session_id: &str,
    clock: Arc<dyn TickClock>,
    listener: Listener,
) -> Arc<ScheduleModule> {
    Arc::new(ScheduleModule::new(
        Box::new(SessionSchedules(session.clone())),
        session_id,
        clock,
        Some(listener),
    ))
}

/// Puts a fired schedule's prompt message (kernel_apis::prompt_schedule::fire_message) where the root reads it.
pub type Deliver = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct State {
    stopped: bool,
    /// The schedules changed since the timer last looked.
    poked: bool,
    /// A slot whose delivery failed waits until then.
    retry_at: HashMap<String, i64>,
}

#[derive(Default)]
pub struct ScheduleTimer {
    state: Mutex<State>,
    bell: Condvar,
}

impl ScheduleTimer {
    pub fn new() -> Arc<ScheduleTimer> {
        Arc::new(ScheduleTimer::default())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the schedule module tells after every change: the timer looks again.
    pub fn listener(self: &Arc<Self>) -> Listener {
        let timer = Arc::downgrade(self);
        Arc::new(move |_| {
            if let Some(timer) = timer.upgrade() {
                timer.poke();
            }
        })
    }

    pub fn poke(&self) {
        self.lock().poked = true;
        self.bell.notify_all();
    }

    pub fn stop(&self) {
        self.lock().stopped = true;
        self.bell.notify_all();
    }

    /// Run the timer on its own thread until [`ScheduleTimer::stop`].
    pub fn start(self: &Arc<Self>, module: Arc<ScheduleModule>, clock: Arc<dyn TickClock>, deliver: Deliver) {
        let timer = self.clone();
        std::thread::spawn(move || timer.run(&module, &*clock, &deliver));
    }

    fn run(&self, module: &ScheduleModule, clock: &dyn TickClock, deliver: &Deliver) {
        loop {
            {
                let mut state = self.lock();
                if state.stopped {
                    return;
                }
                state.poked = false;
            }
            let now = clock.now();
            let mut earliest: Option<i64> = None;
            for entry in module.entries().unwrap_or_default() {
                let (Some(id), Some(due)) = (entry["id"].as_str(), entry["nextDue"].as_i64()) else {
                    continue;
                };
                let retry = self.lock().retry_at.get(id).copied().unwrap_or(i64::MIN);
                let at = due.max(retry);
                if at > now {
                    earliest = Some(earliest.map_or(at, |e| e.min(at)));
                    continue;
                }
                // The module records the run only once the prompt is delivered; a slot it did not take (not due
                // on its clock, already fired) or could not deliver waits instead of repeating.
                match module.fire_with(id, due, &mut |message| deliver(message)) {
                    Ok(Some(_)) => {
                        self.lock().retry_at.remove(id);
                    }
                    Ok(None) | Err(_) => {
                        self.lock().retry_at.insert(id.to_string(), now + RETRY_MS);
                        earliest = Some(earliest.map_or(now + RETRY_MS, |e| e.min(now + RETRY_MS)));
                    }
                }
            }
            let state = self.lock();
            if state.stopped || state.poked {
                continue;
            }
            let limit = earliest.and_then(|at| clock.wait_limit(at));
            let _woken = match (earliest, limit) {
                (Some(_), Some(limit)) => {
                    self.bell
                        .wait_timeout(state, limit.min(daemon::prompt_schedules::MAX_SLEEP))
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                // No deadline, or a clock that moves only when told to: until the schedules change.
                _ => self.bell.wait(state).unwrap_or_else(PoisonError::into_inner),
            };
        }
    }
}

/// The root inbox line of a fired schedule's prompt message.
pub fn prompt_line(message: &Value) -> String {
    let details = &message["details"];
    format!(
        "<scheduled_prompt schedule=\"{}\" id=\"{}\" run=\"{}\">\nThis session's schedule fired: do this now as a new task.\n{}\n</scheduled_prompt>",
        crate::durable_apis::attribute(details["name"].as_str().unwrap_or_default()),
        crate::durable_apis::attribute(details["id"].as_str().unwrap_or_default()),
        details["run"].as_u64().unwrap_or(0),
        message["content"].as_str().unwrap_or_default()
    )
}
