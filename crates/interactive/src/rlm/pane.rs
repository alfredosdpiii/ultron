//! The RLM pane (experimental/rlm-pane.ts): the run as dependency waves in the side pane (`app.rlm.pane`), and the
//! live wave summary above the editor. A run is the current root turn (each cell's work is one wave, a child's work
//! follows it) or one `workflows.run` call (waves are the topological levels of `dependsOn`).

use std::collections::{HashMap, HashSet};

use tui::keybindings::KeybindingsManager;
use tui::utils::{truncate_to_width, visible_width};

use super::graph::{
    GraphKind, GraphNode, GraphProgress, GraphStatus, bar, build_rlm_graph, count, is_finished, rollup, status_color,
    task_status,
};
use super::visualizer::{Color, RlmSnapshot, RlmStyle, RlmTask, format_duration, short_id};
use super::{flat, js_num};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DagNode {
    pub key: String,
    pub name: String,
    pub kind: Option<GraphKind>,
    pub status: Option<GraphStatus>,
    /// Keys of the nodes this one waits for, within its run.
    pub depends_on: Vec<String>,
    /// Shown when the node has no dependencies (e.g. "Start node", "cell 2").
    pub origin: String,
    pub prompt: Option<String>,
    pub category: String,
    pub model: Option<String>,
    pub text: Option<String>,
    pub note: Option<String>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub turns: Option<f64>,
    pub tool_calls: Option<f64>,
    pub progress: Option<GraphProgress>,
}

impl DagNode {
    pub fn status(&self) -> GraphStatus {
        self.status.unwrap_or(GraphStatus::Pending)
    }
    pub fn kind(&self) -> GraphKind {
        self.kind.unwrap_or(GraphKind::Task)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Turn,
    Workflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DagRun {
    /// "turn" or `workflow:<run>`.
    pub key: String,
    pub kind: RunKind,
    pub id: String,
    pub title: String,
    pub nodes: Vec<DagNode>,
    /// Node keys per wave, in display order.
    pub waves: Vec<Vec<String>>,
    /// Dependency edges as (from, to) node keys.
    pub edges: Vec<(String, String)>,
    pub status: GraphStatus,
    pub started_at: Option<f64>,
}

/// The prompt of a task input preview: the `prompt` (or `task`) of `{"prompt": ...}` (possibly cut), else the preview.
pub fn prompt_of(input: Option<&str>) -> Option<String> {
    let input = input?;
    if input.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::String(s)) => return Some(flat(&s, 200)),
        Ok(serde_json::Value::Object(map)) => {
            let prompt = map.get("prompt").filter(|v| !v.is_null()).or_else(|| map.get("task"));
            if let Some(serde_json::Value::String(p)) = prompt {
                return Some(flat(p, 200));
            }
        }
        Ok(_) => {}
        Err(_) => {
            for prefix in ["{\"prompt\":\"", "{\"task\":\""] {
                if let Some(rest) = input.strip_prefix(prefix) {
                    // `((?:[^"\\]|\\.)*)`
                    let mut raw = String::new();
                    let mut chars = rest.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' => match chars.next() {
                                Some(n) => {
                                    raw.push('\\');
                                    raw.push(n);
                                }
                                None => break,
                            },
                            c => raw.push(c),
                        }
                    }
                    let text = raw.replace("\\n", " ");
                    let mut out = String::new();
                    let mut it = text.chars();
                    while let Some(c) = it.next() {
                        if c == '\\' {
                            match it.next() {
                                Some(n) => out.push(n),
                                None => out.push('\\'),
                            }
                        } else {
                            out.push(c);
                        }
                    }
                    return Some(flat(&out, 200));
                }
            }
        }
    }
    Some(flat(input, 200))
}

fn progress_of(statuses: &[GraphStatus]) -> (GraphProgress, GraphStatus) {
    let progress = count(statuses, statuses.len());
    let status = if statuses.is_empty() {
        GraphStatus::Pending
    } else {
        rollup(&progress)
    };
    (progress, status)
}

/// Topological levels of `dependsOn` (unknown dependencies ignored; a cycle is cut where it closes).
pub fn compute_waves(nodes: &[(String, Vec<String>)]) -> Vec<Vec<String>> {
    let by_key: HashMap<&str, &Vec<String>> = nodes.iter().map(|(k, d)| (k.as_str(), d)).collect();
    let mut level: HashMap<String, usize> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();
    fn depth(
        key: &str,
        by_key: &HashMap<&str, &Vec<String>>,
        level: &mut HashMap<String, usize>,
        visiting: &mut HashSet<String>,
    ) -> usize {
        if let Some(l) = level.get(key) {
            return *l;
        }
        if visiting.contains(key) {
            return 0;
        }
        visiting.insert(key.to_string());
        let mut value = 0;
        for dep in by_key.get(key).copied().into_iter().flatten() {
            if by_key.contains_key(dep.as_str()) && dep != key {
                value = value.max(depth(dep, by_key, level, visiting) + 1);
            }
        }
        visiting.remove(key);
        level.insert(key.to_string(), value);
        value
    }
    let mut waves: Vec<Vec<String>> = Vec::new();
    for (key, _) in nodes {
        let index = depth(key, &by_key, &mut level, &mut visiting);
        while waves.len() <= index {
            waves.push(Vec::new());
        }
        waves[index].push(key.clone());
    }
    waves.into_iter().filter(|w| !w.is_empty()).collect()
}

fn task_fields(task: Option<&RlmTask>, node: &mut DagNode) {
    let Some(task) = task else { return };
    node.prompt = prompt_of(task.input.as_deref());
    node.model = task.model.clone();
    node.text = task.last_text.as_deref().map(|t| flat(t, 200));
    node.turns = task.turns;
    node.tool_calls = task.tool_call_count;
}

fn task_name(task: Option<&RlmTask>, node: &GraphNode) -> String {
    let Some(task) = task else { return node.label.clone() };
    let id = short_id(&task.id);
    match node.kind {
        GraphKind::Child => format!("child {id}"),
        GraphKind::Background => format!("background {id}"),
        _ => {
            let def = match task.definition.rfind('@') {
                Some(at)
                    if task.definition[at + 1..].chars().all(|c| c.is_ascii_digit())
                        && at + 1 < task.definition.len() =>
                {
                    &task.definition[..at]
                }
                _ => task.definition.as_str(),
            };
            format!("{def} {id}")
        }
    }
}

fn from_graph(node: &GraphNode, tasks: &HashMap<String, &RlmTask>, depends_on: Vec<String>, origin: &str) -> DagNode {
    let task = node.key.strip_prefix("task:").and_then(|id| tasks.get(id).copied());
    let mut dag = DagNode {
        key: node.key.clone(),
        kind: Some(node.kind),
        status: Some(node.status),
        depends_on,
        origin: origin.to_string(),
        started_at: node.started_at,
        ended_at: node.ended_at,
        note: node
            .note
            .as_ref()
            .map(|n| n.strip_prefix("→ ").unwrap_or(n).to_string()),
        progress: node.progress,
        ..DagNode::default()
    };
    match node.kind {
        GraphKind::Task | GraphKind::Child | GraphKind::Background => {
            dag.name = task_name(task, node);
            dag.category = task.map_or(node.kind.name().to_string(), |t| t.definition.clone());
            task_fields(task, &mut dag);
        }
        GraphKind::Fanout => {
            dag.name = node.label.clone();
            dag.category = "rlm.map".into();
            dag.prompt = node.note.clone();
        }
        GraphKind::Infer => {
            dag.name = node.label.clone();
            dag.category = "rlm.infer".into();
        }
        GraphKind::Job => {
            dag.name = node.label.clone();
            dag.category = "job".into();
            dag.prompt = Some(node.label.strip_prefix("bash ").unwrap_or(&node.label).to_string());
        }
        GraphKind::Workflow => {
            let name = match node.label.rfind(" nodes") {
                Some(at) if node.label.ends_with(" nodes") => {
                    let head = &node.label[..at];
                    match head.rfind(' ') {
                        Some(sp) if head[sp + 1..].chars().all(|c| c.is_ascii_digit()) && sp + 1 < head.len() => {
                            head[..sp].to_string()
                        }
                        _ => node.label.clone(),
                    }
                }
                _ => node.label.clone(),
            };
            dag.name = name;
            dag.category = "workflows.run".into();
            let n = node.children.len();
            dag.prompt = Some(format!("{n} node{}", if n == 1 { "" } else { "s" }));
        }
        _ => {
            dag.name = node.label.clone();
            dag.category = node.kind.name().to_string();
        }
    }
    dag
}

fn tools_node(key: &str, tools: &[&GraphNode], origin: &str) -> DagNode {
    let statuses: Vec<GraphStatus> = tools.iter().map(|t| t.status).collect();
    let (progress, status) = progress_of(&statuses);
    let mut names: Vec<String> = Vec::new();
    for t in tools {
        let name = t
            .details
            .iter()
            .find(|(l, _)| l == "tool")
            .map_or(t.label.clone(), |(_, v)| v.clone());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let starts: Vec<f64> = tools.iter().filter_map(|t| t.started_at).collect();
    let ends: Vec<f64> = tools.iter().filter_map(|t| t.ended_at).collect();
    DagNode {
        key: key.to_string(),
        name: format!("{} tool call{}", tools.len(), if tools.len() == 1 { "" } else { "s" }),
        kind: Some(GraphKind::Tools),
        status: Some(status),
        origin: origin.to_string(),
        category: "tools".into(),
        prompt: Some(names.join(", ")),
        progress: Some(progress),
        started_at: starts.iter().copied().reduce(f64::min),
        ended_at: if is_finished(status) {
            ends.iter().copied().reduce(f64::max)
        } else {
            None
        },
        ..DagNode::default()
    }
}

const MAX_NESTED: usize = 3;

fn turn_run(snapshot: &RlmSnapshot, graph: &GraphNode, tasks: &HashMap<String, &RlmTask>) -> Option<DagRun> {
    let mut nodes: Vec<DagNode> = Vec::new();
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut add_group = |children: &[&GraphNode], origin: &str, group_key: &str| {
        let tools: Vec<&GraphNode> = children.iter().copied().filter(|c| c.kind == GraphKind::Tool).collect();
        let others: Vec<&GraphNode> = children
            .iter()
            .copied()
            .filter(|c| c.kind != GraphKind::Tool && c.kind != GraphKind::Earlier)
            .collect();
        let mut level: Vec<(&GraphNode, DagNode)> = others
            .iter()
            .map(|n| (*n, from_graph(n, tasks, Vec::new(), origin)))
            .collect();
        let mut top: Vec<DagNode> = level.iter().map(|(_, d)| d.clone()).collect();
        if !tools.is_empty() {
            top.push(tools_node(&format!("tools:{group_key}"), &tools, origin));
        }
        if top.is_empty() {
            return;
        }
        waves.push(top.iter().map(|n| n.key.clone()).collect());
        nodes.extend(top);
        for _ in 0..MAX_NESTED {
            if level.is_empty() {
                break;
            }
            let mut next: Vec<(&GraphNode, DagNode)> = Vec::new();
            for (parent, parent_dag) in &level {
                if parent.kind == GraphKind::Workflow {
                    continue;
                }
                for child in &parent.children {
                    if child.kind == GraphKind::Tool || child.kind == GraphKind::Earlier {
                        continue;
                    }
                    let dag = from_graph(child, tasks, vec![parent_dag.key.clone()], origin);
                    edges.push((parent_dag.key.clone(), dag.key.clone()));
                    next.push((child, dag));
                }
            }
            if next.is_empty() {
                break;
            }
            nodes.extend(next.iter().map(|(_, d)| d.clone()));
            waves.push(next.iter().map(|(_, d)| d.key.clone()).collect());
            level = next;
        }
    };
    let cells: Vec<&GraphNode> = graph.children.iter().filter(|c| c.kind == GraphKind::Cell).collect();
    for (index, cell) in cells.iter().enumerate() {
        let children: Vec<&GraphNode> = cell.children.iter().collect();
        add_group(&children, &format!("cell {}", index + 1), &cell.key);
    }
    let loose: Vec<&GraphNode> = graph
        .children
        .iter()
        .filter(|c| c.kind != GraphKind::Cell && c.kind != GraphKind::Earlier)
        .collect();
    if !loose.is_empty() {
        add_group(&loose, "turn", "turn");
    }
    if nodes.is_empty() {
        return None;
    }
    let statuses: Vec<GraphStatus> = nodes.iter().map(DagNode::status).collect();
    let prompt = snapshot
        .turn
        .as_ref()
        .and_then(|t| t.prompt.as_deref())
        .filter(|p| !p.is_empty());
    Some(DagRun {
        key: "turn".into(),
        kind: RunKind::Turn,
        id: match snapshot
            .usage
            .as_ref()
            .and_then(|u| u.root_id.as_deref())
            .filter(|r| !r.is_empty())
        {
            Some(r) => short_id(r.strip_prefix("root-").unwrap_or(r)),
            None => "turn".into(),
        },
        title: prompt.map_or("current turn".to_string(), |p| flat(p, 120)),
        nodes,
        waves,
        edges,
        status: progress_of(&statuses).1,
        started_at: graph.started_at,
    })
}

fn ended_status(status: Option<&str>) -> GraphStatus {
    match status {
        Some("succeeded") => GraphStatus::Done,
        Some("skipped" | "cancelled") => GraphStatus::Cancelled,
        Some("exhausted" | "interrupted") => GraphStatus::Incomplete,
        None => GraphStatus::Pending,
        _ => GraphStatus::Failed,
    }
}

fn workflow_runs(snapshot: &RlmSnapshot) -> Vec<DagRun> {
    let mut member_order: Vec<String> = Vec::new();
    let mut members: HashMap<String, HashMap<String, &RlmTask>> = HashMap::new();
    let mut member_nodes: HashMap<String, Vec<String>> = HashMap::new();
    for task in &snapshot.tasks {
        let Some(w) = &task.workflow else { continue };
        if !members.contains_key(&w.run) {
            member_order.push(w.run.clone());
        }
        let run = members.entry(w.run.clone()).or_default();
        let order = member_nodes.entry(w.run.clone()).or_default();
        if !order.contains(&w.node) {
            order.push(w.node.clone());
        }
        run.insert(w.node.clone(), task);
    }
    let plans: HashMap<&str, &super::visualizer::RlmWorkflowRun> =
        snapshot.workflows.iter().map(|p| (p.run.as_str(), p)).collect();
    let mut plan_order: Vec<String> = Vec::new();
    for p in &snapshot.workflows {
        if !plan_order.contains(&p.run) {
            plan_order.push(p.run.clone());
        }
    }
    let order: Vec<String> = plan_order
        .iter()
        .cloned()
        .chain(member_order.iter().filter(|r| !plans.contains_key(r.as_str())).cloned())
        .collect();
    let turn_start = snapshot.turn.as_ref().and_then(|t| t.started_at);
    let empty = HashMap::new();
    let mut runs = Vec::new();
    for run in order {
        let plan = plans.get(run.as_str());
        let tasks = members.get(&run).unwrap_or(&empty);
        let planned: Vec<PlannedNode> = match plan {
            Some(p) => p
                .nodes
                .iter()
                .map(|n| {
                    (
                        n.id.clone(),
                        n.definition.clone(),
                        n.depends_on.clone(),
                        n.status.clone(),
                        n.reason.clone(),
                    )
                })
                .collect(),
            None => member_nodes[&run]
                .iter()
                .map(|node| {
                    let t = tasks[node];
                    let w = t.workflow.as_ref().expect("member");
                    (w.node.clone(), t.definition.clone(), w.depends_on.clone(), None, None)
                })
                .collect(),
        };
        let ids: HashSet<&str> = planned.iter().map(|p| p.0.as_str()).collect();
        let key_of = |id: &str| format!("wf:{run}:{id}");
        let nodes: Vec<DagNode> = planned
            .iter()
            .map(|(id, definition, depends_on, ended, reason)| {
                let task = tasks.get(id).copied();
                let timing = task.and_then(|t| snapshot.timing.get(&t.id));
                let status = match task {
                    Some(t) => task_status(&t.state),
                    None => ended_status(ended.as_deref()),
                };
                let error = task.and_then(|t| t.result.as_ref()).and_then(|r| r.error.clone());
                let preview = task.and_then(|t| t.result.as_ref()).and_then(|r| r.preview.clone());
                let note = if let Some(e) = error {
                    Some(flat(&e, 160))
                } else if task.is_none() && ended.as_deref() == Some("skipped") {
                    Some(format!(
                        "skipped{}",
                        reason.as_ref().map(|r| format!(": {r}")).unwrap_or_default()
                    ))
                } else if task.is_none() && reason.is_some() {
                    reason.as_ref().map(|r| flat(r, 160))
                } else {
                    preview.map(|p| flat(&p, 160))
                };
                let mut dag = DagNode {
                    key: key_of(id),
                    name: id.clone(),
                    kind: Some(GraphKind::Task),
                    status: Some(status),
                    depends_on: depends_on
                        .iter()
                        .filter(|d| ids.contains(d.as_str()))
                        .map(|d| key_of(d))
                        .collect(),
                    origin: "Start node".into(),
                    category: task.map_or(definition.clone(), |t| t.definition.clone()),
                    started_at: task.and_then(|t| t.started_at).or(timing.and_then(|t| t.started_at)),
                    ended_at: task.and_then(|t| t.ended_at).or(timing.and_then(|t| t.ended_at)),
                    ..DagNode::default()
                };
                task_fields(task, &mut dag);
                dag.note = note;
                dag
            })
            .collect();
        let mut edges = Vec::new();
        for n in &nodes {
            for dep in &n.depends_on {
                edges.push((dep.clone(), n.key.clone()));
            }
        }
        let started = plan
            .and_then(|p| p.started_at)
            .or_else(|| nodes.iter().filter_map(|n| n.started_at).reduce(f64::min));
        let current = matches!((turn_start, started), (Some(t), Some(s)) if s >= t - 1000.0);
        let prompt = snapshot
            .turn
            .as_ref()
            .and_then(|t| t.prompt.as_deref())
            .filter(|p| !p.is_empty());
        let statuses: Vec<GraphStatus> = nodes.iter().map(DagNode::status).collect();
        let waves = compute_waves(
            &nodes
                .iter()
                .map(|n| (n.key.clone(), n.depends_on.clone()))
                .collect::<Vec<_>>(),
        );
        runs.push(DagRun {
            key: format!("workflow:{run}"),
            kind: RunKind::Workflow,
            id: run.strip_prefix("wf-").unwrap_or(&run).to_string(),
            title: match (current, prompt) {
                (true, Some(p)) => flat(p, 120),
                _ => format!("workflow {run}"),
            },
            nodes,
            waves,
            edges,
            status: progress_of(&statuses).1,
            started_at: started,
        });
    }
    runs
}

/// A workflow node as planned: (id, definition, dependsOn, status, reason).
type PlannedNode = (String, String, Vec<String>, Option<String>, Option<String>);

/// Every run the pane can show: the current turn (when it started work), then each workflow run, oldest first.
pub fn build_dag_runs(snapshot: &RlmSnapshot) -> Vec<DagRun> {
    let graph = build_rlm_graph(snapshot);
    let tasks: HashMap<String, &RlmTask> = snapshot.tasks.iter().map(|t| (t.id.clone(), t)).collect();
    let mut runs: Vec<DagRun> = turn_run(snapshot, &graph, &tasks).into_iter().collect();
    runs.extend(workflow_runs(snapshot));
    runs
}

fn is_active(status: GraphStatus) -> bool {
    matches!(status, GraphStatus::Running | GraphStatus::Pending)
}

/// The run shown by default: the newest run with live work, else the newest run.
pub fn default_run(runs: &[DagRun]) -> Option<&DagRun> {
    runs.iter()
        .rev()
        .find(|r| r.nodes.iter().any(|n| is_active(n.status())))
        .or(runs.last())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunCounts {
    pub total: usize,
    pub done: usize,
    pub running: usize,
    pub pending: usize,
    pub failed: usize,
    /// 1-based index of the first wave with unfinished work.
    pub wave: usize,
    pub waves: usize,
}

pub fn run_counts(run: &DagRun) -> RunCounts {
    let statuses: HashMap<&str, GraphStatus> = run.nodes.iter().map(|n| (n.key.as_str(), n.status())).collect();
    let mut c = RunCounts {
        total: run.nodes.len(),
        done: 0,
        running: 0,
        pending: 0,
        failed: 0,
        wave: 0,
        waves: run.waves.len(),
    };
    for n in &run.nodes {
        match n.status() {
            GraphStatus::Done => c.done += 1,
            GraphStatus::Running => c.running += 1,
            GraphStatus::Pending => c.pending += 1,
            GraphStatus::Failed => c.failed += 1,
            _ => {}
        }
    }
    let open = run.waves.iter().position(|w| {
        w.iter()
            .any(|k| !is_finished(statuses.get(k.as_str()).copied().unwrap_or(GraphStatus::Done)))
    });
    c.wave = open.map_or(run.waves.len(), |i| i + 1);
    c
}

// ---- text helpers ----

fn pad_to(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cut = if visible_width(text) > width {
        truncate_to_width(text, width, "…", false)
    } else {
        text.to_string()
    };
    let w = visible_width(&cut);
    format!("{cut}{}", " ".repeat(width.saturating_sub(w)))
}

fn cut(text: &str, width: usize) -> String {
    if visible_width(text) > width {
        truncate_to_width(text, width, "…", false)
    } else {
        text.to_string()
    }
}

fn elapsed_of(node: &DagNode, now: f64) -> Option<String> {
    super::graph::elapsed(node.started_at, node.ended_at, node.status(), now)
}

pub fn status_word(node: &DagNode) -> &'static str {
    match node.status() {
        GraphStatus::Running => "● Running",
        GraphStatus::Pending => "○ Pending",
        GraphStatus::Done => "✓ Done",
        GraphStatus::Failed => "✗ Failed",
        GraphStatus::Cancelled => {
            if node.note.as_deref().is_some_and(|n| n.starts_with("skipped")) {
                "⊘ Skipped"
            } else {
                "⊘ Cancelled"
            }
        }
        GraphStatus::Incomplete => "◐ Incomplete",
    }
}

fn dot(status: GraphStatus) -> &'static str {
    match status {
        GraphStatus::Running => "●",
        GraphStatus::Pending => "○",
        GraphStatus::Done => "✓",
        GraphStatus::Failed => "✗",
        GraphStatus::Cancelled => "⊘",
        GraphStatus::Incomplete => "◐",
    }
}

fn tone(status: GraphStatus) -> Color {
    if status == GraphStatus::Pending {
        Color::Dim
    } else {
        status_color(status)
    }
}

fn progress_line(p: &GraphProgress, width: usize) -> String {
    let finished = p.done + p.failed + p.incomplete;
    let text = format!("{finished}/{}", p.total);
    let cells = (width as isize - visible_width(&text) as isize - 1).clamp(3, 10) as usize;
    format!("{} {text}", bar(finished as f64 / p.total.max(1) as f64, cells))
}

// ---- state and navigation ----

#[derive(Clone, Debug, PartialEq)]
pub struct RlmPaneState {
    /// Selected run (sticky while it exists); the default run otherwise.
    pub run_key: Option<String>,
    pub selected_key: Option<String>,
    /// Folded boxes (one line instead of three).
    pub folded: HashSet<String>,
    /// Node detail cards shown.
    pub details: bool,
    /// First visible body line.
    pub scroll: usize,
    /// Scroll the selected node into view on the next render.
    pub reveal: bool,
}

impl Default for RlmPaneState {
    fn default() -> Self {
        RlmPaneState {
            run_key: None,
            selected_key: None,
            folded: HashSet::new(),
            details: true,
            scroll: 0,
            reveal: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DagAction {
    Next,
    Prev,
    Fold,
    Details,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    PrevRun,
    NextRun,
}

pub fn current_run<'a>(runs: &'a [DagRun], state: &RlmPaneState) -> Option<&'a DagRun> {
    state
        .run_key
        .as_deref()
        .and_then(|k| runs.iter().find(|r| r.key == k))
        .or_else(|| default_run(runs))
}

/// Nodes in navigation order: wave by wave, left to right.
pub fn node_order(run: &DagRun) -> Vec<String> {
    run.waves.iter().flatten().cloned().collect()
}

/// The selected node key: the chosen one while it exists, else the first running node, else the first node.
pub fn selected_node(run: &DagRun, state: &RlmPaneState) -> Option<String> {
    let order = node_order(run);
    if let Some(k) = &state.selected_key
        && order.contains(k)
    {
        return Some(k.clone());
    }
    let by_key: HashMap<&str, &DagNode> = run.nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    order
        .iter()
        .find(|k| {
            by_key
                .get(k.as_str())
                .is_some_and(|n| n.status() == GraphStatus::Running)
        })
        .or(order.first())
        .cloned()
}

pub fn apply_dag_action(state: &RlmPaneState, runs: &[DagRun], action: DagAction, page: usize) -> RlmPaneState {
    let Some(run) = current_run(runs, state) else {
        return state.clone();
    };
    let order = node_order(run);
    let selected = selected_node(run, state);
    let index = selected.as_ref().and_then(|s| order.iter().position(|k| k == s));
    let page = page.max(1);
    let mut next = state.clone();
    match action {
        DagAction::Next | DagAction::Prev => {
            if order.is_empty() {
                return state.clone();
            }
            let n = order.len() as isize;
            let step = if action == DagAction::Next { 1 } else { -1 };
            let at = index.map_or(0, |i| ((i as isize + step + n) % n) as usize);
            next.run_key = Some(run.key.clone());
            next.selected_key = Some(order[at].clone());
            next.reveal = true;
        }
        DagAction::Fold => {
            let Some(sel) = selected else { return state.clone() };
            if !next.folded.remove(&sel) {
                next.folded.insert(sel.clone());
            }
            next.run_key = Some(run.key.clone());
            next.selected_key = Some(sel);
            next.reveal = true;
        }
        DagAction::Details => {
            next.details = !state.details;
            next.reveal = true;
        }
        DagAction::ScrollUp => {
            next.scroll = state.scroll.saturating_sub(1);
            next.reveal = false;
        }
        DagAction::ScrollDown => {
            next.scroll = state.scroll + 1;
            next.reveal = false;
        }
        DagAction::PageUp => {
            next.scroll = state.scroll.saturating_sub(page);
            next.reveal = false;
        }
        DagAction::PageDown => {
            next.scroll = state.scroll + page;
            next.reveal = false;
        }
        DagAction::PrevRun | DagAction::NextRun => {
            if runs.len() < 2 {
                return state.clone();
            }
            let at = runs.iter().position(|r| r.key == run.key).unwrap_or(0) as isize;
            let step = if action == DagAction::NextRun { 1 } else { -1 };
            let n = runs.len() as isize;
            let target = &runs[((at + step + n) % n) as usize];
            next.selected_key = None;
            next.run_key = Some(target.key.clone());
            next.scroll = 0;
            next.reveal = true;
        }
    }
    next
}

// ---- rendering ----

const GAP: usize = 2;
const MIN_BOX: usize = 20;
const MAX_BOX: usize = 30;
const MAX_SINGLE_BOX: usize = 48;

/// Boxes per row and the box width for a wave of `count` nodes in `width` columns.
pub fn wave_geometry(count: usize, width: usize, natural: usize) -> (usize, usize) {
    let room = width.max(1);
    let per_row = count.min((room + GAP) / (MIN_BOX + GAP)).max(1);
    let max = if count == 1 {
        MAX_BOX.max(MAX_SINGLE_BOX.min(natural))
    } else {
        MAX_BOX
    };
    let box_width = max
        .min((room as isize - (GAP * (per_row - 1)) as isize).max(0) as usize / per_row)
        .max(1);
    (per_row, box_width)
}

fn names(run: &DagRun) -> HashMap<&str, &str> {
    run.nodes.iter().map(|n| (n.key.as_str(), n.name.as_str())).collect()
}

fn sub_of(node: &DagNode, run: &DagRun) -> String {
    let names = names(run);
    if node.depends_on.is_empty() {
        node.origin.clone()
    } else {
        format!(
            "← {}",
            node.depends_on
                .iter()
                .map(|k| names.get(k.as_str()).copied().unwrap_or(k))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn natural_box_width(node: &DagNode, run: &DagRun) -> usize {
    visible_width(&format!("> [-] {}", node.name))
        .max(visible_width(&sub_of(node, run)))
        .max(16)
        + 4
}

fn draw_box(lines: &[String], width: usize, border: Color, style: &RlmStyle, bold: bool) -> Vec<String> {
    if width < 5 {
        return lines.iter().map(|l| pad_to(l, width)).collect();
    }
    let inner = width - 4;
    let edge = |t: &str| {
        if bold {
            style.bold(&style.fg(border, t))
        } else {
            style.fg(border, t)
        }
    };
    let mut out = vec![edge(&format!("┌{}┐", "─".repeat(width - 2)))];
    for l in lines {
        out.push(format!("{} {} {}", edge("│"), pad_to(l, inner), edge("│")));
    }
    out.push(edge(&format!("└{}┘", "─".repeat(width - 2))));
    out
}

fn node_box(
    node: &DagNode,
    run: &DagRun,
    width: usize,
    selected: bool,
    folded: bool,
    style: &RlmStyle,
    now: f64,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let marker = if selected {
        style.fg(Color::Accent, ">")
    } else {
        " ".into()
    };
    let fold = style.fg(Color::Dim, if folded { "[+]" } else { "[-]" });
    let name = if selected {
        style.bold(&node.name)
    } else {
        node.name.clone()
    };
    let color = tone(node.status());
    let border = if selected { Color::Accent } else { Color::Dim };
    if folded {
        let glyph = style.fg(color, dot(node.status()));
        let head = cut(&format!("{marker} {fold} {name}"), inner.saturating_sub(2).max(1));
        return draw_box(&[format!("{head} {glyph}")], width, border, style, selected);
    }
    let word = status_word(node);
    let status = match elapsed_of(node, now) {
        Some(t) if visible_width(word) + 2 + visible_width(&t) <= inner => {
            format!("{}  {}", style.fg(color, word), style.fg(Color::Dim, &t))
        }
        _ => style.fg(color, word),
    };
    let sub = match &node.progress {
        Some(p) if matches!(node.kind(), GraphKind::Fanout | GraphKind::Workflow | GraphKind::Tools) => {
            style.fg(tone(node.status()), &progress_line(p, inner))
        }
        _ if !node.depends_on.is_empty() => style.fg(Color::Muted, &sub_of(node, run)),
        _ => style.fg(Color::Dim, &node.origin),
    };
    draw_box(
        &[format!("{marker} {fold} {name}"), status, sub],
        width,
        border,
        style,
        selected,
    )
}

fn card_lines(node: &DagNode, run: &DagRun, width: usize, selected: bool, style: &RlmStyle, now: f64) -> Vec<String> {
    let color = tone(node.status());
    let marker = if selected {
        format!("{} ", style.fg(Color::Accent, ">"))
    } else {
        String::new()
    };
    let head = format!(
        "{marker}{} {}",
        style.fg(color, dot(node.status())),
        style.fg(
            if selected { Color::Accent } else { Color::Text },
            node.prompt.as_deref().unwrap_or(&node.name)
        )
    );
    let meta = match &node.model {
        None => node.category.clone(),
        Some(m) => format!("{} · {m}", node.category),
    };
    let time = elapsed_of(node, now);
    let waited = match run.started_at {
        Some(s) if node.status() == GraphStatus::Pending && now >= s => Some(format_duration(now - s)),
        _ => None,
    };
    let body = if node.status() == GraphStatus::Failed && node.note.is_some() {
        style.fg(Color::Error, node.note.as_deref().unwrap_or(""))
    } else if let Some(t) = &node.text {
        style.fg(Color::Text, t)
    } else if let Some(n) = &node.note {
        style.fg(Color::Muted, &format!("→ {n}"))
    } else if node.status() == GraphStatus::Pending {
        style.fg(
            Color::Dim,
            &format!(
                "waiting{}",
                if node.depends_on.is_empty() {
                    ""
                } else {
                    " for dependencies"
                }
            ),
        )
    } else if let Some(p) = &node.progress {
        style.fg(Color::Muted, &progress_line(p, width.saturating_sub(4)))
    } else {
        style.fg(Color::Dim, "no output yet")
    };
    let mut stats = Vec::new();
    if node.status() == GraphStatus::Pending {
        stats.push(waited.map_or("waiting".to_string(), |w| format!("waiting {w}")));
    } else if let Some(t) = time {
        stats.push(t);
    }
    if let Some(t) = node.turns {
        stats.push(format!("{} turn{}", js_num(t), if t == 1.0 { "" } else { "s" }));
    }
    if let Some(t) = node.tool_calls {
        stats.push(format!("{} tool{}", js_num(t), if t == 1.0 { "" } else { "s" }));
    }
    let lines = vec![
        head,
        style.fg(Color::Muted, &format!("{} · {meta}", node.name)),
        body,
        style.fg(
            Color::Dim,
            &if stats.is_empty() {
                "—".to_string()
            } else {
                stats.join(" · ")
            },
        ),
    ];
    draw_box(
        &lines,
        width,
        if selected { Color::Accent } else { Color::Dim },
        style,
        selected,
    )
}

/// Pack hint items into lines of `width`, two spaces apart.
pub fn pack_hints(items: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in items {
        let candidate = if current.is_empty() {
            item.clone()
        } else {
            format!("{current}  {item}")
        };
        if visible_width(&candidate) <= width || current.is_empty() {
            current = candidate;
        } else {
            lines.push(current);
            current = item.clone();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.iter().map(|l| cut(l, width)).collect()
}

/// The body lines and each node box's first and last body line.
pub struct Body {
    pub lines: Vec<String>,
    pub spans: HashMap<String, (usize, usize)>,
}

/// The scrolled body of the pane: waves of boxes with arrows, the dependencies, and the node cards.
pub fn render_dag_body(run: &DagRun, state: &RlmPaneState, width: usize, style: &RlmStyle, now: f64) -> Body {
    let bound = width.max(1);
    let selected = selected_node(run, state);
    let by_key: HashMap<&str, &DagNode> = run.nodes.iter().map(|n| (n.key.as_str(), n)).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut spans = HashMap::new();
    let center = |text: &str| {
        let pad = (bound as isize - visible_width(text) as isize).max(0) as usize / 2;
        cut(&format!("{}{text}", " ".repeat(pad)), bound)
    };
    for (wave_index, wave) in run.waves.iter().enumerate() {
        let lone = if wave.len() == 1 {
            natural_box_width(by_key[wave[0].as_str()], run)
        } else {
            0
        };
        let (per_row, box_width) = wave_geometry(wave.len(), bound, lone);
        let mut start = 0;
        while start < wave.len() {
            if start > 0 {
                lines.push(String::new());
            }
            let row: Vec<&DagNode> = wave[start..(start + per_row).min(wave.len())]
                .iter()
                .map(|k| by_key[k.as_str()])
                .collect();
            let boxes: Vec<Vec<String>> = row
                .iter()
                .map(|n| {
                    node_box(
                        n,
                        run,
                        box_width,
                        Some(&n.key) == selected.as_ref(),
                        state.folded.contains(&n.key),
                        style,
                        now,
                    )
                })
                .collect();
            let height = boxes.iter().map(Vec::len).max().unwrap_or(0);
            let row_width = row.len() * box_width + GAP * (row.len() - 1);
            let indent = " ".repeat((bound as isize - row_width as isize).max(0) as usize / 2);
            let top = lines.len();
            for line in 0..height {
                let joined = boxes
                    .iter()
                    .map(|b| b.get(line).cloned().unwrap_or_else(|| " ".repeat(box_width)))
                    .collect::<Vec<_>>()
                    .join(&" ".repeat(GAP));
                lines.push(cut(&format!("{indent}{joined}"), bound));
            }
            for (i, n) in row.iter().enumerate() {
                spans.insert(n.key.clone(), (top, top + boxes[i].len() - 1));
            }
            start += per_row;
        }
        if wave.len() > 1 {
            lines.push(cut(&style.fg(Color::Dim, "  · Same frontier"), bound));
        }
        if wave_index < run.waves.len() - 1 {
            lines.push(center(&style.fg(Color::Dim, "│")));
            lines.push(center(&style.fg(Color::Accent, "▼")));
        }
    }
    lines.push(String::new());
    lines.push(style.bold(&style.fg(Color::Text, "Dependencies")));
    if run.edges.is_empty() {
        lines.push(style.fg(
            Color::Dim,
            &cut(
                if run.kind == RunKind::Turn {
                    "  none · each cell's work is one frontier"
                } else {
                    "  none"
                },
                bound,
            ),
        ));
    }
    const EDGE_LIMIT: usize = 40;
    let name = |k: &str| by_key.get(k).map_or(k.to_string(), |n| n.name.clone());
    for (from, to) in run.edges.iter().take(EDGE_LIMIT) {
        lines.push(cut(
            &format!(
                "  {} {} {}",
                style.fg(Color::Text, &name(from)),
                style.fg(Color::Dim, "→"),
                style.fg(Color::Text, &name(to))
            ),
            bound,
        ));
    }
    if run.edges.len() > EDGE_LIMIT {
        lines.push(style.fg(
            Color::Dim,
            &cut(&format!("  … {} more", run.edges.len() - EDGE_LIMIT), bound),
        ));
    }
    if state.details {
        lines.push(String::new());
        lines.push(style.bold(&style.fg(Color::Text, "Node details")));
        for key in node_order(run) {
            let node = by_key[key.as_str()];
            for l in card_lines(node, run, bound, Some(&key) == selected.as_ref(), style, now) {
                lines.push(cut(&l, bound));
            }
        }
    }
    Body { lines, spans }
}

/// How the pane draws.
#[derive(Clone)]
pub struct DagRenderOptions {
    pub style: RlmStyle,
    pub now: f64,
    /// Key hints for the footer, e.g. "Tab/n next".
    pub hints: Vec<String>,
    /// Whether the pane has the input focus (`None`: unknown, drawn as focused).
    pub focused: Option<bool>,
    /// Shown as the first hint, e.g. "alt+w chat".
    pub focus_hint: Option<String>,
    /// Inspection failure: the footer shows it instead of "Connected".
    pub error: Option<String>,
    /// Tasks listed by the host, for the breadcrumb.
    pub task_count: usize,
}

impl DagRenderOptions {
    pub fn new(now: f64) -> DagRenderOptions {
        DagRenderOptions {
            style: RlmStyle::plain(),
            now,
            hints: Vec::new(),
            focused: None,
            focus_hint: None,
            error: None,
            task_count: 0,
        }
    }
}

/// What [`render_rlm_pane`] drew.
pub struct PaneRender {
    pub lines: Vec<String>,
    pub state: RlmPaneState,
    pub run_key: Option<String>,
    pub body_length: usize,
}

/// Render the pane into exactly `height` lines of at most `width` columns: a fixed header, the scrolled body and a
/// fixed footer. Returns the state with the scroll it used.
pub fn render_rlm_pane(
    runs: &[DagRun],
    state: &RlmPaneState,
    width: usize,
    height: usize,
    o: &DagRenderOptions,
) -> PaneRender {
    let style = &o.style;
    let bound = width.max(1);
    let rows = height.max(1);
    let run = current_run(runs, state);
    let mut header = Vec::new();
    match run {
        None => {
            header.push(style.bold(&style.fg(Color::Accent, "RLM")));
            header.push(style.fg(Color::Muted, "RLM pane"));
        }
        Some(run) => {
            let c = run_counts(run);
            let index = runs.iter().position(|r| r.key == run.key).unwrap_or(0) + 1;
            header.push(format!(
                "{} {} {}",
                style.bold(&style.fg(Color::Accent, "RLM")),
                style.fg(Color::Dim, "·"),
                style.fg(Color::Muted, &run.id)
            ));
            header.push(format!(
                "{}  {}",
                style.fg(
                    Color::Muted,
                    &format!(
                        "RLM pane / {}",
                        if run.kind == RunKind::Turn { "turn" } else { "workflow" }
                    )
                ),
                style.fg(Color::Dim, &format!("Tasks ({})", o.task_count))
            ));
            let counter = format!("{index}/{}", runs.len());
            let title = cut(&run.title, bound.saturating_sub(visible_width(&counter) + 2).max(1));
            header.push(format!(
                "{}  {}",
                style.bold(&style.fg(Color::Text, &title)),
                style.fg(Color::Dim, &counter)
            ));
            let word = if c.running > 0 {
                "Running"
            } else if c.pending > 0 {
                "Pending"
            } else if c.failed > 0 {
                "Failed"
            } else {
                "Finished"
            };
            let color = if c.running > 0 {
                Color::Accent
            } else if c.failed > 0 {
                Color::Error
            } else if c.pending > 0 {
                Color::Dim
            } else {
                Color::Success
            };
            let mut extra = vec![format!("Done {}/{}", c.done, c.total)];
            if c.running > 0 {
                extra.push(format!("{} running", c.running));
            }
            if c.failed > 0 {
                extra.push(format!("{} failed", c.failed));
            }
            extra.push(format!("wave {}/{}", c.wave, c.waves));
            header.push(format!(
                "{} {} {}",
                style.fg(color, word),
                style.fg(Color::Dim, "·"),
                style.fg(Color::Muted, &extra.join(" · "))
            ));
        }
    }
    let body = match run {
        None => Body {
            lines: vec![
                String::new(),
                style.fg(Color::Dim, "No RLM work in this turn yet."),
                style.fg(Color::Dim, "Spawned children and workflows.run nodes"),
                style.fg(Color::Dim, "appear here as dependency waves."),
            ],
            spans: HashMap::new(),
        },
        Some(run) => render_dag_body(run, state, bound, style, o.now),
    };
    let mut hints: Vec<String> = o.focus_hint.iter().cloned().collect();
    hints.extend(o.hints.iter().cloned());
    let connection = match &o.error {
        Some(e) => style.fg(
            Color::Error,
            &format!(
                "● {}",
                cut(&format!("Inspection failed: {e}"), bound.saturating_sub(14).max(1))
            ),
        ),
        None => style.fg(Color::Success, "● Connected"),
    };
    let mut footer_hints: Vec<String> = pack_hints(&hints, bound)
        .iter()
        .map(|l| {
            style.fg(
                if o.focused == Some(false) {
                    Color::Dim
                } else {
                    Color::Muted
                },
                l,
            )
        })
        .collect();
    let mut separator = vec![style.fg(Color::Dim, &"─".repeat(bound))];
    let mut blank = vec![String::new()];
    if rows < header.len() + 1 + 2 + footer_hints.len() + 6 {
        blank.clear();
        separator.clear();
        footer_hints = if footer_hints.is_empty() {
            Vec::new()
        } else {
            vec![cut(&footer_hints.join("  "), bound)]
        };
    }
    let fixed = header.len() + blank.len() + separator.len() + 1 + footer_hints.len();
    let room = (rows as isize - fixed as isize).max(1) as usize;
    let max_scroll = body.lines.len().saturating_sub(room);
    let mut scroll = state.scroll.min(max_scroll);
    let selected = run.and_then(|r| selected_node(r, state));
    if state.reveal
        && let Some(sel) = &selected
    {
        if let Some((top, bottom)) = body.spans.get(sel) {
            if *top < scroll {
                scroll = *top;
            } else if *bottom >= scroll + room {
                scroll = (*top).min(bottom + 1 - room);
            }
        }
        scroll = scroll.min(max_scroll);
    }
    let total = body.lines.len();
    let position = if total == 0 {
        "0/0".to_string()
    } else {
        format!("{}-{}/{total}", scroll + 1, total.min(scroll + room))
    };
    let status = cut(&format!("{connection}  {}", style.fg(Color::Dim, &position)), bound);
    let mut lines: Vec<String> = header;
    lines.extend(blank);
    lines.extend(body.lines.iter().skip(scroll).take(room).cloned());
    let footer_len = separator.len() + 1 + footer_hints.len();
    while lines.len() < rows.saturating_sub(footer_len) {
        lines.push(String::new());
    }
    lines.extend(separator);
    lines.push(status);
    lines.extend(footer_hints);
    let mut out: Vec<String> = lines.iter().take(rows).map(|l| cut(l, bound)).collect();
    while out.len() < rows {
        out.push(String::new());
    }
    let mut next = state.clone();
    next.scroll = scroll;
    next.reveal = false;
    PaneRender {
        lines: out,
        state: next,
        run_key: run.map(|r| r.key.clone()),
        body_length: total,
    }
}

// ---- the live wave summary ----

fn summarized(kind: GraphKind) -> bool {
    matches!(
        kind,
        GraphKind::Task | GraphKind::Child | GraphKind::Background | GraphKind::Workflow | GraphKind::Fanout
    )
}

/// The runs whose work the RLM pane shows and that count as new work for its auto-open.
pub fn pane_work_runs(runs: &[DagRun], since: Option<f64>) -> Vec<&DagRun> {
    runs.iter()
        .filter(|run| {
            let nodes: Vec<&DagNode> = run.nodes.iter().filter(|n| summarized(n.kind())).collect();
            if nodes.is_empty() {
                return false;
            }
            if nodes.iter().any(|n| is_active(n.status())) {
                return true;
            }
            let Some(since) = since else { return false };
            run.kind == RunKind::Turn || run.started_at.is_some_and(|s| s >= since - 1000.0)
        })
        .collect()
}

/// "▶ <title> running wave i/N  d/T done, r running" and one line per node, while a run's children or workflow
/// nodes run or wait; empty otherwise.
pub fn render_wave_summary(runs: &[DagRun], width: usize, style: &RlmStyle, now: f64, max_nodes: usize) -> Vec<String> {
    let bound = width.max(1);
    let Some(run) = runs
        .iter()
        .rev()
        .find(|r| r.nodes.iter().any(|n| summarized(n.kind()) && is_active(n.status())))
    else {
        return Vec::new();
    };
    let c = run_counts(run);
    let state = if c.running > 0 { "running" } else { "waiting" };
    let tail = format!(
        " {state} wave {}/{}  {}/{} done, {} running",
        c.wave, c.waves, c.done, c.total, c.running
    );
    let title_room = (bound as isize - 2 - visible_width(&tail) as isize).max(4) as usize;
    let mut lines = vec![cut(
        &format!(
            "{} {}{}",
            style.fg(Color::Accent, "▶"),
            style.bold(&style.fg(Color::Text, &cut(&run.title, title_room))),
            style.fg(Color::Muted, &tail)
        ),
        bound,
    )];
    let rank = |s: GraphStatus| match s {
        GraphStatus::Running => 0,
        GraphStatus::Pending => 1,
        GraphStatus::Failed | GraphStatus::Incomplete => 2,
        _ => 3,
    };
    let mut nodes: Vec<&DagNode> = run.nodes.iter().collect();
    nodes.sort_by_key(|n| rank(n.status()));
    let max = max_nodes.max(1);
    let shown: Vec<&DagNode> = if nodes.len() > max {
        nodes[..max - 1].to_vec()
    } else {
        nodes.clone()
    };
    for node in &shown {
        let head = format!("{} · category:{}", node.name, node.category);
        let time = elapsed_of(node, now);
        let (tail, said) = if node.status() == GraphStatus::Pending {
            let waited = match run.started_at {
                Some(s) if now >= s => format_duration(now - s),
                _ => String::new(),
            };
            (
                format!(
                    " · waiting{}",
                    if waited.is_empty() {
                        String::new()
                    } else {
                        format!(" {waited}")
                    }
                ),
                None,
            )
        } else {
            let said = if node.status() == GraphStatus::Failed {
                node.note.clone()
            } else {
                node.text.clone().or_else(|| {
                    if node.status() == GraphStatus::Done {
                        node.note.clone()
                    } else {
                        None
                    }
                })
            };
            (time.map(|t| format!(" · {t}")).unwrap_or_default(), said)
        };
        let room = bound as isize - 4 - visible_width(&head) as isize - visible_width(&tail) as isize - 3;
        let text = match said {
            Some(s) if room >= 8 => format!(" · {}", cut(&s, room as usize)),
            _ => String::new(),
        };
        let glyph = if node.status() == GraphStatus::Running {
            "▶"
        } else {
            dot(node.status())
        };
        let color = match node.status() {
            GraphStatus::Pending => Color::Dim,
            GraphStatus::Failed => Color::Error,
            _ => Color::Muted,
        };
        lines.push(cut(
            &format!(
                "  {} {}",
                style.fg(tone(node.status()), glyph),
                style.fg(color, &format!("{head}{text}{tail}"))
            ),
            bound,
        ));
    }
    if nodes.len() > shown.len() {
        lines.push(cut(
            &style.fg(Color::Dim, &format!("  +{} more", nodes.len() - shown.len())),
            bound,
        ));
    }
    lines
}

// ---- keys and the pane component ----

const PANE_ACTIONS: [(Option<DagAction>, &str); 11] = [
    (None, "app.rlm.pane.close"),
    (Some(DagAction::Next), "app.rlm.pane.next"),
    (Some(DagAction::Prev), "app.rlm.pane.prev"),
    (Some(DagAction::Fold), "app.rlm.pane.fold"),
    (Some(DagAction::Details), "app.rlm.pane.details"),
    (Some(DagAction::ScrollUp), "app.rlm.pane.scrollUp"),
    (Some(DagAction::ScrollDown), "app.rlm.pane.scrollDown"),
    (Some(DagAction::PageUp), "app.rlm.pane.pageUp"),
    (Some(DagAction::PageDown), "app.rlm.pane.pageDown"),
    (Some(DagAction::PrevRun), "app.rlm.pane.prevRun"),
    (Some(DagAction::NextRun), "app.rlm.pane.nextRun"),
];

/// A key as the pane shows it: "Tab", "Shift-Tab", "↑"; others as they are.
pub fn key_name(key: &str) -> String {
    match key {
        "tab" => "Tab",
        "shift+tab" => "Shift-Tab",
        "space" => "Space",
        "enter" => "Enter",
        "escape" => "Esc",
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "pageUp" => "PgUp",
        "pageDown" => "PgDn",
        other => other,
    }
    .to_string()
}

fn keys_of(keys: &KeybindingsManager, action: &str) -> Vec<String> {
    keys.get_keys(action).iter().map(|k| key_name(k)).collect()
}

/// Footer hints from the live bindings: "Tab/n next", "Shift-Tab/p prev", "Space/Enter fold", "d Details", ...
pub fn rlm_pane_hints(keys: &KeybindingsManager) -> Vec<String> {
    let label = |a: &str| {
        let k = keys_of(keys, a);
        if k.is_empty() {
            "unbound".to_string()
        } else {
            k.join("/")
        }
    };
    let pair = |first: &str, second: &str| {
        let a = keys_of(keys, first).into_iter().next().unwrap_or_else(|| "?".into());
        let b = keys_of(keys, second).into_iter().next().unwrap_or_else(|| "?".into());
        if a.encode_utf16().count() == 1 && b.encode_utf16().count() == 1 {
            format!("{a}{b}")
        } else {
            format!("{a}/{b}")
        }
    };
    vec![
        format!("{} next", label("app.rlm.pane.next")),
        format!("{} prev", label("app.rlm.pane.prev")),
        format!("{} fold", label("app.rlm.pane.fold")),
        format!("{} Details", label("app.rlm.pane.details")),
        format!("{} Scroll", pair("app.rlm.pane.scrollUp", "app.rlm.pane.scrollDown")),
        format!("{} Runs", pair("app.rlm.pane.prevRun", "app.rlm.pane.nextRun")),
        format!("{} Close", label("app.rlm.pane.close")),
    ]
}

/// What a key did in the pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    Ignored,
    Render,
    Close,
}

/// The RLM pane's state across frames.
#[derive(Clone, Debug, Default)]
pub struct RlmPane {
    pub state: RlmPaneState,
}

impl RlmPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a key against the current snapshot; `height` is the pane's height (a page is `height - 12` lines).
    pub fn handle_input(
        &mut self,
        keys: &KeybindingsManager,
        data: &str,
        snapshot: &RlmSnapshot,
        height: usize,
    ) -> PaneEvent {
        let Some(action) = PANE_ACTIONS
            .iter()
            .find(|(_, id)| keys.matches(data, id))
            .map(|(a, _)| *a)
        else {
            return PaneEvent::Ignored;
        };
        let Some(action) = action else {
            return PaneEvent::Close;
        };
        let runs = build_dag_runs(snapshot);
        self.state = apply_dag_action(&self.state, &runs, action, height.saturating_sub(12).max(1));
        PaneEvent::Render
    }

    /// Render `height` lines of `width` columns (one column of left padding next to the split border).
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        keys: &KeybindingsManager,
        snapshot: &RlmSnapshot,
        width: usize,
        height: usize,
        focused: bool,
        toggle_key: Option<&str>,
        style: &RlmStyle,
    ) -> Vec<String> {
        let runs = build_dag_runs(snapshot);
        let inner = width.saturating_sub(1).max(1);
        let rendered = render_rlm_pane(
            &runs,
            &self.state,
            inner,
            height.max(1),
            &DagRenderOptions {
                style: style.clone(),
                now: snapshot.now,
                hints: rlm_pane_hints(keys),
                focused: Some(focused),
                focus_hint: toggle_key.map(|k| format!("{k} {}", if focused { "chat" } else { "focus" })),
                error: snapshot.error.clone(),
                task_count: snapshot.tasks.len(),
            },
        );
        self.state = rendered.state;
        rendered.lines.into_iter().map(|l| format!(" {l}")).collect()
    }
}
