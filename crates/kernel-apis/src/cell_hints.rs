//! Situational hints (`ultron/rlm/hints.ts` `CellHints`, `ultron/rlm/loop-detector.ts`): the host watches how a
//! lane uses the runtime and ends a cell's result with at most one short tagged line, `[hint:<tag>] ...`, when
//! something worth knowing happened in that cell: a command detached into a job, a long wait on a job whose
//! completion would have arrived as an event, polling, truncated output, a large file read into a string, the same
//! exception three cells in a row, the same cell run again and again, or a lane stuck in a loop of failing or
//! alternating cells. Mutes and per-tag counts are [`HintsModule`]'s (`hints.mute`). Hints are off unless
//! `ULTRON_HINTS=on`.

use crate::hints::HintsModule;
use crate::util::{Clock, lock, sha256_hex, system_clock};
use fancy_regex::Regex;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

pub const DEFAULT_HINT_MAX_PER_TAG: u64 = 3;
pub const DEFAULT_BLOCKED_SECONDS: f64 = 60.0;
pub const DEFAULT_READ_HANDLE_BYTES: u64 = 256 * 1024;
/// Same exception type this many cells in a row.
const FAILURE_STREAK: usize = 3;
/// The same successful cell this many times in a row (and again at twice as many).
const CELL_REPEATS: usize = 3;
/// Same status call this many times in one cell.
const POLL_REPEATS: usize = 3;

/// Waits that hold the cell until detached work ends. Waiting for subagents earns no hint (the guide asks for it).
const BLOCKING_WAITS: &[&str] = &["shell.result", "background.result"];

/// Status checks of running work; repeating one is polling.
const STATUS_CALLS: &[&str] = &[
    "shell.get",
    "shell.result",
    "shell.list",
    "background.inspect",
    "background.list",
    "agents.inspect",
    "agents.tasks",
    "agents.status",
    "rlm.list_subagents",
];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid hint pattern")
}

static TRUNCATION_MARKER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\[\.\.\. \d+ bytes truncated(?:; [^\n]*?)? \.\.\.\]"));
static FILE_READ: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\.read\(\s*\)|\.read_text\(|\.read_bytes\(|\.readlines\(|\bopen\("));
static BIG_STRING: LazyLock<Regex> = LazyLock::new(|| regex(r"(?m)^<str: ([\d,]+) chars"));
static SLEEP: LazyLock<Regex> = LazyLock::new(|| regex(r"\b(?:time|asyncio)\.sleep\("));
static LOOP: LazyLock<Regex> = LazyLock::new(|| regex(r"(?m)^\s*(?:while|for|async\s+for)\b"));
static STATUS_CHECK: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"\.running\b|\.status\b|\.done\(|\.result\(|\.inspect\(|rlm\.jobs\(|rlm\.job\(|list_subagents\(|agents\.tasks\(|agents\.status\(|background\.list\(",
    )
});
static QUERY_COMMANDS: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*(?:grep|egrep|fgrep|rg|diff|cmp|test|\[)\b"));

fn matches(re: &Regex, text: &str) -> bool {
    re.is_match(text).unwrap_or(false)
}

/// Hints are off unless ULTRON_HINTS is on (or 1, true, yes).
pub fn hints_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| ["on", "1", "true", "yes"].contains(&v.trim().to_lowercase().as_str()))
}

/// `hintMaxPerTag`: ULTRON_HINTS_MAX, a non-negative integer, else 3.
pub fn hint_max_per_tag(value: Option<&str>) -> u64 {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HINT_MAX_PER_TAG)
}

/// `readHandleBytes`: ULTRON_READ_HANDLE_BYTES, a positive integer, else 256 KiB.
pub fn read_handle_bytes(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_READ_HANDLE_BYTES)
}

/// Whether a finished `bash` command failed: a timeout, or a non-zero exit (1 is an answer for a query command).
pub fn bash_failed(command: &str, exit_code: &Value, timed_out: bool) -> bool {
    if timed_out {
        return true;
    }
    match exit_code.as_f64() {
        None => false,
        Some(0.0) => false,
        Some(code) => !(code == 1.0 && matches(&QUERY_COMMANDS, command)),
    }
}

// ------------------------------------------------------------------------------------------------- loop detector

/// Thresholds of the stuck-loop detector (`LoopLimits`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopLimits {
    /// Failed cells in a row.
    pub consecutive: usize,
    /// Cells in an A, B, A, B run.
    pub alternation: usize,
    /// Failed cells in one run.
    pub total: usize,
}

impl Default for LoopLimits {
    fn default() -> Self {
        Self {
            consecutive: 3,
            alternation: 4,
            total: 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoopTrip {
    pub level: u8,
    /// `consecutive`, `alternation` or `total`.
    pub reason: &'static str,
    pub count: usize,
    pub ename: Option<String>,
}

#[derive(Default)]
struct LaneLoop {
    history: Vec<(String, bool)>,
    consecutive: usize,
    streak_ename: Option<String>,
    total: usize,
    level: u8,
    total_level: u8,
}

/// Code with whitespace and digits removed, so cells differing only in those compare equal.
pub fn normalize_cell(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_digit())
        .collect()
}

/// Length of the A, B, A, B run (A differs from B) that ends the history; 0 when there is none.
pub fn alternation_length(hashes: &[&str]) -> usize {
    let n = hashes.len();
    if n < 2 || hashes[n - 1] == hashes[n - 2] {
        return 0;
    }
    let mut length = 2;
    let mut i = n as isize - 3;
    while i >= 0 && hashes[i as usize] == hashes[i as usize + 2] {
        length += 1;
        i -= 1;
    }
    length
}

/// `ToolLoopDetector`.
#[derive(Default)]
pub struct LoopDetector {
    limits: LoopLimits,
    lanes: HashMap<String, LaneLoop>,
}

impl LoopDetector {
    pub fn new(limits: LoopLimits) -> Self {
        Self {
            limits,
            lanes: HashMap::new(),
        }
    }

    pub fn level(&self, lane: &str) -> u8 {
        self.lanes.get(lane).map_or(0, |s| s.level)
    }

    /// A cell on `lane` ended; a trip when this cell makes the lane look stuck.
    pub fn cell_ended(&mut self, lane: &str, code: &str, failed: bool, ename: Option<&str>) -> Option<LoopTrip> {
        let limits = self.limits;
        let state = self.lanes.entry(lane.to_string()).or_default();
        let hash = sha256_hex(normalize_cell(code).as_bytes())[..16].to_string();
        state.history.push((hash.clone(), failed));
        if state.history.len() > limits.alternation * 2 + 2 {
            state.history.remove(0);
        }
        let ename = ename.unwrap_or("").to_string();
        if failed {
            state.consecutive += 1;
            state.total += 1;
            state.streak_ename = Some(
                if state.consecutive == 1 || state.streak_ename.as_deref() == Some(&ename) {
                    ename
                } else {
                    String::new()
                },
            );
        } else {
            state.consecutive = 0;
            state.streak_ename = None;
        }
        let n = state.history.len();
        let repeat = n >= 3 && state.history[n - 3].0 == hash;
        // Progress: a success that is not part of a back-and-forth ends the phase.
        if !failed && !repeat {
            state.level = 0;
        }
        let hashes: Vec<&str> = state.history.iter().map(|(h, _)| h.as_str()).collect();
        let run = alternation_length(&hashes);
        let run_failures = state.history[n - run..].iter().filter(|(_, f)| *f).count();
        let alternating = run >= limits.alternation && run_failures * 2 >= run;
        let trip = |state: &mut LaneLoop, level: u8, reason: &'static str, count: usize| {
            state.level = level;
            if reason == "total" {
                state.total_level = level;
            }
            let ename = (reason == "consecutive")
                .then(|| state.streak_ename.clone().filter(|e| !e.is_empty()))
                .flatten();
            Some(LoopTrip {
                level,
                reason,
                count,
                ename,
            })
        };
        if state.level == 0 {
            if limits.consecutive > 0 && state.consecutive >= limits.consecutive {
                let count = state.consecutive;
                return trip(state, 1, "consecutive", count);
            }
            if limits.alternation > 0 && alternating {
                return trip(state, 1, "alternation", run);
            }
        } else if state.level == 1 {
            if limits.consecutive > 0 && state.consecutive >= limits.consecutive * 2 {
                let count = state.consecutive;
                return trip(state, 2, "consecutive", count);
            }
            if limits.alternation > 0 && alternating && run >= limits.alternation * 2 {
                return trip(state, 2, "alternation", run);
            }
        }
        // The run total spans phases: it trips once, and escalates once, per run.
        if limits.total > 0 && failed {
            let total = state.total;
            if state.total_level == 0 && state.level == 0 && total >= limits.total {
                return trip(state, 1, "total", total);
            }
            if state.total_level == 1 && state.level < 2 && total >= limits.total * 2 {
                return trip(state, 2, "total", total);
            }
        }
        None
    }

    pub fn reset(&mut self, lane: &str) {
        self.lanes.remove(lane);
    }
}

/// The `stuck-loop` hint text for a trip.
pub fn loop_hint_text(trip: &LoopTrip) -> String {
    let what = match trip.reason {
        "consecutive" => format!(
            "The last {} cells failed{}",
            trip.count,
            trip.ename
                .as_ref()
                .map_or(String::new(), |e| format!(" ({e} each time)"))
        ),
        "alternation" => format!(
            "The last {} cells alternate between two near-identical versions",
            trip.count
        ),
        _ => format!("{} cells have failed in this run", trip.count),
    };
    if trip.level == 1 {
        format!(
            "{what}; you look stuck. Before the next cell, state three different hypotheses for why it fails, then run one cell that tests the most likely one instead of retrying a variant."
        )
    } else {
        format!(
            "{what}, even after the last hint. Stop repeating this approach: name three hypotheses you have not tested yet and test the most likely; if none holds, say what blocks you."
        )
    }
}

// ------------------------------------------------------------------------------------------------------ cell hints

/// `CellHintsOptions`.
#[derive(Clone)]
pub struct CellHintsOptions {
    pub enabled: bool,
    pub max_per_tag: u64,
    /// Completions are announced (ULTRON_ASYNC_EVENTS): the wait and polling hints point at events.
    pub async_events: bool,
    /// `rootDelivery: "next-call"`: nothing wakes the root lane between turns. Without
    /// [`CellHints::set_lane_woken`] only the root lane (`main`) counts as woken, and only without this.
    pub next_call_root: bool,
    pub blocked_seconds: f64,
    pub read_handle_bytes: u64,
    pub loop_limits: LoopLimits,
    pub now: Clock,
}

impl Default for CellHintsOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_tag: DEFAULT_HINT_MAX_PER_TAG,
            async_events: true,
            next_call_root: false,
            blocked_seconds: DEFAULT_BLOCKED_SECONDS,
            read_handle_bytes: DEFAULT_READ_HANDLE_BYTES,
            loop_limits: LoopLimits::default(),
            now: system_clock(),
        }
    }
}

/// How a cell ended, as the rlm tool reports it.
pub struct CellOutcome<'a> {
    /// The result text (after truncation).
    pub text: &'a str,
    /// Exception type when the cell failed.
    pub ename: Option<&'a str>,
}

#[derive(Default)]
struct Cell {
    code: String,
    /// (job id, the yield it waited) of each command that came back running.
    detached: Vec<(String, Option<f64>)>,
    waits: Vec<(i64, i64)>,
    wait_types: Vec<String>,
    /// Status call key → count, in first-call order.
    status_calls: Vec<(String, usize)>,
    bash_failed: bool,
}

#[derive(Default)]
struct LaneState {
    cell: Option<Cell>,
    previous_status: HashSet<String>,
    failure: Option<(String, usize)>,
    repeat: Option<(String, usize)>,
}

/// Whether the host wakes a lane when its work ends (its event wording says "end your turn").
pub type LaneWoken = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// `CellHints`.
pub struct CellHints {
    module: Arc<HintsModule>,
    options: CellHintsOptions,
    lanes: Mutex<HashMap<String, LaneState>>,
    loops: Mutex<LoopDetector>,
    woken: Mutex<Option<LaneWoken>>,
}

impl CellHints {
    pub fn new(module: Arc<HintsModule>, options: CellHintsOptions) -> Self {
        let loops = LoopDetector::new(options.loop_limits);
        Self {
            module,
            options,
            lanes: Mutex::new(HashMap::new()),
            loops: Mutex::new(loops),
            woken: Mutex::new(None),
        }
    }

    /// Which lanes the host wakes when their work ends: the others' hints say to await what the answer needs
    /// (nothing wakes them after they reply) instead of "end your turn".
    pub fn set_lane_woken(&self, woken: LaneWoken) {
        *lock(&self.woken) = Some(woken);
    }

    /// Whether `lane` is woken by the ends of its work.
    fn woken(&self, lane: &str) -> bool {
        let woken = lock(&self.woken).clone();
        match woken {
            Some(woken) => woken(lane),
            None => lane == "main" && !self.options.next_call_root,
        }
    }

    pub fn enabled(&self) -> bool {
        self.options.enabled
    }

    fn now(&self) -> i64 {
        (self.options.now)()
    }

    /// `lane`'s run ended: failure streaks start over with its next run.
    pub fn run_ended(&self, lane: &str) {
        lock(&self.loops).reset(lane);
        if let Some(state) = lock(&self.lanes).get_mut(lane) {
            state.failure = None;
        }
    }

    /// A cell starts on `lane`.
    pub fn begin_cell(&self, lane: &str, code: &str) {
        if !self.enabled() {
            return;
        }
        lock(&self.lanes).entry(lane.to_string()).or_default().cell = Some(Cell {
            code: code.to_string(),
            ..Cell::default()
        });
    }

    /// A host request from `lane`'s kernel finished (`result` is `None` when it failed).
    pub fn observe(
        &self,
        lane: &str,
        kind: &str,
        payload: &Map<String, Value>,
        result: Option<&Value>,
        started_at: i64,
    ) {
        let now = self.now();
        let mut lanes = lock(&self.lanes);
        let Some(cell) = lanes.get_mut(lane).and_then(|s| s.cell.as_mut()) else {
            return;
        };
        let reply = result.and_then(Value::as_object);
        let running = reply.is_some_and(|r| r.get("running") == Some(&Value::Bool(true)));
        if kind == "bash"
            && let Some(reply) = reply
            && !running
        {
            cell.bash_failed = bash_failed(
                payload.get("command").and_then(Value::as_str).unwrap_or(""),
                reply.get("exit_code").unwrap_or(&Value::Null),
                reply.get("timed_out") == Some(&Value::Bool(true)),
            );
        }
        if (kind == "bash" || kind == "shell.bash") && running {
            let id = reply
                .and_then(|r| r.get("job"))
                .and_then(|j| j.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("a job");
            let waited = payload.get("yield_after").and_then(Value::as_f64);
            cell.detached.push((id.to_string(), waited));
        }
        if BLOCKING_WAITS.contains(&kind) {
            cell.waits.push((started_at, now));
            if !cell.wait_types.iter().any(|t| t == kind) {
                cell.wait_types.push(kind.to_string());
            }
        }
        if STATUS_CALLS.contains(&kind) {
            // A status call that found the work finished is a result fetch, not a poll.
            if (kind == "shell.get" || kind == "shell.result") && !running {
                return;
            }
            let key = match payload.get("id").and_then(Value::as_str) {
                Some(id) => format!("{kind}:{id}"),
                None => kind.to_string(),
            };
            match cell.status_calls.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 += 1,
                None => cell.status_calls.push((key, 1)),
            }
        }
    }

    /// The cell on `lane` ended: the hint line to append to its result, if any.
    pub fn end_cell(&self, lane: &str, outcome: &CellOutcome<'_>) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        // A lane nothing wakes gets the hints that say to await what the answer needs.
        let next_call = !self.woken(lane);
        let (mut candidates, code, failed) = {
            let mut lanes = lock(&self.lanes);
            let state = lanes.entry(lane.to_string()).or_default();
            let cell = state.cell.take()?;
            let candidates = self.candidates(state, &cell, outcome, next_call);
            state.previous_status = cell.status_calls.iter().map(|(k, _)| k.clone()).collect();
            (candidates, cell.code, outcome.ename.is_some() || cell.bash_failed)
        };
        let (trip, level) = {
            let mut loops = lock(&self.loops);
            let trip = loops.cell_ended(lane, &code, failed, outcome.ename);
            (trip, loops.level(lane))
        };
        if let Some(trip) = trip {
            candidates.insert(0, ("stuck-loop", loop_hint_text(&trip)));
        }
        if candidates.is_empty() {
            return None;
        }
        self.module.fire(lane, &candidates, self.options.max_per_tag, level > 0)
    }

    /// Every hint this cell earned, most specific first.
    fn candidates(
        &self,
        state: &mut LaneState,
        cell: &Cell,
        outcome: &CellOutcome<'_>,
        next_call: bool,
    ) -> Vec<(&'static str, String)> {
        let events = self.options.async_events;
        let mut found: Vec<(&'static str, String)> = Vec::new();
        // Failure streaks count every cell, whether or not a hint fires for it.
        state.failure = match (outcome.ename, state.failure.take()) {
            (Some(ename), Some((previous, count))) if previous == ename => Some((previous, count + 1)),
            (Some(ename), _) => Some((ename.to_string(), 1)),
            (None, _) => None,
        };
        // Exact repeats only: cells that differ in numbers (paging through a list) are different work.
        let code = cell.code.trim().to_string();
        state.repeat = if outcome.ename.is_none() && !cell.bash_failed {
            match state.repeat.take() {
                Some((previous, count)) if previous == code => Some((code, count + 1)),
                _ => Some((code, 1)),
            }
        } else {
            None
        };
        let repeat = state
            .repeat
            .as_ref()
            .filter(|(_, count)| *count == CELL_REPEATS || *count == CELL_REPEATS * 2)
            .map(|(_, count)| {
                (
                    "repeated-cell",
                    format!(
                        "This exact cell has now run {count} times in a row and succeeded each time; rerunning it will not tell you more. Use the output you have, keep values in variables instead of recomputing them, and wait for running work once (`await job.result()`, `await rlm.collect(...)`) instead of checking again."
                    ),
                )
            });

        if let Some((id, waited)) = cell.detached.first() {
            let after = waited.map_or(String::new(), |w| format!(" after {} s", js_number(w)));
            found.push((
                "job-detached",
                if events && next_call {
                    format!(
                        "The command was still running{after}, so it continues as job {id}. Do other work meanwhile: its completion is reported as a <runtime_event> at the top of a later rlm result, and `await <result>.job.result()` waits for it when your answer needs it (nothing wakes you after you reply). (`yield_after=None` blocks until a command ends; `yield_after=0` detaches at once.)"
                    )
                } else if events {
                    format!(
                        "The command was still running{after}, so it continues as job {id}. Do not wait for it: do other work or end your turn; its completion arrives as a <runtime_event> that starts your next turn. (`yield_after=None` blocks until a command ends; `yield_after=0` detaches at once.)"
                    )
                } else {
                    format!(
                        "The command was still running{after}, so it continues as job {id}; `await <result>.job.result()` waits for it. (`yield_after=None` blocks until a command ends.)"
                    )
                },
            ));
        }
        let waited_ms = union_ms(&cell.waits);
        // Without wake-ups, waiting for a result the answer needs is the right move, not a mistake.
        if events && !next_call && waited_ms as f64 >= self.options.blocked_seconds * 1000.0 {
            let calls: Vec<&str> = cell
                .wait_types
                .iter()
                .map(|t| {
                    if t == "shell.result" {
                        "job.result()"
                    } else {
                        "background.result()"
                    }
                })
                .collect();
            found.push((
                "blocked-on-job",
                format!(
                    "This cell spent {} s waiting in {}. You could have ended your turn instead: completions arrive as <runtime_event> messages that start a new turn, so wait only when you need the result now.",
                    js_round(waited_ms as f64 / 1000.0),
                    calls.join(", ")
                ),
            ));
        }
        let repeated = cell
            .status_calls
            .iter()
            .find(|(_, count)| *count >= POLL_REPEATS)
            .map(|(k, _)| k.clone());
        let again = cell
            .status_calls
            .iter()
            .map(|(k, _)| k)
            .find(|k| state.previous_status.contains(*k))
            .cloned();
        let sleep_loop =
            matches(&SLEEP, &cell.code) && matches(&LOOP, &cell.code) && matches(&STATUS_CHECK, &cell.code);
        if repeated.is_some() || again.is_some() || sleep_loop {
            let what = repeated.or(again);
            let lead = match what {
                Some(what) => format!("Repeated status checks ({})", what.replacen(':', " ", 1)),
                None => "Sleeping in a loop around a status check".into(),
            };
            let advice = if events && next_call {
                "Completions are reported on their own at the top of your next rlm result: do other work instead of checking or sleeping, and when only that result is left, wait once (`await job.result()`, `await rlm.collect(...)`)."
            } else if events {
                "Completions arrive as <runtime_event> messages on their own: do other work or end your turn instead of checking or sleeping."
            } else {
                "Wait once with `await job.result()`, `await rlm.collect(...)` or `await agents.result(id)` instead of checking and sleeping."
            };
            found.push(("poll-loop", format!("{lead} look like polling. {advice}")));
        }
        if let Some((ename, count)) = &state.failure
            && *count >= FAILURE_STREAK
        {
            found.push((
                "repeated-failure",
                format!(
                    "{ename} {count} cells in a row. Read the traceback carefully (the failing line and the message) and change your approach instead of retrying a variant of the same cell."
                ),
            ));
        }
        let truncated = matches(&TRUNCATION_MARKER, outcome.text);
        let big_string_chars = BIG_STRING
            .captures(outcome.text)
            .ok()
            .flatten()
            .and_then(|c| c.get(1).map(|m| m.as_str().replace(',', "")))
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(0);
        let handle_bytes = self.options.read_handle_bytes;
        if matches(&FILE_READ, &cell.code) && (truncated || big_string_chars > handle_bytes) {
            found.push((
                "large-read",
                format!(
                    "This cell read a large file into a string. `await read(path)` returns a ContextHandle for files over {} KiB: use h.search(regex), h.lines(a, b), h.chunks(n) or rlm.map(task, h.chunks(n)) and print only what you need.",
                    js_round(handle_bytes as f64 / 1024.0)
                ),
            ));
        }
        if truncated {
            found.push((
                "output-truncated",
                "The output was cut in the middle. Keep large data in variables and print slices, counts or matches, or load big text as a handle (`h = await rlm.load(text_or_path)`; `h.search`, `h.lines`).".into(),
            ));
        }
        if let Some(repeat) = repeat {
            found.push(repeat);
        }
        found
    }
}

/// `Math.round` for non-negative values.
fn js_round(value: f64) -> i64 {
    (value + 0.5).floor() as i64
}

/// A number as JavaScript prints it in a template string (30, 0.5).
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Total length of possibly overlapping intervals (waits run concurrently under asyncio.gather).
fn union_ms(intervals: &[(i64, i64)]) -> i64 {
    let mut sorted = intervals.to_vec();
    sorted.sort_by_key(|(start, _)| *start);
    let mut total = 0;
    let mut end = i64::MIN;
    for (start, stop) in sorted {
        if stop <= end {
            continue;
        }
        total += stop - start.max(end);
        end = stop;
    }
    total
}
