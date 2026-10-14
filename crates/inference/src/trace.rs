//! Frame state, traces and `rlm.frames` (inference.ts `trace`, `summary`, `writeTrace`, `list`): every finished frame
//! leaves a trace document (task, views, contract, budget, attempts, requests) and a summary in a bounded index,
//! both kept in a [`TraceStore`] (the session values `ultron.rlm.frames/<id>` and `ultron.rlm.frames/index`), so
//! `rlm.frames` reads them in any later cell, and after the session reopens.

use super::{Kind, Ledger, OUTPUT_PREVIEW_CHARS, Outcome, View, bounded, fields, js_len, optional_integer};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const TRACE_INDEX_LIMIT: usize = 200;
pub(crate) const TRACE_REQUEST_LIMIT: usize = 50;
/// The session-value namespace of frame traces; the index is its key `index`.
pub const TRACE_NAMESPACE: &str = "ultron.rlm.frames";

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A random v4 UUID with a prefix (`frame-…`, `budget-…`).
pub(crate) fn uuid(prefix: &str) -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{prefix}-{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Where frame traces live (`FrameTraceStore`): one document per key, replaced whole.
pub trait TraceStore: Send + Sync {
    fn read(&self, key: &str) -> Option<Value>;
    fn write(&self, key: &str, document: &Value) -> Result<(), String>;
}

/// Traces in memory (`createMemoryFrameStore`): a session without durable storage.
#[derive(Default)]
pub struct MemoryTraceStore {
    pub documents: Mutex<HashMap<String, Value>>,
}

impl TraceStore for MemoryTraceStore {
    fn read(&self, key: &str) -> Option<Value> {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    fn write(&self, key: &str, document: &Value) -> Result<(), String> {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_string(), document.clone());
        Ok(())
    }
}

/// The budget node one `rlm.infer` or `rlm.map` call shares among its frames.
pub(crate) struct Budget {
    pub id: String,
    pub node: usize,
    pub depth: u64,
    /// The frame whose budget this one draws on (a nested call), by id.
    pub parent: Option<String>,
    /// The token limit is `rlm.map`'s default, not the caller's: exhaustion then says how to raise it.
    pub default_tokens: bool,
}

impl Budget {
    pub fn snapshot(&self, ledger: &Ledger) -> Value {
        let node = &ledger.nodes[self.node];
        json!({
            "id": self.id, "parent": self.parent, "depth": self.depth,
            "limits": {"calls": node.calls.limit, "tokens": node.tokens.limit},
            "spent": {"calls": node.calls.spent, "tokens": node.tokens.spent},
        })
    }

    /// `budgetSummary`: the shared pool of a frame summary.
    fn summary(&self, ledger: &Ledger) -> Value {
        let node = &ledger.nodes[self.node];
        json!({"id": self.id, "calls": node.calls.limit, "tokens": node.tokens.limit, "depth": self.depth})
    }
}

/// One frame: its request, what it spent and sent, and what came back.
pub(crate) struct Frame {
    pub id: String,
    pub node: usize,
    pub task: String,
    pub views: Vec<View>,
    /// The model the frame ran on; the requested one (`spec.model`) when it never ran.
    pub model: Option<String>,
    pub outputs: Vec<String>,
    pub attempts: Vec<Value>,
    pub requests: Vec<Value>,
    /// Provider-reported input and output tokens and cost (summed into `rlm.map`'s reply).
    pub measured: (u64, u64, f64),
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub kind: &'static str,
    pub batch: usize,
    /// The `rlm-frame@1` task it runs as, and that task's lane.
    pub task_id: Option<String>,
    pub lane: Option<String>,
    /// The task that made the request (`callerTaskId`), and the frame it was made from (a nested frame).
    pub caller_task: Option<String>,
    pub parent_frame: Option<String>,
    /// The call claimed at admission, not yet used by the first request.
    pub call_held: bool,
    /// Why a follow-up request (a tool round) was refused.
    pub exhausted: Option<String>,
}

impl Frame {
    /// A frame whose node is a child of `budget`'s, at `depth`.
    pub fn new(ledger: &mut Ledger, budget: &Budget, task: &str, views: Vec<View>) -> Self {
        Self {
            id: uuid("frame"),
            node: ledger.add(Some(budget.node), None, None),
            task: task.to_string(),
            views,
            model: None,
            outputs: Vec::new(),
            attempts: Vec::new(),
            requests: Vec::new(),
            measured: (0, 0, 0.0),
            started_at: now_ms(),
            ended_at: None,
            kind: "infer",
            batch: 1,
            task_id: None,
            lane: None,
            caller_task: None,
            parent_frame: None,
            call_held: false,
            exhausted: None,
        }
    }

    pub fn spent(&self, ledger: &Ledger) -> Value {
        let node = &ledger.nodes[self.node];
        json!({"calls": node.calls.spent, "tokens": node.tokens.spent})
    }

    pub fn remaining(&self, ledger: &mut Ledger, depth: u64) -> Value {
        json!({
            "calls": ledger.available(self.node, Kind::Calls),
            "tokens": ledger.available(self.node, Kind::Tokens),
            "depth": depth,
        })
    }
}

/// What a frame's trace records besides the frame itself.
pub(crate) struct Spec<'a> {
    pub contract: Option<&'a Value>,
    pub max_repairs: u64,
    pub depth: u64,
    pub model: Option<&'a str>,
}

fn status(outcome: Option<&Outcome>) -> &'static str {
    match outcome {
        None => "running",
        Some(Outcome::Complete(_)) => "complete",
        Some(Outcome::Incomplete(..)) => "incomplete",
        Some(Outcome::Error(_)) => "error",
    }
}

fn nullable(value: &Option<String>) -> Value {
    value.clone().map_or(Value::Null, Value::from)
}

/// `trace(frame)`: the full document `rlm.frames(id)` returns (`status: "running"` while it runs). View metadata
/// only, never the materialized text.
pub(crate) fn trace(
    ledger: &mut Ledger,
    budget: &Budget,
    frame: &Frame,
    spec: &Spec,
    outcome: Option<&Outcome>,
) -> Value {
    let mut out = Map::new();
    out.insert("version".into(), 1.into());
    out.insert("id".into(), frame.id.clone().into());
    out.insert("status".into(), status(outcome).into());
    match outcome {
        Some(Outcome::Incomplete(reason, detail)) => {
            out.insert("reason".into(), (*reason).into());
            out.insert("detail".into(), detail.clone().into());
        }
        Some(Outcome::Error(error)) => {
            out.insert("error".into(), bounded(error, OUTPUT_PREVIEW_CHARS).into());
        }
        Some(Outcome::Complete(value)) => {
            out.insert("value".into(), bounded(&value.to_string(), OUTPUT_PREVIEW_CHARS).into());
        }
        None => {}
    }
    out.insert("task".into(), bounded(&frame.task, OUTPUT_PREVIEW_CHARS).into());
    let views: Vec<Value> = frame
        .views
        .iter()
        .map(|view| {
            let mut item = Map::new();
            item.insert("label".into(), view.label.clone().into());
            item.insert("chars".into(), view.chars.into());
            item.insert("byReference".into(), view.by_reference.into());
            if let Some(digest) = &view.digest {
                item.insert("digest".into(), digest.clone().into());
            }
            if let Some(start) = &view.start {
                item.insert("start".into(), start.clone());
            }
            if let Some(end) = &view.end {
                item.insert("end".into(), end.clone());
            }
            Value::Object(item)
        })
        .collect();
    out.insert("views".into(), views.into());
    out.insert("contract".into(), spec.contract.cloned().unwrap_or(Value::Null));
    out.insert("maxRepairs".into(), spec.max_repairs.into());
    out.insert("depth".into(), spec.depth.into());
    out.insert("budget".into(), budget.snapshot(ledger));
    out.insert("spent".into(), frame.spent(ledger));
    out.insert("remaining".into(), frame.remaining(ledger, spec.depth));
    out.insert("taskId".into(), nullable(&frame.task_id));
    out.insert("lane".into(), nullable(&frame.lane));
    out.insert("callerTaskId".into(), nullable(&frame.caller_task));
    out.insert("parentFrame".into(), nullable(&frame.parent_frame));
    let model = frame.model.as_deref().or(spec.model);
    out.insert("model".into(), model.map_or(Value::Null, Value::from));
    out.insert("startedAt".into(), frame.started_at.into());
    out.insert("endedAt".into(), frame.ended_at.map_or(Value::Null, Value::from));
    out.insert("attempts".into(), frame.attempts.clone().into());
    out.insert("requests".into(), frame.requests.clone().into());
    Value::Object(out)
}

/// `summary(frame)`: one row of `rlm.frames()`.
pub(crate) fn summary(ledger: &Ledger, budget: &Budget, frame: &Frame, outcome: Option<&Outcome>) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), frame.id.clone().into());
    out.insert("status".into(), status(outcome).into());
    if let Some(Outcome::Incomplete(reason, _)) = outcome {
        out.insert("reason".into(), (*reason).into());
    }
    // `task.replace(/\s+/g, " ")`: every run of whitespace becomes one space.
    let mut collapsed = String::new();
    for c in frame.task.chars() {
        if c.is_whitespace() || c == '\u{feff}' {
            if !collapsed.ends_with(' ') {
                collapsed.push(' ');
            }
        } else {
            collapsed.push(c);
        }
    }
    out.insert("task".into(), bounded(&collapsed, 120).into());
    out.insert("views".into(), frame.views.len().into());
    out.insert("spent".into(), frame.spent(ledger));
    out.insert("taskId".into(), nullable(&frame.task_id));
    out.insert("parentFrame".into(), nullable(&frame.parent_frame));
    out.insert("startedAt".into(), frame.started_at.into());
    out.insert("endedAt".into(), frame.ended_at.map_or(Value::Null, Value::from));
    out.insert("kind".into(), frame.kind.into());
    out.insert("batch".into(), frame.batch.into());
    out.insert("callerTaskId".into(), nullable(&frame.caller_task));
    out.insert("lane".into(), nullable(&frame.lane));
    out.insert("budget".into(), budget.summary(ledger));
    Value::Object(out)
}

/// The session's frame traces: the store and the index of the newest 200 summaries (read from the store once).
pub(crate) struct Traces {
    store: std::sync::Arc<dyn TraceStore>,
    index: Option<Vec<Value>>,
}

impl Traces {
    pub fn new(store: std::sync::Arc<dyn TraceStore>) -> Self {
        Self { store, index: None }
    }

    fn index(&mut self) -> &mut Vec<Value> {
        let store = &self.store;
        self.index.get_or_insert_with(|| match store.read("index") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        })
    }

    /// `writeTrace`: the trace document, then the index with the summary (a write failure loses only the trace).
    pub fn write(&mut self, id: &str, trace: &Value, summary: Value) {
        let _ = self.store.write(id, trace);
        let index = self.index();
        match index.iter().position(|item| item["id"] == id) {
            Some(at) => index[at] = summary,
            None => index.push(summary),
        }
        if index.len() > TRACE_INDEX_LIMIT {
            index.drain(..index.len() - TRACE_INDEX_LIMIT);
        }
        let index = Value::Array(index.clone());
        let _ = self.store.write("index", &index);
    }

    /// `rlm.frames`: one frame's trace by id (a running frame's from `live`), else the newest `limit` (default 20)
    /// summaries, newest first, running frames included.
    pub fn list(
        &mut self,
        payload: &Map<String, Value>,
        live_trace: &dyn Fn(&str) -> Option<Value>,
        live_summaries: &[Value],
    ) -> Result<Value, String> {
        fields(payload, &["id", "limit"])?;
        if let Some(id) = payload.get("id") {
            let id = id
                .as_str()
                .filter(|id| id.starts_with("frame-"))
                .ok_or("id must be a frame id")?;
            if let Some(trace) = live_trace(id) {
                return Ok(trace);
            }
            return self.store.read(id).ok_or_else(|| format!("Unknown frame {id}"));
        }
        let limit = optional_integer(payload.get("limit"), "limit", 1, TRACE_INDEX_LIMIT as u64)?.unwrap_or(20);
        let mut merged: Vec<Value> = self
            .index()
            .iter()
            .map(|item| {
                live_summaries
                    .iter()
                    .find(|live| live["id"] == item["id"])
                    .unwrap_or(item)
                    .clone()
            })
            .collect();
        for live in live_summaries {
            if !merged.iter().any(|item| item["id"] == live["id"]) {
                merged.push(live.clone());
            }
        }
        let frames: Vec<Value> = merged.into_iter().rev().take(limit as usize).collect();
        Ok(json!({ "frames": frames }))
    }
}

/// `chars` of a view as the trace reports it.
pub(crate) fn view_chars(text: &str) -> u64 {
    js_len(text) as u64
}
