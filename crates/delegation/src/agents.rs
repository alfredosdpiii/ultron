//! Typed agents of `native-host.ts`: `agents.register` / `list` / `status` / `tasks` / `spawn` / `invoke` /
//! `inspect` / `result` / `cancel`, `background.*`, and the `workflows.run` graph, over one task journal, one
//! definition registry and an optional usage ledger.
//!
//! Every strategy shares one path: input validation before admission, idempotent admission (a retried key neither
//! takes a budget slot nor releases the original's), the task's usage reservation, its timeout capped by the root
//! wall deadline, first-terminal-wins finishing that commits before publishing, subtree cancellation that aborts
//! the work, and output validation. What differs is the work: a deterministic adapter (code), a predict adapter
//! (one model call per attempt, bounded repair), or an `rlm` lane run ([`LaneRunner`]: a fresh lane with its own
//! REPL and, when named, its own model). Only `rlm` opens a lane.
//!
//! Waits block only the request's host thread: `agents.invoke` and workflow nodes cancel their task when the
//! requesting cell is cancelled (TS: the request context's abort); `agents.result` ends with an error then (TS keeps
//! waiting). The model's tool list is untouched: these are kernel APIs (`agents_api.py`).

use crate::contract::Brief;
use crate::definitions::{Definition, DefinitionRegistry, Strategy, is_model};
use crate::schema::json_equal;
use crate::task_store::{TaskJournal, TaskStore, task_fingerprint, uuid_v4};
use crate::usage::{Measurement, Reservation, ReserveRequest, UsageLedger, number};
use crate::verdict::{MAX_VERDICT_REJECTIONS, validate_verdict};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const GRAPH_TASK_LIMIT: usize = 300;
const GRAPH_PREVIEW_CHARS: usize = 240;
const GRAPH_TEXT_CHARS: usize = 160;
const GRAPH_WORKFLOW_LIMIT: usize = 20;
const GRAPH_WORKFLOW_NODE_LIMIT: usize = 64;
const WORKFLOW_MAX_ROUNDS: u64 = 10;
const CONTROLS: [&str; 7] = [
    "permissionPrompts",
    "riskBlocking",
    "capabilityEnforcement",
    "budgetEnforcement",
    "completionGates",
    "refinementApproval",
    "sandboxRequired",
];

/// One deterministic or predict call (`NativeDefinitionAdapterRequest`).
pub struct AdapterRequest<'a> {
    /// The definition's public descriptor.
    pub definition: &'a Value,
    pub input: &'a Value,
    /// Aborted when the task is cancelled or times out.
    pub signal: &'a Signal,
    /// `{attempt, previous, error}` on a predict repair.
    pub repair: Option<Value>,
}

/// A deterministic or predict adapter: the value, or an error that fails the task.
pub type Adapter = Arc<dyn Fn(AdapterRequest<'_>) -> Result<Value, String> + Send + Sync>;

/// One `rlm` strategy run: a prompt on a lane of its own (or a reused one), its model when named.
pub struct LaneRun {
    pub task_id: String,
    /// The lane the run acts as: its kernel's host requests name it, so they resolve to this task.
    pub lane: String,
    /// Run on an existing lane (a retained instance) instead of a fresh one.
    pub reuse: bool,
    pub prompt: String,
    /// `provider/model` when the request or definition names one; `None`: the lane's default model.
    pub model: Option<String>,
    pub timeout_ms: u64,
    pub deadline_at: Option<f64>,
    pub definition: String,
    /// Aborted when the task is cancelled or times out.
    pub signal: Signal,
}

/// The completed run: the assistant text at its tip and the provider-reported usage of its responses (`None`
/// unless every response reported it).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaneReply {
    pub text: String,
    pub usage: Option<Measurement>,
    /// Assistant turns and tool calls of the run (the graph view's counts).
    pub turns: u64,
    pub tool_calls: u64,
}

pub type LaneRunner = Arc<dyn Fn(LaneRun) -> Result<LaneReply, String> + Send + Sync>;

/// One `rlm-frame@1` task run (`NativeFrameRun`): bounded inference owns its prompts, repairs and budget. The
/// frame's value succeeds the task; an error fails it (cancelled once the task's signal is aborted).
pub struct FrameRun {
    pub task_id: String,
    pub input: Value,
    /// The lane the frame acts as (`ultron.rlm-frame.<task id>`): its kernel's requests resolve to this task.
    pub lane: String,
    pub model: Option<String>,
    /// The usage root the task's admission was charged to.
    pub usage_root: Option<String>,
    pub deadline_at: Option<f64>,
    pub timeout_ms: u64,
    /// Aborted when the task is cancelled or times out.
    pub signal: Signal,
}

pub type FrameRunner = Arc<dyn Fn(FrameRun) -> Result<Value, String> + Send + Sync>;

/// A detached task reached its durable terminal result (`DetachedTaskEnd`).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskEnd {
    pub task: Value,
    /// "task_done".
    pub kind: &'static str,
    pub owner_lane: String,
    pub root_id: Option<String>,
    pub awaited: bool,
    pub fetch: String,
    pub cost: Option<f64>,
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
/// Called with a lane name (the lane's abort).
pub type LaneHook = Arc<dyn Fn(&str) + Send + Sync>;
/// Prepares a lane before a task reuses it.
pub type LaneCheck = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
/// Active refinements of a definition id: `[{id, version, text}]`.
pub type RefinementsHook = Arc<dyn Fn(&str) -> Result<Vec<Value>, String> + Send + Sync>;
/// A task's durable end, with its cost and model when known.
pub type TaskEndHook = Arc<dyn Fn(&Value, Option<f64>, Option<&str>) + Send + Sync>;
/// Extra `agents.status` fields for a calling lane.
pub type StatusExtras = Arc<dyn Fn(&str) -> Map<String, Value> + Send + Sync>;
/// A lane and a message or holder, answered yes or no (steering, pinning).
pub type LaneMessage = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;
/// A lane and a holder (unpinning).
pub type LaneRelease = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub struct AgentsOptions {
    pub store: Box<dyn TaskStore>,
    /// Runtime registration needs it (`options.definitionStore`).
    pub definition_store: Option<Box<dyn TaskStore>>,
    pub usage: Option<UsageLedger>,
    pub deterministic: Option<Adapter>,
    pub predict: Option<Adapter>,
    pub lanes: Option<LaneRunner>,
    /// Runs `rlm-frame@1` tasks (bounded inference frames); without it they fail.
    pub frames: Option<FrameRunner>,
    /// Best-effort abort of a task's lane (the lane's own `abort`).
    pub abort_lane: Option<LaneHook>,
    /// Before a task runs on a reused lane (clears per-invocation scratch).
    pub before_lane_reuse: Option<LaneCheck>,
    /// Active refinements for a definition id: `[{id, version, text}]`.
    pub refinements: Option<RefinementsHook>,
    /// Called once a task's terminal result is durable, with its cost and model when known.
    pub on_task_end: Option<TaskEndHook>,
    /// Extra `agents.status` fields for a calling lane.
    pub status_extras: Option<StatusExtras>,
    /// Queue a steering message into a lane; false when it has no live run.
    pub steer: Option<LaneMessage>,
    pub pin_lane: Option<LaneMessage>,
    pub unpin_lane: Option<LaneRelease>,
    /// Charge each root turn to a usage root of its own (see [`Agents::begin_root_turn`]).
    pub root_turns: bool,
    pub now: Clock,
}

impl AgentsOptions {
    /// An in-memory journal, no ledger, no adapters, no lanes, the system clock.
    pub fn new(store: Box<dyn TaskStore>) -> Self {
        AgentsOptions {
            store,
            definition_store: None,
            usage: None,
            deterministic: None,
            predict: None,
            lanes: None,
            frames: None,
            abort_lane: None,
            before_lane_reuse: None,
            refinements: None,
            on_task_end: None,
            status_extras: None,
            steer: None,
            pin_lane: None,
            unpin_lane: None,
            root_turns: false,
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            }),
        }
    }
}

/// A request to admit one task (`TaskRequest`).
#[derive(Clone, Debug, Default)]
struct TaskRequest {
    definition: String,
    input: Value,
    model: Option<String>,
    key: Option<String>,
    timeout_ms: u64,
    lane: Option<String>,
    workflow: Option<Value>,
    untimed: bool,
    usage_root: Option<String>,
    /// The delegation brief (role, instruction, contract, hop) whose contract the result must meet.
    brief: Option<Brief>,
}

#[derive(Default)]
struct Task {
    id: String,
    parent_id: Option<String>,
    definition: String,
    record: Value,
    /// The durable terminal result, or the commit failure.
    outcome: Option<Result<Value, String>>,
    finishing: bool,
    /// Admitted by this owner: its result is awaited, not read.
    live: bool,
    signal: Signal,
    /// Aborted once the task ended (stops its timer).
    done: Signal,
    lane: Option<String>,
    reservation: Option<Reservation>,
    usage_settled: bool,
    /// Model reservations of the task's run; the task's end settles any its run has not.
    model_reservations: Vec<Reservation>,
    notify: Option<&'static str>,
    waiters: usize,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    input_preview: Option<String>,
    workflow: Option<Value>,
    cost: Option<f64>,
    tokens: Option<f64>,
    model: Option<String>,
    turns: Option<u64>,
    tool_calls: Option<u64>,
    last_text: Option<String>,
    verdict: Option<Value>,
    verdict_rejections: u32,
    verdict_problems: Vec<String>,
    verdict_invalid: bool,
}

struct State {
    journal: TaskJournal,
    registry: DefinitionRegistry,
    tasks: Vec<Task>,
    lane_tasks: HashMap<String, String>,
    closed: bool,
    loaded: Option<Result<(), String>>,
    workflow_runs: Vec<Value>,
    root_turn: Option<String>,
    root_turn_active: bool,
    root_aliases: Vec<(String, String)>,
}

impl State {
    fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
    fn parent_of(&self, id: &str) -> Option<String> {
        self.task(id).and_then(|t| t.parent_id.clone())
    }
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    /// Admissions run one at a time (TS serializes them through installation of the live task).
    admissions: Mutex<()>,
    usage: Option<UsageLedger>,
    deterministic: Option<Adapter>,
    predict: Option<Adapter>,
    lanes: Option<LaneRunner>,
    frames: Option<FrameRunner>,
    abort_lane: Option<LaneHook>,
    before_lane_reuse: Option<LaneCheck>,
    refinements: Option<RefinementsHook>,
    on_task_end: Option<TaskEndHook>,
    status_extras: Option<StatusExtras>,
    steer: Option<LaneMessage>,
    pin_lane: Option<LaneMessage>,
    unpin_lane: Option<LaneRelease>,
    root_turns: bool,
    now: Clock,
    events: Mutex<Sender<TaskEnd>>,
}

/// The typed agent host; clones share it.
#[derive(Clone)]
pub struct Agents {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

fn js_trim(text: &str) -> &str {
    kernel_apis::util::js_trim(text)
}

fn nonempty_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}

fn object_input(value: Option<&Value>) -> Result<&Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| "agent input must be a JSON object".to_string())
}

/// `definitionKey`: `id@version`.
fn definition_key(value: Option<&Value>) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|text| harness::task_store::is_definition(text))
        .map(str::to_string)
        .ok_or_else(|| "definition must be id@version".into())
}

/// `taskOptions`: model, key and timeout of a request.
fn task_options(payload: &Map<String, Value>) -> Result<(Option<String>, Option<String>, u64), String> {
    let model = match payload.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) if is_model(model) => Some(model.clone()),
        Some(_) => return Err("model must be provider/model".into()),
    };
    let key = match payload.get("key") {
        None | Some(Value::Null) => None,
        value => Some(nonempty_string(value, "key")?),
    };
    let timeout = match payload.get("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .as_f64()
            .filter(|t| t.fract() == 0.0 && (1.0..=MAX_TIMEOUT_MS as f64).contains(t))
            .map(|t| t as u64)
            .ok_or("timeout_ms must be an integer between 1 and 3600000")?,
    };
    Ok((model, key, timeout))
}

fn stringify(value: &Value) -> String {
    kernel_apis::util::stringify(value)
}

/// `preview(value, limit)`: whitespace collapsed, cut to `limit` UTF-16 units with an ellipsis.
fn preview(value: &Value, limit: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => stringify(other),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if kernel_apis::util::utf16_len(&flat) > limit {
        format!("{}…", kernel_apis::util::utf16_prefix(&flat, limit - 1))
    } else {
        flat
    }
}

fn fetch_hint(definition: &str, id: &str) -> String {
    match definition {
        "rlm-child@1" => format!("await rlm.collect([\"{id}\"])"),
        "background-job@1" => format!("await background.result(\"{id}\")"),
        _ => format!("await agents.result(\"{id}\")"),
    }
}

/// `publicTask`: id, definition, state, parentId?, result?.
fn public_task(record: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "definition", "state", "parentId", "result"] {
        if let Some(value) = record.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// `publicRecord`: id, key, fingerprint, definition, state, result?, parentId?.
fn public_record(record: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "key", "fingerprint", "definition", "state", "result", "parentId"] {
        if let Some(value) = record.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

fn failed(error: impl Into<String>) -> Value {
    json!({ "status": "failed", "error": error.into(), "verification": "unverified" })
}

fn cancelled(error: impl Into<String>) -> Value {
    json!({ "status": "cancelled", "error": error.into(), "verification": "unverified" })
}

fn succeeded(value: Value) -> Value {
    json!({ "status": "succeeded", "value": value, "verification": "unverified" })
}

/// `jsonFrom(text)`: a reply as JSON, a fenced block unwrapped.
fn json_from(text: &str) -> Result<Value, String> {
    let trimmed = js_trim(text);
    let body = trimmed
        .strip_prefix("```")
        .and_then(|rest| {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
            let rest = rest.strip_prefix('\n')?;
            rest.strip_suffix("\n```")
        })
        .unwrap_or(trimmed);
    serde_json::from_str(body).map_err(|error| {
        let shown = if kernel_apis::util::utf16_len(body) > 32 {
            format!("{}...", kernel_apis::util::utf16_prefix(body, 32))
        } else {
            body.to_string()
        };
        format!("Unexpected token in JSON: \"{shown}\" is not valid JSON ({error})")
    })
}

fn text_of(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// `conditionMet`: the compared field (or whole value) equals `equals`.
fn condition_met(value: Option<&Value>, field: Option<&str>, equals: &Value) -> bool {
    let actual = match field {
        None => value,
        Some(field) => value.and_then(Value::as_object).and_then(|map| map.get(field)),
    };
    actual.is_some_and(|actual| json_equal(actual, equals))
}

/// `boundValue`: a subagent's checked verdict when it gave one, else the node's value.
fn bound_value(outcome: Option<&Value>) -> Option<Value> {
    let outcome = outcome?;
    let verdict = &outcome["verdict"];
    if !verdict.is_object() {
        return outcome.get("value").cloned();
    }
    Some(json!({
        "status": verdict["status"], "summary": verdict["summary"], "outputs": verdict["outputs"],
        "evidence": verdict["evidence"], "changed_files": verdict["changed_files"],
        "check": outcome["check"]["outcome"].as_str().unwrap_or("unchecked"),
        "reply": outcome["value"].as_str().map_or(Value::Null, |text| json!(text)),
    }))
}

/// `verdictGate`: a subagent node succeeds only when its verdict passed and was not contradicted.
fn verdict_gate(result: Value) -> Value {
    let verdict = &result["verdict"];
    if result["status"] != "succeeded" || !verdict.is_object() {
        return result;
    }
    let mut gated = result.clone();
    if verdict["status"] != "passed" {
        gated["status"] = json!("failed");
        gated["error"] = json!(format!(
            "Subagent verdict {}: {}",
            verdict["status"].as_str().unwrap_or_default(),
            verdict["summary"].as_str().unwrap_or_default()
        ));
        return gated;
    }
    if result["check"]["outcome"] == "contradicted" {
        let unobserved: Vec<String> = result["check"]["unobserved"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text_of)
            .collect();
        gated["status"] = json!("failed");
        gated["error"] = json!(format!(
            "Subagent verdict contradicted: declared as changed but unchanged: {}",
            unobserved.join(", ")
        ));
    }
    gated
}

/// One validated workflow node.
#[derive(Clone, Debug)]
struct Node {
    id: String,
    definition: String,
    input: Option<Value>,
    depends_on: Vec<String>,
    /// One dependency, or several (fan-in).
    input_from: Option<Result<String, Vec<String>>>,
    when: Option<(String, Option<String>, Value)>,
    join_any: bool,
    revise: Option<(String, Option<String>, Value, u64)>,
    model: Option<String>,
    key: Option<String>,
    timeout_ms: u64,
    /// The node's brief as written (its hop is the workflow caller's plus one).
    brief: Option<Value>,
}

impl Agents {
    pub fn new(options: AgentsOptions) -> (Self, Receiver<TaskEnd>) {
        let (sender, receiver) = channel();
        let registry = DefinitionRegistry::new(
            options.definition_store,
            options.deterministic.is_some(),
            options.predict.is_some(),
        );
        let agents = Agents {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    journal: TaskJournal::new(options.store),
                    registry,
                    tasks: Vec::new(),
                    lane_tasks: HashMap::new(),
                    closed: false,
                    loaded: None,
                    workflow_runs: Vec::new(),
                    root_turn: None,
                    root_turn_active: false,
                    root_aliases: Vec::new(),
                }),
                changed: Condvar::new(),
                admissions: Mutex::new(()),
                usage: options.usage,
                deterministic: options.deterministic,
                predict: options.predict,
                lanes: options.lanes,
                frames: options.frames,
                abort_lane: options.abort_lane,
                before_lane_reuse: options.before_lane_reuse,
                refinements: options.refinements,
                on_task_end: options.on_task_end,
                status_extras: options.status_extras,
                steer: options.steer,
                pin_lane: options.pin_lane,
                unpin_lane: options.unpin_lane,
                root_turns: options.root_turns,
                now: options.now,
                events: Mutex::new(sender),
            }),
        };
        (agents, receiver)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    fn now(&self) -> f64 {
        (self.inner.now)()
    }

    /// `loadTasks`: the registry, the journal (an earlier owner's unfinished tasks become interrupted), and the
    /// ledger, whose open reservations all belong to an ended owner and settle as unknown.
    fn load(&self) -> Result<(), String> {
        let mut state = self.state();
        if state.loaded.is_none() {
            let outcome = (|| {
                state.registry.ready()?;
                for record in state.journal.list()? {
                    let id = record["id"].as_str().unwrap_or_default().to_string();
                    if state.task(&id).is_none() {
                        state.tasks.push(Task {
                            parent_id: record["parentId"].as_str().map(String::from),
                            definition: record["definition"].as_str().unwrap_or_default().into(),
                            outcome: record.get("result").cloned().map(Ok),
                            record,
                            id,
                            ..Task::default()
                        });
                    }
                }
                if let Some(usage) = &self.inner.usage {
                    usage.ready()?;
                    usage.reconcile(&[])?;
                }
                Ok(())
            })();
            state.loaded = Some(outcome);
        }
        state.loaded.clone().unwrap_or(Ok(()))?;
        state.registry.ready()?;
        state.journal.list().map(|_| ())
    }

    /// The task that owns `lane` (`None`: the root lane or a lane this host did not start).
    pub fn caller_task(&self, lane: &str) -> Option<String> {
        self.state().lane_tasks.get(lane).cloned()
    }

    /// Whether `lane` runs a task of this host (its `rlm.finish` is answered here).
    pub fn owns_lane(&self, lane: &str) -> bool {
        self.state().lane_tasks.contains_key(lane)
    }

    /// `beginRootTurn(runId)`: work admitted while the turn runs is charged to the turn's own root.
    pub fn begin_root_turn(&self, run_id: &str) {
        let mut state = self.state();
        let root = Self::root_of_run(&state, run_id);
        state.root_turn = Some(root);
        state.root_turn_active = true;
    }

    /// `endRootTurn(runId)`.
    pub fn end_root_turn(&self, run_id: &str) {
        let mut state = self.state();
        if state.root_turn.as_deref() == Some(Self::root_of_run(&state, run_id).as_str()) {
            state.root_turn_active = false;
        }
    }

    /// `continueRootTurn(runId, rootId)`: a run started by a completion event keeps its earlier root.
    pub fn continue_root_turn(&self, run_id: &str, root_id: &str) {
        let mut state = self.state();
        state.root_aliases.retain(|(run, _)| run != run_id);
        state.root_aliases.push((run_id.into(), root_id.into()));
        if state.root_aliases.len() > 256 {
            state.root_aliases.remove(0);
        }
    }

    fn root_of_run(state: &State, run_id: &str) -> String {
        state
            .root_aliases
            .iter()
            .find(|(run, _)| run == run_id)
            .map_or_else(|| format!("turn:{run_id}"), |(_, root)| root.clone())
    }

    /// `usageRootForLane(lane, runId)`: the root a model turn on `lane` is charged to.
    pub fn usage_root_for_lane(&self, lane: &str, run_id: &str) -> Option<String> {
        let state = self.state();
        if lane == "main" {
            return self.inner.root_turns.then(|| Self::root_of_run(&state, run_id));
        }
        let task = state.lane_tasks.get(lane)?;
        state
            .task(task)
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string())
    }

    /// `admissionRoot(parentId)`.
    fn admission_root(&self, state: &State, parent: Option<&str>) -> Option<String> {
        let inherited = parent
            .and_then(|id| state.task(id))
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string());
        if inherited.is_some() {
            return inherited;
        }
        if !self.inner.root_turns {
            return None;
        }
        if state.root_turn_active && state.root_turn.is_some() {
            return state.root_turn.clone();
        }
        Some(format!("job:{}", uuid_v4()))
    }

    /// `statusRoot(parentId)`.
    fn status_root(&self, state: &State, parent: Option<&str>) -> Option<String> {
        let inherited = parent
            .and_then(|id| state.task(id))
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string());
        inherited.or_else(|| state.root_turn.clone().filter(|_| self.inner.root_turns))
    }

    fn definition(&self, key: &str) -> Result<Definition, String> {
        self.state().registry.get(key)
    }

    /// `request(payload)` of `agents.spawn` / `agents.invoke`.
    fn request(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<TaskRequest, String> {
        fields(payload, &["definition", "input", "model", "key", "timeout_ms", "brief"])?;
        let definition = definition_key(payload.get("definition"))?;
        let Some(input) = payload.get("input") else {
            return Err("Agent input is not JSON".into());
        };
        let item = self.definition(&definition)?;
        {
            let state = self.state();
            if !state.registry.is_valid_input(&item, input) {
                return Err(state.registry.validation_error(&item, input, "input"));
            }
        }
        let (model, key, timeout_ms) = task_options(payload)?;
        let brief = match payload.get("brief") {
            None | Some(Value::Null) => None,
            Some(brief) => Some(Brief::parse(brief, Some(self.hop_below(parent)))?),
        };
        Ok(TaskRequest {
            definition,
            input: input.clone(),
            model,
            key,
            timeout_ms,
            brief,
            ..TaskRequest::default()
        })
    }

    /// The hop of a task `parent` starts: one more than the delegations above it (the root's children are hop 1).
    fn hop_below(&self, parent: Option<&str>) -> u32 {
        let state = self.state();
        let mut hop = 1;
        let mut current = parent.map(str::to_string);
        while let Some(id) = current {
            hop += 1;
            current = state.parent_of(&id);
        }
        hop
    }

    /// `spawnTask`: idempotent admission under the usage ledger, then the run starts on a thread of its own.
    fn spawn_task(
        &self,
        request: TaskRequest,
        cell: Option<&Signal>,
        parent: Option<&str>,
        notify: Option<&'static str>,
    ) -> Result<String, String> {
        let _admitting = lock(&self.inner.admissions);
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        if let Some(reason) = cell.and_then(Signal::reason) {
            return Err(reason);
        }
        let mut fingerprinted = json!({
            "definition": request.definition, "input": request.input, "model": request.model,
            "timeout_ms": request.timeout_ms,
        });
        // A brief is part of what was asked: a retried key with another contract is another task.
        if let Some(brief) = &request.brief {
            fingerprinted["brief"] = brief.to_value();
        }
        let fingerprint = task_fingerprint(&fingerprinted);
        let key = request.key.clone().unwrap_or_else(uuid_v4);
        // An idempotent retry resolves to the existing task before any budget is touched.
        if request.key.is_some() {
            let mut state = self.state();
            let prior = state.journal.list()?.into_iter().find(|t| t["key"] == key.as_str());
            if let Some(prior) = prior {
                if prior["fingerprint"] != fingerprint.as_str() {
                    return Err("Idempotency key reused for a different task".into());
                }
                let id = prior["id"].as_str().unwrap_or_default().to_string();
                Self::adopt(&mut state, prior);
                return Ok(id);
            }
        }
        let strategy = self.definition(&request.definition)?.strategy();
        let root = {
            let state = self.state();
            request
                .usage_root
                .clone()
                .or_else(|| self.admission_root(&state, parent))
        };
        let reservation = match &self.inner.usage {
            Some(usage) => Some(usage.reserve(&ReserveRequest {
                kind: "task".into(),
                root_id: root,
                model_backed: Some(strategy != Strategy::Deterministic),
                request_key: Some(key.clone()),
                timeout_ms: (!request.untimed).then_some(request.timeout_ms as f64),
                ..ReserveRequest::default()
            })?),
            None => None,
        };
        let settle = |status: &str| {
            if let (Some(usage), Some(reservation)) = (&self.inner.usage, &reservation) {
                let _ = usage.settle(reservation, Some(status), None);
            }
        };
        let admitted = self
            .state()
            .journal
            .admit(&request.definition, &fingerprint, &key, parent);
        let (record, created) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                settle("unknown");
                return Err(error);
            }
        };
        let id = record["id"].as_str().unwrap_or_default().to_string();
        if !created {
            settle("succeeded");
            Self::adopt(&mut self.state(), record);
            return Ok(id);
        }
        let now = self.now();
        let deadline_at = reservation.as_ref().and_then(Reservation::deadline_at);
        let admitted_at = reservation.as_ref().map(Reservation::admitted_at);
        let task = Task {
            id: id.clone(),
            parent_id: parent.map(String::from),
            definition: request.definition.clone(),
            record,
            live: true,
            reservation,
            notify,
            started_at: Some(now),
            input_preview: Some(preview(&request.input, GRAPH_PREVIEW_CHARS)),
            workflow: request.workflow.clone(),
            ..Task::default()
        };
        let done = task.done.clone();
        let closed = {
            let mut state = self.state();
            state.tasks.push(task);
            state.closed
        };
        // The timeout, capped by the root wall deadline; only a deadline the ledger capped below the task's own
        // timeout is reported as the root wall deadline.
        let delay = match deadline_at {
            None => (!request.untimed).then_some(request.timeout_ms as f64),
            Some(deadline) => Some(if request.untimed {
                (deadline - now).max(1.0)
            } else {
                (request.timeout_ms as f64).min(deadline - now).max(1.0)
            }),
        };
        let wall = deadline_at.is_some_and(|deadline| {
            request.untimed || deadline < admitted_at.unwrap_or(now) + request.timeout_ms as f64
        });
        if let Some(delay) = delay {
            let host = self.clone();
            let (task_id, timeout) = (id.clone(), request.timeout_ms);
            std::thread::spawn(move || {
                if !done.wait(Duration::from_secs_f64(delay / 1000.0)) {
                    let reason = if wall {
                        "Ultron root wall deadline exceeded".to_string()
                    } else {
                        format!("Ultron task exceeded {timeout}ms timeout")
                    };
                    let _ = host.cancel(&task_id, &reason);
                }
            });
        }
        if closed {
            let _ = self.cancel(&id, "Ultron task host closed");
        } else {
            let host = self.clone();
            let task_id = id.clone();
            std::thread::spawn(move || host.run(&task_id, request));
        }
        Ok(id)
    }

    /// A journal record this owner did not start (an earlier owner's, or a retried key's) joins the task table.
    fn adopt(state: &mut State, record: Value) {
        let id = record["id"].as_str().unwrap_or_default().to_string();
        if state.task(&id).is_some() {
            return;
        }
        state.tasks.push(Task {
            parent_id: record["parentId"].as_str().map(String::from),
            definition: record["definition"].as_str().unwrap_or_default().into(),
            outcome: record.get("result").cloned().map(Ok),
            record,
            id,
            ..Task::default()
        });
    }

    /// `run`: the running transition, the work, then the terminal result.
    fn run(&self, id: &str, request: TaskRequest) {
        let started = {
            let mut state = self.state();
            match state.journal.transition(id, "running", None) {
                Ok(record) => {
                    let finishing = state.task(id).is_none_or(|t| t.finishing || t.outcome.is_some());
                    if let Some(task) = state.task_mut(id)
                        && !finishing
                    {
                        task.record = record;
                    }
                    Ok(!finishing)
                }
                Err(error) => Err(error),
            }
        };
        match started {
            Ok(false) => {}
            Ok(true) => {
                let result = self.execute(id, &request);
                let result = self.with_limit_reason(id, result);
                // The host verifies the brief's contract before the result is committed or seen.
                let result = match &request.brief {
                    Some(brief) => brief.verify(result),
                    None => result,
                };
                let _ = self.finish(id, result);
            }
            Err(error) => {
                let mut state = self.state();
                if let Some(task) = state.task_mut(id) {
                    task.signal.abort(error.clone());
                    task.done.abort("ended");
                    task.outcome = Some(Err(error));
                }
                self.settle_task(&mut state, id, "unknown");
                self.inner.changed.notify_all();
            }
        }
    }

    /// `withLimitReason`: a task that failed once its root's tree ran out of turns, tokens or cost fails with the
    /// limit message.
    fn with_limit_reason(&self, id: &str, result: Value) -> Value {
        if result["status"] != "failed" {
            return result;
        }
        let (Some(usage), root) = (&self.inner.usage, self.root_of_task(id)) else {
            return result;
        };
        match usage.turn_budget_exhausted(root.as_deref()) {
            Ok(Some(reason)) if result["error"] != reason.as_str() => {
                let mut result = result;
                result["error"] = json!(reason);
                result
            }
            _ => result,
        }
    }

    fn root_of_task(&self, id: &str) -> Option<String> {
        self.state()
            .task(id)
            .and_then(|t| t.reservation.as_ref())
            .map(|r| r.root_id().to_string())
    }

    fn reserve_model(
        &self,
        id: &str,
        request_key: String,
        request: &TaskRequest,
    ) -> Result<Option<Reservation>, String> {
        let Some(usage) = &self.inner.usage else {
            return Ok(None);
        };
        // Reserved under the task lock, so a model reservation is never admitted for a task that has
        // already ended (TS passes the task signal to `reserve`); `commit` settles the ones still open.
        let mut state = self.state();
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        if let Some(reason) = task.signal.reason() {
            return Err(reason);
        }
        if task.outcome.is_some() || task.finishing {
            return Err("Ultron task already ended".into());
        }
        let reservation = task.reservation.clone();
        let (root, deadline) = (
            reservation.as_ref().map(|r| r.root_id().to_string()),
            reservation.as_ref().and_then(Reservation::deadline_at),
        );
        let reservation = usage.reserve(&ReserveRequest {
            kind: "model".into(),
            root_id: root,
            parent_task_id: Some(id.into()),
            task_id: Some(id.into()),
            request_key: Some(request_key),
            timeout_ms: (deadline.is_none() && !request.untimed).then_some(request.timeout_ms as f64),
            deadline_at: deadline,
            ..ReserveRequest::default()
        })?;
        if let Some(task) = state.task_mut(id) {
            task.model_reservations.push(reservation.clone());
        }
        Ok(Some(reservation))
    }

    /// `execute`: the strategy's work; every failure is a typed result, never a success.
    fn execute(&self, id: &str, request: &TaskRequest) -> Value {
        let signal = match self.state().task(id) {
            Some(task) => task.signal.clone(),
            None => return failed("Unknown Ultron task"),
        };
        let outcome = self.work(id, request, &signal);
        match outcome {
            Ok(result) => result,
            Err(error) => {
                if signal.is_aborted() {
                    cancelled(error)
                } else {
                    failed(error)
                }
            }
        }
    }

    fn work(&self, id: &str, request: &TaskRequest, signal: &Signal) -> Result<Value, String> {
        let aborted = || signal.reason().map_or(Ok(()), Err);
        aborted()?;
        let definition = self.definition(&request.definition)?;
        let public = definition.public();
        let output_error = |value: &Value| -> Option<String> {
            let state = self.state();
            (!state.registry.is_valid_output(&definition, value))
                .then(|| state.registry.validation_error(&definition, value, "output"))
        };
        match definition.strategy() {
            Strategy::Deterministic => {
                self.state().registry.can_execute(&definition)?;
                let value = match &self.inner.deterministic {
                    None if definition.key() == "identity@1" => request.input.clone(),
                    None => {
                        return Err(format!(
                            "Deterministic definition {} requires an injected deterministic adapter",
                            definition.key()
                        ));
                    }
                    Some(adapter) => adapter(AdapterRequest {
                        definition: &public,
                        input: &request.input,
                        signal,
                        repair: None,
                    })?,
                };
                aborted()?;
                match output_error(&value) {
                    Some(error) => Err(error),
                    None => Ok(succeeded(value)),
                }
            }
            Strategy::Predict => {
                self.state().registry.can_execute(&definition)?;
                let adapter = self.inner.predict.clone().ok_or_else(|| {
                    format!(
                        "Predict definition {} requires an injected predict adapter",
                        definition.key()
                    )
                })?;
                let mut repair: Option<Value> = None;
                for attempt in 0..=definition.max_repairs() {
                    aborted()?;
                    let key = if attempt == 0 {
                        format!("{id}:model")
                    } else {
                        format!("{id}:model:repair-{attempt}")
                    };
                    let reservation = self.reserve_model(id, key, request)?;
                    let answer = adapter(AdapterRequest {
                        definition: &public,
                        input: &request.input,
                        signal,
                        repair: repair.clone(),
                    });
                    let status = match &answer {
                        Ok(_) => "succeeded",
                        Err(_) if signal.is_aborted() => "cancelled",
                        Err(_) => "failed",
                    };
                    if let Some(usage) = &self.inner.usage {
                        if let Some(reservation) = &reservation {
                            usage.settle(reservation, Some(status), None)?;
                        }
                        // A predict attempt is one model turn of its root (the adapter reports no tokens).
                        usage.record_turn(self.root_of_task(id).as_deref(), None, None)?;
                    }
                    let value = answer?;
                    match output_error(&value) {
                        None => return Ok(succeeded(value)),
                        Some(error) => {
                            repair = Some(json!({ "attempt": attempt + 1, "previous": value, "error": error }));
                        }
                    }
                }
                let error = repair
                    .as_ref()
                    .and_then(|r| r["error"].as_str())
                    .unwrap_or("Predict adapter returned an invalid output")
                    .to_string();
                let repairs = definition.max_repairs();
                Err(match repairs {
                    0 => error,
                    1 => format!("{error} after 1 repair attempt"),
                    n => format!("{error} after {n} repair attempts"),
                })
            }
            Strategy::Rlm => self.run_lane(id, request, &definition, signal),
        }
    }

    /// The `rlm` strategy: the definition's prompt (or a subagent's brief) on its own lane.
    fn run_lane(
        &self,
        id: &str,
        request: &TaskRequest,
        definition: &Definition,
        signal: &Signal,
    ) -> Result<Value, String> {
        if definition.id() == "rlm-frame" {
            return self.run_frame(id, request, signal);
        }
        let runner = self
            .inner
            .lanes
            .clone()
            .ok_or("Ultron rlm definitions need a lane runner, and this host has none")?;
        let lane = request
            .lane
            .clone()
            .unwrap_or_else(|| format!("ultron.{}.{id}", definition.id()));
        if request.lane.is_some()
            && let Some(hook) = &self.inner.before_lane_reuse
        {
            hook(&lane)?;
        }
        let model = request.model.clone().or_else(|| definition.model().map(String::from));
        {
            let mut state = self.state();
            // A reused lane now acts for its newest invocation.
            state.lane_tasks.insert(lane.clone(), id.into());
            if let Some(task) = state.task_mut(id) {
                task.lane = Some(lane.clone());
                task.model = model.as_deref().map(|m| preview(&json!(m), 80));
            }
        }
        if signal.is_aborted() {
            self.abort_lane(&lane);
        }
        signal.reason().map_or(Ok(()), Err)?;
        let base = if definition.id() == "rlm-child" {
            request.input["prompt"].as_str().unwrap_or_default().to_string()
        } else {
            format!(
                "{}\n\nInput data:\n{}\n\nOutput contract:\n{}",
                definition.instructions(),
                stringify(&request.input),
                definition.output_description()
            )
        };
        // Refinements are optional: an unavailable refinement service never fails the task.
        let refinements = self
            .inner
            .refinements
            .as_ref()
            .and_then(|hook| hook(definition.id()).ok())
            .unwrap_or_default();
        signal.reason().map_or(Ok(()), Err)?;
        let base = match &request.brief {
            Some(brief) => brief.prompt(&base),
            None => base,
        };
        let prompt = if refinements.is_empty() {
            base
        } else {
            let notes: Vec<String> = refinements
                .iter()
                .map(|r| {
                    let version = match &r["version"] {
                        Value::Null => "unversioned".to_string(),
                        other => stringify(other),
                    };
                    format!(
                        "Active refinement {} (version {version}):\n{}",
                        r["id"].as_str().unwrap_or_default(),
                        r["text"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            format!("{base}\n\n{}", notes.join("\n\n"))
        };
        let reservation = self.reserve_model(id, format!("{id}:model"), request)?;
        let deadline = reservation.as_ref().and_then(Reservation::deadline_at).or_else(|| {
            self.state()
                .task(id)
                .and_then(|t| t.reservation.as_ref())
                .and_then(Reservation::deadline_at)
        });
        let reply = runner(LaneRun {
            task_id: id.into(),
            lane: lane.clone(),
            reuse: request.lane.is_some(),
            prompt,
            model,
            timeout_ms: request.timeout_ms,
            deadline_at: deadline,
            definition: definition.key(),
            signal: signal.clone(),
        });
        let status = match &reply {
            _ if signal.is_aborted() => "cancelled",
            Ok(_) => "succeeded",
            Err(_) => "failed",
        };
        let usage = reply.as_ref().ok().and_then(|r| r.usage.clone());
        {
            let mut state = self.state();
            if let Some(task) = state.task_mut(id) {
                task.cost = usage.as_ref().and_then(|u| u.cost);
                task.tokens = usage.as_ref().and_then(|u| u.total_tokens);
                if let Ok(reply) = &reply {
                    // The run's own entries are authoritative for the graph view's counts.
                    task.turns = Some(task.turns.unwrap_or(0).max(reply.turns));
                    task.tool_calls = Some(task.tool_calls.unwrap_or(0).max(reply.tool_calls));
                    let flat = reply.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !flat.is_empty() {
                        task.last_text = Some(preview(&json!(flat), GRAPH_TEXT_CHARS));
                    }
                }
            }
        }
        if let (Some(ledger), Some(reservation)) = (&self.inner.usage, &reservation) {
            ledger.settle(reservation, Some(status), usage.as_ref())?;
        }
        signal.reason().map_or(Ok(()), Err)?;
        let text = reply?.text;
        if definition.id() == "rlm-child" {
            return self.child_result(id, &text);
        }
        if js_trim(&text).is_empty() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if definition.id() == "background-job" {
            json!(text)
        } else {
            json_from(&text)?
        };
        let state = self.state();
        if !state.registry.is_valid_output(definition, &value) {
            return Err(state.registry.validation_error(definition, &value, "output"));
        }
        Ok(succeeded(value))
    }

    /// An `rlm-frame@1` task: its lane acts for it, and the frame runner does the work.
    fn run_frame(&self, id: &str, request: &TaskRequest, signal: &Signal) -> Result<Value, String> {
        let runner = self
            .inner
            .frames
            .clone()
            .ok_or("rlm-frame@1 tasks are started by rlm.infer and rlm.map only")?;
        let lane = format!("ultron.rlm-frame.{id}");
        let (usage_root, deadline_at) = {
            let mut state = self.state();
            state.lane_tasks.insert(lane.clone(), id.into());
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.lane = Some(lane.clone());
            task.model = request.model.as_deref().map(|m| preview(&json!(m), 80));
            let reservation = task.reservation.as_ref();
            (
                reservation.map(|r| r.root_id().to_string()),
                reservation.and_then(Reservation::deadline_at),
            )
        };
        signal.reason().map_or(Ok(()), Err)?;
        let value = runner(FrameRun {
            task_id: id.into(),
            input: request.input.clone(),
            lane,
            model: request.model.clone(),
            usage_root,
            deadline_at,
            timeout_ms: request.timeout_ms,
            signal: signal.clone(),
        })?;
        Ok(succeeded(value))
    }

    /// `childResult` without a workspace to compare: the reply (or the verdict's summary), the verdict, and an
    /// unchecked check when a verdict was given or rejected.
    fn child_result(&self, id: &str, text: &str) -> Result<Value, String> {
        let state = self.state();
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        let verdict = task.verdict.clone();
        if js_trim(text).is_empty() && verdict.is_none() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if js_trim(text).is_empty() {
            verdict.as_ref().map_or(Value::Null, |v| v["summary"].clone())
        } else {
            json!(text)
        };
        let concurrent = self.concurrent_with(&state, task);
        let mut check = json!({
            "outcome": if task.verdict_invalid { "invalid" } else { "unchecked" },
            "unobserved": [], "unreported": [], "unlisted": [], "concurrent": concurrent,
            "reason": "no workspace to compare",
        });
        if task.verdict_invalid {
            check["problems"] = json!(task.verdict_problems);
        }
        let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                                 "verdict": verdict.clone().unwrap_or(Value::Null) });
        if verdict.is_some() || task.verdict_invalid {
            result["check"] = check;
        }
        if verdict.is_none() {
            result["unverified"] = json!(true);
        }
        Ok(result)
    }

    /// `concurrentWith`: other lane-backed tasks whose runs overlapped this one, outside its own line.
    fn concurrent_with(&self, state: &State, task: &Task) -> Vec<String> {
        let start = task.started_at.unwrap_or(0.0);
        let end = self.now();
        let mut ancestors = Vec::new();
        let mut current = task.parent_id.clone();
        while let Some(id) = current {
            current = state.parent_of(&id);
            ancestors.push(id);
        }
        let descends = |other: &Task| {
            let mut current = other.parent_id.clone();
            while let Some(id) = current {
                if id == task.id {
                    return true;
                }
                current = state.parent_of(&id);
            }
            false
        };
        state
            .tasks
            .iter()
            .filter(|other| other.id != task.id && other.lane.is_some())
            .filter(|other| !other.definition.starts_with("rlm-frame@"))
            .filter(|other| {
                other.started_at.is_some_and(|s| s <= end) && other.ended_at.unwrap_or(f64::INFINITY) >= start
            })
            .filter(|other| !ancestors.contains(&other.id) && !descends(other))
            .map(|other| other.id.clone())
            .collect()
    }

    fn abort_lane(&self, lane: &str) {
        if let Some(hook) = &self.inner.abort_lane {
            hook(lane);
        }
    }

    fn settle_task(&self, state: &mut State, id: &str, status: &str) {
        let Some(task) = state.task_mut(id) else {
            return;
        };
        if task.usage_settled {
            return;
        }
        task.usage_settled = true;
        let Some(usage) = &self.inner.usage else {
            return;
        };
        // A run cut short (cancel, root deadline) has not settled its model call: it ends with its task,
        // before the result is published, as TS settles it when the aborted prompt unwinds. Settling a
        // reservation the run already settled is a no-op.
        for reservation in std::mem::take(&mut task.model_reservations) {
            let _ = usage.settle(&reservation, Some(status), None);
        }
        if let Some(reservation) = task.reservation.clone() {
            let _ = usage.settle(&reservation, Some(status), None);
        }
    }

    /// `finish`: the first terminal result wins; nothing is published until the journal commits it.
    fn finish(&self, id: &str, result: Value) -> Result<Value, String> {
        {
            let mut state = self.state();
            loop {
                let task = state.task(id).ok_or("Unknown Ultron task")?;
                if let Some(outcome) = &task.outcome {
                    return outcome.clone();
                }
                if !task.finishing {
                    break;
                }
                state = self.inner.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            if let Some(task) = state.task_mut(id) {
                task.finishing = true;
            }
        }
        self.commit(id, result)
    }

    /// The terminal transition of a task this caller is finishing.
    fn commit(&self, id: &str, result: Value) -> Result<Value, String> {
        let terminal = match result["status"].as_str() {
            Some("succeeded") => "completed",
            Some(other) => other,
            None => "failed",
        }
        .to_string();
        let mut state = self.state();
        let committed = state.journal.transition(id, &terminal, Some(result));
        let now = self.now();
        let closed = state.closed;
        let owner_lane = state
            .parent_of(id)
            .and_then(|p| state.task(&p).and_then(|t| t.lane.clone()))
            .unwrap_or_else(|| "main".into());
        let outcome = committed.map(|record| {
            if let Some(task) = state.task_mut(id) {
                task.record = record.clone();
            }
            record["result"].clone()
        });
        let Some(task) = state.task_mut(id) else {
            return Err("Unknown Ultron task".into());
        };
        task.ended_at.get_or_insert(now);
        task.outcome = Some(outcome.clone());
        task.done.abort("ended");
        let lane = task.lane.clone();
        if let Err(error) = &outcome {
            task.signal.abort(error.clone());
            drop(state);
            if let Some(lane) = lane {
                self.abort_lane(&lane);
            }
            self.inner.changed.notify_all();
            return outcome;
        }
        let status = match outcome.as_ref().ok().and_then(|r| r["status"].as_str()) {
            Some("succeeded") => "succeeded",
            Some("cancelled") => "cancelled",
            Some("interrupted") => "unknown",
            _ => "failed",
        };
        self.settle_task(&mut state, id, status);
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        let record = public_record(&task.record);
        let event = (task.notify.is_some() && !closed).then(|| TaskEnd {
            task: record.clone(),
            kind: task.notify.unwrap_or("task_done"),
            owner_lane,
            root_id: task.reservation.as_ref().map(|r| r.root_id().to_string()),
            awaited: task.waiters > 0,
            fetch: fetch_hint(&task.definition, id),
            cost: task.cost,
        });
        let (cost, model) = (task.cost, task.model.clone());
        drop(state);
        self.inner.changed.notify_all();
        if let Some(hook) = &self.inner.on_task_end {
            hook(&record, cost, model.as_deref());
        }
        if let Some(event) = event {
            let _ = lock(&self.inner.events).send(event);
        }
        outcome
    }

    /// `cancel`: the task (when it has not ended) and every unfinished descendant; siblings keep running.
    fn cancel(&self, id: &str, reason: &str) -> Result<Value, String> {
        let begun = {
            let mut state = self.state();
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            if task.outcome.is_some() || task.finishing {
                None
            } else {
                // Only the first party to end a task aborts its work.
                task.finishing = true;
                task.signal.abort(reason);
                Some(task.lane.clone())
            }
        };
        let own = match begun {
            Some(lane) => {
                if let Some(lane) = lane {
                    self.abort_lane(&lane);
                }
                self.commit(id, cancelled(reason))
            }
            None => self.wait_outcome(id, None),
        };
        let children: Vec<String> = self
            .state()
            .tasks
            .iter()
            .filter(|t| t.parent_id.as_deref() == Some(id) && t.outcome.is_none())
            .map(|t| t.id.clone())
            .collect();
        let cascade = format!("Ancestor {id} cancelled: {reason}");
        let threads: Vec<_> = children
            .into_iter()
            .map(|child| {
                let (host, cascade) = (self.clone(), cascade.clone());
                std::thread::spawn(move || {
                    let _ = host.cancel(&child, &cascade);
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
        own
    }

    /// Waits for a task's terminal result. `cell`: the requesting cell; when it is cancelled the wait ends with an
    /// error (`cancel_task`: and the task is cancelled first, as an invoke's own task is).
    fn wait_outcome(&self, id: &str, cell: Option<(&Signal, bool)>) -> Result<Value, String> {
        let mut state = self.state();
        if let Some(task) = state.task_mut(id) {
            task.waiters += 1;
        }
        let mut cancelled_task = false;
        let outcome = loop {
            let task = state.task(id).ok_or("Unknown Ultron task")?;
            if let Some(outcome) = &task.outcome {
                break outcome.clone();
            }
            if !task.live && !task.finishing {
                break task
                    .record
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "Unknown Ultron task".into());
            }
            if let Some((signal, cancel_task)) = cell
                && let Some(reason) = signal.reason()
            {
                if !cancel_task {
                    break Err(format!("Waiting for {id} was cancelled: {reason}"));
                }
                if !cancelled_task {
                    cancelled_task = true;
                    drop(state);
                    let _ = self.cancel(id, "Parent task aborted");
                    state = self.state();
                    continue;
                }
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        if let Some(task) = state.task_mut(id) {
            task.waiters -= 1;
        }
        outcome
    }

    /// Whether `id` is `caller`'s own task or a descendant (the root lane sees everything).
    fn visible(state: &State, records: &[Value], id: &str, caller: Option<&str>) -> bool {
        let Some(caller) = caller else {
            return true;
        };
        let mut current = records.iter().find(|r| r["id"] == id);
        while let Some(record) = current {
            if record["id"] == caller {
                return true;
            }
            current = record["parentId"]
                .as_str()
                .and_then(|parent| records.iter().find(|r| r["id"] == parent));
        }
        let _ = state;
        false
    }

    /// Closes the host: later requests are refused and every unfinished task is cancelled.
    pub fn close(&self) {
        let _admitting = lock(&self.inner.admissions);
        let unfinished: Vec<String> = {
            let mut state = self.state();
            state.closed = true;
            state
                .tasks
                .iter()
                .filter(|t| t.outcome.is_none())
                .map(|t| t.id.clone())
                .collect()
        };
        for id in unfinished {
            let _ = self.cancel(&id, "Ultron task host closed");
        }
    }

    /// The kernel host handler of `lane`: this host's requests, everything else to `inner`.
    pub fn handler(&self, lane: &str, inner: Arc<dyn HostHandler>) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        move |kind: &str, payload: Map<String, Value>, signal: &Signal| {
            if Agents::serves(kind) || (kind == "rlm.finish" && host.owns_lane(&lane)) {
                host.handle(kind, Value::Object(payload), &lane, Some(signal))
            } else {
                inner.handle(kind, payload, signal)
            }
        }
    }

    /// The request types this host answers for any lane (`rlm.finish` only for its own lanes).
    pub fn serves(kind: &str) -> bool {
        kind == "ping" || kind == "workflows.run" || kind.starts_with("agents.") || kind.starts_with("background.")
    }

    /// Serve one host request from `lane`; `cell` is the requesting cell's signal.
    pub fn handle(&self, kind: &str, payload: Value, lane: &str, cell: Option<&Signal>) -> Result<Value, String> {
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        self.load()?;
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        let parent = self.caller_task(lane);
        let parent = parent.as_deref();
        let records = self.state().journal.list()?;
        let visible = |id: &str| Self::visible(&self.state(), &records, id, parent);
        if ["ping", "agents.list", "agents.tasks"].contains(&kind) {
            fields(&payload, &[])?;
        }
        if kind == "agents.status" {
            fields(&payload, &["graph"])?;
        }
        match kind {
            "ping" => Ok(json!({ "ok": true })),
            "agents.list" => Ok(Value::from(self.state().registry.list())),
            "agents.status" | "agents.tasks" => self.status(&payload, lane, parent, &records),
            "agents.register" => {
                fields(&payload, &["definition"])?;
                self.state()
                    .registry
                    .register(payload.get("definition").unwrap_or(&Value::Null))
            }
            "background.start" => {
                fields(&payload, &["prompt", "model", "key", "timeout_ms"])?;
                let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
                if kernel_apis::util::utf16_len(&prompt) > 65_536 {
                    return Err("Background prompt exceeds 65536 characters".into());
                }
                let key = payload
                    .get("key")
                    .and_then(Value::as_str)
                    .map(|k| format!("background:{k}"));
                if key.as_ref().is_some_and(|k| kernel_apis::util::utf16_len(k) > 264) {
                    return Err("Background key is too long".into());
                }
                let request = TaskRequest {
                    definition: "background-job@1".into(),
                    input: json!({ "prompt": prompt }),
                    model: payload.get("model").and_then(Value::as_str).map(String::from),
                    key,
                    timeout_ms: payload
                        .get("timeout_ms")
                        .and_then(Value::as_f64)
                        .map_or(DEFAULT_TIMEOUT_MS, |t| t as u64),
                    ..TaskRequest::default()
                };
                let id = self.spawn_task(request, cell, parent, Some("task_done"))?;
                Ok(public_task(&self.record(&id)))
            }
            "background.list" => {
                fields(&payload, &[])?;
                Ok(Value::from(
                    records
                        .iter()
                        .filter(|r| r["definition"] == "background-job@1")
                        .filter(|r| visible(r["id"].as_str().unwrap_or_default()))
                        .map(public_task)
                        .collect::<Vec<_>>(),
                ))
            }
            "background.inspect" | "background.result" | "background.stop" => {
                fields(&payload, &["id"])?;
                let id = nonempty_string(payload.get("id"), "id")?;
                let stored = records
                    .iter()
                    .find(|r| r["id"] == id.as_str() && r["definition"] == "background-job@1");
                let Some(stored) = stored.filter(|_| visible(&id)) else {
                    return Err("Unknown background job".into());
                };
                match kind {
                    "background.inspect" => Ok(public_task(&self.record(&id))),
                    "background.stop" => {
                        let result = self.cancel(&id, "Background job stopped")?;
                        Ok(json!({ "cancelled": result["status"] == "cancelled" }))
                    }
                    _ => match self.is_live(&id) {
                        true => self.wait_outcome(&id, cell.map(|c| (c, false))),
                        false => Ok(stored.get("result").cloned().unwrap_or(Value::Null)),
                    },
                }
            }
            "agents.spawn" | "agents.invoke" => {
                let spawn = kind == "agents.spawn";
                let request = self.request(&payload, parent)?;
                let id = self.spawn_task(request, cell, parent, spawn.then_some("task_done"))?;
                if spawn {
                    let record = self.record(&id);
                    return Ok(json!({ "id": id, "state": record["state"] }));
                }
                self.result_of(&id, cell.map(|c| (c, true)))
            }
            "agents.inspect" | "agents.result" | "agents.cancel" => {
                fields(&payload, &["id"])?;
                let id = nonempty_string(payload.get("id"), "id")?;
                let stored = records.iter().find(|r| r["id"] == id.as_str());
                let Some(stored) = stored.filter(|_| visible(&id)) else {
                    return Err("Unknown Ultron task".into());
                };
                match kind {
                    "agents.inspect" => Ok(public_task(stored)),
                    "agents.cancel" => {
                        // Stopping a task stops its subtree, even when the task itself already finished.
                        let result = self.cancel(&id, "Ultron task cancelled")?;
                        Ok(json!({ "cancelled": result["status"] == "cancelled" }))
                    }
                    _ => match self.is_live(&id) {
                        true => self.wait_outcome(&id, cell.map(|c| (c, false))),
                        false => Ok(stored.get("result").cloned().unwrap_or(Value::Null)),
                    },
                }
            }
            "rlm.finish" => self.finish_request(payload, parent),
            "workflows.run" => {
                let nodes = self.workflow(&payload)?;
                self.run_workflow(nodes, cell, parent)
            }
            other => Err(format!("Ultron RLM host request is not wired: {other}")),
        }
    }

    /// Admit an `rlm-frame@1` task for `lane` (as `agents.spawn` with this payload, under the lane's task) without
    /// a completion notice: inference waits for its result itself. Returns the task id.
    pub fn spawn_frame(&self, lane: &str, payload: Value) -> Result<String, String> {
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        self.load()?;
        let parent = self.caller_task(lane);
        let request = self.request(&payload, parent.as_deref())?;
        self.spawn_task(request, None, parent.as_deref(), None)
    }

    /// Cancel task `id` and its subtree with `reason` (inference cancels its frame tasks with the calling cell).
    pub fn cancel_task(&self, id: &str, reason: &str) -> Result<Value, String> {
        self.cancel(id, reason)
    }

    fn is_live(&self, id: &str) -> bool {
        self.state().task(id).is_some_and(|t| t.live)
    }

    fn record(&self, id: &str) -> Value {
        self.state().task(id).map(|t| t.record.clone()).unwrap_or(Value::Null)
    }

    /// The result of a task this request admitted (or found by its key): awaited when live, else as stored.
    fn result_of(&self, id: &str, cell: Option<(&Signal, bool)>) -> Result<Value, String> {
        if self.is_live(id) {
            return self.wait_outcome(id, cell);
        }
        Ok(self.record(id).get("result").cloned().unwrap_or(Value::Null))
    }

    fn status(
        &self,
        payload: &Map<String, Value>,
        lane: &str,
        parent: Option<&str>,
        records: &[Value],
    ) -> Result<Value, String> {
        let root = {
            let state = self.state();
            self.status_root(&state, parent)
        };
        let usage = match &self.inner.usage {
            Some(ledger) => Some(ledger.status(root.as_deref())?),
            None => None,
        };
        let state = self.state();
        let shown: Vec<&Value> = records
            .iter()
            .filter(|r| Self::visible(&state, records, r["id"].as_str().unwrap_or_default(), parent))
            .collect();
        let graph = payload.get("graph") == Some(&Value::Bool(true));
        let listed = if graph && shown.len() > GRAPH_TASK_LIMIT {
            &shown[shown.len() - GRAPH_TASK_LIMIT..]
        } else {
            &shown[..]
        };
        let tasks: Vec<Value> = listed
            .iter()
            .map(|record| {
                if graph {
                    let id = record["id"].as_str().unwrap_or_default();
                    self.graph_task(state.task(id), record)
                } else {
                    public_task(record)
                }
            })
            .collect();
        let mut out = Map::new();
        out.insert("definitions".into(), Value::from(state.registry.list()));
        out.insert("tasks".into(), Value::from(tasks));
        if graph && shown.len() > listed.len() {
            out.insert("truncatedTasks".into(), json!(shown.len() - listed.len()));
        }
        if graph {
            let workflows: Vec<Value> = state
                .workflow_runs
                .iter()
                .filter(|run| match run["parentId"].as_str() {
                    None => parent.is_none(),
                    Some(owner) => Self::visible(&state, records, owner, parent),
                })
                .map(graph_workflow)
                .collect();
            out.insert("workflows".into(), Value::from(workflows));
        }
        out.insert("usage".into(), usage.clone().unwrap_or(Value::Null));
        out.insert(
            "limits".into(),
            usage.as_ref().map_or(Value::Null, |u| u["limits"].clone()),
        );
        out.insert(
            "spend".into(),
            match &usage {
                None => Value::Null,
                Some(u) => json!({
                    "root": u["rootId"], "turns": u["turns"]["turns"], "tokens": u["turns"]["tokens"],
                    "costUsd": u["cost"]["spentUsd"], "maxTotalTurns": u["limits"]["maxTotalTurns"],
                    "maxTotalTokens": u["limits"]["maxTotalTokens"], "maxCostUsd": u["limits"]["maxCostUsd"],
                }),
            },
        );
        drop(state);
        if let Some(extras) = &self.inner.status_extras {
            out.extend(extras(lane));
        }
        out.insert(
            "controls".into(),
            Value::Object(CONTROLS.iter().map(|k| (k.to_string(), json!(false))).collect()),
        );
        Ok(Value::Object(out))
    }

    /// `graphTask`: the bounded graph view of a task.
    fn graph_task(&self, task: Option<&Task>, record: &Value) -> Value {
        let mut out = public_task(record);
        if let Some(map) = out.as_object_mut() {
            map.shift_remove("result");
        }
        let id = record["id"].as_str().unwrap_or_default();
        let definition = record["definition"].as_str().unwrap_or_default();
        if let Some(task) = task {
            let mut put = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    out[key] = value;
                }
            };
            put("lane", task.lane.clone().map(Value::from));
            put("startedAt", task.started_at.map(number));
            put("endedAt", task.ended_at.map(number));
            put("cost", task.cost.map(number));
            put("tokens", task.tokens.map(number));
            put("input", task.input_preview.clone().map(Value::from));
            put("workflow", task.workflow.clone());
            put("model", task.model.clone().map(Value::from));
            put("turns", task.turns.map(Value::from));
            put("toolCallCount", task.tool_calls.map(Value::from));
            put("lastText", task.last_text.clone().map(Value::from));
        }
        if let Some(result) = record.get("result") {
            let mut view = json!({ "status": result["status"] });
            if let Some(error) = result.get("error") {
                view["error"] = json!(preview(error, GRAPH_PREVIEW_CHARS));
            }
            if let Some(value) = result.get("value") {
                view["preview"] = json!(preview(value, GRAPH_PREVIEW_CHARS));
            }
            out["result"] = view;
        }
        out["fetch"] = json!(fetch_hint(definition, id));
        out
    }

    /// `rlm.finish` from a subagent lane this host started (`agents.spawn` of `rlm-child@1`).
    fn finish_request(&self, payload: Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        let mut state = self.state();
        let Some(task) = parent.and_then(|id| state.task_mut(id)) else {
            return Err("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply".into());
        };
        if task.definition != "rlm-child@1" {
            return Err("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply".into());
        }
        if task.outcome.is_some() || task.finishing {
            return Err("rlm.finish: this subagent has already ended".into());
        }
        if task.verdict_invalid {
            return Err("rlm.finish: no attempts left; end your turn, your reply is returned unverified".into());
        }
        match validate_verdict(&payload, None) {
            Ok(verdict) => {
                let status = verdict["status"].clone();
                task.verdict = Some(verdict);
                Ok(json!({ "recorded": true, "status": status,
                    "next": "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran." }))
            }
            Err(problems) => {
                task.verdict_rejections += 1;
                task.verdict_problems = problems.clone();
                let left = (MAX_VERDICT_REJECTIONS + 1).saturating_sub(task.verdict_rejections);
                let listed = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
                if left > 0 || task.verdict.is_some() {
                    let tail = if task.verdict.is_none() {
                        format!(" ({left} attempt{} left)", if left == 1 { "" } else { "s" })
                    } else {
                        "; your earlier verdict stands until then".into()
                    };
                    return Err(format!(
                        "rlm.finish rejected:\n{listed}\nFix these and call rlm.finish again{tail}."
                    ));
                }
                task.verdict_invalid = true;
                Err(format!(
                    "rlm.finish rejected:\n{listed}\nNo attempts left: end your turn; your reply is returned unverified, with these problems."
                ))
            }
        }
    }

    /// `workflow(payload)`: the whole graph is validated before any effect.
    fn workflow(&self, payload: &Map<String, Value>) -> Result<Vec<Node>, String> {
        fields(payload, &["nodes", "key"])?;
        let items = payload
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or("nodes must be an array")?;
        let workflow_key = match payload.get("key") {
            None => None,
            value => Some(nonempty_string(value, "Workflow key")?),
        };
        let condition = |value: &Map<String, Value>, name: &str| -> Result<(Option<String>, Value), String> {
            let equals = value
                .get("equals")
                .ok_or_else(|| format!("{name}.equals must be a JSON value"))?;
            let field = match value.get("field") {
                None => None,
                field => Some(nonempty_string(field, &format!("{name}.field"))?),
            };
            Ok((field, equals.clone()))
        };
        let mut nodes = Vec::new();
        for value in items {
            let node = object_input(Some(value))?;
            fields(
                node,
                &[
                    "id",
                    "definition",
                    "input",
                    "model",
                    "key",
                    "timeout_ms",
                    "dependsOn",
                    "inputFrom",
                    "when",
                    "join",
                    "revise",
                    "worktree",
                    "brief",
                ],
            )?;
            let id = nonempty_string(node.get("id"), "Workflow node ID")?;
            let definition = definition_key(node.get("definition"))?;
            match node.get("worktree") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => {}
                Some(Value::Bool(true)) => {
                    if definition != "rlm-child@1" {
                        return Err(format!(
                            "Workflow node {id}: worktree applies only to rlm-child@1 nodes (subagents)"
                        ));
                    }
                    return Err(format!(
                        "Workflow node {id}: worktree nodes are not available in this host; spawn the subagent with rlm.spawn(worktree=True)"
                    ));
                }
                Some(Value::String(s)) if s == "auto" => {
                    if definition != "rlm-child@1" {
                        return Err(format!(
                            "Workflow node {id}: worktree applies only to rlm-child@1 nodes (subagents)"
                        ));
                    }
                    return Err(format!(
                        "Workflow node {id}: worktree nodes are not available in this host; spawn the subagent with rlm.spawn(worktree=True)"
                    ));
                }
                Some(_) => {
                    return Err(format!(
                        "Workflow node {id} worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
                    ));
                }
            }
            let item = self.definition(&definition)?;
            // A node that could never execute is a validation error, not a runtime failure after effects.
            self.state().registry.can_execute(&item)?;
            let depends_on: Vec<String> = match node.get("dependsOn") {
                None => Vec::new(),
                Some(Value::Array(list))
                    if list.iter().all(|d| d.as_str().is_some_and(|d| !js_trim(d).is_empty()))
                        && list.iter().enumerate().all(|(i, d)| !list[..i].contains(d)) =>
                {
                    list.iter().filter_map(text_of).collect()
                }
                Some(_) => return Err("dependsOn must be an array of unique nonempty strings".into()),
            };
            let input_from = match node.get("inputFrom") {
                None => None,
                Some(Value::Array(list)) => {
                    let sources = list
                        .iter()
                        .map(|s| nonempty_string(Some(s), "inputFrom entry"))
                        .collect::<Result<Vec<_>, _>>()?;
                    if sources.is_empty() || sources.iter().enumerate().any(|(i, s)| sources[..i].contains(s)) {
                        return Err("inputFrom must be a nonempty array of unique dependencies".into());
                    }
                    Some(Err(sources))
                }
                value => Some(Ok(nonempty_string(value, "inputFrom")?)),
            };
            let join_any = match node.get("join") {
                None => false,
                Some(Value::String(join)) if join == "all" => false,
                Some(Value::String(join)) if join == "any" => true,
                Some(_) => return Err("join must be \"all\" or \"any\"".into()),
            };
            if join_any {
                if depends_on.len() < 2 {
                    return Err("join \"any\" requires at least two dependencies".into());
                }
                if matches!(input_from, Some(Ok(_))) {
                    return Err(
                        "join \"any\" requires inputFrom to be an array (fan-in of succeeded dependencies)".into(),
                    );
                }
            }
            let when = match node.get("when") {
                None => None,
                Some(value) => {
                    let route = object_input(Some(value))?;
                    fields(route, &["node", "field", "equals"])?;
                    let target = nonempty_string(route.get("node"), "when.node")?;
                    if !depends_on.contains(&target) {
                        return Err("when.node must name a dependency".into());
                    }
                    let (field, equals) = condition(route, "when")?;
                    Some((target, field, equals))
                }
            };
            let revise = match node.get("revise") {
                None => None,
                Some(value) => {
                    let lp = object_input(Some(value))?;
                    fields(lp, &["from", "until", "max_rounds"])?;
                    let rounds = lp
                        .get("max_rounds")
                        .and_then(Value::as_f64)
                        .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= WORKFLOW_MAX_ROUNDS as f64)
                        .ok_or_else(|| {
                            format!("revise.max_rounds must be an integer between 1 and {WORKFLOW_MAX_ROUNDS}")
                        })?;
                    let Some(until) = lp.get("until") else {
                        return Err("revise.until is required".into());
                    };
                    let until = object_input(Some(until))?;
                    fields(until, &["field", "equals"])?;
                    let from = nonempty_string(lp.get("from"), "revise.from")?;
                    let (field, equals) = condition(until, "revise.until")?;
                    Some((from, field, equals, rounds as u64))
                }
            };
            match &input_from {
                Some(sources) => {
                    let list = match sources {
                        Ok(one) => vec![one.clone()],
                        Err(many) => many.clone(),
                    };
                    if list.iter().any(|s| !depends_on.contains(s)) {
                        return Err("inputFrom must name a dependency".into());
                    }
                    if node.contains_key("input") {
                        return Err("Specify input or inputFrom, not both".into());
                    }
                }
                None => {
                    let Some(input) = node.get("input") else {
                        return Err("Agent input is not JSON".into());
                    };
                    let state = self.state();
                    if !state.registry.is_valid_input(&item, input) {
                        return Err(state.registry.validation_error(&item, input, "input"));
                    }
                }
            }
            let (model, key, timeout_ms) = task_options(node)?;
            let brief = match node.get("brief") {
                None | Some(Value::Null) => None,
                Some(brief) => {
                    Brief::parse(brief, None).map_err(|error| format!("Workflow node {id}: {error}"))?;
                    Some(brief.clone())
                }
            };
            nodes.push(Node {
                brief,
                key: key.or_else(|| workflow_key.as_ref().map(|k| format!("{k}:{id}"))),
                id,
                definition,
                input: node.get("input").cloned(),
                depends_on,
                input_from,
                when,
                join_any,
                revise,
                model,
                timeout_ms,
            });
        }
        for (i, node) in nodes.iter().enumerate() {
            if nodes[..i].iter().any(|other| other.id == node.id) {
                return Err("Duplicate workflow node ID".into());
            }
        }
        for node in &nodes {
            if node.depends_on.iter().any(|d| !nodes.iter().any(|n| &n.id == d)) {
                return Err("Unknown workflow dependency".into());
            }
        }
        let mut pending: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        while !pending.is_empty() {
            let ready: Vec<&str> = nodes
                .iter()
                .filter(|n| {
                    pending.contains(&n.id.as_str()) && n.depends_on.iter().all(|d| !pending.contains(&d.as_str()))
                })
                .map(|n| n.id.as_str())
                .collect();
            if ready.is_empty() {
                return Err("Workflow contains a cycle".into());
            }
            pending.retain(|id| !ready.contains(id));
        }
        let ancestors = |id: &str| {
            let mut seen: Vec<String> = Vec::new();
            let mut stack = vec![id.to_string()];
            while let Some(current) = stack.pop() {
                for dependency in nodes
                    .iter()
                    .find(|n| n.id == current)
                    .map_or(&[][..], |n| &n.depends_on[..])
                {
                    if !seen.contains(dependency) {
                        seen.push(dependency.clone());
                        stack.push(dependency.clone());
                    }
                }
            }
            seen
        };
        let mut reviewers: Vec<String> = Vec::new();
        for node in &nodes {
            let Some((from, ..)) = &node.revise else { continue };
            let reviewer = nodes
                .iter()
                .find(|n| &n.id == from)
                .ok_or("revise.from must name a workflow node")?;
            if !reviewer.depends_on.contains(&node.id) {
                return Err("revise.from must depend on the revised node".into());
            }
            if reviewer.revise.is_some() {
                return Err("A revision reviewer cannot itself be revised".into());
            }
            if reviewer.when.is_some() {
                return Err("A revision reviewer cannot have a when route".into());
            }
            if reviewers.contains(&reviewer.id) {
                return Err("A node can review only one revision loop".into());
            }
            reviewers.push(reviewer.id.clone());
            let before = ancestors(&node.id);
            if reviewer.depends_on.iter().any(|d| d != &node.id && !before.contains(d)) {
                return Err("A revision reviewer may depend only on the revised node and its ancestors".into());
            }
        }
        Ok(nodes)
    }

    /// `workflowInput`: the input a ready node runs with, or the explicit outcome that replaces running it.
    fn workflow_input(node: &Node, output: &Map<String, Value>) -> Result<Value, Value> {
        let status = |d: &str| output[d]["status"].as_str().unwrap_or_default().to_string();
        if !node.join_any {
            if let Some(unmet) = node.depends_on.iter().find(|d| status(d) != "succeeded") {
                return Err(json!({ "status": "skipped",
                    "reason": format!("Dependency {unmet} did not succeed ({})", status(unmet)) }));
            }
        } else if !node.depends_on.iter().any(|d| status(d) == "succeeded") {
            let listed: Vec<String> = node.depends_on.iter().map(|d| format!("{d}: {}", status(d))).collect();
            return Err(json!({ "status": "skipped",
                "reason": format!("No dependency succeeded ({})", listed.join(", ")) }));
        }
        if let Some((target, field, equals)) = &node.when {
            let label = match field {
                None => target.clone(),
                Some(field) => format!("{target}.{field}"),
            };
            if status(target) != "succeeded" {
                return Err(json!({ "status": "skipped",
                    "reason": format!("Route dependency {target} did not succeed ({})", status(target)) }));
            }
            if !condition_met(bound_value(output.get(target)).as_ref(), field.as_deref(), equals) {
                return Err(json!({ "status": "skipped", "reason": format!("Route condition on {label} not met") }));
            }
        }
        Ok(match &node.input_from {
            None => node.input.clone().unwrap_or(Value::Null),
            Some(Ok(source)) => bound_value(output.get(source)).unwrap_or(Value::Null),
            Some(Err(sources)) => Value::Object(
                sources
                    .iter()
                    .filter(|s| status(s) == "succeeded")
                    .map(|s| (s.clone(), bound_value(output.get(s)).unwrap_or(Value::Null)))
                    .collect(),
            ),
        })
    }

    /// `workflowTask`: validates a bound input, admits the node's task, and waits for its durable result.
    #[allow(clippy::too_many_arguments)]
    fn workflow_task(
        &self,
        node: &Node,
        input: Value,
        key: Option<String>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        gate: bool,
        admitted: Option<&Sender<()>>,
    ) -> Value {
        // Tells the workflow its admission was attempted, so the next ready node is admitted after it (TS order).
        let signal_admitted = || {
            if let Some(sender) = admitted {
                let _ = sender.send(());
            }
        };
        let definition = match self.definition(&node.definition) {
            Ok(definition) => definition,
            Err(error) => {
                signal_admitted();
                return failed(format!("Admission refused: {error}"));
            }
        };
        {
            let state = self.state();
            if !state.registry.is_valid_input(&definition, &input) {
                signal_admitted();
                return failed(format!(
                    "Bound input rejected: {}",
                    state.registry.validation_error(&definition, &input, "input")
                ));
            }
        }
        let brief = match &node.brief {
            None => None,
            Some(brief) => match Brief::parse(brief, Some(self.hop_below(parent))) {
                Ok(brief) => Some(brief),
                Err(error) => {
                    signal_admitted();
                    return failed(format!("Admission refused: {error}"));
                }
            },
        };
        let request = TaskRequest {
            definition: node.definition.clone(),
            input,
            model: node.model.clone(),
            key,
            timeout_ms: node.timeout_ms,
            workflow: Some(json!({ "run": run, "node": node.id, "dependsOn": node.depends_on,
                                   "join": if node.join_any { "any" } else { "all" } })),
            brief,
            ..TaskRequest::default()
        };
        let spawned = self.spawn_task(request, cell, parent, None);
        signal_admitted();
        let id = match spawned {
            Ok(id) => id,
            Err(error) => return failed(format!("Admission refused: {error}")),
        };
        let result = match self.result_of(&id, cell.map(|c| (c, true))) {
            Ok(result) => result,
            Err(error) => failed(error),
        };
        if gate { verdict_gate(result) } else { result }
    }

    /// `workflowLoop`: the revised node, then its reviewer, until `until` holds or the rounds run out.
    #[allow(clippy::too_many_arguments)]
    fn workflow_loop(
        &self,
        work: &Node,
        reviewer: &Node,
        first: Value,
        output: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        admitted: Option<&Sender<()>>,
    ) -> (Value, Value) {
        let Some((_, until_field, until_equals, max_rounds)) = work.revise.clone() else {
            return (failed("not a revision loop"), failed("not a revision loop"));
        };
        let round_key = |node: &Node, round: u64| node.key.as_ref().map(|k| format!("{k}:round-{round}"));
        let mut rounds: Vec<Value> = Vec::new();
        let finish = |outcome: &str, mut work_result: Value, review: Value, rounds: &[Value]| {
            work_result["revision"] = json!({ "outcome": outcome, "rounds": rounds.len(), "max_rounds": max_rounds });
            work_result["rounds"] = Value::from(rounds.to_vec());
            (work_result, review)
        };
        let mut input = first.clone();
        let mut round = 1;
        loop {
            let first_round = if round == 1 { admitted } else { None };
            let work_result = self.workflow_task(
                work,
                input.clone(),
                round_key(work, round),
                cell,
                parent,
                run,
                true,
                first_round,
            );
            if work_result["status"] != "succeeded" {
                let skipped = json!({ "status": "skipped",
                    "reason": format!("Dependency {} did not succeed ({})", work.id, work_result["status"].as_str().unwrap_or_default()) });
                rounds.push(json!({ "round": round, "work": work_result }));
                return finish("failed", work_result, skipped, &rounds);
            }
            let mut view = output.clone();
            view.insert(work.id.clone(), work_result.clone());
            let review = match Self::workflow_input(reviewer, &view) {
                Err(outcome) => outcome,
                Ok(review_input) => self.workflow_task(
                    reviewer,
                    review_input,
                    round_key(reviewer, round),
                    cell,
                    parent,
                    run,
                    false,
                    None,
                ),
            };
            rounds.push(json!({ "round": round, "work": work_result, "review": review }));
            if review["status"] != "succeeded" {
                let mut failure = json!({ "status": "failed" });
                if let Some(value) = work_result.get("value") {
                    failure["value"] = value.clone();
                }
                failure["error"] = json!(format!(
                    "Reviewer {} did not succeed in round {round} ({})",
                    reviewer.id,
                    review["status"].as_str().unwrap_or_default()
                ));
                failure["verification"] = json!("unverified");
                return finish("failed", failure, review, &rounds);
            }
            if condition_met(
                bound_value(Some(&review)).as_ref(),
                until_field.as_deref(),
                &until_equals,
            ) {
                return finish("converged", work_result, review, &rounds);
            }
            if round == max_rounds {
                let mut exhausted = json!({ "status": "exhausted" });
                if let Some(value) = work_result.get("value") {
                    exhausted["value"] = value.clone();
                }
                exhausted["verification"] = json!("unverified");
                return finish("exhausted", exhausted, review, &rounds);
            }
            input = json!({ "input": first, "previous": bound_value(Some(&work_result)).unwrap_or(Value::Null),
                            "review": bound_value(Some(&review)).unwrap_or(Value::Null), "round": round + 1 });
            round += 1;
        }
    }

    /// `runWorkflow`: ready nodes run together; a dependency is satisfied only by its durable terminal result.
    fn run_workflow(&self, nodes: Vec<Node>, cell: Option<&Signal>, parent: Option<&str>) -> Result<Value, String> {
        let run = format!("wf-{}", &uuid_v4()[..8]);
        {
            let mut state = self.state();
            let mut record = json!({ "run": run });
            if let Some(parent) = parent {
                record["parentId"] = json!(parent);
            }
            record["startedAt"] = number(self.now());
            record["nodes"] = Value::from(
                nodes
                    .iter()
                    .map(|n| {
                        json!({ "id": n.id, "definition": n.definition, "dependsOn": n.depends_on,
                                     "join": if n.join_any { "any" } else { "all" } })
                    })
                    .collect::<Vec<_>>(),
            );
            record["ended"] = json!({});
            state.workflow_runs.push(record);
            let excess = state.workflow_runs.len().saturating_sub(GRAPH_WORKFLOW_LIMIT);
            state.workflow_runs.drain(..excess);
        }
        let reviewers: Vec<String> = nodes
            .iter()
            .filter_map(|n| n.revise.as_ref().map(|(from, ..)| from.clone()))
            .collect();
        let mut output = Map::new();
        while output.len() < nodes.len() {
            let ready: Vec<&Node> = nodes
                .iter()
                .filter(|n| {
                    !output.contains_key(&n.id)
                        && !reviewers.contains(&n.id)
                        && n.depends_on.iter().all(|d| output.contains_key(d))
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            let snapshot = output.clone();
            let results: Vec<Vec<(String, Value)>> = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for node in &ready {
                    let (snapshot, run, nodes) = (&snapshot, &run, &nodes);
                    let (admitted, wait) = channel::<()>();
                    handles.push(scope.spawn(move || {
                        let reviewer = node
                            .revise
                            .as_ref()
                            .and_then(|(from, ..)| nodes.iter().find(|n| &n.id == from));
                        match Self::workflow_input(node, snapshot) {
                            Err(outcome) => {
                                let mut out = vec![(node.id.clone(), outcome.clone())];
                                if let Some(reviewer) = reviewer {
                                    out.push((
                                        reviewer.id.clone(),
                                        json!({ "status": "skipped",
                                            "reason": format!("Dependency {} did not succeed ({})", node.id,
                                                outcome["status"].as_str().unwrap_or_default()) }),
                                    ));
                                }
                                out
                            }
                            Ok(input) => match reviewer {
                                Some(reviewer) => {
                                    let (work, review) = self.workflow_loop(
                                        node,
                                        reviewer,
                                        input,
                                        snapshot,
                                        cell,
                                        parent,
                                        run,
                                        Some(&admitted),
                                    );
                                    vec![(node.id.clone(), work), (reviewer.id.clone(), review)]
                                }
                                None => vec![(
                                    node.id.clone(),
                                    self.workflow_task(
                                        node,
                                        input,
                                        node.key.clone(),
                                        cell,
                                        parent,
                                        run,
                                        true,
                                        Some(&admitted),
                                    ),
                                )],
                            },
                        }
                    }));
                    // Admission in ready order: the next node waits until this one's admission was attempted.
                    let _ = wait.recv();
                }
                handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
            });
            let mut state = self.state();
            for (id, result) in results.into_iter().flatten() {
                let reason = result
                    .get("reason")
                    .and_then(Value::as_str)
                    .or_else(|| result.get("error").and_then(Value::as_str));
                if let Some(record) = state.workflow_runs.iter_mut().find(|r| r["run"] == run.as_str()) {
                    let mut ended = json!({ "status": result["status"] });
                    if let Some(reason) = reason {
                        ended["reason"] = json!(preview(&json!(reason), GRAPH_TEXT_CHARS));
                    }
                    record["ended"][&id] = ended;
                }
                output.insert(id, result);
            }
        }
        let mut state = self.state();
        let now = self.now();
        if let Some(record) = state.workflow_runs.iter_mut().find(|r| r["run"] == run.as_str()) {
            record["endedAt"] = number(now);
        }
        Ok(Value::Object(output))
    }
}

/// `graphWorkflow`: a run's planned nodes (bounded) and how the finished ones ended.
fn graph_workflow(record: &Value) -> Value {
    let nodes = record["nodes"].as_array().cloned().unwrap_or_default();
    let shown: Vec<Value> = nodes
        .iter()
        .take(GRAPH_WORKFLOW_NODE_LIMIT)
        .map(|node| {
            let mut node = node.clone();
            if let Some(ended) = record["ended"]
                .get(node["id"].as_str().unwrap_or_default())
                .and_then(Value::as_object)
            {
                for (key, value) in ended {
                    node[key] = value.clone();
                }
            }
            node
        })
        .collect();
    let mut out = json!({ "run": record["run"] });
    if let Some(parent) = record.get("parentId") {
        out["parentId"] = parent.clone();
    }
    out["startedAt"] = record["startedAt"].clone();
    if let Some(ended) = record.get("endedAt") {
        out["endedAt"] = ended.clone();
    }
    out["nodes"] = Value::from(shown);
    if nodes.len() > GRAPH_WORKFLOW_NODE_LIMIT {
        out["truncatedNodes"] = json!(nodes.len() - GRAPH_WORKFLOW_NODE_LIMIT);
    }
    out
}

impl kernel_apis::TaskHost for Agents {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        self.caller_task(lane)
    }

    fn task_lane(&self, task_id: &str) -> Option<String> {
        self.state().task(task_id).and_then(|t| t.lane.clone())
    }

    fn strategy(&self, definition: &str) -> Result<String, String> {
        self.load()?;
        let key = definition_key(Some(&json!(definition)))?;
        Ok(self.definition(&key)?.strategy().as_str().into())
    }

    fn tasks(&self) -> Result<Vec<Value>, String> {
        self.load()?;
        self.state().journal.list()
    }

    /// Module work (instance invocations, skill runs, schedule firings, verifiers) is detached: it is collected
    /// later or awaited explicitly, and stops with its parent task's subtree.
    fn spawn(&self, request: &Value, parent: Option<&str>, signal: &Signal) -> Result<Value, String> {
        self.load()?;
        let definition = definition_key(request.get("definition"))?;
        let item = self.definition(&definition)?;
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        {
            let state = self.state();
            if !state.registry.is_valid_input(&item, &input) {
                return Err(state.registry.validation_error(&item, &input, "input"));
            }
        }
        let task = TaskRequest {
            definition,
            input,
            model: request.get("model").and_then(Value::as_str).map(String::from),
            key: request.get("key").and_then(Value::as_str).map(String::from),
            timeout_ms: request
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
            lane: request.get("lane").and_then(Value::as_str).map(String::from),
            ..TaskRequest::default()
        };
        let _ = signal;
        let id = self.spawn_task(task, None, parent, None)?;
        Ok(public_record(&self.record(&id)))
    }

    fn result(&self, task_id: &str) -> Result<Value, String> {
        if self.state().task(task_id).is_none() {
            return Err("Unknown Ultron task".into());
        }
        self.result_of(task_id, None)
    }

    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
        if self.state().task(task_id).is_none() {
            return Err("Unknown Ultron task".into());
        }
        Agents::cancel(self, task_id, reason)
    }

    fn steer(&self, task_id: &str, message: &str, _signal: &Signal) -> Result<bool, String> {
        let lane = {
            let state = self.state();
            match state.task(task_id) {
                Some(task) if task.outcome.is_none() => task.lane.clone(),
                _ => None,
            }
        };
        Ok(match (lane, &self.inner.steer) {
            (Some(lane), Some(steer)) => steer(&lane, message),
            _ => false,
        })
    }

    fn usage(&self) -> Result<Option<Value>, String> {
        let root = {
            let state = self.state();
            self.status_root(&state, None)
        };
        match &self.inner.usage {
            Some(ledger) => ledger.status(root.as_deref()).map(Some),
            None => Ok(None),
        }
    }

    fn now(&self) -> i64 {
        Agents::now(self) as i64
    }

    fn pin_lane(&self, lane: &str, holder: &str) -> bool {
        self.inner.pin_lane.as_ref().is_some_and(|pin| pin(lane, holder))
    }

    fn unpin_lane(&self, lane: &str, holder: &str) {
        if let Some(unpin) = &self.inner.unpin_lane {
            unpin(lane, holder);
        }
    }
}

/// One predict model call: the system and user prompts of `createPredictAdapter`.
pub struct PredictCall<'a> {
    pub system: &'a str,
    pub user: String,
    pub signal: &'a Signal,
}

/// A finished model call: its text parts joined, its `stopReason` and `errorMessage`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Completion {
    pub text: String,
    pub stop_reason: String,
    pub error_message: Option<String>,
}

/// One model completion (`completeSimple`); an `Err` is a call that could not be made.
pub type Complete = Arc<dyn Fn(PredictCall<'_>) -> Result<Completion, String> + Send + Sync>;

pub const PREDICT_SYSTEM_PROMPT: &str = "You are a typed function. Reply with a single JSON value that satisfies the output schema. No prose, no code fences.";

/// `createPredictAdapter`: one model call per attempt with no tools and no kernel; the reply is parsed as JSON (a
/// fenced block unwrapped) and returned as text when it is not JSON, so the host's validation drives the repair.
pub fn predict_adapter(complete: Complete) -> Adapter {
    Arc::new(move |request: AdapterRequest<'_>| {
        let definition = request.definition;
        let repair = match &request.repair {
            None => String::new(),
            Some(repair) => format!(
                "\n\nYour previous answer was rejected (attempt {}): {}\nPrevious answer: {}",
                stringify(&repair["attempt"]),
                repair["error"].as_str().unwrap_or_default(),
                stringify(&repair["previous"])
            ),
        };
        let user = format!(
            "{}\n\nInput:\n{}\n\nOutput schema:\n{}\n\nOutput contract:\n{}{repair}",
            definition["instructions"].as_str().unwrap_or_default(),
            stringify(request.input),
            stringify(&definition["outputSchema"]),
            definition["outputDescription"].as_str().unwrap_or_default(),
        );
        let reply = complete(PredictCall {
            system: PREDICT_SYSTEM_PROMPT,
            user,
            signal: request.signal,
        })?;
        if reply.stop_reason == "error" || reply.stop_reason == "aborted" {
            return Err(reply
                .error_message
                .unwrap_or_else(|| format!("Predict call {}", reply.stop_reason)));
        }
        let text = reply.text;
        Ok(json_from(&text).unwrap_or_else(|_| {
            let trimmed = js_trim(&text);
            json!(strip_fence(trimmed))
        }))
    })
}

fn strip_fence(text: &str) -> &str {
    text.strip_prefix("```")
        .and_then(|rest| {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
            rest.strip_prefix('\n')?.strip_suffix("\n```")
        })
        .unwrap_or(text)
}
