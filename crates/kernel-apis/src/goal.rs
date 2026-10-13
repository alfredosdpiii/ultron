//! The session goal (`/goal`, port of `ultron/goal.ts` GoalDriver): a person sets one objective and a background job
//! (its own REPL, under the usage root `goal:<id>`) works on it until the goal ends.
//!
//! - Only a person sets, edits, pauses, resumes or clears the goal ([`GoalDriver::command`], the worker's
//!   `goal.command` request). The model reads it (`goal.get`), tests with the host's check (`goal.check`) and ends it
//!   (`goal.complete`, `goal.blocked`), naming the revision it worked on.
//! - `goal.complete` with a check command completes only if the host's own run of it exits 0; without one the
//!   completion is recorded as unverified. `goal.blocked` needs BLOCK_MIN_CHECKS checks when there is a check command.
//!   STUCK_CHECKS identical failing checks in a row pause the goal (and stop its job shortly after the reply).
//! - A job that ends while the goal is active starts again only if its run changed the check result
//!   ([`GoalDriver::task_ended`]); otherwise the goal pauses.
//! - Durable (Rust): a goal's job is a task on the task journal, started with the idempotent key `<goal id>:<run>`.
//!   When the worker reopens ([`GoalDriver::recover`]) the goal is reconciled with the journal through
//!   [`GoalJobs::job_state`] and [`GoalJobs::job_with_key`]: a run admitted but not yet recorded is adopted, a job
//!   still running is kept, a job the restart interrupted is followed by the next run (the goal stays active), and a
//!   job that ended while no worker ran is settled as [`GoalDriver::task_ended`] would have. Checks, the stuck count
//!   and progress are in the goal document, written with every change. A worker whose `GoalJobs` keeps no journal
//!   (`job_state` is `None`) behaves as TypeScript: an active goal whose job was running loads as paused.
//!
//! The document `{version: 1, goal}` is the session value `ultron.module/goal`. Requests and commands run one at a
//! time (TS interleaves `goal.*` requests with commands; only their order of effects matters here).

use crate::store::{DocumentStore, read_versioned};
use crate::util::{Clock, fields, js_trim, lock, sha256_hex, stringify, utf16_len, utf16_prefix, uuid};
use crate::{ApiModule, Request, TaskHost};
use fancy_regex::Regex;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The worker request behind `/goal` (payload `{args}`).
pub const GOAL_REQUEST: &str = "goal.command";
/// Failing checks in a row with the same result that pause the goal.
pub const STUCK_CHECKS: i64 = 10;
/// Checks needed before the model may declare the goal blocked (when there is a check command).
pub const BLOCK_MIN_CHECKS: i64 = 3;
/// A hanging check command is stopped after this long.
pub const CHECK_TIMEOUT_SECONDS: u64 = 600;
const CHECK_OUTPUT_CHARS: usize = 2000;
const TEXT_MAX_CHARS: usize = 4000;
const EVIDENCE_MAX: usize = 20;
/// Usage root prefix of goal jobs (`GOAL_USAGE_ROOT_PREFIX` of usage.ts).
pub const GOAL_USAGE_ROOT_PREFIX: &str = "goal:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheckResult {
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub output: String,
}

/// What the task journal says about a goal job.
#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    /// Admitted and not ended (it runs under this worker).
    Running,
    /// Ended, with its durable result (`NativeResult`; `status: "interrupted"` when a restart cut it off).
    Ended(Value),
}

/// The task host's error for work it stopped because it closed (`HOST_CLOSED` of native-host.ts).
pub const HOST_CLOSED: &str = "Ultron task host closed";

/// A job result that says the worker went away under the run (a restart interrupted it, or its host closed), not
/// that the run itself ended.
fn cut_off(result: &Value) -> bool {
    result["status"] == "interrupted" || (result["status"] == "cancelled" && result["error"] == HOST_CLOSED)
}

/// What the driver needs from its worker: run the check in the session's working directory, start and stop the
/// goal's background job, and name the lane a job runs on.
pub trait GoalJobs: Send + Sync {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String>;
    /// Starts the goal's background job with `prompt`; returns its task id.
    fn start_job(&self, prompt: &str, key: &str, usage_root: &str, signal: &Signal) -> Result<String, String>;
    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String>;
    fn lane_of_job(&self, task_id: &str) -> Option<String>;
    /// The journal's state of a job task; `None` when the worker keeps no task journal.
    fn job_state(&self, _task_id: &str) -> Option<JobState> {
        None
    }
    /// The task admitted with `key`, if any.
    fn job_with_key(&self, _key: &str) -> Option<String> {
        None
    }
}

fn regexes() -> &'static [(Regex, &'static str); 3] {
    static CELL: OnceLock<[(Regex, &'static str); 3]> = OnceLock::new();
    CELL.get_or_init(|| {
        [
            (Regex::new(r"(?i)0x[0-9a-f]+").expect("regex"), "0x"),
            (
                Regex::new(r"(?<![A-Za-z0-9_])[0-9]+(?:\.[0-9]+)?\s?(?:ms|s|sec|secs|seconds)(?![A-Za-z0-9_])")
                    .expect("regex"),
                "<t>",
            ),
            (Regex::new(r"\s+").expect("regex"), " "),
        ]
    })
}

/// `text.slice(-units)` in UTF-16 code units.
fn utf16_suffix(text: &str, units: usize) -> &str {
    let total = utf16_len(text);
    if total <= units {
        return text;
    }
    let skip = total - units;
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        if used >= skip {
            return &text[index..];
        }
        used += ch.len_utf16();
    }
    ""
}

/// A check result's fingerprint: exit status and output, with durations and addresses left out.
pub fn check_signature(result: &GoalCheckResult) -> String {
    let mut output = utf16_suffix(&result.output, CHECK_OUTPUT_CHARS).to_string();
    for (pattern, replacement) in regexes() {
        output = pattern.replace_all(&output, *replacement).into_owned();
    }
    let status = if result.timed_out {
        "timeout".to_string()
    } else {
        result.exit_code.map_or("null".to_string(), |code| code.to_string())
    };
    sha256_hex(format!("{status}\n{}", js_trim(&output)).as_bytes())[..16].to_string()
}

fn text_of(goal: &Value, key: &str) -> String {
    goal[key].as_str().unwrap_or_default().to_string()
}

fn int_of(goal: &Value, key: &str) -> i64 {
    goal[key].as_i64().unwrap_or(0)
}

/// The brief of the goal's background job.
pub fn goal_brief(goal: &Value) -> String {
    let revision = &goal["revision"];
    let check = match goal["check"].as_str() {
        None => "There is no check command: when you call goal.complete, give concrete evidence (it is recorded as unverified).".to_string(),
        Some(command) => format!(
            "Check: `{command}`. `await goal.check()` runs it on the host and returns {{passed, exit_code, output}}; goal.complete runs it again and succeeds only if it exits 0."
        ),
    };
    let mut parts = vec![
        format!(
            "You are working on the session goal (revision {revision}) on your own, as a background job: nobody is waiting on your turns, and nothing limits your time."
        ),
        format!("Objective: {}", text_of(goal, "objective")),
        check,
        "Work in your REPL: keep what you tried and learned in Python (`state['goal']`), change the code, test, and use the result to choose the next step. Delegate independent attempts with `rlm.spawn(brief, name=..., worktree=True)` and `await rlm.merge(...)` the one that works.".to_string(),
        format!(
            "Keep going until `await goal.complete({revision}, summary, evidence=[...])` succeeds. Only if something outside you blocks the goal after real attempts: `await goal.blocked({revision}, reason)`. Ten identical failing checks in a row pause the goal, so change your approach when a check keeps failing the same way. The user may change the goal: `await goal.get()` shows the current revision."
        ),
    ];
    if let Some(answer) = goal["last_answer"].as_str() {
        parts.push(format!("The previous run on this goal ended with: {answer}"));
    }
    parts.join("\n\n")
}

/// A short status for `/goal`.
pub fn goal_status_text(goal: Option<&Value>) -> String {
    let Some(goal) = goal else {
        return "No goal. Set one with /goal <objective>.".into();
    };
    let reason = goal["reason"]
        .as_str()
        .filter(|r| !r.is_empty())
        .map(|r| format!(": {r}"))
        .unwrap_or_default();
    let stuck = int_of(goal, "stuck");
    let mut lines = vec![
        format!(
            "Goal (revision {}, {}{reason}): {}",
            goal["revision"],
            text_of(goal, "status"),
            text_of(goal, "objective")
        ),
        format!(
            "Check: {}. Runs: {}. Checks: {}{}.",
            goal["check"].as_str().unwrap_or("none (completion is unverified)"),
            goal["runs"],
            goal["checks"],
            if stuck > 1 {
                format!(" ({stuck} identical failures in a row)")
            } else {
                String::new()
            }
        ),
    ];
    if let Some(job) = goal["job"].as_str() {
        lines.push(format!("Job: {job} (running)."));
    }
    let last = &goal["last_check"];
    if last.is_object() {
        let outcome = if last["passed"] == true {
            "passed".to_string()
        } else if last["timed_out"] == true {
            "timed out".to_string()
        } else {
            format!("exit {}", last["exit_code"])
        };
        lines.push(format!("Last check: {outcome} ({}).", text_of(last, "command")));
    }
    let completion = &goal["completion"];
    if completion.is_object() {
        lines.push(format!(
            "Completed{}: {}",
            if completion["verified"] == true {
                " (check passed)"
            } else {
                " (unverified)"
            },
            text_of(completion, "summary")
        ));
    }
    lines.join("\n")
}

fn public_goal(goal: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "revision",
        "objective",
        "check",
        "status",
        "reason",
        "runs",
        "checks",
        "stuck",
        "last_check",
        "completion",
    ] {
        out.insert(key.into(), goal[key].clone());
    }
    Value::Object(out)
}

fn text(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(utf16_prefix(text, TEXT_MAX_CHARS).to_string()),
        _ => Err(format!("{name} must be a non-empty string")),
    }
}

fn evidence_list(value: Option<&Value>) -> Result<Vec<Value>, String> {
    let items = value.and_then(Value::as_array).filter(|items| !items.is_empty()).ok_or(
        "evidence must be a non-empty list of strings: commands with their outcome (\"npm test: exit 0\") or files with lines",
    )?;
    if !items
        .iter()
        .all(|item| item.as_str().is_some_and(|text| !js_trim(text).is_empty()))
    {
        return Err("every evidence item must be a non-empty string".into());
    }
    Ok(items
        .iter()
        .take(EVIDENCE_MAX)
        .map(|item| Value::from(utf16_prefix(item.as_str().unwrap_or_default(), TEXT_MAX_CHARS)))
        .collect())
}

fn tail(output: &str) -> String {
    if utf16_len(output) > CHECK_OUTPUT_CHARS {
        format!("…{}", utf16_suffix(output, CHECK_OUTPUT_CHARS - 1))
    } else {
        output.to_string()
    }
}

/// JavaScript `String(value)` for a task's error.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        other => stringify(other),
    }
}

struct State {
    goal: Option<Value>,
    loaded: bool,
    closed: bool,
}

pub struct GoalDriver {
    store: Arc<dyn DocumentStore>,
    jobs: Arc<dyn GoalJobs>,
    now: Clock,
    state: Mutex<State>,
}

impl GoalDriver {
    pub fn new(store: Arc<dyn DocumentStore>, jobs: Arc<dyn GoalJobs>, now: Clock) -> Self {
        Self {
            store,
            jobs,
            now,
            state: Mutex::new(State {
                goal: None,
                loaded: false,
                closed: false,
            }),
        }
    }

    fn now(&self) -> i64 {
        (self.now)()
    }

    fn load(&self, state: &mut State) -> Result<(), String> {
        if state.loaded {
            return Ok(());
        }
        state.loaded = true;
        let saved = read_versioned("ultron.module/goal", self.store.read()?, "version")?;
        let goal = saved.and_then(|saved| saved.get("goal").filter(|goal| goal.is_object()).cloned());
        if state.goal.is_none() {
            state.goal = goal;
        }
        self.reconcile(state);
        Ok(())
    }

    /// Reconciles an active goal with the task journal after a reopen (see the module docs).
    fn reconcile(&self, state: &mut State) {
        let Some(goal) = state.goal.as_mut().filter(|goal| goal["status"] == "active") else {
            return;
        };
        let mut changed = false;
        if goal["job"].is_null() {
            // An active goal without a job was starting a run when the worker died: the run is written before it is
            // admitted, so its key names the task if the admission happened.
            let key = format!("{}:{}", text_of(goal, "id"), int_of(goal, "runs"));
            match self.jobs.job_with_key(&key) {
                Some(task) => {
                    goal["job"] = task.into();
                    changed = true;
                }
                None => {
                    self.start_job(state, &Signal::new());
                    return;
                }
            }
        }
        let Some(job) = goal["job"].as_str().map(str::to_string) else {
            return;
        };
        match self.jobs.job_state(&job) {
            Some(JobState::Running) => {
                if changed {
                    self.persist(state);
                }
            }
            Some(JobState::Ended(result)) if cut_off(&result) => {
                // The worker going away cut the run off, not the job's own work: the goal goes on with its next run.
                goal["job"] = Value::Null;
                goal["last_answer"] = "The previous run was interrupted when Ultron restarted; continue from the goal's state and the files.".into();
                self.start_job(state, &Signal::new());
            }
            Some(JobState::Ended(result)) => self.job_ended(state, &job, Some(&result)),
            None => {
                // No journal: a job does not outlive the worker that ran it, so the goal pauses (TypeScript).
                goal["job"] = Value::Null;
                goal["status"] = "paused".into();
                goal["reason"] = "Ultron restarted while the goal job ran; /goal resume continues".into();
            }
        }
    }

    /// Loads the goal and reconciles it with the task journal (the worker's open); returns it.
    pub fn recover(&self) -> Result<Option<Value>, String> {
        self.get()
    }

    /// Writes the document; as in TS a failed write does not fail the operation.
    fn persist(&self, state: &State) {
        let _ = self.store.write(&json!({"version": 1, "goal": state.goal}));
    }

    /// The goal now (a copy), after the document loads.
    pub fn get(&self) -> Result<Option<Value>, String> {
        let mut state = lock(&self.state);
        self.load(&mut state)?;
        Ok(state.goal.clone())
    }

    /// Stops starting jobs (shutdown).
    pub fn close(&self) {
        lock(&self.state).closed = true;
    }

    fn start_job(&self, state: &mut State, signal: &Signal) {
        if state.closed {
            return;
        }
        let now = self.now();
        let goal = state.goal.as_mut().expect("a goal");
        goal["runs"] = (int_of(goal, "runs") + 1).into();
        goal["run_progress"] = false.into();
        goal["updated_at"] = now.into();
        let id = text_of(goal, "id");
        let key = format!("{id}:{}", goal["runs"]);
        // The run is durable before its task is admitted, so a reopen finds the task by this key.
        goal["job"] = Value::Null;
        self.persist(state);
        let goal = state.goal.as_mut().expect("a goal");
        match self.jobs.start_job(
            &goal_brief(goal),
            &key,
            &format!("{GOAL_USAGE_ROOT_PREFIX}{id}"),
            signal,
        ) {
            Ok(job) => goal["job"] = job.into(),
            Err(error) => {
                goal["job"] = Value::Null;
                goal["status"] = "paused".into();
                goal["reason"] = format!("the goal job could not start: {error}").into();
            }
        }
        self.persist(state);
    }

    fn stop_job(&self, goal: &mut Value, reason: &str) {
        let job = goal["job"].as_str().map(str::to_string);
        goal["job"] = Value::Null;
        if let Some(job) = job {
            let _ = self.jobs.stop_job(&job, reason);
        }
    }

    fn set_status(&self, state: &mut State, status: &str, reason: Option<&str>) {
        let now = self.now();
        let goal = state.goal.as_mut().expect("a goal");
        goal["status"] = status.into();
        goal["reason"] = reason.map_or(Value::Null, Value::from);
        goal["updated_at"] = now.into();
        self.persist(state);
    }

    /// `/goal [objective | pause | resume | clear | check [command] | status]`; returns the text to show.
    pub fn command(&self, args: &str, signal: &Signal) -> Result<String, String> {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        self.load(state)?;
        let text = js_trim(args);
        if text.is_empty() || text == "status" {
            return Ok(goal_status_text(state.goal.as_ref()));
        }
        if text == "clear" {
            let Some(goal) = state.goal.as_mut() else {
                return Ok("No goal to clear.".into());
            };
            self.stop_job(goal, "goal cleared");
            state.goal = None;
            self.persist(state);
            return Ok("Goal cleared.".into());
        }
        if text == "pause" {
            match state.goal.as_mut() {
                Some(goal) if goal["status"] == "active" => self.stop_job(goal, "goal paused"),
                _ => return Ok(goal_status_text(state.goal.as_ref())),
            }
            self.set_status(state, "paused", Some("paused by you; /goal resume continues"));
            return Ok(goal_status_text(state.goal.as_ref()));
        }
        if text == "resume" {
            match state.goal.as_ref() {
                Some(goal) if goal["status"] != "complete" && goal["status"] != "active" => {}
                _ => return Ok(goal_status_text(state.goal.as_ref())),
            }
            self.set_status(state, "active", None);
            state.goal.as_mut().expect("a goal")["stuck"] = 0.into();
            self.start_job(state, signal);
            return Ok(goal_status_text(state.goal.as_ref()));
        }
        if text == "check" || text.starts_with("check ") {
            let now = self.now();
            let Some(goal) = state.goal.as_mut() else {
                return Ok("Set a goal first: /goal <objective>.".into());
            };
            let command = js_trim(&text["check".len()..]);
            goal["check"] = if command.is_empty() {
                Value::Null
            } else {
                utf16_prefix(command, TEXT_MAX_CHARS).into()
            };
            goal["revision"] = (int_of(goal, "revision") + 1).into();
            goal["stuck"] = 0.into();
            goal["last_signature"] = Value::Null;
            goal["updated_at"] = now.into();
            self.persist(state);
            return Ok(goal_status_text(state.goal.as_ref()));
        }
        let mut revision = 1;
        if let Some(goal) = state.goal.as_mut() {
            self.stop_job(goal, "goal replaced");
            revision = int_of(goal, "revision") + 1;
        }
        let now = self.now();
        state.goal = Some(json!({
            "id": format!("goal-{}", &uuid()[..8]),
            "revision": revision,
            "objective": utf16_prefix(text, TEXT_MAX_CHARS),
            "check": null,
            "status": "active",
            "reason": null,
            "job": null,
            "runs": 0,
            "checks": 0,
            "stuck": 0,
            "last_signature": null,
            "run_progress": false,
            "last_check": null,
            "last_answer": null,
            "created_at": now,
            "updated_at": now,
            "completion": null,
        }));
        self.start_job(state, signal);
        Ok(format!(
            "{}\nAdd the check that decides when it is done: /goal check <command>.",
            goal_status_text(state.goal.as_ref())
        ))
    }

    /// A task ended (the worker's task-end observer; `result` is its `NativeResult`): a goal job that ended early is
    /// started again when its run made progress, otherwise the goal pauses.
    pub fn task_ended(&self, task_id: &str, result: Option<&Value>) -> Result<(), String> {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        self.load(state)?;
        self.job_ended(state, task_id, result);
        Ok(())
    }

    fn job_ended(&self, state: &mut State, task_id: &str, result: Option<&Value>) {
        let now = self.now();
        let Some(goal) = state.goal.as_mut() else {
            return;
        };
        if state.closed || goal["job"] != task_id {
            return;
        }
        goal["job"] = Value::Null;
        let answer = match result.map(|result| (&result["value"], result.get("error"))) {
            Some((Value::String(value), _)) => value.clone(),
            Some((_, Some(error))) if !error.is_null() => js_string(error),
            _ => String::new(),
        };
        goal["last_answer"] = if utf16_len(&answer) > 1000 {
            format!("{}…", utf16_prefix(&answer, 999)).into()
        } else if answer.is_empty() {
            Value::Null
        } else {
            answer.into()
        };
        goal["updated_at"] = now.into();
        if goal["status"] != "active" {
            self.persist(state);
            return;
        }
        if goal["run_progress"] == true {
            self.start_job(state, &Signal::new());
            return;
        }
        self.set_status(
            state,
            "paused",
            Some("the goal job ended without progress; /goal resume continues"),
        );
    }

    fn run_check(&self, state: &mut State) -> Result<Value, String> {
        let goal = state.goal.as_mut().expect("a goal");
        let command = text_of(goal, "check");
        let result = self.jobs.run_check(&command, CHECK_TIMEOUT_SECONDS)?;
        let passed = result.exit_code == Some(0) && !result.timed_out;
        let signature = check_signature(&result);
        let same = goal["last_signature"] == signature.as_str();
        if !same {
            goal["run_progress"] = true.into();
        }
        goal["stuck"] = (if passed {
            0
        } else if same {
            int_of(goal, "stuck") + 1
        } else {
            1
        })
        .into();
        goal["last_signature"] = signature.into();
        goal["checks"] = (int_of(goal, "checks") + 1).into();
        let now = self.now();
        goal["last_check"] = json!({
            "command": command,
            "passed": passed,
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
            "output": tail(&result.output),
            "at": now,
        });
        goal["updated_at"] = now.into();
        let run = goal["last_check"].clone();
        self.persist(state);
        Ok(run)
    }

    /// The model's call must name the active goal's current revision.
    fn current(goal: &Value, revision: Option<&Value>) -> Result<(), String> {
        if goal["status"] != "active" {
            return Err(format!("The session goal is {}, not active", text_of(goal, "status")));
        }
        let matches = revision
            .and_then(Value::as_f64)
            .zip(goal["revision"].as_f64())
            .is_some_and(|(a, b)| a == b);
        if !matches {
            return Err(format!(
                "The goal is at revision {} (the user changed it): read it with goal.get() and work on that",
                goal["revision"]
            ));
        }
        Ok(())
    }

    /// One `goal.*` request from `lane`.
    pub fn handle_request(&self, kind: &str, payload: &Map<String, Value>, lane: &str) -> Result<Value, String> {
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        self.load(state)?;
        if kind == "goal.get" {
            fields(payload, &[])?;
            return Ok(state.goal.as_ref().map_or(Value::Null, public_goal));
        }
        let goal = state.goal.as_ref().ok_or("There is no session goal")?;
        let job = goal["job"].as_str().and_then(|job| self.jobs.lane_of_job(job));
        if lane != "main" && Some(lane) != job.as_deref() {
            return Err("Only the goal's job or the root agent works the session goal; subagents report to it".into());
        }
        match kind {
            "goal.check" => {
                fields(payload, &[])?;
                if goal["check"].is_null() {
                    return Err("This goal has no check command (the user sets one with /goal check)".into());
                }
                let mut run = self.run_check(state)?;
                let goal = state.goal.as_ref().expect("a goal");
                if goal["status"] == "active" && int_of(goal, "stuck") >= STUCK_CHECKS {
                    let stuck = int_of(goal, "stuck");
                    self.set_status(
                        state,
                        "paused",
                        Some(&format!(
                            "{stuck} failing checks in a row gave the same result; /goal resume continues"
                        )),
                    );
                    let goal = state.goal.as_mut().expect("a goal");
                    // Stopped after this reply reaches the job's cell.
                    if let Some(stopping) = goal["job"].as_str().map(str::to_string) {
                        let jobs = self.jobs.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(100));
                            let _ = jobs.stop_job(&stopping, "goal stuck");
                        });
                    }
                    goal["job"] = Value::Null;
                    run["paused"] = true.into();
                    run["reason"] = goal["reason"].clone();
                    return Ok(run);
                }
                run["paused"] = false.into();
                Ok(run)
            }
            "goal.complete" => {
                fields(payload, &["revision", "summary", "evidence"])?;
                Self::current(goal, payload.get("revision"))?;
                let summary = text(payload.get("summary"), "summary")?;
                let evidence = evidence_list(payload.get("evidence"))?;
                let checked = !goal["check"].is_null();
                if checked {
                    let run = self.run_check(state)?;
                    if run["passed"] != true {
                        return Ok(json!({
                            "complete": false,
                            "status": state.goal.as_ref().expect("a goal")["status"],
                            "check": run,
                            "message": "The check failed, so the goal is not complete: fix the cause and call goal.complete again.",
                        }));
                    }
                }
                let now = self.now();
                state.goal.as_mut().expect("a goal")["completion"] =
                    json!({"summary": summary, "evidence": evidence, "verified": checked, "at": now});
                self.set_status(state, "complete", None);
                let goal = state.goal.as_ref().expect("a goal");
                Ok(
                    json!({"complete": true, "status": goal["status"], "verified": checked, "check": goal["last_check"]}),
                )
            }
            "goal.blocked" => {
                fields(payload, &["revision", "reason"])?;
                Self::current(goal, payload.get("revision"))?;
                let reason = text(payload.get("reason"), "reason")?;
                let checks = int_of(goal, "checks");
                if !goal["check"].is_null() && checks < BLOCK_MIN_CHECKS {
                    return Err(format!(
                        "goal.blocked is accepted after {BLOCK_MIN_CHECKS} checks ({checks} so far): keep working, test with goal.check(), and try another approach"
                    ));
                }
                self.set_status(state, "blocked", Some(&reason));
                Ok(json!({"status": "blocked", "reason": reason}))
            }
            _ => Err(format!("Ultron RLM host request is not wired: {kind}")),
        }
    }
}

impl ApiModule for GoalDriver {
    fn prefixes(&self) -> &[&str] {
        &["goal."]
    }

    fn start(&self, _host: &dyn TaskHost) -> Result<(), String> {
        self.load(&mut lock(&self.state))
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.handle_request(request.kind, request.payload, request.lane)
    }
}

/// [`GoalJobs`] over the worker's task journal and shell: a goal job is a `background-job@1` task admitted through
/// [`TaskHost::spawn`] with the goal's run key, and a check is the host's blocking `bash`.
pub struct TaskGoalJobs {
    tasks: Arc<dyn crate::TaskHost>,
    shell: Arc<dyn kernel::HostHandler>,
}

impl TaskGoalJobs {
    pub fn new(tasks: Arc<dyn crate::TaskHost>, shell: Arc<dyn kernel::HostHandler>) -> TaskGoalJobs {
        TaskGoalJobs { tasks, shell }
    }

    fn task(&self, matches: impl Fn(&Value) -> bool) -> Option<Value> {
        self.tasks.tasks().ok()?.into_iter().find(matches)
    }
}

impl GoalJobs for TaskGoalJobs {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String> {
        let mut payload = Map::new();
        payload.insert("command".into(), command.into());
        payload.insert("timeout".into(), timeout_seconds.into());
        let reply = self.shell.handle("bash", payload, &Signal::new())?;
        Ok(GoalCheckResult {
            exit_code: reply["exit_code"].as_i64(),
            timed_out: reply["timed_out"] == true,
            output: reply["output"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// The usage root is not passed: [`TaskHost::spawn`] charges the job to its admission root.
    fn start_job(&self, prompt: &str, key: &str, _usage_root: &str, signal: &Signal) -> Result<String, String> {
        let request = json!({"definition": "background-job@1", "input": {"prompt": prompt}, "key": key});
        let task = self.tasks.spawn(&request, None, signal)?;
        task["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "the task host returned no task id".into())
    }

    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        self.tasks.cancel(task_id, reason).map(|_| ())
    }

    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        self.tasks.task_lane(task_id)
    }

    fn job_state(&self, task_id: &str) -> Option<JobState> {
        let task = self.task(|task| task["id"] == task_id)?;
        Some(match task.get("result").filter(|result| !result.is_null()) {
            Some(result) => JobState::Ended(result.clone()),
            None => JobState::Running,
        })
    }

    fn job_with_key(&self, key: &str) -> Option<String> {
        self.task(|task| task["key"] == key)
            .and_then(|task| task["id"].as_str().map(str::to_string))
    }
}
