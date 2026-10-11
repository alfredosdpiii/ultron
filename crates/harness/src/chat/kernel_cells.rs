//! A [`CellExecutor`] over persistent Python kernels (`crates/kernel`): one kernel per REPL lane, opened on first
//! use. A cell's host requests go to the composition root's handler, except `cell.output`, which publishes durable
//! partial output into the cell's `pi.live` slot (what an interrupted cell's result carries after a crash).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ai::Tool;
use durable::ConversationId;
use kernel::{ExecutionResult, HostHandler, Kernel, Signal, Status};
use serde_json::{Map, Value, json};

use super::cell::{CellCall, CellExecutor, CellProgress, CellResult, Replay};
use crate::scheduler::Cancel;
use crate::scheduler::cancel::lock;

/// Opens the kernel of one conversation's lane: every conversation (the root, each child) has its own kernels.
pub type OpenLane = Arc<dyn Fn(ConversationId, &str) -> Result<Arc<Kernel>, String> + Send + Sync>;
/// The host handler serving one cell's requests (the composition root's stack, delegation operations).
pub type CellHandlers = Arc<dyn Fn(&CellCall) -> Arc<dyn HostHandler> + Send + Sync>;
/// Decides whether a recorded cell may run again after a restart.
pub type ReplayPolicy = Arc<dyn Fn(&CellCall) -> Replay + Send + Sync>;

/// The `rlm` tool declaration: Python code for the persistent REPL, and an optional lane.
pub fn rlm_tool(description: &str) -> Tool {
    Tool {
        name: "rlm".into(),
        description: description.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Python code for the persistent REPL" },
            },
            "required": ["code"],
        }),
        constrained_sampling: None,
    }
}

/// Cells run in per-lane kernels.
pub struct KernelCells {
    tool: Tool,
    open: OpenLane,
    handlers: Option<CellHandlers>,
    replay: Option<ReplayPolicy>,
    lanes: Mutex<HashMap<(ConversationId, String), Arc<Kernel>>>,
}

impl KernelCells {
    /// `open` starts a lane's kernel; `handlers` serve the cells' other host requests.
    pub fn new(tool: Tool, open: OpenLane, handlers: Option<CellHandlers>) -> KernelCells {
        KernelCells {
            tool,
            open,
            handlers,
            replay: None,
            lanes: Mutex::new(HashMap::new()),
        }
    }

    /// Mark some recorded cells replay-safe (default: none).
    pub fn with_replay(mut self, replay: ReplayPolicy) -> KernelCells {
        self.replay = Some(replay);
        self
    }

    /// A conversation lane's kernel, opening it on first use.
    pub fn lane(&self, conversation: ConversationId, name: &str) -> Result<Arc<Kernel>, String> {
        let key = (conversation, name.to_owned());
        if let Some(kernel) = lock(&self.lanes).get(&key) {
            return Ok(kernel.clone());
        }
        let kernel = (self.open)(conversation, name)?;
        Ok(lock(&self.lanes).entry(key).or_insert(kernel).clone())
    }

    /// Lanes with a kernel, as `(conversation, lane)`.
    pub fn open_lanes(&self) -> Vec<(ConversationId, String)> {
        lock(&self.lanes).keys().cloned().collect()
    }

    /// Shut every lane's kernel down.
    pub fn shutdown(&self) {
        for kernel in lock(&self.lanes).drain().map(|(_, kernel)| kernel) {
            kernel.shutdown();
        }
    }
}

/// Serves one cell's host requests: `cell.output` to its progress, the rest to the root's handler.
struct CellHandler {
    progress: CellProgress,
    inner: Option<Arc<dyn HostHandler>>,
}

impl HostHandler for CellHandler {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind == "cell.output" {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .ok_or("cell.output needs a `text` string")?;
            self.progress.output(text);
            return Ok(Value::Null);
        }
        match &self.inner {
            Some(inner) => inner.handle(kind, payload, signal),
            None => Err(format!("Unknown host request {kind}")),
        }
    }
}

/// The text the model reads for a cell: output, the last expression's value, and the error.
pub fn cell_text(result: &ExecutionResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for stream in [&result.stdout, &result.stderr] {
        let stream = stream.trim_end_matches('\n');
        if !stream.is_empty() {
            parts.push(stream.to_owned());
        }
    }
    if let Some(value) = result.result.as_ref().filter(|value| !value.is_empty()) {
        parts.push(value.clone());
    }
    if let Some(error) = &result.error {
        if error.traceback.is_empty() {
            parts.push(format!("{}: {}", error.ename, error.evalue));
        } else {
            parts.push(error.traceback.join("\n"));
        }
    }
    if parts.is_empty() {
        "(no result)".into()
    } else {
        parts.join("\n")
    }
}

impl CellExecutor for KernelCells {
    fn tool(&self) -> Tool {
        self.tool.clone()
    }

    fn replay(&self, call: &CellCall) -> Replay {
        self.replay.as_ref().map_or(Replay::Unsafe, |replay| replay(call))
    }

    fn execute(&self, call: &CellCall, progress: &CellProgress, signal: &Cancel) -> Result<CellResult, String> {
        let kernel = self.lane(call.conversation_id, call.lane())?;
        let kernel_signal = Signal::new();
        let bridge = kernel_signal.clone();
        let registration = signal.on_cancel(move |reason| bridge.abort(reason.to_string()));
        if registration.is_none() {
            kernel_signal.abort(signal.reason().map(|reason| reason.to_string()).unwrap_or_default());
        }
        let handler: Arc<dyn HostHandler> = Arc::new(CellHandler {
            progress: progress.clone(),
            inner: self.handlers.as_ref().map(|handlers| handlers(call)),
        });
        let executed = kernel.execute(call.code(), Some(&kernel_signal), Some(handler));
        if let Some(id) = registration {
            signal.off(id);
        }
        let result = executed.map_err(|error| error.to_string())?;
        Ok(CellResult {
            is_error: result.status == Status::Error,
            ..CellResult::text(cell_text(&result))
        })
    }
}
