//! `schedule.*` (schedule_api.py, Rust host only): prompts a session sends itself later, the proactive agent.
//!
//! `schedule.every(interval, prompt, ...)` and `schedule.at(when, prompt, ...)` record a schedule as a session value
//! (`ultron.schedule` / `<id>`, append-only journal writes replayed on open; the last write per id wins, a cancel is
//! a write too). The server (crates/daemon `prompt_schedules`) owns timing: when a schedule is due it asks the
//! session's worker to [`ScheduleModule::fire`] it, which records the run and returns the prompt as a displayed
//! custom message ([`CUSTOM_TYPE`], details `{name, id, run}`) that starts a run. A fire names the due time it was
//! for: a duplicate or stale fire (another server generation, a value changed since) does nothing. Missed fires
//! collapse into one: the next due time is the first slot after now.
//!
//! Each value carries the session id that created it, so a fork that copies the session's values never fires them.

use crate::artifacts::KeyedDocuments;
use crate::schedule_tick::{SystemTickClock, TickClock};
use crate::util::{iso, lock};
use crate::{ApiModule, Request};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The session-value namespace of the schedules (one value per schedule id).
pub const NAMESPACE: &str = "ultron.schedule";
/// The custom message type of a fired schedule's prompt.
pub const CUSTOM_TYPE: &str = "ultron.schedule";
pub const MIN_INTERVAL_MS: i64 = 60_000;
pub const MAX_INTERVAL_MS: i64 = 365 * DAY_MS;
/// Active schedules one session may hold (each run is a model turn the user pays for).
pub const MAX_ACTIVE: usize = 32;
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_RUNS: u64 = 100_000;
/// A test clock: the file holds the current time in epoch ms; schedules and the server's timer read it (the timer
/// looks at it every [`FILE_CLOCK_POLL`]). Never set in the product.
pub const CLOCK_FILE_ENV: &str = "__ULTRON_SCHEDULE_CLOCK_FILE";
pub const FILE_CLOCK_POLL: Duration = Duration::from_millis(20);

const SECOND_MS: i64 = 1000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;

/// `every`: repeats each `interval_ms`; `at`: runs once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Every,
    At,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    /// Ran its last run (`at`, `max_runs`, `until`).
    Done,
    Cancelled,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Done => "done",
            State::Cancelled => "cancelled",
        }
    }
}

/// One schedule as its session value records it.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub session_id: String,
    pub prompt: String,
    pub kind: Kind,
    pub interval_ms: Option<i64>,
    /// No run is due after this time (epoch ms).
    pub until: Option<i64>,
    pub max_runs: Option<u64>,
    pub created_at: i64,
    /// When the next run is due (epoch ms); `None` once the schedule ended.
    pub next_due: Option<i64>,
    pub runs: u64,
    pub last_fired: Option<i64>,
    pub state: State,
}

impl Schedule {
    pub fn active(&self) -> bool {
        self.state == State::Active && self.next_due.is_some()
    }

    /// The session value.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "sessionId": self.session_id, "prompt": self.prompt,
            "kind": match self.kind { Kind::Every => "every", Kind::At => "at" },
            "intervalMs": self.interval_ms, "until": self.until, "maxRuns": self.max_runs,
            "createdAt": self.created_at, "nextDue": self.next_due, "runs": self.runs,
            "lastFired": self.last_fired, "state": self.state.name(),
        })
    }

    /// A session value read back; `None` when it is not a schedule.
    pub fn from_json(value: &Value) -> Option<Schedule> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        Some(Schedule {
            id: text("id")?,
            name: text("name")?,
            session_id: text("sessionId")?,
            prompt: text("prompt")?,
            kind: match value["kind"].as_str()? {
                "every" => Kind::Every,
                "at" => Kind::At,
                _ => return None,
            },
            interval_ms: value["intervalMs"].as_i64(),
            until: value["until"].as_i64(),
            max_runs: value["maxRuns"].as_u64(),
            created_at: value["createdAt"].as_i64()?,
            next_due: value["nextDue"].as_i64(),
            runs: value["runs"].as_u64()?,
            last_fired: value["lastFired"].as_i64(),
            state: match value["state"].as_str()? {
                "active" => State::Active,
                "done" => State::Done,
                "cancelled" => State::Cancelled,
                _ => return None,
            },
        })
    }

    /// What `schedule.list()` gives a cell: the value plus `every` (`"15m"`) and the ISO times.
    pub fn view(&self) -> Value {
        let mut view = self.to_json();
        view["every"] = self.interval_ms.map(format_interval).into();
        view["next"] = self.next_due.map(iso).into();
        view["untilIso"] = self.until.map(iso).into();
        view
    }

    /// What the server keeps of an active schedule: `{id, name, nextDue}`.
    pub fn entry(&self) -> Value {
        json!({"id": self.id, "name": self.name, "nextDue": self.next_due})
    }

    /// One line of `/schedules` and `ultron schedule list`: name, next fire, runs, repeat, id.
    pub fn line(&self) -> String {
        let next = match (self.next_due, self.state) {
            (Some(due), State::Active) => format!("next {}", iso(due)),
            (_, state) => state.name().to_string(),
        };
        let runs = match self.max_runs {
            Some(max) => format!("runs {}/{max}", self.runs),
            None => format!("runs {}", self.runs),
        };
        let repeat = match self.interval_ms {
            Some(ms) => format!("every {}", format_interval(ms)),
            None => "once".to_string(),
        };
        format!("{}  {next}  {runs}  {repeat}  {}", self.name, self.id)
    }

    /// The first due slot after `now` that follows `due` (one fire for every slot missed), or `None` when the
    /// schedule ends after `runs` runs.
    fn next_after(&self, due: i64, now: i64) -> Option<i64> {
        let interval = self.interval_ms.filter(|_| self.kind == Kind::Every)?.max(1);
        if self.max_runs.is_some_and(|max| self.runs >= max) {
            return None;
        }
        let skipped = (now - due).max(0) / interval;
        let next = due + (skipped + 1) * interval;
        if self.until.is_some_and(|until| next > until) {
            return None;
        }
        Some(next)
    }
}

/// `"90s"`, `"15m"`, `"1h30m"`, `"1d"` or a number of seconds (a JSON number or a numeric string), in ms; at least
/// [`MIN_INTERVAL_MS`], at most a year.
pub fn parse_interval(value: &Value) -> Result<i64, String> {
    let invalid =
        || format!("schedule interval must be like \"90s\", \"15m\", \"1h\" or \"1d\", or seconds (got {value})");
    let ms = match value {
        Value::Number(n) => {
            let seconds = n.as_f64().ok_or_else(invalid)?;
            if !seconds.is_finite() || seconds <= 0.0 {
                return Err(invalid());
            }
            (seconds * 1000.0).round() as i64
        }
        Value::String(text) => {
            let text = text.trim();
            if let Ok(seconds) = text.parse::<f64>() {
                return parse_interval(&json!(seconds)).map_err(|_| invalid());
            }
            let mut total: i64 = 0;
            let mut digits = String::new();
            let mut groups = 0;
            for c in text.chars() {
                if c.is_ascii_digit() {
                    digits.push(c);
                    continue;
                }
                let unit = match c {
                    's' => SECOND_MS,
                    'm' => MINUTE_MS,
                    'h' => HOUR_MS,
                    'd' => DAY_MS,
                    _ => return Err(invalid()),
                };
                let n: i64 = digits.parse().map_err(|_| invalid())?;
                digits.clear();
                groups += 1;
                total = total.saturating_add(n.saturating_mul(unit));
            }
            if groups == 0 || !digits.is_empty() {
                return Err(invalid());
            }
            total
        }
        _ => return Err(invalid()),
    };
    if ms < MIN_INTERVAL_MS {
        return Err(format!(
            "schedule interval must be at least 60s (got {})",
            format_interval(ms)
        ));
    }
    if ms > MAX_INTERVAL_MS {
        return Err("schedule interval must be at most 365d".into());
    }
    Ok(ms)
}

/// An interval as the largest whole units: `1d`, `2h`, `1h30m`, `90s`.
pub fn format_interval(ms: i64) -> String {
    if ms > 0 && ms % SECOND_MS != 0 {
        return format!("{ms}ms");
    }
    let mut left = ms / SECOND_MS;
    let mut out = String::new();
    for (unit, seconds) in [("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if left >= seconds {
            out.push_str(&format!("{}{unit}", left / seconds));
            left %= seconds;
        }
    }
    if out.is_empty() { "0s".into() } else { out }
}

/// The fired prompt as a transcript message: a displayed custom message the model reads as a user message.
pub fn fire_message(schedule: &Schedule, run: u64, timestamp: i64) -> Value {
    json!({
        "role": "custom",
        "customType": CUSTOM_TYPE,
        "content": schedule.prompt,
        "display": true,
        "details": {"name": schedule.name, "id": schedule.id, "run": run},
        "timestamp": timestamp,
    })
}

/// `/schedules`: one [`Schedule::line`] per schedule, active first.
pub fn text(schedules: &[Schedule]) -> String {
    if schedules.is_empty() {
        return "No schedules. A cell creates one with `await schedule.every(\"1h\", prompt)`.".into();
    }
    let mut sorted: Vec<&Schedule> = schedules.iter().collect();
    sorted.sort_by_key(|s| (!s.active(), s.next_due.unwrap_or(i64::MAX)));
    sorted.iter().map(|s| s.line()).collect::<Vec<_>>().join("\n")
}

/// A test clock read from a file ([`CLOCK_FILE_ENV`]); an unreadable file reads as 0.
pub struct FileTickClock {
    pub path: PathBuf,
}

impl TickClock for FileTickClock {
    fn now(&self) -> i64 {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    fn wait_limit(&self, _deadline: i64) -> Option<Duration> {
        Some(FILE_CLOCK_POLL)
    }
}

/// The schedules' clock for an environment: the test clock file when [`CLOCK_FILE_ENV`] names one, else wall time.
pub fn clock_for(var: &dyn Fn(&str) -> Option<String>) -> Arc<dyn TickClock> {
    match var(CLOCK_FILE_ENV).filter(|path| !path.is_empty()) {
        Some(path) => Arc::new(FileTickClock {
            path: PathBuf::from(path),
        }),
        None => Arc::new(SystemTickClock),
    }
}

/// Told the session's active schedules ([`Schedule::entry`]) after every change.
pub type Listener = Arc<dyn Fn(Vec<Value>) + Send + Sync>;

/// A fire that started a run: the schedule after it and the prompt message.
pub struct Fired {
    pub schedule: Schedule,
    pub message: Value,
}

/// `schedule.*` over the session's values.
pub struct ScheduleModule {
    store: Box<dyn KeyedDocuments>,
    session_id: String,
    clock: Arc<dyn TickClock>,
    listener: Option<Listener>,
    /// Serializes read-modify-write of the values.
    writing: Mutex<()>,
}

fn field<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    payload.get(key).filter(|value| !value.is_null())
}

impl ScheduleModule {
    pub fn new(
        store: Box<dyn KeyedDocuments>,
        session_id: &str,
        clock: Arc<dyn TickClock>,
        listener: Option<Listener>,
    ) -> Self {
        ScheduleModule {
            store,
            session_id: session_id.to_string(),
            clock,
            listener,
            writing: Mutex::new(()),
        }
    }

    pub fn now(&self) -> i64 {
        self.clock.now()
    }

    /// This session's schedules, in the order they were created (values copied from another session are not).
    pub fn all(&self) -> Result<Vec<Schedule>, String> {
        let mut found: Vec<Schedule> = self
            .store
            .list("")?
            .iter()
            .filter_map(|(_, value)| Schedule::from_json(value))
            .filter(|s| s.session_id == self.session_id)
            .collect();
        found.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(found)
    }

    /// The active schedules as the server keeps them.
    pub fn entries(&self) -> Result<Vec<Value>, String> {
        Ok(self.all()?.iter().filter(|s| s.active()).map(Schedule::entry).collect())
    }

    fn put(&self, schedule: &Schedule) -> Result<(), String> {
        self.store.set(&schedule.id, &schedule.to_json())?;
        if let Some(listener) = &self.listener {
            listener(self.entries()?);
        }
        Ok(())
    }

    /// `due`: `{at_ms}` (epoch ms) or `{in_ms}` (from now) under `prefix`; `None` when neither is given.
    fn time(payload: &Map<String, Value>, prefix: &str, now: i64) -> Result<Option<i64>, String> {
        let at = field(payload, &format!("{prefix}_ms"));
        let after = field(payload, &format!("{prefix}_in_ms"));
        let number = |value: &Value, what: &str| {
            value
                .as_f64()
                .filter(|n| n.is_finite())
                .map(|n| n.round() as i64)
                .ok_or_else(|| format!("schedule {what} must be a time or a number of seconds"))
        };
        match (at, after) {
            (Some(_), Some(_)) => Err(format!("schedule {prefix} is given twice")),
            (Some(at), None) => number(at, prefix).map(Some),
            (None, Some(after)) => number(after, prefix).map(|ms| Some(now.saturating_add(ms))),
            (None, None) => Ok(None),
        }
    }

    fn create(&self, payload: &Map<String, Value>, kind: Kind) -> Result<Schedule, String> {
        let allowed: &[&str] = match kind {
            Kind::Every => &["interval", "prompt", "name", "until_ms", "until_in_ms", "max_runs"],
            Kind::At => &["when_ms", "when_in_ms", "prompt", "name"],
        };
        crate::util::fields(payload, allowed)?;
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .ok_or("schedule prompt must be a non-empty string")?;
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!("schedule prompt must be at most {MAX_PROMPT_BYTES} bytes"));
        }
        let _writing = lock(&self.writing);
        let now = self.now();
        let (interval_ms, next_due, until, max_runs) = match kind {
            Kind::Every => {
                let interval = parse_interval(payload.get("interval").unwrap_or(&Value::Null))?;
                let until = Self::time(payload, "until", now)?;
                let max_runs = match field(payload, "max_runs") {
                    None => None,
                    Some(value) => Some(
                        value
                            .as_u64()
                            .filter(|n| (1..=MAX_RUNS).contains(n))
                            .ok_or_else(|| format!("schedule max_runs must be an integer from 1 to {MAX_RUNS}"))?,
                    ),
                };
                let first = now + interval;
                if until.is_some_and(|until| until < first) {
                    return Err(format!("schedule until is before the first run ({})", iso(first)));
                }
                (Some(interval), first, until, max_runs)
            }
            Kind::At => {
                let due = Self::time(payload, "when", now)?.ok_or("schedule.at needs a time")?;
                if due <= now {
                    return Err(format!("schedule.at time must be in the future (got {})", iso(due)));
                }
                (None, due, None, None)
            }
        };
        let existing = self.all()?;
        let active: Vec<&Schedule> = existing.iter().filter(|s| s.active()).collect();
        if active.len() >= MAX_ACTIVE {
            return Err(format!("a session can have at most {MAX_ACTIVE} active schedules"));
        }
        let id = format!("sch-{}", &crate::util::uuid().replace('-', "")[..8]);
        let name = match payload.get("name").filter(|n| !n.is_null()) {
            None => id.clone(),
            Some(name) => {
                let name = name
                    .as_str()
                    .map(str::trim)
                    .filter(|n| !n.is_empty() && n.len() <= MAX_NAME_BYTES && !n.contains(char::is_whitespace))
                    .ok_or_else(|| format!("schedule name must be one word of 1 to {MAX_NAME_BYTES} bytes"))?;
                if active.iter().any(|s| s.name == name) {
                    return Err(format!("a schedule named {name} is already active"));
                }
                name.to_string()
            }
        };
        let schedule = Schedule {
            id,
            name,
            session_id: self.session_id.clone(),
            prompt: prompt.to_string(),
            kind,
            interval_ms,
            until,
            max_runs,
            created_at: now,
            next_due: Some(next_due),
            runs: 0,
            last_fired: None,
            state: State::Active,
        };
        self.put(&schedule)?;
        Ok(schedule)
    }

    /// Cancel the active schedule whose id, else name, is `name_or_id`.
    pub fn cancel(&self, name_or_id: &str) -> Result<Schedule, String> {
        let _writing = lock(&self.writing);
        let all = self.all()?;
        let found = all
            .iter()
            .find(|s| s.active() && s.id == name_or_id)
            .or_else(|| all.iter().find(|s| s.active() && s.name == name_or_id))
            .ok_or_else(|| format!("no active schedule {name_or_id}"))?;
        let mut cancelled = found.clone();
        cancelled.state = State::Cancelled;
        cancelled.next_due = None;
        self.put(&cancelled)?;
        Ok(cancelled)
    }

    /// Run schedule `id` for its slot `due`: records the run (and the next slot after now) and returns the prompt
    /// message. `None`: nothing to run (not active, not due yet, or `due` is not its next slot: a duplicate fire).
    pub fn fire(&self, id: &str, due: i64) -> Result<Option<Fired>, String> {
        let _writing = lock(&self.writing);
        let now = self.now();
        let Some(mut schedule) = self.all()?.into_iter().find(|s| s.id == id) else {
            return Ok(None);
        };
        if !schedule.active() || schedule.next_due != Some(due) || due > now {
            return Ok(None);
        }
        // A slot past `until` never runs (it was only reachable through a stale value).
        if schedule.until.is_some_and(|until| due > until) {
            schedule.state = State::Done;
            schedule.next_due = None;
            self.put(&schedule)?;
            return Ok(None);
        }
        schedule.runs += 1;
        schedule.last_fired = Some(now);
        schedule.next_due = schedule.next_after(due, now);
        if schedule.next_due.is_none() {
            schedule.state = State::Done;
        }
        self.put(&schedule)?;
        let message = fire_message(&schedule, schedule.runs, now);
        Ok(Some(Fired { schedule, message }))
    }
}

impl ApiModule for ScheduleModule {
    fn prefixes(&self) -> &[&str] {
        &["schedule."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        if request.caller_task.is_some() {
            return Err(format!("{} is available to the root agent only", request.kind));
        }
        match request.kind {
            "schedule.every" => self.create(request.payload, Kind::Every).map(|s| s.view()),
            "schedule.at" => self.create(request.payload, Kind::At).map(|s| s.view()),
            "schedule.list" => {
                let all = self.all()?;
                if request.payload.get("text") == Some(&Value::Bool(true)) {
                    return Ok(text(&all).into());
                }
                Ok(all.iter().map(Schedule::view).collect::<Vec<_>>().into())
            }
            "schedule.cancel" => {
                let target = request
                    .payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("schedule.cancel needs a schedule id or name")?;
                self.cancel(target).map(|s| s.view())
            }
            other => Err(format!("Unknown schedule request: {other}")),
        }
    }
}
