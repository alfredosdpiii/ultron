//! The RLM run as a live graph (experimental/rlm-graph.ts): the root turn, its `rlm` cells and everything they
//! started (typed agent tasks, `rlm.spawn` children, `rlm.infer` frames, `rlm.map` fan-outs, workflows, shell jobs,
//! tool calls), laid out with box-drawing connectors, and the three levels the TUI shows: a one-line footer summary,
//! the docked panel, and the focus view's body (see [`super::focus`]).

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tui::utils::{truncate_to_width, visible_width};

use super::visualizer::{
    CellStatus, Color, RlmFrame, RlmJob, RlmRootCell, RlmSnapshot, RlmStyle, RlmTask, RlmToolCall, format_duration,
    render_context_lines, short_id,
};
use super::{flat, js_num};
use crate::components::js_to_fixed;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphStatus {
    Running,
    Pending,
    Done,
    Failed,
    Cancelled,
    Incomplete,
}

impl GraphStatus {
    pub fn name(self) -> &'static str {
        match self {
            GraphStatus::Running => "running",
            GraphStatus::Pending => "pending",
            GraphStatus::Done => "done",
            GraphStatus::Failed => "failed",
            GraphStatus::Cancelled => "cancelled",
            GraphStatus::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GraphKind {
    Turn,
    Cell,
    Task,
    Child,
    Background,
    Infer,
    Fanout,
    Job,
    Tool,
    Workflow,
    Earlier,
    /// The RLM pane's "N tool calls" node.
    Tools,
}

impl GraphKind {
    pub fn name(self) -> &'static str {
        match self {
            GraphKind::Turn => "turn",
            GraphKind::Cell => "cell",
            GraphKind::Task => "task",
            GraphKind::Child => "child",
            GraphKind::Background => "background",
            GraphKind::Infer => "infer",
            GraphKind::Fanout => "fanout",
            GraphKind::Job => "job",
            GraphKind::Tool => "tool",
            GraphKind::Workflow => "workflow",
            GraphKind::Earlier => "earlier",
            GraphKind::Tools => "tools",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphProgress {
    pub total: usize,
    pub done: usize,
    pub running: usize,
    pub pending: usize,
    pub incomplete: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// Stable across refreshes (selection and collapse state are keyed by it).
    pub key: String,
    pub kind: GraphKind,
    pub label: String,
    pub status: GraphStatus,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub tokens: Option<f64>,
    pub cost: Option<f64>,
    pub progress: Option<GraphProgress>,
    /// Workflow join marker, e.g. "⇐ plan+code (all)".
    pub join: Option<String>,
    pub note: Option<String>,
    pub children: Vec<GraphNode>,
    pub details: Vec<(String, String)>,
    /// Frame trace id to fetch for details (`rlm.frames {id}`).
    pub trace_id: Option<String>,
}

impl GraphNode {
    fn new(key: String, kind: GraphKind, label: String, status: GraphStatus) -> GraphNode {
        GraphNode {
            key,
            kind,
            label,
            status,
            started_at: None,
            ended_at: None,
            tokens: None,
            cost: None,
            progress: None,
            join: None,
            note: None,
            children: Vec::new(),
            details: Vec::new(),
            trace_id: None,
        }
    }
}

fn d(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_string(), value.into())
}

pub fn is_finished(status: GraphStatus) -> bool {
    !matches!(status, GraphStatus::Running | GraphStatus::Pending)
}

pub fn task_status(state: &str) -> GraphStatus {
    match state {
        "running" => GraphStatus::Running,
        "admitted" => GraphStatus::Pending,
        "completed" => GraphStatus::Done,
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn frame_status(frame: &RlmFrame) -> GraphStatus {
    match frame.status.as_str() {
        "running" => {
            if frame.task_id == super::visualizer::Tri::Null {
                GraphStatus::Pending
            } else {
                GraphStatus::Running
            }
        }
        "complete" => GraphStatus::Done,
        "incomplete" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn job_status(job: &RlmJob) -> GraphStatus {
    match job.status.as_str() {
        "running" => GraphStatus::Running,
        "completed" => {
            if job.exit_code == Some(0.0) {
                GraphStatus::Done
            } else {
                GraphStatus::Failed
            }
        }
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" | "timed_out" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

/// `JSON.stringify` of a result value.
fn safe_json(value: &Value) -> String {
    value.to_string()
}

fn first_code_line(code: &str) -> String {
    code.split('\n')
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
        .to_string()
}

/// Combine children statuses into a parent status.
pub fn rollup(c: &GraphProgress) -> GraphStatus {
    if c.running > 0 {
        return GraphStatus::Running;
    }
    if c.pending > 0 {
        return if c.done + c.failed + c.incomplete > 0 {
            GraphStatus::Running
        } else {
            GraphStatus::Pending
        };
    }
    if c.failed > 0 && c.done == 0 && c.incomplete == 0 {
        return GraphStatus::Failed;
    }
    if c.failed + c.incomplete > 0 {
        return GraphStatus::Incomplete;
    }
    GraphStatus::Done
}

/// `count(statuses, total)`.
pub fn count(statuses: &[GraphStatus], total: usize) -> GraphProgress {
    let mut p = GraphProgress {
        total,
        ..GraphProgress::default()
    };
    for s in statuses {
        match s {
            GraphStatus::Done => p.done += 1,
            GraphStatus::Running => p.running += 1,
            GraphStatus::Pending => p.pending += 1,
            GraphStatus::Incomplete => p.incomplete += 1,
            _ => p.failed += 1,
        }
    }
    p.pending += total.saturating_sub(statuses.len());
    p
}

fn min_defined(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |m, v| Some(m.map_or(v, |m: f64| m.min(v))))
}

fn max_defined(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
}

fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |s, v| Some(s.unwrap_or(0.0) + v))
}

fn task_kind(task: &RlmTask) -> GraphKind {
    match task.definition.as_str() {
        "rlm-child@1" => GraphKind::Child,
        "background-job@1" => GraphKind::Background,
        _ => GraphKind::Task,
    }
}

fn task_label(task: &RlmTask) -> String {
    let id = short_id(&task.id);
    match task_kind(task) {
        GraphKind::Child => format!("rlm.spawn child {id}"),
        GraphKind::Background => format!("background {id}"),
        _ => format!(
            "{} {} {id}",
            task.workflow.as_ref().map_or("agent", |w| w.node.as_str()),
            task.definition
        ),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => safe_json(other),
    }
}

fn task_note(task: &RlmTask) -> Option<String> {
    let result = task.result.as_ref()?;
    if let Some(e) = result.error.as_deref().filter(|e| !e.is_empty()) {
        return Some(flat(e, 120));
    }
    if let Some(p) = result.preview.as_deref().filter(|p| !p.is_empty()) {
        return Some(format!("→ {}", flat(p, 120)));
    }
    match &result.value {
        None | Some(Value::Null) => None,
        Some(v) => Some(format!("→ {}", flat(&value_text(v), 120))),
    }
}

fn fetch_hint(task: &RlmTask) -> String {
    if let Some(f) = task.fetch.as_deref().filter(|f| !f.is_empty()) {
        return f.to_string();
    }
    match task.definition.as_str() {
        "rlm-child@1" => format!("await rlm.collect([\"{}\"])", task.id),
        "background-job@1" => format!("await background.result(\"{}\")", task.id),
        _ => format!("await agents.result(\"{}\")", task.id),
    }
}

fn task_node(task: &RlmTask, snapshot: &RlmSnapshot) -> GraphNode {
    let timing = snapshot.timing.get(&task.id);
    let mut details = vec![
        d("id", &task.id),
        d("definition", &task.definition),
        d("state", &task.state),
    ];
    let present = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(l) = present(&task.lane) {
        details.push(d("lane", l));
    }
    if let Some(p) = present(&task.parent_id) {
        details.push(d("parent", p));
    }
    if let Some(w) = present(&task.worktree) {
        details.push(d("worktree", w));
    }
    if let Some(w) = &task.workflow {
        let after = if w.depends_on.is_empty() {
            String::new()
        } else {
            format!(" after {} ({})", w.depends_on.join(", "), w.join)
        };
        details.push(d("workflow", format!("{} node {}{after}", w.run, w.node)));
    }
    if let Some(i) = present(&task.input) {
        details.push(d("input", i));
    }
    if let Some(r) = &task.result {
        if let Some(e) = r.error.as_deref().filter(|e| !e.is_empty()) {
            details.push(d("error", flat(e, 400)));
        } else if let Some(p) = r.preview.as_deref().filter(|p| !p.is_empty()) {
            details.push(d("result", p));
        } else if let Some(v) = r.value.as_ref().filter(|v| !v.is_null()) {
            details.push(d("result", flat(&value_text(v), 400)));
        }
    }
    if snapshot.retained.contains(&task.id) {
        details.push(d("retained", "open instance keeps this lane's kernel"));
    }
    if let Some(p) = snapshot.progress.get(&task.id) {
        details.push(d("progress", format!("{} · {} receipts", p.classification, p.receipts)));
    }
    details.push(d("fetch", fetch_hint(task)));
    let mut node = GraphNode::new(
        format!("task:{}", task.id),
        task_kind(task),
        task_label(task),
        task_status(&task.state),
    );
    node.started_at = task.started_at.or(timing.and_then(|t| t.started_at));
    node.ended_at = task.ended_at.or(timing.and_then(|t| t.ended_at));
    node.tokens = task.tokens;
    node.cost = task.cost;
    node.note = task_note(task);
    if let Some(w) = task.workflow.as_ref().filter(|w| !w.depends_on.is_empty()) {
        let suffix = if w.depends_on.len() > 1 {
            format!(" ({})", w.join)
        } else {
            String::new()
        };
        node.join = Some(format!("⇐ {}{suffix}", w.depends_on.join("+")));
    }
    node.details = details;
    node
}

fn frame_group_node(key: &str, frames: &[&RlmFrame]) -> GraphNode {
    let first = frames[0];
    let batch = (first.batch.unwrap_or(frames.len() as f64) as usize).max(frames.len());
    let is_map = first.kind.as_deref() == Some("map") || batch > 1;
    let statuses: Vec<GraphStatus> = frames.iter().map(|f| frame_status(f)).collect();
    let progress = count(&statuses, batch);
    let status = rollup(&progress);
    let calls = sum(frames.iter().map(|f| Some(f.spent_calls))).unwrap_or(0.0);
    let tokens = sum(frames.iter().map(|f| Some(f.spent_tokens))).unwrap_or(0.0);
    let started_at = min_defined(frames.iter().map(|f| f.started_at));
    let ended_at = if is_finished(status) {
        max_defined(frames.iter().map(|f| f.ended_at))
    } else {
        None
    };
    let mut details = Vec::new();
    if is_map {
        details.push(d(
            "frames",
            format!(
                "{batch} ({} done · {} running · {} queued · {} incomplete · {} failed)",
                progress.done, progress.running, progress.pending, progress.incomplete, progress.failed
            ),
        ));
    }
    details.push(d("task", flat(&first.task, 300)));
    match &first.budget {
        Some(b) => {
            let id = b.id.strip_prefix("budget-").unwrap_or(&b.id);
            details.push(d(
                "budget",
                format!(
                    "{} · calls {}/{} · tokens {}/{} · depth {}",
                    super::js_slice(id, 8),
                    js_num(calls),
                    b.calls.map_or("∞".to_string(), js_num),
                    js_num(tokens),
                    b.tokens.map_or("∞".to_string(), js_num),
                    js_num(b.depth)
                ),
            ));
        }
        None => details.push(d(
            "spent",
            format!("{} calls · {} tokens", js_num(calls), js_num(tokens)),
        )),
    }
    let troubled: Vec<&&RlmFrame> = frames
        .iter()
        .filter(|f| f.status == "incomplete" || f.status == "error")
        .take(3)
        .collect();
    for f in &troubled {
        let reason = f.reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
        details.push(d(&f.status, format!("{}{reason}", f.id)));
    }
    if is_map {
        details.push(d("traces", "await rlm.frames(limit=200)"));
        if let Some(t) = troubled.first() {
            details.push(d("fetch", format!("await rlm.frames(\"{}\")", t.id)));
        }
    } else {
        details.push(d("trace", &first.id));
        if let Some(r) = first.reason.as_deref().filter(|r| !r.is_empty()) {
            details.push(d("reason", r));
        }
        details.push(d("fetch", format!("await rlm.frames(\"{}\")", first.id)));
    }
    let mut reasons: Vec<String> = Vec::new();
    for r in frames.iter().filter_map(|f| f.reason.clone()) {
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    let label = if is_map {
        format!("rlm.map {batch} frame{}", if batch == 1 { "" } else { "s" })
    } else {
        format!(
            "rlm.infer {}",
            super::js_slice(first.id.strip_prefix("frame-").unwrap_or(&first.id), 8)
        )
    };
    let mut node = GraphNode::new(
        format!("frames:{key}"),
        if is_map { GraphKind::Fanout } else { GraphKind::Infer },
        label,
        status,
    );
    node.started_at = started_at;
    node.ended_at = ended_at;
    node.tokens = Some(tokens);
    if is_map {
        node.progress = Some(progress);
        node.note = Some(flat(&first.task, 80));
    } else {
        node.note = Some(if reasons.is_empty() {
            flat(&first.task, 80)
        } else {
            reasons.join(", ")
        });
        node.trace_id = Some(first.id.clone());
    }
    node.details = details;
    node
}

fn job_node(job: &RlmJob, now: f64) -> GraphNode {
    let status = job_status(job);
    let exit = job.exit_code.map_or("null".to_string(), js_num);
    let mut details = vec![
        d("id", &job.id),
        d("command", flat(&job.command, 400)),
        d(
            "status",
            if job.status == "completed" {
                format!("exit {exit}")
            } else {
                job.status.clone()
            },
        ),
    ];
    if let Some(l) = job.lane.as_deref().filter(|l| !l.is_empty()) {
        details.push(d("lane", l));
    }
    if let Some(t) = job.tail.as_deref().filter(|t| !t.is_empty()) {
        details.push(d("output", flat(t, 300)));
    }
    details.push(d("fetch", format!("await (await rlm.job(\"{}\")).read()", job.id)));
    let mut node = GraphNode::new(
        format!("job:{}", job.id),
        GraphKind::Job,
        format!("bash {}", flat(&job.command, 60)),
        status,
    );
    node.started_at = job.started_at;
    node.ended_at = match job.ended_at {
        Some(e) => Some(e),
        None if status == GraphStatus::Running => None,
        None => Some(now),
    };
    if job.status == "completed" && job.exit_code != Some(0.0) {
        node.note = Some(format!("exit {exit}"));
    }
    if job.status != "completed" && job.status != "running" {
        node.note = Some(job.status.clone());
    }
    node.details = details;
    node
}

fn tool_call_status(call: &RlmToolCall) -> GraphStatus {
    match call.status.as_str() {
        "running" => GraphStatus::Running,
        "completed" => GraphStatus::Done,
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn tool_call_node(call: &RlmToolCall, now: f64) -> GraphNode {
    let status = tool_call_status(call);
    let mut details = vec![
        d("id", &call.id),
        d("tool", &call.name),
        d(
            "via",
            if call.native {
                "model tool call"
            } else {
                "rlm cell (Python)"
            },
        ),
        d("status", &call.status),
    ];
    let present = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(l) = present(&call.lane) {
        details.push(d("lane", l));
    }
    if let Some(i) = present(&call.input) {
        details.push(d("input", flat(&i, 400)));
    }
    if let Some(e) = present(&call.error) {
        details.push(d("error", flat(&e, 400)));
    } else if let Some(p) = present(&call.preview) {
        details.push(d(
            if status == GraphStatus::Running {
                "progress"
            } else {
                "result"
            },
            flat(&p, 400),
        ));
    }
    if !call.native {
        details.push(d("fetch", format!("await tools.result(\"{}\")", call.id)));
    }
    let note = match (&call.error, present(&call.preview)) {
        (Some(e), _) => Some(flat(e, 100)),
        (None, Some(p)) if status == GraphStatus::Done => Some(format!("→ {}", flat(&p, 100))),
        _ => None,
    };
    let mut node = GraphNode::new(
        format!("tool:{}", call.id),
        GraphKind::Tool,
        flat(&call.label, 80),
        status,
    );
    node.started_at = call.started_at;
    node.ended_at = match call.ended_at {
        Some(e) => Some(e),
        None if status == GraphStatus::Running => None,
        None => Some(now),
    };
    node.note = note;
    node.details = details;
    node
}

fn cell_node(cell: &RlmRootCell, index: usize) -> GraphNode {
    let status = match cell.status {
        CellStatus::Running => GraphStatus::Running,
        CellStatus::Ok => GraphStatus::Done,
        CellStatus::Error => GraphStatus::Failed,
    };
    let code: Vec<String> = cell
        .code
        .split('\n')
        .map(|l| l.replace('\t', "  ").trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let mut details = vec![d("call", &cell.tool_call_id)];
    for l in code.iter().take(6) {
        details.push(d("code", l));
    }
    if code.len() > 6 {
        details.push(d("code", format!("… {} more lines", code.len() - 6)));
    }
    if let Some(o) = cell.output.as_deref().filter(|o| !o.is_empty()) {
        details.push(d(
            if cell.status == CellStatus::Error {
                "error"
            } else {
                "output"
            },
            flat(o, 400),
        ));
    }
    let mut node = GraphNode::new(
        format!("cell:{}", cell.tool_call_id),
        GraphKind::Cell,
        format!("cell {}  {}", index + 1, flat(&first_code_line(&cell.code), 100)),
        status,
    );
    node.started_at = cell.started_at;
    node.ended_at = cell.ended_at;
    if cell.status == CellStatus::Error
        && let Some(o) = cell.output.as_deref().filter(|o| !o.is_empty())
    {
        node.note = Some(flat(o.split('\n').next_back().unwrap_or(""), 100));
    }
    node.details = details;
    node
}

/// Stable chronological sort, recursively.
fn sort_chronological(nodes: &mut [GraphNode]) {
    let key = |n: &GraphNode| n.started_at.unwrap_or(9_007_199_254_740_991.0);
    nodes.sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal));
    for n in nodes {
        sort_chronological(&mut n.children);
    }
}

/// `buildRlmGraph`.
pub fn build_rlm_graph(snapshot: &RlmSnapshot) -> GraphNode {
    let now = snapshot.now;
    let turn_start = snapshot.turn.as_ref().and_then(|t| t.started_at);
    let frame_task_ids: HashSet<&str> = snapshot.frames.iter().filter_map(|f| f.task_id.as_str()).collect();

    // Arena: nodes and their children by index.
    let mut arena: Vec<GraphNode> = Vec::new();
    let mut kids: Vec<Vec<usize>> = Vec::new();
    let add = |arena: &mut Vec<GraphNode>, kids: &mut Vec<Vec<usize>>, node: GraphNode| {
        arena.push(node);
        kids.push(Vec::new());
        arena.len() - 1
    };
    let mut items: Vec<(usize, Option<String>, Option<f64>, bool)> = Vec::new();

    // Frame groups, in first-seen order.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&RlmFrame>> = HashMap::new();
    for f in &snapshot.frames {
        let key = f.budget.as_ref().map_or(f.id.clone(), |b| b.id.clone());
        if !groups.contains_key(&key) {
            group_order.push(key.clone());
        }
        groups.entry(key).or_default().push(f);
    }
    let mut frame_group_of: HashMap<String, usize> = HashMap::new();
    for key in &group_order {
        let frames = &groups[key];
        let node = frame_group_node(key, frames);
        let at = node.started_at;
        let index = add(&mut arena, &mut kids, node);
        for f in frames {
            if let Some(id) = f.task_id.as_str() {
                frame_group_of.insert(id.to_string(), index);
            }
        }
        let caller = frames[0]
            .caller_task_id
            .as_str()
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        items.push((index, caller, at, false));
    }

    // Frame tasks the frame listing does not cover fan out per parent.
    let mut orphan_order: Vec<String> = Vec::new();
    let mut orphans: HashMap<String, Vec<&RlmTask>> = HashMap::new();
    let mut tasks: Vec<&RlmTask> = Vec::new();
    for t in &snapshot.tasks {
        if t.definition == "rlm-frame@1" {
            if !frame_task_ids.contains(t.id.as_str()) {
                let parent = t.parent_id.clone().unwrap_or_default();
                if !orphans.contains_key(&parent) {
                    orphan_order.push(parent.clone());
                }
                orphans.entry(parent).or_default().push(t);
            }
            continue;
        }
        tasks.push(t);
    }
    for parent in &orphan_order {
        let list = &orphans[parent];
        let statuses: Vec<GraphStatus> = list.iter().map(|t| task_status(&t.state)).collect();
        let progress = count(&statuses, statuses.len());
        let started = min_defined(
            list.iter()
                .map(|t| t.started_at.or(snapshot.timing.get(&t.id).and_then(|x| x.started_at))),
        );
        let mut node = GraphNode::new(
            format!(
                "frametasks:{}",
                if parent.is_empty() { "root" } else { parent.as_str() }
            ),
            GraphKind::Fanout,
            format!("frames {}", list.len()),
            rollup(&progress),
        );
        node.started_at = started;
        node.progress = Some(progress);
        node.details = vec![
            d("frames", format!("{} rlm-frame tasks", list.len())),
            d("traces", "await rlm.frames()"),
        ];
        let index = add(&mut arena, &mut kids, node);
        items.push((index, (!parent.is_empty()).then(|| parent.clone()), started, false));
    }

    // Tasks, with workflow members grouped under one workflow node per run.
    let mut task_nodes: HashMap<String, usize> = HashMap::new();
    let mut workflow_order: Vec<String> = Vec::new();
    let mut workflows: HashMap<String, (Vec<&RlmTask>, Vec<usize>)> = HashMap::new();
    for t in &tasks {
        let node = task_node(t, snapshot);
        let at = node.started_at;
        let index = add(&mut arena, &mut kids, node);
        task_nodes.insert(t.id.clone(), index);
        if let Some(w) = &t.workflow {
            if !workflows.contains_key(&w.run) {
                workflow_order.push(w.run.clone());
            }
            let group = workflows.entry(w.run.clone()).or_default();
            group.0.push(t);
            group.1.push(index);
            continue;
        }
        items.push((index, t.parent_id.clone().filter(|p| !p.is_empty()), at, false));
    }
    for run in &workflow_order {
        let (group_tasks, group_nodes) = &workflows[run];
        let statuses: Vec<GraphStatus> = group_nodes.iter().map(|&i| arena[i].status).collect();
        let progress = count(&statuses, statuses.len());
        let started = min_defined(group_nodes.iter().map(|&i| arena[i].started_at));
        let status = rollup(&progress);
        let ended = if is_finished(status) {
            max_defined(group_nodes.iter().map(|&i| arena[i].ended_at))
        } else {
            None
        };
        let joined: Vec<&&RlmTask> = group_tasks
            .iter()
            .filter(|t| t.workflow.as_ref().is_some_and(|w| w.depends_on.len() > 1))
            .collect();
        let joins = joined.len();
        let mut node = GraphNode::new(
            format!("workflow:{run}"),
            GraphKind::Workflow,
            format!(
                "workflow {} {} nodes",
                run.strip_prefix("wf-").unwrap_or(run),
                group_nodes.len()
            ),
            status,
        );
        node.started_at = started;
        node.ended_at = ended;
        node.tokens = sum(group_nodes.iter().map(|&i| arena[i].tokens));
        node.cost = sum(group_nodes.iter().map(|&i| arena[i].cost));
        node.progress = Some(progress);
        if joins > 0 {
            node.note = Some(format!("{joins} join{}", if joins == 1 { "" } else { "s" }));
        }
        let join_text = joined
            .iter()
            .map(|t| {
                let w = t.workflow.as_ref().expect("member");
                format!("{} ⇐ {} ({})", w.node, w.depends_on.join("+"), w.join)
            })
            .collect::<Vec<_>>()
            .join("; ");
        node.details = vec![
            d("run", run.as_str()),
            d(
                "nodes",
                group_tasks
                    .iter()
                    .map(|t| t.workflow.as_ref().map(|w| w.node.clone()).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            d(
                "joins",
                if join_text.is_empty() {
                    "none".to_string()
                } else {
                    join_text
                },
            ),
            d("fetch", "the workflows.run(...) call returns every node's outcome"),
        ];
        let index = add(&mut arena, &mut kids, node);
        kids[index] = group_nodes.clone();
        let parent = group_tasks[0].parent_id.clone().filter(|p| !p.is_empty());
        items.push((index, parent, started, false));
    }

    // Jobs and tool calls belong to the lane that started them.
    let mut lane_task: HashMap<String, String> = HashMap::new();
    for t in &tasks {
        if let Some(lane) = t.lane.as_deref().filter(|l| !l.is_empty()) {
            lane_task.insert(lane.to_string(), t.id.clone());
        }
    }
    let owner_of = |lane: &Option<String>| {
        lane.as_deref()
            .filter(|l| !l.is_empty() && *l != "main")
            .and_then(|l| lane_task.get(l).cloned())
    };
    for job in &snapshot.jobs {
        let index = add(&mut arena, &mut kids, job_node(job, now));
        items.push((index, owner_of(&job.lane), job.started_at, false));
    }
    for call in &snapshot.tool_calls {
        let index = add(&mut arena, &mut kids, tool_call_node(call, now));
        items.push((index, owner_of(&call.lane), call.started_at, call.native));
    }

    // A parent chain that loops is cut.
    let parent_of: HashMap<&str, Option<&str>> =
        tasks.iter().map(|t| (t.id.as_str(), t.parent_id.as_deref())).collect();
    let in_cycle = |id: &str| {
        let mut seen = HashSet::new();
        let mut current = Some(id);
        while let Some(c) = current {
            if !seen.insert(c) {
                return true;
            }
            current = parent_of.get(c).copied().flatten();
        }
        false
    };
    for item in &mut items {
        let key = &arena[item.0].key;
        if let Some(id) = key.strip_prefix("task:")
            && item.1.is_some()
            && in_cycle(id)
        {
            item.1 = None;
        }
    }

    // Attach by parent task (or the frame group of a nested frame's task), else to a root cell by time.
    let cell_list: Vec<RlmRootCell> = if !snapshot.cells.is_empty() {
        snapshot.cells.clone()
    } else {
        snapshot.root_cell.clone().into_iter().collect()
    };
    let cell_indexes: Vec<usize> = cell_list
        .iter()
        .enumerate()
        .map(|(i, c)| add(&mut arena, &mut kids, cell_node(c, i)))
        .collect();
    let mut turn_children: Vec<usize> = Vec::new();
    let mut earlier: Vec<usize> = Vec::new();
    for (index, parent_task, at, outside) in &items {
        if let Some(parent) = parent_task {
            let target = task_nodes
                .get(parent)
                .copied()
                .or_else(|| frame_group_of.get(parent).copied());
            if let Some(p) = target
                && p != *index
            {
                kids[p].push(*index);
                continue;
            }
        }
        let cell = match at {
            Some(at) if !outside => cell_list.iter().position(|c| {
                c.started_at
                    .is_some_and(|s| *at >= s - 250.0 && *at <= c.ended_at.unwrap_or(now) + 250.0)
            }),
            _ => None,
        };
        if let Some(c) = cell {
            kids[cell_indexes[c]].push(*index);
            continue;
        }
        let current = turn_start.is_none() || at.is_some_and(|a| a >= turn_start.unwrap_or(0.0) - 1000.0);
        if current || !is_finished(arena[*index].status) {
            turn_children.push(*index);
        } else {
            earlier.push(*index);
        }
    }

    // Materialize the tree (a node is placed once; repeated placement is cut).
    fn build(i: usize, arena: &[GraphNode], kids: &[Vec<usize>], placed: &mut HashSet<usize>) -> GraphNode {
        placed.insert(i);
        let mut node = arena[i].clone();
        node.children = kids[i]
            .iter()
            .filter(|c| !placed.contains(c))
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|c| build(c, arena, kids, placed))
            .collect();
        node
    }
    let mut placed = HashSet::new();
    let mut children: Vec<GraphNode> = Vec::new();
    if !earlier.is_empty() {
        let mut nodes: Vec<GraphNode> = earlier.iter().map(|&i| build(i, &arena, &kids, &mut placed)).collect();
        sort_chronological(&mut nodes);
        let statuses: Vec<GraphStatus> = nodes.iter().map(|n| n.status).collect();
        let mut node = GraphNode::new(
            "earlier".into(),
            GraphKind::Earlier,
            format!("earlier turns · {} finished", nodes.len()),
            rollup(&count(&statuses, statuses.len())),
        );
        node.details = vec![d("items", format!("{} nodes started before this turn", nodes.len()))];
        node.children = nodes;
        children.push(node);
    }
    let mut cell_nodes: Vec<GraphNode> = cell_indexes
        .iter()
        .map(|&i| build(i, &arena, &kids, &mut placed))
        .collect();
    for c in &mut cell_nodes {
        sort_chronological(&mut c.children);
    }
    let mut loose: Vec<GraphNode> = turn_children
        .iter()
        .map(|&i| build(i, &arena, &kids, &mut placed))
        .collect();
    sort_chronological(&mut loose);

    let statuses: Vec<GraphStatus> = cell_nodes.iter().chain(loose.iter()).map(|n| n.status).collect();
    let active = statuses
        .iter()
        .any(|s| matches!(s, GraphStatus::Running | GraphStatus::Pending));
    let usage = snapshot.usage.as_ref();
    let turn_started = turn_start.or(usage.and_then(|u| u.started_at));
    let last_end = max_defined(cell_nodes.iter().chain(loose.iter()).map(|n| n.ended_at));
    let tokens = usage.and_then(|u| {
        u.turn_tokens
            .filter(|t| *t != 0.0)
            .or(u.usage_total_tokens.filter(|t| *t != 0.0))
    });
    let cost = usage.and_then(|u| u.spent_usd.or(u.usage_cost));
    let failed = statuses.contains(&GraphStatus::Failed);
    let mut details = Vec::new();
    if let Some(p) = snapshot
        .turn
        .as_ref()
        .and_then(|t| t.prompt.as_deref())
        .filter(|p| !p.is_empty())
    {
        details.push(d("prompt", flat(p, 300)));
    }
    if let Some(r) = usage.and_then(|u| u.root_id.as_deref()).filter(|r| !r.is_empty()) {
        details.push(d("root", r));
    }
    details.push(d("cells", cell_nodes.len().to_string()));
    if let Some(t) = snapshot.truncated_tasks.filter(|t| *t != 0.0) {
        details.push(d("listing", format!("{} older tasks not listed", js_num(t))));
    }
    let prompt = snapshot
        .turn
        .as_ref()
        .and_then(|t| t.prompt.as_deref())
        .filter(|p| !p.is_empty());
    let mut turn = GraphNode::new(
        "turn".into(),
        GraphKind::Turn,
        match prompt {
            Some(p) => format!("turn  “{}”", flat(p, 60)),
            None => "turn".into(),
        },
        if active {
            GraphStatus::Running
        } else if failed {
            GraphStatus::Failed
        } else if statuses.is_empty() {
            GraphStatus::Pending
        } else {
            GraphStatus::Done
        },
    );
    turn.started_at = turn_started;
    turn.ended_at = if active { None } else { last_end };
    turn.tokens = tokens;
    turn.cost = cost.filter(|c| *c > 0.0);
    turn.details = details;
    children.extend(cell_nodes);
    children.extend(loose);
    turn.children = children;
    turn
}

// ---- layout ----

#[derive(Clone, Debug, PartialEq)]
pub struct GraphRow<'a> {
    pub node: &'a GraphNode,
    pub depth: usize,
    /// For each ancestor level below the root: whether that ancestor was the last child (no rail).
    pub rails: Vec<bool>,
    pub last: bool,
    pub collapsed: bool,
    /// Descendants hidden by collapse.
    pub hidden: usize,
    pub parent_key: Option<String>,
}

/// Collapse overrides by node key: true collapsed, false expanded; absent means automatic.
pub type CollapseState = HashMap<String, bool>;

/// Finished subtrees fold, unless something under them still runs.
pub fn default_collapsed(node: &GraphNode) -> bool {
    if node.children.is_empty() || node.kind == GraphKind::Turn {
        return false;
    }
    if node.kind == GraphKind::Earlier {
        return true;
    }
    is_finished(node.status) && !has_live_descendant(node)
}

fn has_live_descendant(node: &GraphNode) -> bool {
    node.children
        .iter()
        .any(|c| !is_finished(c.status) || has_live_descendant(c))
}

fn descendants(node: &GraphNode) -> usize {
    node.children.iter().map(|c| 1 + descendants(c)).sum()
}

/// `layoutGraph`.
pub fn layout_graph<'a>(root: &'a GraphNode, collapse: &CollapseState) -> Vec<GraphRow<'a>> {
    fn visit<'a>(
        node: &'a GraphNode,
        depth: usize,
        rails: Vec<bool>,
        last: bool,
        parent: Option<&str>,
        collapse: &CollapseState,
        rows: &mut Vec<GraphRow<'a>>,
    ) {
        let collapsed = !node.children.is_empty()
            && collapse
                .get(&node.key)
                .copied()
                .unwrap_or_else(|| default_collapsed(node));
        rows.push(GraphRow {
            node,
            depth,
            rails: rails.clone(),
            last,
            collapsed,
            hidden: if collapsed { descendants(node) } else { 0 },
            parent_key: parent.map(str::to_string),
        });
        if collapsed {
            return;
        }
        let n = node.children.len();
        for (i, child) in node.children.iter().enumerate() {
            let child_rails = if depth == 0 {
                Vec::new()
            } else {
                let mut r = rails.clone();
                r.push(last);
                r
            };
            visit(
                child,
                depth + 1,
                child_rails,
                i == n - 1,
                Some(&node.key),
                collapse,
                rows,
            );
        }
    }
    let mut rows = Vec::new();
    visit(root, 0, Vec::new(), true, None, collapse, &mut rows);
    rows
}

// ---- rendering helpers ----

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_glyph(frame: i64) -> &'static str {
    SPINNER[(frame.unsigned_abs() % 10) as usize]
}

pub fn status_glyph(status: GraphStatus, spinner: &str) -> String {
    match status {
        GraphStatus::Running => spinner.to_string(),
        GraphStatus::Pending => "○".into(),
        GraphStatus::Done => "✓".into(),
        GraphStatus::Failed => "✗".into(),
        GraphStatus::Cancelled => "⊘".into(),
        GraphStatus::Incomplete => "◐".into(),
    }
}

pub fn status_color(status: GraphStatus) -> Color {
    match status {
        GraphStatus::Running => Color::Accent,
        GraphStatus::Pending => Color::Muted,
        GraphStatus::Done => Color::Success,
        GraphStatus::Failed => Color::Error,
        GraphStatus::Cancelled => Color::Dim,
        GraphStatus::Incomplete => Color::Warning,
    }
}

/// `formatTokens`.
pub fn format_tokens(tokens: f64) -> String {
    if tokens < 1000.0 {
        js_num(tokens.round())
    } else if tokens < 10_000.0 {
        format!("{}k", js_to_fixed(tokens / 1000.0, 1))
    } else if tokens < 1_000_000.0 {
        format!("{}k", js_num((tokens / 1000.0).round()))
    } else {
        format!("{}M", js_to_fixed(tokens / 1_000_000.0, 1))
    }
}

/// `formatCost`.
pub fn format_cost(cost: f64) -> String {
    if cost >= 0.01 {
        format!("${}", js_to_fixed(cost, 2))
    } else {
        format!("${}", js_to_fixed(cost, 3))
    }
}

/// `formatBytes`.
pub fn format_bytes(bytes: f64) -> String {
    let mib = bytes / (1024.0 * 1024.0);
    if mib >= 1024.0 {
        format!("{} GiB", js_to_fixed(mib / 1024.0, 1))
    } else {
        format!("{} MiB", js_num(mib.round()))
    }
}

/// `▰▰▰▱▱` filled in proportion; at least one cell shows any nonzero progress.
pub fn bar(fraction: f64, cells: usize) -> String {
    let clamped = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut filled = (clamped * cells as f64).round() as usize;
    if filled == 0 && clamped > 0.0 {
        filled = 1;
    }
    if filled == cells && clamped < 1.0 {
        filled = cells.saturating_sub(1);
    }
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells.saturating_sub(filled)))
}

const MAX_PLAUSIBLE_MS: f64 = 7.0 * 24.0 * 60.0 * 60_000.0;

/// Elapsed time of a node (running: until `now`).
pub fn elapsed(started_at: Option<f64>, ended_at: Option<f64>, status: GraphStatus, now: f64) -> Option<String> {
    let start = started_at?;
    let end = match ended_at {
        Some(e) => e,
        None if is_finished(status) => return None,
        None => now,
    };
    let duration = (end - start).max(0.0);
    (duration <= MAX_PLAUSIBLE_MS).then(|| format_duration(duration))
}

fn node_elapsed(node: &GraphNode, now: f64) -> Option<String> {
    elapsed(node.started_at, node.ended_at, node.status, now)
}

fn meta(node: &GraphNode, now: f64, compact: bool) -> String {
    let mut parts = Vec::new();
    if let Some(t) = node_elapsed(node, now) {
        parts.push(t);
    }
    if !compact && let Some(t) = node.tokens.filter(|t| *t > 0.0) {
        parts.push(format!("{} tok", format_tokens(t)));
    }
    if !compact && let Some(c) = node.cost.filter(|c| *c > 0.0) {
        parts.push(format_cost(c));
    }
    parts.join(" ")
}

fn progress_text(p: &GraphProgress, cells: usize, style: &RlmStyle) -> String {
    let finished = p.done + p.incomplete + p.failed;
    let mut parts = vec![format!(
        "{} {finished}/{}",
        style.fg(
            if p.failed + p.incomplete > 0 {
                Color::Warning
            } else {
                Color::Success
            },
            &bar(finished as f64 / p.total.max(1) as f64, cells)
        ),
        p.total
    )];
    if p.running > 0 {
        parts.push(format!("{} running", p.running));
    }
    if p.incomplete > 0 {
        parts.push(style.fg(Color::Warning, &format!("{} incomplete", p.incomplete)));
    }
    if p.failed > 0 {
        parts.push(style.fg(Color::Error, &format!("{} failed", p.failed)));
    }
    parts.join(" · ")
}

/// How rows are drawn.
#[derive(Clone)]
pub struct RowRenderOptions {
    pub style: RlmStyle,
    pub now: f64,
    pub spinner: String,
    /// Key of the selected node (focus mode): drawn with a marker.
    pub selected_key: Option<String>,
    /// Reserve a two-column selection gutter (focus mode).
    pub gutter: bool,
}

impl RowRenderOptions {
    pub fn new(style: RlmStyle, now: f64, spinner: &str) -> RowRenderOptions {
        RowRenderOptions {
            style,
            now,
            spinner: spinner.to_string(),
            selected_key: None,
            gutter: false,
        }
    }
}

/// The connector prefix of a row; deep trees compress rails to one column per level on narrow widths.
fn connector(row: &GraphRow, width: usize) -> String {
    if row.depth == 0 {
        return String::new();
    }
    let compress = row.depth * 3 > 12.max(width / 3);
    let (rail, gap) = if compress { ("│", " ") } else { ("│  ", "   ") };
    let tee = match (compress, row.last) {
        (true, true) => "└",
        (true, false) => "├",
        (false, true) => "└─ ",
        (false, false) => "├─ ",
    };
    let mut out: String = row.rails.iter().map(|last| if *last { gap } else { rail }).collect();
    out.push_str(tee);
    out
}

/// One graph row, fitted to `width`.
pub fn render_graph_row(row: &GraphRow, width: usize, o: &RowRenderOptions) -> String {
    let style = &o.style;
    let node = row.node;
    let narrow = width < 60;
    let selected = o.selected_key.as_deref() == Some(node.key.as_str());
    let gutter = if o.gutter {
        if selected {
            style.fg(Color::Accent, "▶ ")
        } else {
            "  ".into()
        }
    } else {
        String::new()
    };
    let color = status_color(node.status);
    let glyph = if node.kind == GraphKind::Turn {
        style.fg(
            color,
            if node.status == GraphStatus::Running {
                &o.spinner
            } else {
                "◆"
            },
        )
    } else {
        style.fg(color, &status_glyph(node.status, &o.spinner))
    };
    let label_color = match node.kind {
        GraphKind::Earlier => Color::Muted,
        GraphKind::Turn | GraphKind::Cell => Color::ToolTitle,
        _ => Color::Text,
    };
    let label = if selected {
        style.bold(&style.fg(label_color, &node.label))
    } else {
        style.fg(label_color, &node.label)
    };
    let fold = if row.collapsed {
        format!("{} ", style.fg(Color::Dim, &format!("[+{}]", row.hidden)))
    } else {
        String::new()
    };
    let mut parts = vec![format!("{glyph} {fold}{label}")];
    if let Some(p) = &node.progress
        && matches!(node.kind, GraphKind::Fanout | GraphKind::Workflow)
    {
        parts.push(progress_text(p, if narrow { 5 } else { 10 }, style));
    }
    if let Some(j) = &node.join {
        parts.push(style.fg(Color::Accent, j));
    }
    if let Some(n) = node.note.as_deref().filter(|n| !n.is_empty())
        && !narrow
    {
        parts.push(style.fg(
            match node.status {
                GraphStatus::Failed => Color::Error,
                GraphStatus::Incomplete => Color::Warning,
                _ => Color::Muted,
            },
            n,
        ));
    }
    let left = format!(
        "{gutter}{}{}",
        style.fg(Color::Dim, &connector(row, width)),
        parts.join(" ")
    );
    let right = meta(node, o.now, narrow);
    if right.is_empty() {
        return truncate_to_width(&left, width, "…", false);
    }
    let right_width = visible_width(&right);
    let room = width as isize - right_width as isize - 1;
    if room < 12 {
        return truncate_to_width(&format!("{left} {}", style.fg(Color::Dim, &right)), width, "…", false);
    }
    let fitted = truncate_to_width(&left, room as usize, "…", false);
    let pad = (width as isize - visible_width(&fitted) as isize - right_width as isize).max(1) as usize;
    format!("{fitted}{}{}", " ".repeat(pad), style.fg(Color::Dim, &right))
}

/// Detail lines for an expanded node, indented under it with a dotted rail.
pub fn render_details(
    row: &GraphRow,
    extra: Option<&[(String, String)]>,
    width: usize,
    o: &RowRenderOptions,
    max_lines: usize,
) -> Vec<String> {
    let style = &o.style;
    let indent = format!(
        "{}{}",
        if o.gutter { "  " } else { "" },
        " ".repeat((row.depth * 3 + 2).min(width / 3))
    );
    let entries: Vec<&(String, String)> = row.node.details.iter().chain(extra.unwrap_or(&[]).iter()).collect();
    let mut lines = Vec::new();
    for (label, value) in entries.iter().take(max_lines).map(|e| (&e.0, &e.1)) {
        let color = match label.as_str() {
            "error" | "failed" => Color::Error,
            "fetch" | "traces" => Color::Accent,
            _ => Color::Muted,
        };
        let padded = format!("{label:<9}");
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}{} {}",
                style.fg(Color::Dim, "┆ "),
                style.fg(Color::Dim, &padded),
                style.fg(color, value)
            ),
            width,
            "…",
            false,
        ));
    }
    if entries.len() > max_lines {
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}",
                style.fg(Color::Dim, &format!("┆ … {} more", entries.len() - max_lines))
            ),
            width,
            "…",
            false,
        ));
    }
    lines
}

/// The rows to show when there are more than `max`: a window keeping `focus` in view.
pub fn window_rows(total: usize, max: usize, focus: usize) -> (usize, usize) {
    if total <= max {
        return (0, total);
    }
    let size = max.max(1);
    let clamped = focus.min(total - 1);
    let mut start = (clamped as isize - size as isize + 2).max(0) as usize;
    if clamped < size - 1 {
        start = 0;
    }
    start = start.min(total - size);
    (start, start + size)
}

// ---- gauges and the kernel strip ----

#[derive(Clone, Debug, PartialEq)]
pub struct Gauge {
    pub label: String,
    pub used: f64,
    pub limit: Option<f64>,
    pub text: String,
}

/// `budgetGauges`.
pub fn budget_gauges(snapshot: &RlmSnapshot, _graph: &GraphNode) -> Vec<Gauge> {
    let mut gauges = Vec::new();
    let usage = snapshot.usage.as_ref();
    let limits = snapshot.limits.as_ref();
    if let Some(admitted) = usage.and_then(|u| u.admitted_tasks) {
        let max = limits.and_then(|l| l.max_admitted_tasks);
        gauges.push(Gauge {
            label: "tasks".into(),
            used: admitted,
            limit: max,
            text: match max {
                None => js_num(admitted),
                Some(m) => format!("{}/{}", js_num(admitted), js_num(m)),
            },
        });
    }
    if let (Some(max_wall), Some(remaining)) = (
        limits.and_then(|l| l.max_wall_ms),
        usage.and_then(|u| u.remaining_wall_ms),
    ) {
        let used = (max_wall - remaining).max(0.0);
        gauges.push(Gauge {
            label: "wall".into(),
            used,
            limit: Some(max_wall),
            text: format!("{}/{}", format_duration(used), format_duration(max_wall)),
        });
    }
    let token_limit = limits
        .and_then(|l| l.max_total_tokens)
        .or(usage.and_then(|u| u.max_total_tokens));
    let tokens = usage
        .and_then(|u| {
            u.turn_tokens
                .filter(|t| *t != 0.0)
                .or(u.usage_total_tokens.filter(|t| *t != 0.0))
        })
        .unwrap_or(0.0);
    if tokens > 0.0 || token_limit.is_some() {
        gauges.push(Gauge {
            label: "tokens".into(),
            used: tokens,
            limit: token_limit,
            text: match token_limit {
                None => format_tokens(tokens),
                Some(l) => format!("{}/{}", format_tokens(tokens), format_tokens(l)),
            },
        });
    }
    let turn_limit = limits
        .and_then(|l| l.max_total_turns)
        .or(usage.and_then(|u| u.max_total_turns));
    if let (Some(limit), Some(turns)) = (turn_limit, usage.and_then(|u| u.turns)) {
        gauges.push(Gauge {
            label: "turns".into(),
            used: turns,
            limit: Some(limit),
            text: format!("{}/{}", js_num(turns), js_num(limit)),
        });
    }
    let spent = usage.and_then(|u| u.spent_usd.or(u.usage_cost)).unwrap_or(0.0);
    let cost_limit = usage
        .and_then(|u| u.max_cost_usd)
        .or(limits.and_then(|l| l.max_cost_usd));
    if spent > 0.0 || cost_limit.is_some() {
        gauges.push(Gauge {
            label: "cost".into(),
            used: spent,
            limit: cost_limit,
            text: match cost_limit {
                None => format_cost(spent),
                Some(l) => format!("{}/{}", format_cost(spent), format_cost(l)),
            },
        });
    }
    // The frame budget of the newest active (else newest) inference call.
    let mut order: Vec<String> = Vec::new();
    let mut by_budget: HashMap<String, Vec<&RlmFrame>> = HashMap::new();
    for f in &snapshot.frames {
        let Some(b) = &f.budget else { continue };
        if !by_budget.contains_key(&b.id) {
            order.push(b.id.clone());
        }
        by_budget.entry(b.id.clone()).or_default().push(f);
    }
    let mut candidates: Vec<&Vec<&RlmFrame>> = order.iter().map(|k| &by_budget[k]).collect();
    let running = |fs: &Vec<&RlmFrame>| fs.iter().any(|f| f.status == "running");
    let newest = |fs: &Vec<&RlmFrame>| max_defined(fs.iter().map(|f| f.started_at)).unwrap_or(0.0);
    candidates.sort_by(|l, r| {
        (running(r) as i32 - running(l) as i32)
            .cmp(&0)
            .then_with(|| newest(r).partial_cmp(&newest(l)).unwrap_or(std::cmp::Ordering::Equal))
    });
    if let Some(chosen) = candidates.first() {
        let budget = chosen[0].budget.as_ref().expect("budgeted");
        let calls = sum(chosen.iter().map(|f| Some(f.spent_calls))).unwrap_or(0.0);
        let frame_tokens = sum(chosen.iter().map(|f| Some(f.spent_tokens))).unwrap_or(0.0);
        gauges.push(Gauge {
            label: "calls".into(),
            used: calls,
            limit: budget.calls,
            text: match budget.calls {
                None => js_num(calls),
                Some(c) => format!("{}/{}", js_num(calls), js_num(c)),
            },
        });
        if let Some(t) = budget.tokens {
            gauges.push(Gauge {
                label: "frame tok".into(),
                used: frame_tokens,
                limit: Some(t),
                text: format!("{}/{}", format_tokens(frame_tokens), format_tokens(t)),
            });
        }
        gauges.push(Gauge {
            label: "depth".into(),
            used: budget.depth,
            limit: None,
            text: js_num(budget.depth),
        });
    }
    gauges
}

fn gauge_color(fraction: f64) -> Color {
    if fraction >= 0.85 {
        Color::Error
    } else if fraction >= 0.6 {
        Color::Warning
    } else {
        Color::Success
    }
}

/// Gauges packed into at most `max_lines` lines of `width`.
pub fn render_gauges(gauges: &[Gauge], width: usize, style: &RlmStyle, max_lines: usize) -> Vec<String> {
    let cells = if width < 50 { 4 } else { 6 };
    let rendered: Vec<String> = gauges
        .iter()
        .map(|g| match g.limit {
            Some(limit) if limit > 0.0 => {
                let fraction = g.used / limit;
                format!(
                    "{} {} {}",
                    style.fg(Color::Dim, &g.label),
                    style.fg(gauge_color(fraction), &bar(fraction, cells)),
                    style.fg(Color::Text, &g.text)
                )
            }
            _ => format!("{} {}", style.fg(Color::Dim, &g.label), style.fg(Color::Text, &g.text)),
        })
        .collect();
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in rendered {
        let candidate = if current.is_empty() {
            item.clone()
        } else {
            format!("{current}  {item}")
        };
        if visible_width(&candidate) <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(current);
            if lines.len() >= max_lines {
                current = String::new();
                break;
            }
            current = item;
        }
    }
    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines.iter().map(|l| truncate_to_width(l, width, "…", false)).collect()
}

fn format_guard_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{}ms", js_num(ms.round()))
    } else {
        format!("{}s", js_to_fixed(ms / 1000.0, 1))
    }
}

/// `kernels ■■◆□······ 3/16 · 1 busy · 1 pinned · 2 evicted · 212 MiB · loki 85ms turn/1.4s`.
pub fn render_kernel_strip(snapshot: &RlmSnapshot, width: usize, style: &RlmStyle) -> Option<String> {
    let pool = snapshot.pool.as_ref()?;
    let busy = pool.lanes.iter().filter(|l| l.running > 0.0).count();
    let pinned = pool.lanes.iter().filter(|l| !l.pinned_by.is_empty()).count();
    let mut parts = Vec::new();
    if pool.max_live <= 24.0 && width >= 50 {
        let cells: String = pool
            .lanes
            .iter()
            .map(|l| {
                if l.running > 0.0 {
                    style.fg(Color::Accent, "■")
                } else if !l.pinned_by.is_empty() {
                    style.fg(Color::Warning, "◆")
                } else {
                    style.fg(Color::Muted, "□")
                }
            })
            .collect();
        let free = (pool.max_live as isize - pool.lanes.len() as isize).max(0) as usize;
        parts.push(format!(
            "{} {cells}{} {}/{}",
            style.fg(Color::Dim, "kernels"),
            style.fg(Color::Dim, &"·".repeat(free)),
            js_num(pool.live),
            js_num(pool.max_live)
        ));
    } else {
        parts.push(format!(
            "{} {}/{}",
            style.fg(Color::Dim, "kernels"),
            js_num(pool.live),
            js_num(pool.max_live)
        ));
    }
    parts.push(format!("{busy} busy"));
    parts.push(format!("{pinned} pinned"));
    if let Some(e) = pool.evictions {
        parts.push(format!("{} evicted", js_num(e)));
    }
    if let Some(m) = pool.memory_bytes {
        let cap = match pool.memory_cap_bytes {
            Some(c) if width >= 100 => format!(" (cap {}/kernel)", format_bytes(c)),
            _ => String::new(),
        };
        parts.push(format!("mem {}{cap}", format_bytes(m)));
    }
    for g in pool.guards.iter().flatten() {
        if g.checks == 0.0 && g.after_checks == 0.0 {
            continue;
        }
        let blocked = if g.blocked > 0.0 {
            style.fg(Color::Warning, &format!(" {} blocked", js_num(g.blocked)))
        } else {
            String::new()
        };
        parts.push(format!(
            "{} {} turn/{}{blocked}",
            style.fg(Color::Dim, &g.name.to_lowercase()),
            format_guard_ms(g.turn_ms),
            format_guard_ms(g.total_ms)
        ));
    }
    Some(truncate_to_width(
        &parts.join(&style.fg(Color::Dim, " · ")),
        width,
        "…",
        false,
    ))
}

// ---- the three levels ----

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphTotals {
    pub running: usize,
    pub tasks: usize,
    pub failed: usize,
    pub jobs_running: usize,
    pub tools_running: usize,
    pub fanout_keys: Vec<String>,
    pub cell_key: Option<String>,
    pub nodes: usize,
}

pub fn graph_totals(root: &GraphNode) -> GraphTotals {
    let mut t = GraphTotals::default();
    fn visit(node: &GraphNode, t: &mut GraphTotals) {
        t.nodes += 1;
        if !matches!(node.kind, GraphKind::Turn | GraphKind::Earlier)
            && matches!(node.status, GraphStatus::Running | GraphStatus::Pending)
        {
            t.running += 1;
        }
        if matches!(node.kind, GraphKind::Task | GraphKind::Child | GraphKind::Background) {
            t.tasks += 1;
        }
        if node.status == GraphStatus::Failed && node.kind != GraphKind::Earlier {
            t.failed += 1;
        }
        if node.kind == GraphKind::Job && node.status == GraphStatus::Running {
            t.jobs_running += 1;
        }
        if node.kind == GraphKind::Tool && node.status == GraphStatus::Running {
            t.tools_running += 1;
        }
        if node.kind == GraphKind::Fanout {
            t.fanout_keys.push(node.key.clone());
        }
        if node.kind != GraphKind::Earlier {
            for c in &node.children {
                visit(c, t);
            }
        }
    }
    visit(root, &mut t);
    let cells: Vec<&GraphNode> = root.children.iter().filter(|c| c.kind == GraphKind::Cell).collect();
    t.cell_key = cells
        .iter()
        .rev()
        .find(|c| c.status == GraphStatus::Running)
        .or(cells.last())
        .map(|c| c.key.clone());
    t
}

fn find<'a>(root: &'a GraphNode, key: &str) -> Option<&'a GraphNode> {
    if root.key == key {
        return Some(root);
    }
    root.children.iter().find_map(|c| find(c, key))
}

fn count_active_tasks(root: &GraphNode) -> usize {
    let mut active = 0;
    fn visit(node: &GraphNode, active: &mut usize) {
        if matches!(node.kind, GraphKind::Task | GraphKind::Child | GraphKind::Background) && !is_finished(node.status)
        {
            *active += 1;
        }
        if node.kind != GraphKind::Earlier {
            for c in &node.children {
                visit(c, active);
            }
        }
    }
    visit(root, &mut active);
    active
}

/// True while anything in the graph runs.
pub fn graph_active(root: &GraphNode) -> bool {
    root.status == GraphStatus::Running || graph_totals(root).running > 0
}

/// How the views draw.
#[derive(Clone)]
pub struct RlmViewOptions {
    pub style: RlmStyle,
    pub spinner_frame: i64,
    /// Key hint for the focus view, e.g. "alt+g".
    pub focus_key: Option<String>,
    pub collapse: CollapseState,
}

impl Default for RlmViewOptions {
    fn default() -> Self {
        RlmViewOptions {
            style: RlmStyle::plain(),
            spinner_frame: 0,
            focus_key: None,
            collapse: CollapseState::new(),
        }
    }
}

/// Level (a): one line while the panel is hidden; `None` when nothing runs.
pub fn render_rlm_footer(snapshot: &RlmSnapshot, width: usize, o: &RlmViewOptions) -> Option<String> {
    let graph = build_rlm_graph(snapshot);
    let root_running = snapshot
        .root_cell
        .as_ref()
        .is_some_and(|c| c.status == CellStatus::Running);
    if !graph_active(&graph) && !root_running {
        return None;
    }
    let style = &o.style;
    let spinner = spinner_glyph(o.spinner_frame);
    let totals = graph_totals(&graph);
    let mut parts = Vec::new();
    let time = node_elapsed(&graph, snapshot.now);
    parts.push(format!("turn{}", time.map(|t| format!(" {t}")).unwrap_or_default()));
    if let Some(cell) = totals.cell_key.as_deref().and_then(|k| find(&graph, k)) {
        let index = graph
            .children
            .iter()
            .filter(|c| c.kind == GraphKind::Cell)
            .position(|c| c.key == cell.key)
            .map_or(0, |i| i + 1);
        let cell_time = node_elapsed(cell, snapshot.now);
        let suffix = match (cell.status, cell_time) {
            (GraphStatus::Running, Some(t)) => format!(" {t}"),
            _ => String::new(),
        };
        parts.push(format!("cell {index}{suffix}"));
    }
    let fanouts: Vec<&GraphNode> = totals.fanout_keys.iter().filter_map(|k| find(&graph, k)).collect();
    let fanout = fanouts
        .iter()
        .find(|n| n.status == GraphStatus::Running)
        .or(fanouts.last());
    if let Some(p) = fanout.and_then(|f| f.progress) {
        let finished = p.done + p.incomplete + p.failed;
        parts.push(format!(
            "map {} {finished}/{}",
            bar(finished as f64 / p.total.max(1) as f64, 5),
            p.total
        ));
    }
    if totals.tasks > 0 {
        let active = count_active_tasks(&graph);
        parts.push(format!(
            "{} task{}{}",
            totals.tasks,
            if totals.tasks == 1 { "" } else { "s" },
            if active > 0 {
                format!(" ({active} active)")
            } else {
                String::new()
            }
        ));
    }
    if totals.jobs_running > 0 {
        parts.push(format!(
            "{} job{}",
            totals.jobs_running,
            if totals.jobs_running == 1 { "" } else { "s" }
        ));
    }
    if totals.tools_running > 0 {
        parts.push(format!(
            "{} tool call{}",
            totals.tools_running,
            if totals.tools_running == 1 { "" } else { "s" }
        ));
    }
    if totals.failed > 0 {
        parts.push(style.fg(Color::Error, &format!("{} failed", totals.failed)));
    }
    if let Some(c) = graph.cost.filter(|c| *c > 0.0) {
        parts.push(format_cost(c));
    }
    let summary = format!(
        "{} {}",
        style.fg(Color::Accent, &format!("◆ rlm {spinner}")),
        style.fg(Color::Muted, &parts.join(" · "))
    );
    let hint = o
        .focus_key
        .as_ref()
        .map(|k| style.fg(Color::Dim, &format!(" · {k} graph")))
        .unwrap_or_default();
    let bound = width.max(1);
    if !hint.is_empty() && visible_width(&summary) + visible_width(&hint) <= bound {
        return Some(format!("{summary}{hint}"));
    }
    Some(truncate_to_width(&summary, bound, "…", false))
}

fn header_line(graph: &GraphNode, snapshot: &RlmSnapshot, style: &RlmStyle, spinner: &str, suffix: &str) -> String {
    let totals = graph_totals(graph);
    let state = if graph_active(graph) {
        style.fg(Color::Accent, &format!("{spinner} running"))
    } else if graph.status == GraphStatus::Failed {
        style.fg(Color::Error, "✗ failed")
    } else if graph.children.is_empty() {
        style.fg(Color::Dim, "idle")
    } else {
        style.fg(Color::Success, "✓ done")
    };
    let mut parts = vec![format!("{} nodes", totals.nodes - 1)];
    if totals.running > 0 {
        parts.push(format!("{} active", totals.running));
    }
    if let Some(e) = &snapshot.error {
        parts.push(style.fg(Color::Error, &format!("inspection failed: {}", flat(e, 80))));
    }
    format!(
        "{} {state} {}{suffix}",
        style.bold(&style.fg(Color::Accent, "RLM")),
        style.fg(Color::Muted, &parts.join(" · "))
    )
}

/// Level (b): the docked panel.
pub fn render_rlm_dock(snapshot: &RlmSnapshot, width: usize, o: &RlmViewOptions, max_rows: usize) -> Vec<String> {
    let style = &o.style;
    let spinner = spinner_glyph(o.spinner_frame);
    let bound = width.max(1);
    let graph = build_rlm_graph(snapshot);
    let hint = o
        .focus_key
        .as_ref()
        .map(|k| style.fg(Color::Dim, &format!("  {k} focus")))
        .unwrap_or_default();
    let mut lines = vec![truncate_to_width(
        &header_line(&graph, snapshot, style, spinner, &hint),
        bound,
        "…",
        false,
    )];
    lines.extend(render_gauges(&budget_gauges(snapshot, &graph), bound, style, 2));
    let all = layout_graph(&graph, &o.collapse);
    let row_options = RowRenderOptions::new(style.clone(), snapshot.now, spinner);
    lines.push(render_graph_row(&all[0], bound, &row_options));
    let rows = &all[1..];
    let max = 2.max(max_rows.saturating_sub(1));
    let focus = rows
        .iter()
        .rposition(|r| r.node.status == GraphStatus::Running)
        .unwrap_or(rows.len().saturating_sub(1));
    let (start, end) = if rows.is_empty() {
        (0, 0)
    } else {
        window_rows(rows.len(), max, focus)
    };
    if start > 0 {
        lines.push(style.fg(
            Color::Dim,
            &truncate_to_width(&format!("   ↑ {start} more"), bound, "…", false),
        ));
    }
    for row in &rows[start..end] {
        lines.push(render_graph_row(row, bound, &row_options));
    }
    if end < rows.len() {
        let below = &rows[end..];
        let active = below.iter().filter(|r| !is_finished(r.node.status)).count();
        lines.push(style.fg(
            Color::Dim,
            &truncate_to_width(
                &format!(
                    "  ↓ {} more{}",
                    below.len(),
                    if active > 0 {
                        format!(" ({active} active)")
                    } else {
                        String::new()
                    }
                ),
                bound,
                "…",
                false,
            ),
        ));
    }
    if let Some(strip) = render_kernel_strip(snapshot, bound, style) {
        lines.push(strip);
    }
    lines.extend(render_context_lines(snapshot.context.as_ref(), style));
    lines.iter().map(|l| truncate_to_width(l, bound, "…", false)).collect()
}

/// Frame trace (from `rlm.frames {id}`) as detail lines.
pub fn frame_trace_details(trace: &Value) -> Vec<(String, String)> {
    if !trace.is_object() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if let Some(m) = trace["model"].as_str() {
        lines.push(d("model", m));
    }
    if let Some(x) = trace["detail"].as_str() {
        lines.push(d("detail", flat(x, 300)));
    }
    if let Some(x) = trace["value"].as_str() {
        lines.push(d("value", flat(x, 400)));
    }
    if let Some(x) = trace["error"].as_str() {
        lines.push(d("error", flat(x, 400)));
    }
    if let Some(v) = trace["views"].as_array() {
        lines.push(d(
            "views",
            format!("{} context view{}", v.len(), if v.len() == 1 { "" } else { "s" }),
        ));
    }
    if let Some(a) = trace["attempts"].as_array() {
        lines.push(d("attempts", a.len().to_string()));
    }
    let r = &trace["remaining"];
    if r.is_object() {
        let show = |v: &Value, fallback: &str| match v {
            Value::Null => fallback.to_string(),
            Value::Number(n) => js_num(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        lines.push(d(
            "remaining",
            format!(
                "calls {} · tokens {} · depth {}",
                show(&r["calls"], "∞"),
                show(&r["tokens"], "∞"),
                show(&r["depth"], "?")
            ),
        ));
    }
    lines
}
