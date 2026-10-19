//! The server's timer for scheduled prompts (`schedule.*`, kernel_apis::prompt_schedule; Rust-only).
//!
//! Each server generation keeps the active schedules of the sessions in its store: read from the session journals
//! when it starts (values of `ultron.schedule` whose session id is the journal's own), then replaced by what the
//! sessions' workers report after each change (`worker_schedules`) and what each fire answers. One thread sleeps
//! until the earliest due time on a [`TickClock`] (wall time in the product; no polling) or until it is poked. A due
//! schedule is delivered to its session ([`Deliver`]: the hub starts or adopts the worker and sends it
//! `scheduleFire {id, due}`); the worker records the run and starts it, or queues it after the run in progress.
//! Missed slots collapse into one fire (the worker sets the next slot after now), a session whose file is gone is
//! dropped, and a delivery that failed is tried again after [`RETRY_MS`].

use crate::catalog::{self, Meta};
use crate::chord::{Service, ServiceError};
use crate::routed::Hub;
use kernel_apis::TickClock;
use kernel_apis::prompt_schedule::{NAMESPACE, Schedule};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

/// A delivery that failed (the worker could not start, the session is open elsewhere) is tried again after this.
pub const RETRY_MS: i64 = 60_000;
/// The longest the timer sleeps before it reads the clock again (a suspended machine, a changed wall clock).
pub const MAX_SLEEP: Duration = Duration::from_secs(300);

/// What a delivered fire answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    /// The session's active schedules after the fire (`[{id, name, nextDue}]`).
    Done(Value),
    /// The session no longer exists: its schedules are dropped.
    Gone,
    /// Not delivered now; tried again after [`RETRY_MS`].
    Retry(String),
}

/// Delivers a due schedule to its session.
pub trait Deliver: Send + Sync {
    fn fire(&self, meta: &Meta, id: &str, due: i64) -> Delivery;
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    id: String,
    next_due: i64,
}

#[derive(Default)]
struct State {
    sessions: HashMap<PathBuf, (Meta, Vec<Entry>)>,
    in_flight: HashSet<(PathBuf, String)>,
    retry_at: HashMap<(PathBuf, String), i64>,
    stopped: bool,
    /// How often the timer looked at the clock (it does once per wake-up, never in a loop).
    passes: u64,
}

type Log = Box<dyn Fn(&str) + Send + Sync>;

pub struct Scheduler {
    me: Weak<Scheduler>,
    clock: Arc<dyn TickClock>,
    deliver: Arc<dyn Deliver>,
    state: Mutex<State>,
    bell: Condvar,
    log: Option<Log>,
}

fn entries(value: &Value) -> Vec<Entry> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Entry {
                id: e["id"].as_str()?.to_string(),
                next_due: e["nextDue"].as_i64()?,
            })
        })
        .collect()
}

impl Scheduler {
    pub fn new(clock: Arc<dyn TickClock>, deliver: Arc<dyn Deliver>, log: Option<Log>) -> Arc<Scheduler> {
        Arc::new_cyclic(|me| Scheduler {
            me: me.clone(),
            clock,
            deliver,
            state: Mutex::new(State::default()),
            bell: Condvar::new(),
            log,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the schedules of every session in `session_dir`, then run the timer on its own thread.
    pub fn start(&self, session_dir: PathBuf) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            let loaded = scan_sessions(&session_dir);
            let Some(me) = me.upgrade() else { return };
            {
                let mut state = me.lock();
                for (meta, schedules) in loaded {
                    let active: Vec<Value> = schedules.iter().filter(|s| s.active()).map(Schedule::entry).collect();
                    // A worker's report that arrived while the store was read is newer.
                    if !state.sessions.contains_key(&meta.path) && !active.is_empty() {
                        state
                            .sessions
                            .insert(meta.path.clone(), (meta, entries(&Value::Array(active))));
                    }
                }
            }
            me.run();
        });
    }

    /// The active schedules of `meta`'s session are now `active` (`[{id, name, nextDue}]`).
    pub fn update(&self, meta: &Meta, active: &Value) {
        let mut state = self.lock();
        let list = entries(active);
        let ids: HashSet<&str> = list.iter().map(|e| e.id.as_str()).collect();
        state
            .retry_at
            .retain(|(path, id), _| *path != meta.path || ids.contains(id.as_str()));
        if list.is_empty() {
            state.sessions.remove(&meta.path);
        } else {
            state.sessions.insert(meta.path.clone(), (meta.clone(), list));
        }
        drop(state);
        self.bell.notify_all();
    }

    /// Look at the clock again now.
    pub fn poke(&self) {
        // Taken first so a timer that is between reading the clock and waiting cannot miss the bell.
        drop(self.lock());
        self.bell.notify_all();
    }

    pub fn stop(&self) {
        self.lock().stopped = true;
        self.bell.notify_all();
    }

    /// Deliveries in progress.
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight.len()
    }

    /// How many times the timer has looked at the clock.
    pub fn passes(&self) -> u64 {
        self.lock().passes
    }

    /// `(session id, schedule id, due)` of every schedule the timer holds, sorted.
    pub fn pending(&self) -> Vec<(String, String, i64)> {
        let state = self.lock();
        let mut out: Vec<(String, String, i64)> = state
            .sessions
            .values()
            .flat_map(|(meta, list)| list.iter().map(|e| (meta.id.clone(), e.id.clone(), e.next_due)))
            .collect();
        out.sort();
        out
    }

    fn run(&self) {
        let mut state = self.lock();
        loop {
            if state.stopped {
                return;
            }
            state.passes += 1;
            let now = self.clock.now();
            let mut due = Vec::new();
            let mut earliest: Option<i64> = None;
            for (path, (meta, list)) in &state.sessions {
                for entry in list {
                    let key = (path.clone(), entry.id.clone());
                    if state.in_flight.contains(&key) {
                        continue;
                    }
                    let at = entry
                        .next_due
                        .max(state.retry_at.get(&key).copied().unwrap_or(i64::MIN));
                    if at <= now {
                        due.push((meta.clone(), entry.clone()));
                    } else {
                        earliest = Some(earliest.map_or(at, |e: i64| e.min(at)));
                    }
                }
            }
            for (meta, entry) in due {
                state.in_flight.insert((meta.path.clone(), entry.id.clone()));
                let me = self.me.clone();
                std::thread::spawn(move || {
                    if let Some(me) = me.upgrade() {
                        me.delivered(&meta, &entry);
                    }
                });
            }
            let limit = earliest.and_then(|at| self.clock.wait_limit(at));
            state = match (earliest, limit) {
                (Some(_), Some(limit)) => {
                    self.bell
                        .wait_timeout(state, limit.min(MAX_SLEEP))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
                _ => self.bell.wait(state).unwrap_or_else(|e| e.into_inner()),
            };
        }
    }

    fn delivered(&self, meta: &Meta, entry: &Entry) {
        let result = self.deliver.fire(meta, &entry.id, entry.next_due);
        let now = self.clock.now();
        let key = (meta.path.clone(), entry.id.clone());
        let mut state = self.lock();
        state.in_flight.remove(&key);
        match result {
            Delivery::Done(active) => {
                let list = entries(&active);
                // A fire the session did not take for this slot (its clock is behind) waits instead of repeating.
                if list.iter().any(|e| e.id == entry.id && e.next_due == entry.next_due) {
                    state.retry_at.insert(key, now + RETRY_MS);
                } else {
                    state.retry_at.remove(&key);
                }
                if list.is_empty() {
                    state.sessions.remove(&meta.path);
                } else {
                    state.sessions.insert(meta.path.clone(), (meta.clone(), list));
                }
            }
            Delivery::Gone => {
                state.sessions.remove(&meta.path);
                state.retry_at.retain(|(path, _), _| *path != meta.path);
            }
            Delivery::Retry(error) => {
                state.retry_at.insert(key, now + RETRY_MS);
                if let Some(log) = &self.log {
                    log(&format!(
                        "Session {}: schedule {} could not fire: {error}",
                        meta.id, entry.id
                    ));
                }
            }
        }
        drop(state);
        self.bell.notify_all();
    }
}

/// The schedules a session journal holds (its own: values copied from another session by a fork are left out), in
/// creation order, read without opening the session (its worker may be writing it): complete lines only.
pub fn scan_journal(path: &Path) -> Vec<Schedule> {
    let Some(meta) = session_journal::repo::read_metadata(path) else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if !content.contains(NAMESPACE) {
        return Vec::new();
    }
    let mut values: Vec<(String, Value)> = Vec::new();
    for line in session_journal::io::complete_lines(&content).into_iter().skip(1) {
        let Ok(writes) = session_journal::codec::parse_transaction(line) else {
            continue;
        };
        for write in writes {
            if write["kind"] != "value" || write["namespace"] != NAMESPACE {
                continue;
            }
            let Some(key) = write["key"].as_str() else { continue };
            values.retain(|(k, _)| k != key);
            if write["op"] == "set" {
                values.push((key.to_string(), write["value"].clone()));
            }
        }
    }
    let mut found: Vec<Schedule> = values
        .iter()
        .filter_map(|(_, value)| Schedule::from_json(value))
        .filter(|s| s.session_id == meta.id)
        .collect();
    found.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    found
}

/// Every session of `session_dir` with schedules, and its schedules.
pub fn scan_sessions(session_dir: &Path) -> Vec<(Meta, Vec<Schedule>)> {
    let mut sessions = catalog::list(session_dir);
    sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    sessions
        .into_iter()
        .map(|meta| {
            let schedules = scan_journal(&meta.path);
            (meta, schedules)
        })
        .filter(|(_, schedules)| !schedules.is_empty())
        .collect()
}

/// The session operation that fires a schedule.
pub fn fire_call(id: &str, due: i64) -> Value {
    json!({"serviceId": crate::worker::OPS_SERVICE, "member": "op", "args": [{"op": "scheduleFire", "id": id, "due": due}]})
}

/// The session operation that cancels a schedule.
pub fn cancel_call(id: &str) -> Value {
    json!({"serviceId": crate::worker::OPS_SERVICE, "member": "op", "args": [{"op": "scheduleCancel", "id": id}]})
}

/// Fires through the hub: the session's worker is started or adopted (crate::routed `Hub::session_call`).
pub struct HubDeliver(pub Weak<Hub>);

impl Deliver for HubDeliver {
    fn fire(&self, meta: &Meta, id: &str, due: i64) -> Delivery {
        let Some(hub) = self.0.upgrade() else {
            return Delivery::Retry("the server is closed".into());
        };
        if !meta.path.exists() {
            return Delivery::Gone;
        }
        match hub.session_call(&meta.id, fire_call(id, due)) {
            Ok(answer) => Delivery::Done(answer.map(|a| a["schedules"].clone()).unwrap_or(Value::Null)),
            Err(ServiceError { code: Some(code), .. }) if code == "session_not_found" => Delivery::Gone,
            Err(error) => Delivery::Retry(error.message),
        }
    }
}

/// The server service `ultron.schedules` (unlisted; `ultron schedule cancel`): `cancel(sessionId, id)` cancels
/// through the session's worker (started for it when none runs).
pub fn server_service(hub: Weak<Hub>) -> Service {
    Service::new("ultron.schedules")
        .unlisted()
        .method("cancel", move |args| {
            let hub = hub
                .upgrade()
                .ok_or_else(|| ServiceError::internal("Server is closed"))?;
            let text = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
            let (session, id) = (text(0), text(1));
            if session.is_empty() || id.is_empty() {
                return Err(ServiceError::coded(
                    "service_invalid_value",
                    "cancel needs a session id and a schedule id",
                ));
            }
            hub.session_call(&session, cancel_call(&id))
        })
}

/// A session's values as the schedule module's store, for a journal this process opened itself.
struct JournalValues(Mutex<session_journal::Session>);

impl kernel_apis::artifacts::KeyedDocuments for JournalValues {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value(NAMESPACE, key)
            .cloned())
    }

    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .commit(&[session_journal::Write::set(NAMESPACE, key, value.clone())])
            .map(drop)
    }

    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .scan_values(NAMESPACE, prefix)
            .into_iter()
            .map(|stored| (stored.key.clone(), stored.value.clone()))
            .collect())
    }
}

/// Cancel schedule `name_or_id` of the session at `path` in its journal directly, when no worker holds it (no
/// server runs): the session's locks are taken as a worker takes them, so a worker starting meanwhile waits or is
/// refused.
pub fn cancel_in_journal(path: &Path, name_or_id: &str, clock: Arc<dyn TickClock>) -> Result<Schedule, String> {
    let _owner = durable::owner::OwnerLock::acquire(path).map_err(|error| match error {
        durable::owner::OwnerError::InUse { pid } => crate::worker::session_in_use(
            &session_journal::repo::read_metadata(path)
                .map(|m| m.id)
                .unwrap_or_default(),
            pid,
        ),
        durable::owner::OwnerError::Io(error) => error.to_string(),
    })?;
    let _lock = crate::lock::Lock::acquire(
        path,
        Duration::from_millis(2_000),
        Duration::from_millis(25),
        Duration::from_millis(4_000),
    )
    .map_err(|e| e.to_string())?;
    let metadata =
        session_journal::repo::read_metadata(path).ok_or_else(|| format!("Not a session file: {}", path.display()))?;
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("Not a session file: {}", path.display()))?;
    let session = session_journal::SessionRepo::new(root, session_journal::SystemClock::shared()).open(&metadata)?;
    let module = kernel_apis::prompt_schedule::ScheduleModule::new(
        Box::new(JournalValues(Mutex::new(session))),
        &metadata.id,
        clock,
        None,
    );
    module.cancel(name_or_id)
}
