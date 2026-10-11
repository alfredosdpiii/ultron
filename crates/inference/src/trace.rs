//! Frame state, traces and `rlm.frames` (inference.ts `trace`, `summary`, `writeTrace`, `list`): every finished frame
//! leaves a trace document (task, views, contract, budget, attempts, requests) and a summary in a bounded index.

use super::{Kind, Ledger, OUTPUT_PREVIEW_CHARS, Outcome, View, bounded, fields, js_len, optional_integer};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const TRACE_INDEX_LIMIT: usize = 200;
pub(crate) const TRACE_REQUEST_LIMIT: usize = 50;

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

/// The budget node one `rlm.infer` or `rlm.map` call shares among its frames.
pub(crate) struct Budget {
    pub id: String,
    pub node: usize,
    pub depth: u64,
    /// The token limit is `rlm.map`'s default, not the caller's: exhaustion then says how to raise it.
    pub default_tokens: bool,
}

impl Budget {
    pub fn snapshot(&self, ledger: &Ledger) -> Value {
        let node = &ledger.nodes[self.node];
        json!({
            "id": self.id, "parent": null, "depth": self.depth,
            "limits": {"calls": node.calls.limit, "tokens": node.tokens.limit},
            "spent": {"calls": node.calls.spent, "tokens": node.tokens.spent},
        })
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
    pub kind: &'static str,
    pub batch: usize,
}

impl Frame {
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
            kind: "infer",
            batch: 1,
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

/// What a finished frame's trace records besides the frame itself.
pub(crate) struct Spec<'a> {
    pub contract: Option<&'a Value>,
    pub max_repairs: u64,
    pub depth: u64,
    pub model: Option<&'a str>,
}

fn status(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Complete(_) => "complete",
        Outcome::Incomplete(..) => "incomplete",
        Outcome::Error(_) => "error",
    }
}

/// `trace(frame)`: the full document `rlm.frames(id)` returns. This host runs frames as direct calls, so `taskId`
/// and `lane` are null.
pub(crate) fn trace(
    ledger: &mut Ledger,
    budget: &Budget,
    frame: &Frame,
    spec: &Spec,
    outcome: &Outcome,
    ended_at: u64,
) -> Value {
    let mut out = Map::new();
    out.insert("version".into(), 1.into());
    out.insert("id".into(), frame.id.clone().into());
    out.insert("status".into(), status(outcome).into());
    match outcome {
        Outcome::Incomplete(reason, detail) => {
            out.insert("reason".into(), (*reason).into());
            out.insert("detail".into(), detail.clone().into());
        }
        Outcome::Error(error) => {
            out.insert("error".into(), bounded(error, OUTPUT_PREVIEW_CHARS).into());
        }
        Outcome::Complete(value) => {
            out.insert("value".into(), bounded(&value.to_string(), OUTPUT_PREVIEW_CHARS).into());
        }
    }
    out.insert("task".into(), bounded(&frame.task, OUTPUT_PREVIEW_CHARS).into());
    let views: Vec<Value> = frame
        .views
        .iter()
        .map(|view| {
            let mut item = Map::new();
            item.insert("label".into(), view.label.clone().into());
            item.insert("chars".into(), js_len(&view.text).into());
            item.insert("byReference".into(), false.into());
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
    out.insert("taskId".into(), Value::Null);
    out.insert("lane".into(), Value::Null);
    out.insert("callerTaskId".into(), Value::Null);
    out.insert("parentFrame".into(), Value::Null);
    let model = frame.model.as_deref().or(spec.model);
    out.insert("model".into(), model.map_or(Value::Null, Value::from));
    out.insert("startedAt".into(), frame.started_at.into());
    out.insert("endedAt".into(), ended_at.into());
    out.insert("attempts".into(), frame.attempts.clone().into());
    out.insert("requests".into(), frame.requests.clone().into());
    Value::Object(out)
}

/// `summary(frame)`: one row of `rlm.frames()`.
pub(crate) fn summary(ledger: &Ledger, budget: &Budget, frame: &Frame, outcome: &Outcome, ended_at: u64) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), frame.id.clone().into());
    out.insert("status".into(), status(outcome).into());
    if let Outcome::Incomplete(reason, _) = outcome {
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
    out.insert("taskId".into(), Value::Null);
    out.insert("parentFrame".into(), Value::Null);
    out.insert("startedAt".into(), frame.started_at.into());
    out.insert("endedAt".into(), ended_at.into());
    out.insert("kind".into(), frame.kind.into());
    out.insert("batch".into(), frame.batch.into());
    out.insert("callerTaskId".into(), Value::Null);
    out.insert("lane".into(), Value::Null);
    let node = &ledger.nodes[budget.node];
    out.insert(
        "budget".into(),
        json!({"id": budget.id, "calls": node.calls.limit, "tokens": node.tokens.limit, "depth": budget.depth}),
    );
    Value::Object(out)
}

/// The session's frame traces (in memory) and the index of the newest 200 summaries.
#[derive(Default)]
pub(crate) struct Traces {
    documents: HashMap<String, Value>,
    index: Vec<Value>,
}

impl Traces {
    pub fn write(&mut self, id: &str, trace: Value, summary: Value) {
        self.documents.insert(id.to_string(), trace);
        match self.index.iter().position(|item| item["id"] == id) {
            Some(at) => self.index[at] = summary,
            None => self.index.push(summary),
        }
        if self.index.len() > TRACE_INDEX_LIMIT {
            self.index.drain(..self.index.len() - TRACE_INDEX_LIMIT);
        }
    }

    /// `rlm.frames`: one frame's trace by id, else the newest `limit` (default 20) summaries, newest first.
    pub fn list(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        fields(payload, &["id", "limit"])?;
        if let Some(id) = payload.get("id") {
            let id = id
                .as_str()
                .filter(|id| id.starts_with("frame-"))
                .ok_or("id must be a frame id")?;
            return self
                .documents
                .get(id)
                .cloned()
                .ok_or_else(|| format!("Unknown frame {id}"));
        }
        let limit = optional_integer(payload.get("limit"), "limit", 1, TRACE_INDEX_LIMIT as u64)?.unwrap_or(20);
        let frames: Vec<Value> = self.index.iter().rev().take(limit as usize).cloned().collect();
        Ok(json!({ "frames": frames }))
    }
}
