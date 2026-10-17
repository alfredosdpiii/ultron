//! The durable REPL cell: the RLM equivalent of upstream's `pi.tool` (spec §8.4), the task behind every call of the
//! model's one tool. Port of upstream `harness/tool.ts` for model-issued calls.
//!
//! `call` reads the call from its assistant entry, checks the tool and arguments, and commits intent (the code, its
//! lane, and the replay policy) before the cell runs. The cell then runs through a [`CellExecutor`] (normally the
//! lane's persistent kernel), publishing throttled partial output into its `pi.live` slot, and one commit appends the
//! result entry and completes. `execute` is reached only by recovery: a cell whose intent was committed before a
//! crash is never run again unless both the recorded and the current policy say it is replay-safe; otherwise the
//! model gets an `interrupted` result carrying the committed partial output, exactly once.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ai::{AssistantContent, Message, Tool};
use durable::{
    ConversationId, EntryDraft, EntryId, EntryRecord, TaskDefinition, TaskId, TaskOutcome, TaskOutcomeError,
    TaskRecord, TaskState, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Services;
use super::entries;
use super::generation::Control;
use super::live::{CELL_KIND, clear_progress, finish_slot, live, slot, slot_index, slot_path};
use super::throttle::Throttle;
use super::usage::{Bucket, record_usage};
use super::util::{d, from_value, strict, to_value};
use crate::scheduler::cancel::lock;
use crate::scheduler::{Cancel, HarnessError, NextState, Result, Task, TaskRuntime};

/// Bytes of running output a slot retains.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Lines of running output a slot retains.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Whether a cell whose intent was committed may run again after a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Replay {
    Safe,
    #[default]
    Unsafe,
}

/// One diagnostic of a result; rendered into the content the model reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
}

impl Diagnostic {
    pub fn error(code: &str, message: &str) -> Diagnostic {
        Diagnostic {
            severity: "error".into(),
            code: code.into(),
            message: message.into(),
        }
    }

    fn warn(code: &str, message: &str) -> Diagnostic {
        Diagnostic {
            severity: "warn".into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

/// What a cell produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellResult {
    /// Content blocks (`{type: "text", text}` or images); `None` uses the running output the cell published.
    pub output: Option<Vec<Value>>,
    pub is_error: bool,
    pub details: Option<Value>,
    /// Spend of host operations the cell made (pi-ai `Usage` JSON), recorded under the tool's name.
    pub usage: Option<Value>,
    pub diagnostics: Vec<Diagnostic>,
    /// Ask the run to end (`terminate`) or to continue in a fresh context (`handoff`).
    pub control: Option<Control>,
}

impl CellResult {
    pub fn text(text: impl Into<String>) -> CellResult {
        CellResult {
            output: Some(vec![json!({ "type": "text", "text": text.into() })]),
            ..CellResult::default()
        }
    }

    /// An error result the Harness writes itself: no content and one `error` diagnostic.
    pub fn error(code: &str, message: &str) -> CellResult {
        CellResult {
            output: Some(Vec::new()),
            is_error: true,
            diagnostics: vec![Diagnostic::error(code, message)],
            ..CellResult::default()
        }
    }
}

/// One cell call as the executor sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct CellCall {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
    pub call_id: String,
    pub name: String,
    /// The final arguments recorded with the intent: `code`, and `lane` when the call names one.
    pub arguments: Value,
}

impl CellCall {
    pub fn code(&self) -> &str {
        self.arguments["code"].as_str().unwrap_or_default()
    }

    /// The REPL lane the cell runs in: `main` unless the call names another.
    pub fn lane(&self) -> &str {
        self.arguments["lane"].as_str().unwrap_or("main")
    }
}

/// Runs cells: normally one persistent kernel per lane. The model's tool list is exactly [`CellExecutor::tool`].
pub trait CellExecutor: Send + Sync {
    /// The REPL tool the model is offered.
    fn tool(&self) -> Tool;

    /// Check and normalize a call's arguments, or say why they are invalid. Default: a `code` string and an optional
    /// `lane` string.
    fn prepare(&self, arguments: &Value) -> std::result::Result<Value, String> {
        if !arguments["code"].is_string() {
            return Err("rlm needs a `code` string".into());
        }
        if arguments.get("lane").is_some_and(|lane| !lane.is_string()) {
            return Err("rlm's `lane` must be a string".into());
        }
        Ok(arguments.clone())
    }

    /// Whether a recorded intent may run again after a restart. Cells are unsafe unless the executor knows better.
    fn replay(&self, call: &CellCall) -> Replay {
        let _ = call;
        Replay::Unsafe
    }

    /// Run one cell. `progress` publishes running output; `signal` is cancelled by an abort mark or close, and an
    /// `Err` after it is the cancellation.
    fn execute(
        &self,
        call: &CellCall,
        progress: &CellProgress,
        signal: &Cancel,
    ) -> std::result::Result<CellResult, String>;
}

/// The running output a cell retains (its head, within the limits) and what the limits dropped.
#[derive(Clone, Debug, Default, PartialEq)]
struct Retained {
    text: String,
    lines: usize,
    dropped_bytes: usize,
    dropped_lines: usize,
}

impl Retained {
    fn push(&mut self, chunk: &str) {
        if self.dropped_bytes > 0 {
            self.dropped_bytes += chunk.len();
            self.dropped_lines += chunk.matches('\n').count();
            return;
        }
        let mut taken = 0;
        for (index, character) in chunk.char_indices() {
            let end = index + character.len_utf8();
            let lines = self.lines + usize::from(character == '\n');
            if self.text.len() + end > DEFAULT_MAX_BYTES || lines > DEFAULT_MAX_LINES {
                break;
            }
            taken = end;
            self.lines = lines;
        }
        self.text.push_str(&chunk[..taken]);
        let rest = &chunk[taken..];
        if !rest.is_empty() {
            self.dropped_bytes += rest.len();
            self.dropped_lines += rest.matches('\n').count().max(1);
        }
    }
}

/// What a running cell has published: its retained output and last details.
type Published = (Retained, Option<Value>);

/// Publishes a running cell's output and details into its `pi.live` slot, throttled.
/// Clones share it, so a host request served on another thread (the cell's `cell.output`) can publish too.
#[derive(Clone)]
pub struct CellProgress {
    retained: Arc<Mutex<(Retained, Option<Value>)>>,
    throttle: Arc<Mutex<Option<Throttle<Published>>>>,
}

impl CellProgress {
    fn new(runtime: &TaskRuntime, interval: Duration) -> CellProgress {
        let committer = runtime.clone();
        let throttle = Throttle::new(interval, move |(retained, details): (Retained, Option<Value>)| {
            let conversation_id = committer.conversation_id();
            let task_id = committer.task_id();
            let result = committer.commit(&committer.ctx(), |tx, _| {
                let live = live(tx, conversation_id)?;
                let Some(index) = slot_index(&live, task_id)? else {
                    return Ok(None);
                };
                let current = d(live.get(&slot_path(index, Some("output"))))?;
                if current.as_ref().and_then(Value::as_str).unwrap_or_default() != retained.text {
                    d(live.set(&slot_path(index, Some("output")), json!(retained.text)))?;
                }
                if retained.dropped_bytes > 0 {
                    d(live.set(&slot_path(index, Some("droppedBytes")), json!(retained.dropped_bytes)))?;
                    d(live.set(&slot_path(index, Some("droppedLines")), json!(retained.dropped_lines)))?;
                }
                if let Some(details) = details {
                    d(live.set(&slot_path(index, Some("details")), details))?;
                }
                Ok(None)
            });
            if let Err(error) = result
                && !committer.signal().is_cancelled()
            {
                let _ = committer.report(&error);
            }
        });
        CellProgress {
            retained: Arc::new(Mutex::new((Retained::default(), None))),
            throttle: Arc::new(Mutex::new(Some(throttle))),
        }
    }

    /// A progress sink that publishes nothing, for executors driven outside a task (tests, tools).
    pub fn detached() -> CellProgress {
        CellProgress {
            retained: Arc::new(Mutex::new((Retained::default(), None))),
            throttle: Arc::new(Mutex::new(None)),
        }
    }

    /// Append running output.
    pub fn output(&self, chunk: &str) {
        let snapshot = {
            let mut state = lock(&self.retained);
            state.0.push(chunk);
            state.clone()
        };
        if let Some(throttle) = lock(&self.throttle).as_ref() {
            throttle.mark(snapshot);
        }
    }

    /// Replace the running details.
    pub fn details(&self, value: Value) {
        let snapshot = {
            let mut state = lock(&self.retained);
            state.1 = Some(strict(value));
            state.clone()
        };
        if let Some(throttle) = lock(&self.throttle).as_ref() {
            throttle.mark(snapshot);
        }
    }

    /// The output retained so far.
    pub fn retained_output(&self) -> String {
        lock(&self.retained).0.text.clone()
    }

    /// Stop publishing (later output is kept but not committed) and return what was retained.
    fn stop(self) -> (Retained, Option<Value>) {
        let throttle = lock(&self.throttle).take();
        if let Some(throttle) = throttle {
            throttle.stop();
        }
        lock(&self.retained).clone()
    }
}

/// The cell task's input and checkpoint (spec §8.4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum CellCheckpoint {
    Call,
    /// Durable intent: the final arguments and the replay policy recorded before execution.
    Execute {
        arguments: Value,
        replay: Replay,
    },
}

static CELL_DEFINITION: LazyLock<TaskDefinition> =
    LazyLock::new(|| TaskDefinition::new(CELL_KIND, 2, |_| to_value(&CellCheckpoint::Call)));

pub(crate) fn cell_definition() -> &'static TaskDefinition {
    &CELL_DEFINITION
}

/// The built-in cell task over `services`.
pub(crate) fn cell_task(services: Arc<Services>) -> Task {
    let (call, execute, abort) = (services.clone(), services.clone(), services);
    Task::define(CELL_KIND, 2, |input| (CELL_DEFINITION.initial)(input))
        // Version 1 had only model-issued calls.
        .migrate(|input, checkpoint, _| {
            let mut input = input.clone();
            if input.get("kind").is_none() {
                input["kind"] = json!("model");
            }
            Ok((input, checkpoint.clone()))
        })
        .phase("call", move |task, runtime| call_phase(&call, task, runtime))
        .phase("execute", move |task, runtime| execute_phase(&execute, task, runtime))
        .abort(move |task, runtime| abort_handler(&abort, task, runtime))
        .build()
}

fn checkpoint_of(task: &TaskRecord) -> Result<CellCheckpoint> {
    match &task.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => from_value(checkpoint.clone(), "cell checkpoint"),
        _ => Err(HarnessError::msg("A terminal cell has no checkpoint")),
    }
}

/// The call (`id`, `name`, `arguments`) from the assistant entry.
fn read_call(runtime: &TaskRuntime, task: &TaskRecord) -> Result<(String, String, Value)> {
    let assistant = EntryId(task.input["assistant"].as_u64().unwrap_or_default());
    let call_id = task.input["callId"].as_str().unwrap_or_default().to_owned();
    let entry = runtime.read(|tx| Ok(tx.entry_of(entries::ASSISTANT, assistant)?))?;
    let message = entry
        .and_then(|entry| entry.model)
        .and_then(|model| model.into_iter().next());
    if let Some(message) = message
        && let Message::Assistant(message) = super::context::message_of(message)?
    {
        for content in message.content {
            if let AssistantContent::ToolCall(call) = content
                && call.id == call_id
            {
                return Ok((call.id, call.name, call.arguments));
            }
        }
    }
    Err(HarnessError::Error(format!(
        "Entry {} has no tool call {call_id}",
        assistant.0
    )))
}

/// How a cell task ends; the result entry is appended either way. `failed` (execution threw or was interrupted)
/// records cancellation intent for what the cell owns; a result with `is_error` still completes.
enum Ending {
    Completed,
    Aborted,
    Failed(String),
}

fn call_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let (call_id, name, arguments) = read_call(runtime, task)?;
    let tool = services.cells.tool();
    if name != tool.name {
        let error = CellResult::error("tool_unavailable", &format!("Tool {name} is not available"));
        return settle(
            runtime,
            task,
            &call_id,
            &name,
            Ending::Completed,
            |_| error.clone(),
            None,
        );
    }
    let arguments = match services.cells.prepare(&arguments) {
        Ok(arguments) => strict(arguments),
        Err(message) => {
            let invalid = CellResult::error("invalid_arguments", &message);
            return settle(
                runtime,
                task,
                &call_id,
                &name,
                Ending::Completed,
                |_| invalid.clone(),
                None,
            );
        }
    };
    let call = CellCall {
        conversation_id: runtime.conversation_id(),
        task_id: runtime.task_id(),
        call_id: call_id.clone(),
        name: name.clone(),
        arguments: arguments.clone(),
    };
    // `before_tool`: a blocked cell never runs; its result is the reason (`immediateError`), and `terminate` ends
    // the run after the round.
    if let Some(gate) = &services.hooks.gate_tool {
        let run = runtime.read(|tx| super::gate::gate_run(tx, call.conversation_id))?;
        if let Some(block) = gate(&run, &call) {
            let blocked = CellResult {
                output: Some(vec![json!({ "type": "text", "text": block.reason })]),
                is_error: true,
                control: block.terminate.then_some(Control {
                    terminate: Some(true),
                    handoff: None,
                }),
                ..CellResult::default()
            };
            return settle(
                runtime,
                task,
                &call_id,
                &name,
                Ending::Completed,
                |_| blocked.clone(),
                None,
            );
        }
    }
    let replay = services.cells.replay(&call);
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        if let Some(index) = slot_index(&live, task_id)? {
            d(live.set(&slot_path(index, Some("status")), json!("running")))?;
        }
        Ok(Some(NextState::Running {
            checkpoint: to_value(&CellCheckpoint::Execute {
                arguments: arguments.clone(),
                replay,
            }),
        }))
    })?;
    run(services, runtime, task, call)
}

/// Recovery after intent: run again only when the recorded and the current policy both say `safe`.
fn execute_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let CellCheckpoint::Execute { arguments, replay } = checkpoint_of(task)? else {
        return Err(HarnessError::msg("execute phase without intent"));
    };
    let (call_id, name, _) = read_call(runtime, task)?;
    let call = CellCall {
        conversation_id: runtime.conversation_id(),
        task_id: runtime.task_id(),
        call_id: call_id.clone(),
        name: name.clone(),
        arguments,
    };
    if replay == Replay::Safe && name == services.cells.tool().name && services.cells.replay(&call) == Replay::Safe {
        // The rerun reports from scratch; clear what the interrupted attempt published.
        let conversation_id = runtime.conversation_id();
        let task_id = runtime.task_id();
        runtime.commit(&runtime.ctx(), |tx, _| {
            let live = live(tx, conversation_id)?;
            if let Some(index) = slot_index(&live, task_id)? {
                clear_progress(&live, index)?;
            }
            Ok(None)
        })?;
        return run(services, runtime, task, call);
    }
    let message = format!("Tool {name} was interrupted and may have partially run");
    settle(
        runtime,
        task,
        &call_id,
        &name,
        Ending::Failed(message.clone()),
        |slot| from_slot(slot, "interrupted", &message),
        None,
    )
}

fn abort_handler(_services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let (call_id, name, _) = read_call(runtime, task)?;
    let executing = matches!(checkpoint_of(task)?, CellCheckpoint::Execute { .. });
    let (code, message) = if task.abort_reason.is_none() {
        ("aborted", format!("Tool {name} was aborted"))
    } else if executing {
        (
            "interrupted",
            format!("Tool {name} was interrupted by a restart and may have partially run"),
        )
    } else {
        (
            "abandoned",
            format!("Tool {name} was not started: its caller ended with a restart"),
        )
    };
    settle(
        runtime,
        task,
        &call_id,
        &name,
        Ending::Aborted,
        |slot| from_slot(slot, code, &message),
        None,
    )
}

/// Execute the cell, then settle its result.
fn run(services: &Services, runtime: &TaskRuntime, task: &TaskRecord, call: CellCall) -> Result<()> {
    let interval = Duration::from_millis((services.settings)().output_interval_ms);
    let progress = CellProgress::new(runtime, interval);
    let started = Instant::now();
    let executed = services.cells.execute(&call, &progress, runtime.signal());
    let duration = started.elapsed().as_millis() as u64;
    let (retained, details) = progress.stop();
    let (mut result, ending) = match executed {
        Ok(result) => (result, Ending::Completed),
        Err(message) => {
            runtime.signal().check()?;
            let result = CellResult {
                output: None,
                is_error: true,
                diagnostics: vec![Diagnostic::error("tool_error", &message)],
                ..CellResult::default()
            };
            (result, Ending::Failed(format!("Tool {} threw", call.name)))
        }
    };
    if result.output.is_none() {
        result.output = Some(if retained.text.is_empty() {
            Vec::new()
        } else {
            vec![json!({ "type": "text", "text": retained.text })]
        });
        if retained.dropped_bytes > 0 {
            result.diagnostics.insert(0, truncated(&retained, Some("head")));
        }
    }
    if result.details.is_none() {
        result.details = details;
    }
    settle(
        runtime,
        task,
        &call.call_id,
        &call.name,
        ending,
        |_| result.clone(),
        Some(duration),
    )
}

fn truncated(retained: &Retained, retain: Option<&str>) -> Diagnostic {
    let kept = match retain {
        Some("head") => " to its beginning",
        Some(_) => " to its end",
        None => "",
    };
    Diagnostic::warn(
        "truncated",
        &format!(
            "Output truncated{kept}: {} lines, {} bytes dropped",
            retained.dropped_lines, retained.dropped_bytes
        ),
    )
}

/// An error result from the slot's durable partial output, details and diagnostics.
fn from_slot(slot: Option<&Value>, code: &str, message: &str) -> CellResult {
    let mut diagnostics: Vec<Diagnostic> = slot
        .and_then(|slot| slot.get("diagnostics"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default();
    let dropped_bytes = slot.and_then(|slot| slot["droppedBytes"].as_u64()).unwrap_or(0) as usize;
    if dropped_bytes > 0 {
        let dropped_lines = slot.and_then(|slot| slot["droppedLines"].as_u64()).unwrap_or(0) as usize;
        diagnostics.push(truncated(
            &Retained {
                dropped_bytes,
                dropped_lines,
                ..Retained::default()
            },
            None,
        ));
    }
    diagnostics.push(Diagnostic::error(code, message));
    let output = slot
        .and_then(|slot| slot["output"].as_str())
        .filter(|text| !text.is_empty())
        .map(|text| vec![json!({ "type": "text", "text": text })])
        .unwrap_or_default();
    CellResult {
        output: Some(output),
        is_error: true,
        details: slot.and_then(|slot| slot.get("details").cloned()),
        diagnostics,
        ..CellResult::default()
    }
}

/// Commit the cell's terminal state: append its result entry, mark its slot done with the entry, and complete with
/// `{ kind: "model", entryId, control? }`. `build` receives the slot so interruption and abort report the durable
/// partial output.
fn settle(
    runtime: &TaskRuntime,
    _task: &TaskRecord,
    call_id: &str,
    name: &str,
    ending: Ending,
    build: impl FnOnce(Option<&Value>) -> CellResult,
    duration_ms: Option<u64>,
) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        let current = slot(&live, task_id)?;
        let result = build(current.as_ref());
        let entry = append_tool_result(tx, conversation_id, call_id, name, &result, runtime.now()?, duration_ms)?;
        if let Some(index) = slot_index(&live, task_id)? {
            finish_slot(&live, index)?;
            d(live.set(&slot_path(index, Some("entry")), json!(entry.id.0)))?;
        }
        let mut settled = json!({ "kind": "model", "entryId": entry.id.0 });
        if let (Ending::Completed, Some(control)) = (&ending, &result.control) {
            settled["control"] = strict(to_value(control));
        }
        let outcome = match ending {
            Ending::Completed => TaskOutcome::Completed { result: settled },
            Ending::Aborted => TaskOutcome::Aborted {
                reason: None,
                result: Some(settled),
            },
            Ending::Failed(message) => TaskOutcome::Failed {
                error: TaskOutcomeError { message, detail: None },
                result: Some(settled),
            },
        };
        Ok(Some(NextState::Terminal { outcome }))
    })
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let lines: Vec<String> = diagnostics
        .iter()
        .map(|diagnostic| format!("[{}] {}", diagnostic.severity, diagnostic.message))
        .collect();
    format!("<harness>\n{}\n</harness>", lines.join("\n"))
}

/// Append a `pi.tool-result` entry. The content ends with the rendered diagnostics, so the stored message is exactly
/// what the model sees; `data` keeps the structured list. A result's usage is added to `pi.usage` in the same commit.
pub(crate) fn append_tool_result(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    call_id: &str,
    name: &str,
    result: &CellResult,
    timestamp: u64,
    duration_ms: Option<u64>,
) -> Result<EntryRecord> {
    let mut content = result.output.clone().unwrap_or_default();
    if !result.diagnostics.is_empty() {
        content.push(json!({ "type": "text", "text": render_diagnostics(&result.diagnostics) }));
    }
    let mut message = json!({
        "role": "toolResult",
        "toolCallId": call_id,
        "toolName": name,
        "content": content,
    });
    if let Some(details) = &result.details {
        message["details"] = details.clone();
    }
    if let Some(usage) = &result.usage {
        message["usage"] = usage.clone();
    }
    message["isError"] = json!(result.is_error);
    if let Some(duration) = duration_ms {
        message["durationMs"] = json!(duration);
    }
    message["timestamp"] = json!(timestamp);
    if let Some(usage) = &result.usage {
        record_usage(tx, conversation_id, Bucket::Tools, name, usage)?;
    }
    Ok(tx.append_entry(
        conversation_id,
        EntryDraft {
            model: Some(vec![message]),
            data: Some(json!({ "diagnostics": result.diagnostics })),
            ..EntryDraft::new(entries::TOOL_RESULT)
        },
    )?)
}
