//! The session's runtime counters (`ultron/session-stats.ts`): what the runtime sees while a session runs and no
//! other session value keeps, for the session report (`/usage`, `ultron usage`). They are the session value
//! `ultron.module/stats`, written as the TypeScript recorder writes it, so either implementation reads the other's.
//!
//! Cells are counted where they run (every lane's `rlm` tool), host requests where the kernels make them, guard
//! outcomes as the file hooks record them, steers as they are queued for the root, refused work as the usage ledger
//! refuses it, merges and subagent models as the delegation host settles them. Nothing here holds code, output,
//! paths or prompts: only counts, API names, task ids and times.
//!
//! Recording never fails and never blocks on the store: changes are counted in memory and saved
//! `flush_ms` (500 ms) after the first unsaved one, and on [`SessionStats::flush`]. A store that cannot be written
//! keeps the counters in memory and the next change tries again; a stored document in a format this build does not
//! know is left as it is.

mod external_root;

use crate::durable_store::{SharedSession, lock as lock_session};
use regex::Regex;
use serde_json::{Map, Value, json};
use session_journal::Write;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The module name: the session value `ultron.module/stats`.
pub const SESSION_STATS_MODULE: &str = "stats";
const NAMESPACE: &str = "ultron.module";
const STATS_VERSION: u64 = 1;
/// Distinct host request types kept (later ones are not added), and task ids kept per map (the newest).
const MAX_KEYS: usize = 200;
/// How long after the first unsaved change the counters are saved.
pub const DEFAULT_FLUSH: Duration = Duration::from_millis(500);

/// REPL APIs a cell's code can name, in the order the report lists them.
pub const CELL_APIS: [&str; 16] = [
    "bash",
    "read",
    "edit",
    "write",
    "rlm.load",
    "rlm.infer",
    "rlm.map",
    "rlm.spawn",
    "rlm.collect",
    "rlm.merge",
    "mcp.call",
    "tools.call",
    "workflows.run",
    "agents.*",
    "background.start",
    "view_image",
];

static API_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    // `(?<![\w.])name\s*\(`: the regex crate has no look-behind, so the preceding character is matched instead.
    let bare = |name: &str| format!(r"(?:^|[^A-Za-z0-9_.]){name}\s*\(");
    let dotted = |name: &str| format!(r"(?:^|[^A-Za-z0-9_]){}\s*\(", name.replace('.', r"\."));
    CELL_APIS
        .iter()
        .map(|&name| {
            let pattern = match name {
                "bash" | "read" | "edit" | "write" | "view_image" => bare(name),
                "agents.*" => r"(?:^|[^A-Za-z0-9_])agents\.(?:spawn|invoke|register)\s*\(".to_string(),
                _ => dotted(name),
            };
            (name, Regex::new(&pattern).expect("API pattern compiles"))
        })
        .collect()
});

/// The REPL APIs a cell's code names (each at most once): a static read of the source, so a call inside a function
/// the cell only defines counts too, and a call made through an alias does not.
pub fn cell_apis(code: &str) -> Vec<&'static str> {
    API_PATTERNS
        .iter()
        .filter(|(_, pattern)| pattern.is_match(code))
        .map(|(name, _)| *name)
        .collect()
}

static NUDGES: LazyLock<[(&'static str, Regex); 3]> = LazyLock::new(|| {
    let re = |p: &str| Regex::new(p).expect("nudge pattern compiles");
    [
        (
            "skill",
            re(r"^\[Ultron\] [0-9]+ tool rounds in a row succeeded(?-u:\b)"),
        ),
        (
            "wait",
            re(
                r"^\[Ultron\] (?:1 subagent or task you started is|[0-9]+ subagents or tasks you started are) still running(?-u:\b)",
            ),
        ),
        (
            "toolRounds",
            re(r"^\[Ultron\] You have (?:now )?used [0-9]+ rounds of tool calls(?-u:\b)"),
        ),
    ]
});

/// Which steer a `[Ultron] ...` message is (`skill`, `wait` or `toolRounds`; see crate::nudge), or None.
pub fn classify_nudge(text: &str) -> Option<&'static str> {
    if !text.starts_with("[Ultron] ") {
        return None;
    }
    NUDGES.iter().find(|(_, re)| re.is_match(text)).map(|(kind, _)| *kind)
}

/// Which counter a lane's cells go to: the root lane, `rlm.spawn` subagents, or anything else (typed agents, jobs).
pub fn stats_lane(lane: &str) -> &'static str {
    if lane == "main" {
        "root"
    } else if lane.starts_with("ultron.rlm-child.") {
        "subagents"
    } else {
        "other"
    }
}

/// A count as JSON: an integer when it is one (as JavaScript prints it), else the number.
fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

fn count(value: Option<&Value>) -> f64 {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => 0.0,
    }
}

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// A map's entries in JavaScript's enumeration order (array-index keys first, ascending).
fn entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    config::json::js_key_order(map)
        .into_iter()
        .map(|k| (k, &map[k]))
        .collect()
}

fn counts(value: Option<&Value>) -> Value {
    let mut out = Map::new();
    if let Some(map) = object(value) {
        for (key, item) in entries(map) {
            if count(Some(item)) > 0.0 {
                out.insert(key.clone(), num(count(Some(item))));
            }
        }
    }
    Value::Object(out)
}

fn strings(value: Option<&Value>) -> Value {
    let mut out = Map::new();
    if let Some(map) = object(value) {
        for (key, item) in entries(map) {
            if let Some(text) = item.as_str() {
                out.insert(key.clone(), json!(text));
            }
        }
    }
    Value::Object(out)
}

fn empty_cells() -> Value {
    json!({"count": 0, "failed": 0, "apis": {}})
}

fn empty_guard() -> Value {
    json!({"checks": 0, "blocked": 0, "unchecked": 0, "afterChecks": 0, "afterFindings": 0, "ms": 0})
}

/// `emptySessionStats(root, now)`.
pub fn empty_session_stats(root: &str, now: u64) -> Value {
    json!({
        "version": STATS_VERSION,
        "root": root,
        "since": now,
        "updatedAt": now,
        "cells": {"root": empty_cells(), "subagents": empty_cells(), "other": empty_cells()},
        "hostCalls": {},
        "secretsMasked": 0,
        "guards": {},
        "nudges": {"toolRounds": 0, "wait": 0, "skill": 0},
        "usageLimitBlocks": 0,
        "merges": {},
        "childModels": {},
        "externalTurns": {"count": 0, "wallMs": 0},
    })
}

/// `parseSessionStats`: a stored document with every field present, or None when `saved` is not one (not an object,
/// or another version). Unknown fields are dropped and malformed counters read as zero.
pub fn parse_session_stats(saved: &Value) -> Option<Value> {
    let item = saved.as_object()?;
    if let Some(version) = item.get("version")
        && version.as_f64() != Some(STATS_VERSION as f64)
    {
        return None;
    }
    // `(item.x ?? {})`: null and absent read as an empty part.
    let part = |value: Option<&Value>| value.filter(|v| !v.is_null()).cloned();
    let cells = part(item.get("cells"));
    let cells_of = |name: &str| {
        let c = cells.as_ref().and_then(|c| part(c.get(name)));
        json!({
            "count": num(count(c.as_ref().and_then(|c| c.get("count")))),
            "failed": num(count(c.as_ref().and_then(|c| c.get("failed")))),
            "apis": counts(c.as_ref().and_then(|c| c.get("apis"))),
        })
    };
    let mut guards = Map::new();
    if let Some(map) = object(item.get("guards")) {
        for (name, value) in entries(map) {
            let value = part(Some(value));
            let g = |k: &str| num(count(value.as_ref().and_then(|v| v.get(k))));
            guards.insert(
                name.clone(),
                json!({
                    "checks": g("checks"),
                    "blocked": g("blocked"),
                    "unchecked": g("unchecked"),
                    "afterChecks": g("afterChecks"),
                    "afterFindings": g("afterFindings"),
                    "ms": g("ms"),
                }),
            );
        }
    }
    let nudges = part(item.get("nudges"));
    let nudge = |k: &str| num(count(nudges.as_ref().and_then(|n| n.get(k))));
    let turns = part(item.get("externalTurns"));
    let turn = |k: &str| num(count(turns.as_ref().and_then(|t| t.get(k))));
    let root = if item.get("root").and_then(Value::as_str) == Some("external") {
        "external"
    } else {
        "lane"
    };
    Some(json!({
        "version": STATS_VERSION,
        "root": root,
        "since": num(count(item.get("since"))),
        "updatedAt": num(count(item.get("updatedAt"))),
        "cells": {"root": cells_of("root"), "subagents": cells_of("subagents"), "other": cells_of("other")},
        "hostCalls": counts(item.get("hostCalls")),
        "secretsMasked": num(count(item.get("secretsMasked"))),
        "guards": guards,
        "nudges": {"toolRounds": nudge("toolRounds"), "wait": nudge("wait"), "skill": nudge("skill")},
        "usageLimitBlocks": num(count(item.get("usageLimitBlocks"))),
        "merges": strings(item.get("merges")),
        "childModels": strings(item.get("childModels")),
        "externalTurns": {"count": turn("count"), "wallMs": turn("wallMs")},
    }))
}

/// What the store holds, read as `readVersioned` reads it.
pub enum Stored {
    /// Nothing usable (no value, not a stats document, or the store could not be read): count from zero.
    Fresh,
    /// A stats document (an unversioned one reads as version 1).
    Document(Value),
    /// A document in a format version this build does not know: counted in memory, never overwritten.
    Foreign,
}

/// `parseSessionStats(readVersioned("ultron.module/stats", saved))`.
pub fn read_stored(saved: Option<&Value>) -> Stored {
    let Some(Value::Object(document)) = saved else {
        return Stored::Fresh;
    };
    let mut document = document.clone();
    match document.get("version") {
        None => {
            document.insert("version".into(), json!(STATS_VERSION));
        }
        Some(version) if version.as_f64() == Some(STATS_VERSION as f64) && version.is_number() => {}
        Some(_) => return Stored::Foreign,
    }
    parse_session_stats(&Value::Object(document)).map_or(Stored::Fresh, Stored::Document)
}

/// Where the counters are kept (`HostModuleStore`).
pub trait StatsStore: Send + Sync {
    fn read(&self) -> Result<Option<Value>, String>;
    fn write(&self, document: &Value) -> Result<(), String>;
}

/// The session value `ultron.module/stats` of a session journal (`createSessionModuleStore(session, "stats")`).
pub struct SessionStore(pub SharedSession);

impl StatsStore for SessionStore {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(lock_session(&self.0).value(NAMESPACE, SESSION_STATS_MODULE).cloned())
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        lock_session(&self.0)
            .commit(&[Write::set(NAMESPACE, SESSION_STATS_MODULE, document.clone())])
            .map(|_| ())
    }
}

/// Milliseconds since the epoch (tests: a fake clock).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// One guard outcome (`GuardRecord`, the fields counted here).
pub struct GuardOutcome<'a> {
    pub guard: &'a str,
    /// `before_write`, `before_shell` or `after_cell`.
    pub phase: &'a str,
    /// `allowed`, `blocked`, `unchecked`, `clean`, `advisory` or `findings`.
    pub outcome: &'a str,
    pub ms: f64,
}

type Change = Box<dyn Fn(&mut Value) + Send>;

struct State {
    document: Value,
    store: Option<Arc<dyn StatsStore>>,
    /// Changes made before the stored document was read; applied on top of it once it is.
    early: Option<Vec<Change>>,
    dirty: bool,
    /// A save is scheduled (`#timer`).
    timer: bool,
    closed: bool,
    foreign: bool,
    open_turns: HashMap<String, u64>,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the scheduled save early (a flush or close took its place).
    wake: Condvar,
    /// Saves run one at a time, in order.
    writes: Mutex<()>,
    now: Clock,
    flush_after: Duration,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `SessionStatsRecorder`: counts in memory and saves the document shortly after a change.
pub struct SessionStats {
    shared: Arc<Shared>,
}

impl Drop for SessionStats {
    fn drop(&mut self) {
        // A scheduled save of a recorder nobody holds is not waited for.
        lock(&self.shared.state).timer = false;
        self.shared.wake.notify_all();
    }
}

fn add(value: &mut Value, n: f64) {
    *value = num(count(Some(value)) + n);
}

/// `keep(map, key, value)`: `key` as the newest entry of a bounded map (the oldest past [`MAX_KEYS`] are dropped).
fn keep(map: &mut Value, key: &str, value: &str) {
    let Some(map) = map.as_object_mut() else {
        return;
    };
    map.shift_remove(key);
    map.insert(key.to_string(), json!(value));
    let excess = map.len().saturating_sub(MAX_KEYS);
    let old: Vec<String> = map.keys().take(excess).cloned().collect();
    for key in old {
        map.shift_remove(&key);
    }
}

impl SessionStats {
    /// A recorder for a session whose root is `root` (`lane`, or `external` for Claude Code over MCP), counting
    /// from `now()`; it saves to nothing until [`SessionStats::attach`] gives it its store.
    pub fn new(root: &str, now: Clock, flush_after: Duration) -> SessionStats {
        let document = empty_session_stats(root, now());
        SessionStats {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    document,
                    store: None,
                    early: Some(Vec::new()),
                    dirty: false,
                    timer: false,
                    closed: false,
                    foreign: false,
                    open_turns: HashMap::new(),
                }),
                wake: Condvar::new(),
                writes: Mutex::new(()),
                now,
                flush_after,
            }),
        }
    }

    /// A recorder on the system clock that saves 500 ms after a change.
    pub fn system(root: &str) -> SessionStats {
        SessionStats::new(root, Arc::new(|| crate::lane::now_ms() as u64), DEFAULT_FLUSH)
    }

    /// Read the stored counters from `store` and keep counting on top of them (`#load`); changes made before are
    /// applied to what was stored. The session keeps the kind of root it has now.
    pub fn attach(&self, store: Arc<dyn StatsStore>) {
        let read = store.read();
        let mut state = lock(&self.shared.state);
        if state.store.is_some() || state.early.is_none() {
            return;
        }
        match read.map(|saved| read_stored(saved.as_ref())) {
            Ok(Stored::Document(mut stored)) => {
                stored["root"] = state.document["root"].clone();
                state.document = stored;
            }
            Ok(Stored::Foreign) => state.foreign = true,
            Ok(Stored::Fresh) | Err(_) => {}
        }
        state.store = Some(store);
        let early = state.early.take().unwrap_or_default();
        let changed = !early.is_empty();
        for change in &early {
            change(&mut state.document);
        }
        if changed {
            self.schedule(&mut state);
        }
    }

    fn change(&self, change: impl Fn(&mut Value) + Send + 'static) {
        let mut state = lock(&self.shared.state);
        if state.closed {
            return;
        }
        if let Some(early) = &mut state.early {
            early.push(Box::new(change));
            return;
        }
        change(&mut state.document);
        self.schedule(&mut state);
    }

    fn schedule(&self, state: &mut State) {
        state.dirty = true;
        state.document["updatedAt"] = json!((self.shared.now)());
        if state.timer || state.closed {
            return;
        }
        state.timer = true;
        let shared = self.shared.clone();
        std::thread::spawn(move || {
            let state = lock(&shared.state);
            let (mut state, _) = shared
                .wake
                .wait_timeout_while(state, shared.flush_after, |s| s.timer)
                .unwrap_or_else(PoisonError::into_inner);
            // A flush or close that came first took the save over.
            if !state.timer {
                return;
            }
            state.timer = false;
            drop(state);
            save(&shared);
        });
    }

    /// Something ran in the session: its counters are saved even while they are all zero.
    pub fn touch(&self) {
        self.change(|_| {});
    }

    /// One cell ran on `lane`: whether it failed, and how many secrets were masked in its output.
    pub fn cell(&self, lane: &str, code: &str, failed: bool, masked: usize) {
        let (apis, cells) = (cell_apis(code), stats_lane(lane));
        self.change(move |document| {
            let cells = &mut document["cells"][cells];
            add(&mut cells["count"], 1.0);
            if failed {
                add(&mut cells["failed"], 1.0);
            }
            for api in &apis {
                let entry = &mut cells["apis"][*api];
                add(entry, 1.0);
            }
            add(&mut document["secretsMasked"], masked as f64);
        });
    }

    /// A kernel made a host request of this type.
    pub fn host_call(&self, kind: &str) {
        let kind = kind.to_string();
        self.change(move |document| {
            let Some(calls) = document["hostCalls"].as_object_mut() else {
                return;
            };
            if !calls.contains_key(&kind) && calls.len() >= MAX_KEYS {
                return;
            }
            let entry = calls.entry(kind.clone()).or_insert(json!(0));
            add(entry, 1.0);
        });
    }

    /// One guard outcome (Loki, extensions).
    pub fn guard(&self, record: &GuardOutcome<'_>) {
        let (name, ms) = (record.guard.to_string(), record.ms);
        // A shell command checked before it runs counts as a before-write check: it is one.
        let before = matches!(record.phase, "before_write" | "before_shell");
        let (blocked, unchecked) = (record.outcome == "blocked", record.outcome == "unchecked");
        // Advisory-only reports are not findings to fix.
        let findings = matches!(record.outcome, "findings" | "unchecked");
        self.change(move |document| {
            let Some(guards) = document["guards"].as_object_mut() else {
                return;
            };
            let guard = guards.entry(name.clone()).or_insert_with(empty_guard);
            add(&mut guard["ms"], count(Some(&json!(ms))));
            if before {
                add(&mut guard["checks"], 1.0);
                if blocked {
                    add(&mut guard["blocked"], 1.0);
                }
                if unchecked {
                    add(&mut guard["unchecked"], 1.0);
                }
            } else {
                add(&mut guard["afterChecks"], 1.0);
                if findings {
                    add(&mut guard["afterFindings"], 1.0);
                }
            }
        });
    }

    /// A steer sent to the root; text that is not one of the known steers is not counted.
    pub fn nudge(&self, message: &str) {
        let Some(kind) = classify_nudge(message) else {
            return;
        };
        self.change(move |document| add(&mut document["nudges"][kind], 1.0));
    }

    /// A tool call, model request or admission refused because a root's limit was hit.
    pub fn usage_limit_block(&self) {
        self.change(|document| add(&mut document["usageLimitBlocks"], 1.0));
    }

    /// How `rlm.merge` ended for a worktree child.
    pub fn merge(&self, task_id: &str, status: &str) {
        let (task_id, status) = (task_id.to_string(), status.to_string());
        self.change(move |document| keep(&mut document["merges"], &task_id, &status));
    }

    /// The model an `rlm.spawn` subagent ran on.
    pub fn child_model(&self, task_id: &str, model: &str) {
        let (task_id, model) = (task_id.to_string(), model.to_string());
        self.change(move |document| keep(&mut document["childModels"], &task_id, &model));
    }

    /// A turn of an external root began (Claude Code's UserPromptSubmit, or its first cell).
    pub fn external_turn_started(&self, turn: &str) {
        {
            let mut state = lock(&self.shared.state);
            if state.open_turns.contains_key(turn) {
                return;
            }
            let now = (self.shared.now)();
            state.open_turns.insert(turn.to_string(), now);
        }
        self.change(|document| add(&mut document["externalTurns"]["count"], 1.0));
    }

    pub fn external_turn_ended(&self, turn: &str) {
        let elapsed = {
            let mut state = lock(&self.shared.state);
            let Some(started) = state.open_turns.remove(turn) else {
                return;
            };
            (self.shared.now)().saturating_sub(started)
        };
        self.change(move |document| add(&mut document["externalTurns"]["wallMs"], elapsed as f64));
    }

    /// The counters as they are now, saved or not.
    pub fn snapshot(&self) -> Value {
        let state = lock(&self.shared.state);
        let mut document = state.document.clone();
        for change in state.early.iter().flatten() {
            change(&mut document);
        }
        document
    }

    /// Save unsaved changes now. Never fails: a store that refuses the write leaves them for the next change.
    pub fn flush(&self) {
        {
            let mut state = lock(&self.shared.state);
            state.timer = false;
        }
        self.shared.wake.notify_all();
        save(&self.shared);
    }

    /// Save and stop recording (open external turns end now).
    pub fn close(&self) {
        let open: Vec<String> = lock(&self.shared.state).open_turns.keys().cloned().collect();
        for turn in open {
            self.external_turn_ended(&turn);
        }
        self.flush();
        let mut state = lock(&self.shared.state);
        state.closed = true;
        state.timer = false;
        drop(state);
        self.shared.wake.notify_all();
    }
}

fn save(shared: &Shared) {
    let _one_at_a_time = lock(&shared.writes);
    let (store, document) = {
        let mut state = lock(&shared.state);
        let Some(store) = state.store.clone() else {
            return;
        };
        if !state.dirty || state.foreign {
            return;
        }
        state.dirty = false;
        (store, state.document.clone())
    };
    if store.write(&document).is_err() {
        // The counters stay in memory; the next change tries again.
        lock(&shared.state).dirty = true;
    }
}
