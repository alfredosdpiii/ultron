//! Data model and defensive parsing for the live RLM views (experimental/rlm-visualizer.ts). The TUI polls read-only
//! inspections (`agents.status {graph: true}`, `instances.list`, `rlm.pool`, `rlm.frames`, `ctx.state`) and reads the
//! root turn's `rlm` cells from the lane transcript; this module turns them into an [`RlmSnapshot`].

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;
use tui::theme::Theme;

use super::{flat, js_slice};
use crate::components::{bold, fg};

/// A task's result (`agents.status`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmTaskResult {
    pub status: Option<String>,
    /// Present (possibly `null`) when the payload had a `value` key.
    pub value: Option<Value>,
    pub error: Option<String>,
    /// Bounded result preview (`agents.status {graph: true}`).
    pub preview: Option<String>,
}

/// A task's workflow node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmWorkflowMembership {
    pub run: String,
    pub node: String,
    pub depends_on: Vec<String>,
    pub join: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmTask {
    pub id: String,
    pub definition: String,
    pub state: String,
    pub parent_id: Option<String>,
    pub result: Option<RlmTaskResult>,
    pub lane: Option<String>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub cost: Option<f64>,
    pub tokens: Option<f64>,
    pub input: Option<String>,
    pub fetch: Option<String>,
    pub workflow: Option<RlmWorkflowMembership>,
    pub model: Option<String>,
    pub turns: Option<f64>,
    pub tool_call_count: Option<f64>,
    pub last_text: Option<String>,
    pub worktree: Option<String>,
}

/// One planned workflow node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmWorkflowNode {
    pub id: String,
    pub definition: String,
    pub depends_on: Vec<String>,
    pub join: String,
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// A `workflows.run` plan from `agents.status {graph: true}`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmWorkflowRun {
    pub run: String,
    pub parent_id: Option<String>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub nodes: Vec<RlmWorkflowNode>,
    pub truncated_nodes: Option<f64>,
}

/// One usage reservation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmReservation {
    pub task_id: Option<String>,
    pub admitted_at: Option<f64>,
}

/// The root's usage (`agents.status` `usage`, read leniently).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmUsage {
    pub root_id: Option<String>,
    pub admitted_tasks: Option<f64>,
    pub remaining_wall_ms: Option<f64>,
    pub started_at: Option<f64>,
    pub deadline_at: Option<f64>,
    /// `usage.cost`, `usage.totalTokens`, `usage.calls`.
    pub usage_cost: Option<f64>,
    pub usage_total_tokens: Option<f64>,
    pub usage_calls: Option<f64>,
    pub reservations: Vec<RlmReservation>,
    pub spent_usd: Option<f64>,
    pub max_cost_usd: Option<f64>,
    pub turns: Option<f64>,
    pub turn_tokens: Option<f64>,
    pub max_total_turns: Option<f64>,
    pub max_total_tokens: Option<f64>,
}

impl RlmUsage {
    /// The usage object as the worker sends it.
    pub fn from_json(v: &Value) -> Option<RlmUsage> {
        if !v.is_object() {
            return None;
        }
        let n = |v: &Value| v.as_f64();
        let s = |v: &Value| v.as_str().map(str::to_string);
        Some(RlmUsage {
            root_id: s(&v["rootId"]),
            admitted_tasks: n(&v["admittedTasks"]),
            remaining_wall_ms: n(&v["remainingWallMs"]),
            started_at: n(&v["startedAt"]),
            deadline_at: n(&v["deadlineAt"]),
            usage_cost: n(&v["usage"]["cost"]),
            usage_total_tokens: n(&v["usage"]["totalTokens"]),
            usage_calls: n(&v["usage"]["calls"]),
            reservations: v["reservations"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| RlmReservation {
                    task_id: s(&r["taskId"]),
                    admitted_at: n(&r["admittedAt"]),
                })
                .collect(),
            spent_usd: n(&v["cost"]["spentUsd"]),
            max_cost_usd: n(&v["cost"]["maxCostUsd"]),
            turns: n(&v["turns"]["turns"]),
            turn_tokens: n(&v["turns"]["tokens"]),
            max_total_turns: n(&v["turns"]["maxTotalTurns"]),
            max_total_tokens: n(&v["turns"]["maxTotalTokens"]),
        })
    }
}

/// The root's limits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmLimits {
    pub max_admitted_tasks: Option<f64>,
    pub max_wall_ms: Option<f64>,
    pub max_cost_usd: Option<f64>,
    pub max_total_tokens: Option<f64>,
    pub max_total_turns: Option<f64>,
}

impl RlmLimits {
    pub fn from_json(v: &Value) -> Option<RlmLimits> {
        if !v.is_object() {
            return None;
        }
        Some(RlmLimits {
            max_admitted_tasks: v["maxAdmittedTasks"].as_f64(),
            max_wall_ms: v["maxWallMs"].as_f64(),
            max_cost_usd: v["maxCostUsd"].as_f64(),
            max_total_tokens: v["maxTotalTokens"].as_f64(),
            max_total_turns: v["maxTotalTurns"].as_f64(),
        })
    }
}

/// One kernel lane of the pool.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmPoolLane {
    pub lane: String,
    pub running: f64,
    pub pinned_by: Vec<String>,
    pub memory_bytes: Option<f64>,
}

/// A file-write guard's counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmGuard {
    pub name: String,
    pub checks: f64,
    pub blocked: f64,
    pub unchecked: f64,
    pub after_checks: f64,
    pub after_findings: f64,
    pub turn_ms: f64,
    pub total_ms: f64,
}

/// The kernel pool (`rlm.pool`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmPool {
    pub live: f64,
    pub max_live: f64,
    pub lanes: Vec<RlmPoolLane>,
    pub evictions: Option<f64>,
    pub memory_bytes: Option<f64>,
    pub memory_cap_bytes: Option<f64>,
    pub guards: Option<Vec<RlmGuard>>,
}

/// A frame's shared budget node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmBudget {
    pub id: String,
    pub calls: Option<f64>,
    pub tokens: Option<f64>,
    pub depth: f64,
}

/// A string field that may be absent, `null`, or a string (`taskId`, `callerTaskId`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Tri {
    #[default]
    Absent,
    Null,
    Some(String),
}

impl Tri {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Tri::Some(s) => Some(s),
            _ => None,
        }
    }
}

/// One inference frame (`rlm.frames`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmFrame {
    pub id: String,
    pub status: String,
    pub reason: Option<String>,
    pub task: String,
    pub spent_calls: f64,
    pub spent_tokens: f64,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub task_id: Tri,
    pub kind: Option<String>,
    pub batch: Option<f64>,
    pub caller_task_id: Tri,
    pub lane: Option<String>,
    pub budget: Option<RlmBudget>,
}

/// A host-owned shell job.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmJob {
    pub id: String,
    pub status: String,
    pub command: String,
    pub exit_code: Option<f64>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub lane: Option<String>,
    pub tail: Option<String>,
}

/// An extension/MCP tool call from the REPL, or a tool the model called directly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmToolCall {
    pub id: String,
    pub name: String,
    pub label: String,
    pub status: String,
    /// `repl` or `native`.
    pub native: bool,
    pub lane: Option<String>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub input: Option<String>,
    pub preview: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellStatus {
    #[default]
    Running,
    Ok,
    Error,
}

/// One root `rlm` cell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmRootCell {
    pub tool_call_id: String,
    pub code: String,
    pub status: CellStatus,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub output: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmProgress {
    pub classification: String,
    pub receipts: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RlmTiming {
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
}

/// The current root turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmTurn {
    pub started_at: Option<f64>,
    pub prompt: Option<String>,
}

/// One forgotten or pinned context item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmContextItem {
    pub id: String,
    pub kind: Option<String>,
    pub preview: Option<String>,
    pub source: Option<String>,
    pub reason: Option<String>,
}

/// Root context control state (`ctx.state`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmContextState {
    pub forgotten: Vec<RlmContextItem>,
    pub forgotten_count: f64,
    pub collapsed_count: f64,
    pub pinned: Vec<RlmContextItem>,
    pub pinned_count: f64,
    pub notes: f64,
}

/// Everything the views draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RlmSnapshot {
    pub now: f64,
    pub tasks: Vec<RlmTask>,
    pub usage: Option<RlmUsage>,
    pub limits: Option<RlmLimits>,
    pub pool: Option<RlmPool>,
    pub root_cell: Option<RlmRootCell>,
    /// Task ids that belong to an open retained instance.
    pub retained: HashSet<String>,
    pub progress: HashMap<String, RlmProgress>,
    pub timing: HashMap<String, RlmTiming>,
    /// Recent inference frames, newest first.
    pub frames: Vec<RlmFrame>,
    pub jobs: Vec<RlmJob>,
    pub tool_calls: Vec<RlmToolCall>,
    /// Last inspection failure.
    pub error: Option<String>,
    pub context: Option<RlmContextState>,
    /// Root `rlm` cells of the current turn, oldest first.
    pub cells: Vec<RlmRootCell>,
    pub turn: Option<RlmTurn>,
    pub truncated_tasks: Option<f64>,
    pub workflows: Vec<RlmWorkflowRun>,
}

/// The theme colors the views use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Accent,
    Success,
    Error,
    Warning,
    Muted,
    Dim,
    Text,
    ToolTitle,
    MdCodeBlock,
}

impl Color {
    pub fn token(self) -> &'static str {
        match self {
            Color::Accent => "accent",
            Color::Success => "success",
            Color::Error => "error",
            Color::Warning => "warning",
            Color::Muted => "muted",
            Color::Dim => "dim",
            Color::Text => "text",
            Color::ToolTitle => "toolTitle",
            Color::MdCodeBlock => "mdCodeBlock",
        }
    }
}

type FgFn = Rc<dyn Fn(Color, &str) -> String>;
type BoldFn = Rc<dyn Fn(&str) -> String>;

/// Color hooks: the theme in the TUI, identity in tests (`PLAIN_STYLE`).
#[derive(Clone)]
pub struct RlmStyle {
    pub fg: FgFn,
    pub bold: BoldFn,
}

impl RlmStyle {
    pub fn plain() -> RlmStyle {
        RlmStyle {
            fg: Rc::new(|_, t| t.to_string()),
            bold: Rc::new(str::to_string),
        }
    }

    /// `rlmStyle` of client-tui.ts: `theme.fg` and `theme.bold`.
    pub fn themed(theme: Rc<Theme>) -> RlmStyle {
        RlmStyle {
            fg: Rc::new(move |c, t| fg(&theme, c.token(), t)),
            bold: Rc::new(bold),
        }
    }

    pub fn fg(&self, color: Color, text: &str) -> String {
        (self.fg)(color, text)
    }

    pub fn bold(&self, text: &str) -> String {
        (self.bold)(text)
    }
}

const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "interrupted"];

pub fn is_active_state(state: &str) -> bool {
    !TERMINAL.contains(&state)
}

/// `oneLine`: whitespace collapsed, at most 200 UTF-16 units.
pub(crate) fn one_line(text: &str) -> String {
    flat(text, 200)
}

/// `shortId`: the id without `ultron-task-`/`ultron-instance-`, first 8 characters.
pub fn short_id(id: &str) -> String {
    let rest = id
        .strip_prefix("ultron-task-")
        .or_else(|| id.strip_prefix("ultron-instance-"))
        .unwrap_or(id);
    js_slice(rest, 8)
}

/// `formatDuration`.
pub fn format_duration(ms: f64) -> String {
    use crate::components::js_to_fixed;
    if ms < 1000.0 {
        return format!("{}ms", super::js_num(ms.round()));
    }
    let seconds = ms / 1000.0;
    if seconds < 10.0 {
        return format!("{}s", js_to_fixed(seconds, 1));
    }
    if seconds < 60.0 {
        return format!("{}s", seconds.floor());
    }
    let minutes = (seconds / 60.0).floor();
    if minutes < 60.0 {
        let rest = (seconds % 60.0).floor();
        return if rest == 0.0 || minutes >= 10.0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{rest}s")
        };
    }
    let hours = (minutes / 60.0).floor();
    let rest = minutes % 60.0;
    if rest == 0.0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{rest}m")
    }
}

/// Tracks first-seen and finished times (task records carry no timestamps).
#[derive(Clone, Debug, Default)]
pub struct RlmClock {
    marks: HashMap<String, RlmTiming>,
}

impl RlmClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&mut self, key: &str, active: bool, now: f64, known_start: Option<f64>) -> RlmTiming {
        match self.marks.get_mut(key) {
            None => {
                let mark = if active {
                    RlmTiming {
                        started_at: Some(known_start.unwrap_or(now)),
                        ended_at: None,
                    }
                } else {
                    RlmTiming::default()
                };
                self.marks.insert(key.to_string(), mark);
                mark
            }
            Some(mark) => {
                if active {
                    if let Some(start) = known_start {
                        mark.started_at = Some(start);
                    }
                    if mark.started_at.is_none() {
                        mark.started_at = Some(now);
                    }
                    mark.ended_at = None;
                } else if mark.started_at.is_some() && mark.ended_at.is_none() {
                    mark.ended_at = Some(now);
                }
                *mark
            }
        }
    }

    pub fn timings(&mut self, tasks: &[RlmTask], usage: Option<&RlmUsage>, now: f64) -> HashMap<String, RlmTiming> {
        let mut admitted = HashMap::new();
        for r in usage.map(|u| u.reservations.as_slice()).unwrap_or(&[]) {
            if let (Some(id), Some(at)) = (&r.task_id, r.admitted_at) {
                admitted.insert(id.clone(), at);
            }
        }
        tasks
            .iter()
            .map(|t| {
                let timing = self.mark(
                    &format!("task:{}", t.id),
                    is_active_state(&t.state),
                    now,
                    admitted.get(&t.id).copied(),
                );
                (t.id.clone(), timing)
            })
            .collect()
    }
}

// ---- defensive parsing ----

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A finite number field.
fn n(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect()
}

/// What `parseAgentsStatus` returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentsStatus {
    pub truncated_tasks: Option<f64>,
    pub tasks: Vec<RlmTask>,
    pub usage: Option<RlmUsage>,
    pub limits: Option<RlmLimits>,
    pub jobs: Vec<RlmJob>,
    pub tool_calls: Vec<RlmToolCall>,
    pub workflows: Vec<RlmWorkflowRun>,
}

/// `parseAgentsStatus` (`agents.status {graph: true}`).
pub fn parse_agents_status(value: &Value) -> AgentsStatus {
    let body = if value.is_object() { value } else { &Value::Null };
    let mut tasks = Vec::new();
    for t in body["tasks"].as_array().into_iter().flatten() {
        if !t.is_object() {
            continue;
        }
        let (Some(id), Some(state)) = (s(t, "id"), s(t, "state")) else {
            continue;
        };
        let result = t.get("result").filter(|r| r.is_object()).map(|r| RlmTaskResult {
            status: s(r, "status"),
            value: r.get("value").cloned(),
            error: s(r, "error"),
            preview: s(r, "preview"),
        });
        let w = &t["workflow"];
        let workflow =
            (w.is_object() && w["run"].is_string() && w["node"].is_string()).then(|| RlmWorkflowMembership {
                run: s(w, "run").unwrap_or_default(),
                node: s(w, "node").unwrap_or_default(),
                depends_on: strings(&w["dependsOn"]),
                join: s(w, "join").unwrap_or_else(|| "all".into()),
            });
        tasks.push(RlmTask {
            id,
            definition: s(t, "definition").unwrap_or_else(|| "task".into()),
            state,
            parent_id: s(t, "parentId"),
            result,
            lane: s(t, "lane"),
            input: s(t, "input"),
            fetch: s(t, "fetch"),
            model: s(t, "model"),
            last_text: s(t, "lastText"),
            worktree: s(t, "worktree"),
            started_at: n(t, "startedAt"),
            ended_at: n(t, "endedAt"),
            cost: n(t, "cost"),
            tokens: n(t, "tokens"),
            turns: n(t, "turns"),
            tool_call_count: n(t, "toolCallCount"),
            workflow,
        });
    }
    let usage_value = &body["usage"];
    let limits = if body["limits"].is_object() {
        RlmLimits::from_json(&body["limits"])
    } else {
        RlmLimits::from_json(&usage_value["limits"])
    };
    let mut jobs = Vec::new();
    for j in body["jobs"].as_array().into_iter().flatten() {
        let (Some(id), Some(status)) = (s(j, "id"), s(j, "status")) else {
            continue;
        };
        jobs.push(RlmJob {
            id,
            status,
            command: s(j, "command").unwrap_or_default(),
            exit_code: j.get("exitCode").and_then(Value::as_f64),
            started_at: j.get("startedAt").and_then(Value::as_f64),
            ended_at: j.get("endedAt").and_then(Value::as_f64),
            lane: s(j, "lane"),
            tail: s(j, "tail"),
        });
    }
    let mut tool_calls = Vec::new();
    for c in body["toolCalls"].as_array().into_iter().flatten() {
        let (Some(id), Some(status)) = (s(c, "id"), s(c, "status")) else {
            continue;
        };
        let name = s(c, "name").unwrap_or_else(|| "tool".into());
        tool_calls.push(RlmToolCall {
            id,
            label: s(c, "label").unwrap_or_else(|| name.clone()),
            name,
            status,
            native: c["source"] == "native",
            started_at: c.get("startedAt").and_then(Value::as_f64),
            ended_at: c.get("endedAt").and_then(Value::as_f64),
            lane: s(c, "lane"),
            input: s(c, "input"),
            preview: s(c, "preview"),
            error: s(c, "error"),
        });
    }
    let mut workflows = Vec::new();
    for r in body["workflows"].as_array().into_iter().flatten() {
        let Some(run) = s(r, "run") else { continue };
        let nodes = r["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| {
                Some(RlmWorkflowNode {
                    id: s(node, "id")?,
                    definition: s(node, "definition").unwrap_or_else(|| "task".into()),
                    depends_on: strings(&node["dependsOn"]),
                    join: s(node, "join").unwrap_or_else(|| "all".into()),
                    status: s(node, "status"),
                    reason: s(node, "reason"),
                })
            })
            .collect();
        workflows.push(RlmWorkflowRun {
            run,
            parent_id: s(r, "parentId"),
            started_at: n(r, "startedAt"),
            ended_at: n(r, "endedAt"),
            truncated_nodes: n(r, "truncatedNodes"),
            nodes,
        });
    }
    AgentsStatus {
        truncated_tasks: body["truncatedTasks"].as_f64(),
        tasks,
        usage: RlmUsage::from_json(usage_value),
        limits,
        jobs,
        tool_calls,
        workflows,
    }
}

/// `parseRetained` (`instances.list`): task ids owned by open retained instances.
pub fn parse_retained(value: &Value) -> HashSet<String> {
    let mut retained = HashSet::new();
    for instance in value.as_array().into_iter().flatten() {
        if instance["state"] != "open" {
            continue;
        }
        if let Some(id) = instance["task_id"].as_str() {
            retained.insert(id.to_string());
        }
        for invocation in instance["invocations"].as_array().into_iter().flatten() {
            if let Some(id) = invocation["task_id"].as_str() {
                retained.insert(id.to_string());
            }
        }
    }
    retained
}

/// `parsePool` (`rlm.pool`).
pub fn parse_pool(value: &Value) -> Option<RlmPool> {
    let (Some(live), Some(max_live)) = (value["live"].as_f64(), value["maxLive"].as_f64()) else {
        return None;
    };
    let lanes = value["lanes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|lane| {
            Some(RlmPoolLane {
                lane: s(lane, "lane")?,
                running: lane["running"].as_f64().unwrap_or(0.0),
                pinned_by: strings(&lane["pinnedBy"]),
                memory_bytes: n(lane, "memoryBytes"),
            })
        })
        .collect();
    let guards = value["guards"].as_array().map(|items| {
        items
            .iter()
            .filter_map(|g| {
                let count = |k: &str| g[k].as_f64().unwrap_or(0.0);
                Some(RlmGuard {
                    name: s(g, "name")?,
                    checks: count("checks"),
                    blocked: count("blocked"),
                    unchecked: count("unchecked"),
                    after_checks: count("afterChecks"),
                    after_findings: count("afterFindings"),
                    turn_ms: count("turnMs"),
                    total_ms: count("totalMs"),
                })
            })
            .collect()
    });
    Some(RlmPool {
        live,
        max_live,
        lanes,
        evictions: value["evictions"].as_f64(),
        memory_bytes: n(value, "memoryBytes"),
        memory_cap_bytes: n(value, "memoryCapBytes"),
        guards,
    })
}

fn tri(v: &Value, key: &str) -> Tri {
    match v.get(key) {
        Some(Value::String(s)) => Tri::Some(s.clone()),
        Some(Value::Null) => Tri::Null,
        _ => Tri::Absent,
    }
}

/// `parseFrames` (`rlm.frames`).
pub fn parse_frames(value: &Value) -> Vec<RlmFrame> {
    value["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let id = s(f, "id")?;
            let status = s(f, "status")?;
            let b = &f["budget"];
            let budget = b["id"].as_str().map(|id| RlmBudget {
                id: id.to_string(),
                calls: b["calls"].as_f64(),
                tokens: b["tokens"].as_f64(),
                depth: b["depth"].as_f64().unwrap_or(0.0),
            });
            Some(RlmFrame {
                id,
                status,
                reason: s(f, "reason"),
                task: s(f, "task").unwrap_or_default(),
                spent_calls: f["spent"]["calls"].as_f64().unwrap_or(0.0),
                spent_tokens: f["spent"]["tokens"].as_f64().unwrap_or(0.0),
                started_at: f.get("startedAt").and_then(Value::as_f64),
                ended_at: f.get("endedAt").and_then(Value::as_f64),
                task_id: tri(f, "taskId"),
                caller_task_id: tri(f, "callerTaskId"),
                kind: s(f, "kind"),
                lane: s(f, "lane"),
                batch: n(f, "batch"),
                budget,
            })
        })
        .collect()
}

/// `parseProgress` (`progress.assess`).
pub fn parse_progress(value: &Value) -> Option<RlmProgress> {
    Some(RlmProgress {
        classification: s(value, "classification")?,
        receipts: value["receipts"].as_array().map_or(0, Vec::len),
    })
}

/// `parseContextState` (`ctx.state`).
pub fn parse_context_state(value: &Value) -> Option<RlmContextState> {
    if !value.is_object() {
        return None;
    }
    let list = |items: &Value| -> Vec<RlmContextItem> {
        items
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(RlmContextItem {
                    id: s(item, "id")?,
                    kind: s(item, "kind"),
                    preview: s(item, "preview"),
                    source: s(item, "source"),
                    reason: s(item, "reason"),
                })
            })
            .collect()
    };
    let count = |v: &Value| v.as_f64().filter(|x| x.is_finite()).unwrap_or(0.0);
    Some(RlmContextState {
        forgotten: list(&value["forgotten"]),
        forgotten_count: count(&value["forgottenCount"]),
        collapsed_count: count(&value["collapsedCount"]),
        pinned: list(&value["pinned"]),
        pinned_count: count(&value["pinnedCount"]),
        notes: count(&value["notes"]),
    })
}

/// Context lines of the panel: a summary line, then the latest forgotten and pinned items (at most three each).
pub fn render_context_lines(context: Option<&RlmContextState>, style: &RlmStyle) -> Vec<String> {
    let Some(c) = context else { return Vec::new() };
    if c.forgotten_count + c.collapsed_count + c.pinned_count + c.notes == 0.0 {
        return Vec::new();
    }
    use super::js_num;
    let mut lines = vec![style.fg(
        Color::Muted,
        &format!(
            "context: {} forgotten · {} collapsed · {} pinned · {} notes",
            js_num(c.forgotten_count),
            js_num(c.collapsed_count),
            js_num(c.pinned_count),
            js_num(c.notes)
        ),
    )];
    let tail = |items: &[RlmContextItem]| items[items.len().saturating_sub(3)..].to_vec();
    for item in tail(&c.forgotten) {
        let why = match (&item.reason, &item.source) {
            (Some(r), _) if !r.is_empty() => format!(" ({})", one_line(r)),
            (_, Some(s)) if !s.is_empty() => format!(" ({s})"),
            _ => String::new(),
        };
        lines.push(style.fg(
            Color::Dim,
            &format!(
                "  forgot {} {} {}{why}",
                short_id(&item.id),
                item.kind.clone().unwrap_or_default(),
                one_line(item.preview.as_deref().unwrap_or(""))
            ),
        ));
    }
    for item in tail(&c.pinned) {
        lines.push(style.fg(
            Color::Dim,
            &format!(
                "  pinned {} {} {}",
                short_id(&item.id),
                item.kind.clone().unwrap_or_default(),
                one_line(item.preview.as_deref().unwrap_or(""))
            ),
        ));
    }
    lines
}

// ---- the root turn from the lane transcript ----

/// A running tool of the open operation (`LaneSnapshot.operation.runningTools`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunningTool {
    /// `running` while it runs.
    pub status: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub result: Value,
    pub is_error: bool,
}

/// What the cell extractors read: the transcript entries (`{message, timestamp?}`, or bare messages) and the open
/// operation's running tools (`None`: no operation).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptView {
    pub transcript: Vec<Value>,
    pub running_tools: Option<Vec<RunningTool>>,
}

impl TranscriptView {
    /// From the TUI's Pi messages (each becomes `{message}`); `running` is the open operation's tools.
    pub fn from_messages(messages: &[Value], running: Option<Vec<RunningTool>>) -> TranscriptView {
        TranscriptView {
            transcript: messages.iter().map(|m| serde_json::json!({"message": m})).collect(),
            running_tools: running,
        }
    }
}

fn code_of(args: &Value) -> String {
    args["code"].as_str().unwrap_or("").to_string()
}

fn text_of(result: &Value) -> Option<String> {
    let content = result["content"].as_array()?;
    let text: String = content
        .iter()
        .map(|p| {
            if p["type"] == "text" {
                p["text"].as_str().unwrap_or("")
            } else {
                ""
            }
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

/// `extractRootCell`: the current or last root `rlm` cell.
pub fn extract_root_cell(view: Option<&TranscriptView>, clock: &mut RlmClock, now: f64) -> Option<RlmRootCell> {
    let view = view?;
    let running = view
        .running_tools
        .as_ref()
        .and_then(|tools| tools.iter().rev().find(|t| t.tool_name == "rlm"));
    if let Some(tool) = running {
        let active = tool.status == "running";
        let timing = clock.mark(&format!("cell:{}", tool.tool_call_id), active, now, None);
        return Some(RlmRootCell {
            tool_call_id: tool.tool_call_id.clone(),
            code: code_of(&tool.args),
            status: if active {
                CellStatus::Running
            } else if tool.is_error {
                CellStatus::Error
            } else {
                CellStatus::Ok
            },
            started_at: timing.started_at,
            ended_at: timing.ended_at,
            output: if active { None } else { text_of(&tool.result) },
        });
    }
    let entries = &view.transcript;
    for index in (0..entries.len()).rev() {
        let message = &entries[index]["message"];
        if message["role"] != "toolResult" || message["toolName"] != "rlm" {
            continue;
        }
        let Some(id) = message["toolCallId"].as_str() else {
            continue;
        };
        let mut code = String::new();
        let mut started = None;
        for back in (0..index).rev() {
            let assistant = &entries[back]["message"];
            if assistant["role"] != "assistant" {
                continue;
            }
            let Some(content) = assistant["content"].as_array() else {
                continue;
            };
            let Some(call) = content.iter().find(|p| p["type"] == "toolCall" && p["id"] == id) else {
                continue;
            };
            code = code_of(&call["arguments"]);
            started = assistant["timestamp"].as_f64();
            break;
        }
        let observed = clock.mark(&format!("cell:{id}"), false, now, None);
        return Some(RlmRootCell {
            tool_call_id: id.to_string(),
            code,
            status: if message["isError"] == true {
                CellStatus::Error
            } else {
                CellStatus::Ok
            },
            started_at: observed.started_at.or(started),
            ended_at: observed.ended_at.or(message["timestamp"].as_f64()),
            output: text_of(message),
        });
    }
    None
}

/// `extractTurn`: the last user message (time and preview) and the root `rlm` cells after it, oldest first.
pub fn extract_turn(
    view: Option<&TranscriptView>,
    clock: &mut RlmClock,
    now: f64,
    max_cells: usize,
) -> (Option<RlmTurn>, Vec<RlmRootCell>) {
    let Some(view) = view else { return (None, Vec::new()) };
    let entries = &view.transcript;
    let start = entries.iter().rposition(|e| e["message"]["role"] == "user");
    let turn = start.map(|start| {
        let entry = &entries[start];
        let message = &entry["message"];
        let text = match &message["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .map(|p| {
                    if p["type"] == "text" {
                        p["text"].as_str().unwrap_or("")
                    } else {
                        ""
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        let at = if entry["timestamp"].is_number() {
            entry["timestamp"].as_f64()
        } else {
            message["timestamp"].as_f64()
        };
        RlmTurn {
            started_at: at,
            prompt: Some(one_line(&text)),
        }
    });
    let mut order: Vec<String> = Vec::new();
    let mut cells: HashMap<String, RlmRootCell> = HashMap::new();
    let put = |order: &mut Vec<String>, cells: &mut HashMap<String, RlmRootCell>, cell: RlmRootCell| {
        if !cells.contains_key(&cell.tool_call_id) {
            order.push(cell.tool_call_id.clone());
        }
        cells.insert(cell.tool_call_id.clone(), cell);
    };
    for item in &entries[start.map_or(0, |s| s + 1)..] {
        let message = &item["message"];
        if message["role"] == "assistant"
            && let Some(content) = message["content"].as_array()
        {
            for part in content {
                if part["type"] != "toolCall" || part["name"] != "rlm" {
                    continue;
                }
                let Some(id) = part["id"].as_str() else { continue };
                let observed = clock.mark(&format!("cell:{id}"), false, now, None);
                put(
                    &mut order,
                    &mut cells,
                    RlmRootCell {
                        tool_call_id: id.to_string(),
                        code: code_of(&part["arguments"]),
                        status: CellStatus::Running,
                        started_at: observed.started_at.or(message["timestamp"].as_f64()),
                        ended_at: None,
                        output: None,
                    },
                );
            }
        } else if message["role"] == "toolResult"
            && message["toolName"] == "rlm"
            && let Some(id) = message["toolCallId"].as_str()
        {
            let known = cells.get(id).cloned();
            let observed = clock.mark(&format!("cell:{id}"), false, now, None);
            put(
                &mut order,
                &mut cells,
                RlmRootCell {
                    tool_call_id: id.to_string(),
                    code: known.as_ref().map(|c| c.code.clone()).unwrap_or_default(),
                    status: if message["isError"] == true {
                        CellStatus::Error
                    } else {
                        CellStatus::Ok
                    },
                    started_at: known.and_then(|c| c.started_at),
                    ended_at: observed.ended_at.or(message["timestamp"].as_f64()),
                    output: text_of(message),
                },
            );
        }
    }
    for tool in view.running_tools.iter().flatten() {
        if tool.tool_name != "rlm" {
            continue;
        }
        let active = tool.status == "running";
        let timing = clock.mark(&format!("cell:{}", tool.tool_call_id), active, now, None);
        let known = cells.get(&tool.tool_call_id).cloned();
        let code = code_of(&tool.args);
        put(
            &mut order,
            &mut cells,
            RlmRootCell {
                tool_call_id: tool.tool_call_id.clone(),
                code: if code.is_empty() {
                    known.as_ref().map(|c| c.code.clone()).unwrap_or_default()
                } else {
                    code
                },
                status: if active {
                    CellStatus::Running
                } else if tool.is_error {
                    CellStatus::Error
                } else {
                    CellStatus::Ok
                },
                started_at: timing.started_at.or(known.and_then(|c| c.started_at)),
                ended_at: timing.ended_at,
                output: if active { None } else { text_of(&tool.result) },
            },
        );
    }
    let open = view.running_tools.is_some();
    let list: Vec<RlmRootCell> = order
        .iter()
        .map(|id| {
            let mut cell = cells[id].clone();
            if cell.status == CellStatus::Running && !open {
                cell.status = CellStatus::Error;
                cell.output = Some(cell.output.unwrap_or_else(|| "interrupted".into()));
            }
            cell
        })
        .collect();
    let skip = list.len().saturating_sub(max_cells);
    (turn, list[skip..].to_vec())
}

/// A root cell from its JSON form (`{toolCallId, code, status: running|ok|error, startedAt?, endedAt?, output?}`).
pub fn parse_cell(value: &Value) -> Option<RlmRootCell> {
    Some(RlmRootCell {
        tool_call_id: s(value, "toolCallId")?,
        code: s(value, "code").unwrap_or_default(),
        status: match value["status"].as_str() {
            Some("ok") => CellStatus::Ok,
            Some("error") => CellStatus::Error,
            _ => CellStatus::Running,
        },
        started_at: n(value, "startedAt"),
        ended_at: n(value, "endedAt"),
        output: s(value, "output"),
    })
}

impl RlmSnapshot {
    /// A snapshot from the inspection payloads and the transcript-derived turn, in one JSON object:
    /// `{now, agentsStatus, instances, pool, frames, ctx, cells, turn: {startedAt?, prompt?}, rootCell, timing:
    /// {taskId: {startedAt?, endedAt?}}, progress: {taskId: progress.assess payload}, error}` (every key optional).
    /// `agentsStatus` is `agents.status {graph: true}`, `instances` `instances.list`, `pool` `rlm.pool`, `frames`
    /// `rlm.frames`, `ctx` `ctx.state`; `cells`/`turn`/`rootCell` come from [`extract_turn`]/[`extract_root_cell`]
    /// (JSON form of [`parse_cell`]), `timing` from [`RlmClock::timings`].
    pub fn from_wire(w: &Value) -> RlmSnapshot {
        let status = parse_agents_status(&w["agentsStatus"]);
        RlmSnapshot {
            now: w["now"].as_f64().unwrap_or(0.0),
            tasks: status.tasks,
            usage: status.usage,
            limits: status.limits,
            jobs: status.jobs,
            tool_calls: status.tool_calls,
            workflows: status.workflows,
            truncated_tasks: status.truncated_tasks,
            pool: parse_pool(&w["pool"]),
            retained: parse_retained(&w["instances"]),
            frames: parse_frames(&w["frames"]),
            context: parse_context_state(&w["ctx"]),
            cells: w["cells"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parse_cell)
                .collect(),
            turn: w["turn"].is_object().then(|| RlmTurn {
                started_at: n(&w["turn"], "startedAt"),
                prompt: s(&w["turn"], "prompt"),
            }),
            root_cell: parse_cell(&w["rootCell"]),
            timing: w["timing"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(id, t)| {
                    (
                        id.clone(),
                        RlmTiming {
                            started_at: n(t, "startedAt"),
                            ended_at: n(t, "endedAt"),
                        },
                    )
                })
                .collect(),
            progress: w["progress"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(id, p)| Some((id.clone(), parse_progress(p)?)))
                .collect(),
            error: s(w, "error"),
        }
    }
}
