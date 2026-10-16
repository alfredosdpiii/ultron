//! The bridge between the session actor ([`crate::core`]) and the durable conversation runtime (`harness::chat`)
//! that runs every root turn.
//!
//! The runtime owns the turn: its generation task requests the model (with the request pinned at preparation, so a
//! restarted worker resends exactly that request), its cell tasks run the REPL cells (never replayed: a cell a dead
//! worker left becomes an `interrupted` result with the output it had published), and its inbox holds steering and
//! follow-up messages until a boundary places them. The actor keeps the TypeScript surface: the lane-watch events
//! clients see and the format-4 operation journal TypeScript reads.
//!
//! - [`TurnModels`] is the runtime's provider. Before each request it asks the actor to begin the generation (the
//!   journal's `assistant.ready`, the user messages the request takes) and to build the request exactly as the
//!   TypeScript agent loop does, from the session's transcript; stream events go to the actor as they arrive.
//! - [`TurnCells`] runs a cell through the lane's `rlm` tool and publishes what the cell's host operations print as
//!   its durable partial output.
//! - [`observer`] sees each durable commit: a committed answer, the start of its tool round and each committed cell
//!   result reach the actor as the agent loop's events, so the transcript commits only what the runtime committed.

use crate::durable_store::Observer;
use agent::{AgentEvent, AgentTool, AgentToolResult};
use ai::{AbortSignal, AssistantContent, AssistantMessage, AssistantMessageEvent, Message, Model, StopReason, Tool};
use durable::{ROOT_CONVERSATION_ID, StorageWrite};
use harness::chat::{CellCall, CellExecutor, CellProgress, CellResult, Models, StreamRequest};
use harness::scheduler::{Cancel, HarnessError};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the actor answers a generation's start with: the request messages and the key to send them with.
pub struct GenerationStart {
    pub messages: Vec<Message>,
    pub api_key: Option<String>,
}

/// A one-shot answer the asking runtime thread waits for, together with its own invocation signal.
pub struct Answer<T> {
    value: Arc<Mutex<Option<T>>>,
    ready: Cancel,
}

impl<T> Clone for Answer<T> {
    fn clone(&self) -> Self {
        Answer {
            value: self.value.clone(),
            ready: self.ready.clone(),
        }
    }
}

impl<T> Default for Answer<T> {
    fn default() -> Self {
        Answer {
            value: Arc::new(Mutex::new(None)),
            ready: Cancel::new(),
        }
    }
}

impl<T> Answer<T> {
    pub fn give(&self, value: T) {
        *lock(&self.value) = Some(value);
        self.ready.abort();
    }

    /// The answer, or `None` when `signal` is cancelled first.
    fn wait(&self, signal: &Cancel) -> Option<T> {
        let linked = Cancel::any(&[signal, &self.ready]);
        linked.signal().wait();
        lock(&self.value).take()
    }
}

/// What the runtime's threads send the actor.
pub enum Request {
    /// A generation is about to request the model; `users` are the user messages placed since the previous one.
    Generation {
        users: Vec<Message>,
        answer: Answer<Result<GenerationStart, StartError>>,
    },
    /// An agent-loop event of the running turn.
    Event(Box<AgentEvent>),
    /// A response overflowed the context window (`crate::auto_compaction::overflowed`): the actor compacts and asks
    /// for the request again, or settles it as the answer to commit.
    Overflow {
        message: Box<AssistantMessage>,
        answer: Answer<Settled>,
    },
    /// The root's tool round ran its last cell (`turn_end`, before its results commit and the boundary places
    /// queued messages): `calls` calls, `failed` of them errors. The actor answers once it steered the run if due.
    RoundEnded {
        calls: usize,
        failed: usize,
        answer: Answer<()>,
    },
}

/// Why the actor could not start a generation.
pub enum StartError {
    /// The request could not be built: the generation answers with this error.
    Request(String),
    /// The checkpoint before the request failed (an automatic compaction): the turn ends with this error.
    Checkpoint(String),
}

/// What the actor makes of an overflowed response.
pub enum Settled {
    /// The runtime commits this answer: the error the overflow is committed as.
    Answer(Box<AssistantMessage>),
    /// The context was compacted: request again.
    Retry,
    /// The compaction failed after the overflowed answer was committed: the turn ends with this error.
    Failed(String),
}

/// The actor's side of the bridge.
pub type ToActor = Arc<dyn Fn(Request) + Send + Sync>;

/// A result in the agent loop's shape, as the actor journals it.
#[derive(Clone)]
struct Reported {
    name: String,
    result: AgentToolResult,
    is_error: bool,
}

#[derive(Default)]
struct Round {
    /// The calls of the current tool round, in call order.
    calls: Vec<(String, String)>,
    done: HashMap<String, Reported>,
}

/// State shared by the provider, the cell executor and the observer.
pub struct Bridge {
    to_actor: ToActor,
    /// Results of cells this process ran, in the agent loop's shape, until their committed result is observed.
    results: Mutex<HashMap<String, Reported>>,
    round: Mutex<Round>,
    /// Set while the user's abort of the running turn is in effect: an aborted response then still reaches the
    /// transcript, as the agent loop commits it.
    user_abort: Mutex<bool>,
}

impl Bridge {
    pub fn new(to_actor: ToActor) -> Arc<Bridge> {
        Arc::new(Bridge {
            to_actor,
            results: Mutex::new(HashMap::new()),
            round: Mutex::new(Round::default()),
            user_abort: Mutex::new(false),
        })
    }

    /// Send the actor a request, in order with the runtime's own.
    pub fn request(&self, request: Request) {
        (self.to_actor)(request);
    }

    fn send(&self, event: AgentEvent) {
        (self.to_actor)(Request::Event(Box::new(event)));
    }

    pub fn set_user_abort(&self, aborted: bool) {
        *lock(&self.user_abort) = aborted;
    }

    /// Forget the current round (a new turn starts).
    pub fn clear(&self) {
        *lock(&self.round) = Round::default();
        lock(&self.results).clear();
    }

    /// An assistant answer committed by the runtime (or recovered from it): the agent loop's `message_end`, then the
    /// start of each call of its tool round.
    pub fn answer_committed(&self, message: AssistantMessage) {
        let calls: Vec<(String, String, Value)> = message
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some((call.id.clone(), call.name.clone(), call.arguments.clone())),
                _ => None,
            })
            .collect();
        let round = message.stop_reason == StopReason::ToolUse && !calls.is_empty();
        self.send(AgentEvent::MessageEnd {
            message: Message::Assistant(message),
        });
        if !round {
            return;
        }
        *lock(&self.round) = Round {
            calls: calls.iter().map(|(id, name, _)| (id.clone(), name.clone())).collect(),
            done: HashMap::new(),
        };
        for (id, name, args) in calls {
            self.send(AgentEvent::ToolExecutionStart {
                tool_call_id: id,
                tool_name: name,
                args,
            });
        }
    }

    /// The calls and failures of the current round once every call has a result (run here or committed).
    fn round_complete(&self) -> Option<(usize, usize)> {
        let round = lock(&self.round);
        let results = lock(&self.results);
        if round.calls.is_empty() {
            return None;
        }
        let mut failed = 0;
        for (id, _) in &round.calls {
            let reported = results.get(id).or_else(|| round.done.get(id))?;
            failed += usize::from(reported.is_error);
        }
        Some((round.calls.len(), failed))
    }

    /// The start of a tool round whose calls already started before a restart (no new start events).
    pub fn resume_round(&self, calls: Vec<(String, String)>, done: Vec<(String, AgentToolResult, bool)>) {
        let mut round = Round {
            calls,
            done: HashMap::new(),
        };
        for (id, result, is_error) in done {
            let name = round
                .calls
                .iter()
                .find(|(each, _)| *each == id)
                .map(|(_, name)| name.clone())
                .unwrap_or_default();
            round.done.insert(id, Reported { name, result, is_error });
        }
        *lock(&self.round) = round;
    }

    /// A cell result committed by the runtime: the agent loop's `tool_execution_end`, and once every call of the
    /// round has one, the result messages in call order.
    pub fn result_committed(&self, message: &Value) {
        let call_id = message["toolCallId"].as_str().unwrap_or_default().to_string();
        let reported = lock(&self.results).remove(&call_id).unwrap_or_else(|| Reported {
            name: message["toolName"].as_str().unwrap_or("rlm").to_string(),
            result: AgentToolResult {
                content: serde_json::from_value(message["content"].clone()).unwrap_or_default(),
                details: message.get("details").cloned().unwrap_or_else(|| json!({})),
                usage: serde_json::from_value(message["usage"].clone()).ok(),
                terminate: None,
            },
            is_error: message["isError"] == true,
        });
        self.send(AgentEvent::ToolExecutionEnd {
            tool_call_id: call_id.clone(),
            tool_name: reported.name.clone(),
            result: reported.result.clone(),
            is_error: reported.is_error,
        });
        let finished = {
            let mut round = lock(&self.round);
            round.done.insert(call_id, reported);
            if round.calls.iter().all(|(id, _)| round.done.contains_key(id)) {
                let round = std::mem::take(&mut *round);
                Some(
                    round
                        .calls
                        .iter()
                        .filter_map(|(id, _)| round.done.get(id).map(|r| (id.clone(), r.clone())))
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        };
        for (id, reported) in finished.into_iter().flatten() {
            let message = Message::ToolResult(ai::ToolResultMessage {
                tool_call_id: id,
                tool_name: reported.name,
                content: reported.result.content,
                details: Some(reported.result.details).filter(|d| !d.is_null()),
                usage: reported.result.usage,
                is_error: reported.is_error,
                timestamp: crate::lane::now_ms(),
            });
            self.send(AgentEvent::MessageStart {
                message: message.clone(),
            });
            self.send(AgentEvent::MessageEnd { message });
        }
    }
}

/// Fault injection for the durability tests: `__ULTRON_DURABLE_CRASH=<answer|result>:<marker file>` kills the
/// process right after the first durable commit of that kind, before anything else sees it (the marker file makes
/// it happen once across restarts).
fn crash_point() -> Option<(String, std::path::PathBuf)> {
    let spec = std::env::var("__ULTRON_DURABLE_CRASH").ok()?;
    let (kind, marker) = spec.split_once(':')?;
    Some((kind.to_string(), std::path::PathBuf::from(marker)))
}

fn maybe_crash(point: &Option<(String, std::path::PathBuf)>, kind: &str) {
    if let Some((wanted, marker)) = point
        && wanted == kind
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        std::process::abort();
    }
}

/// Sees each entry of another conversation once it is durable (a subagent lane's context entries landing).
pub type Committed = Arc<dyn Fn(&durable::EntryRecord) + Send + Sync>;

/// The observer of the session's durable commits: root-conversation answers and cell results reach the actor;
/// every other conversation's entries go to `committed`.
pub fn observer(bridge: Arc<Bridge>, committed: Committed) -> Observer {
    let crash = crash_point();
    Arc::new(move |writes: &[StorageWrite]| {
        for write in writes {
            let StorageWrite::Entry { value } = write else { continue };
            if value.conversation_id != ROOT_CONVERSATION_ID {
                committed(value);
                continue;
            }
            let Some(message) = value.model.as_ref().and_then(|model| model.first()) else {
                continue;
            };
            match value.kind.as_str() {
                harness::chat::entries::ASSISTANT => maybe_crash(&crash, "answer"),
                harness::chat::entries::TOOL_RESULT => maybe_crash(&crash, "result"),
                _ => {}
            }
            match value.kind.as_str() {
                harness::chat::entries::ASSISTANT => {
                    if let Ok(message) = serde_json::from_value::<AssistantMessage>(message.clone()) {
                        bridge.answer_committed(message);
                    }
                }
                harness::chat::entries::TOOL_RESULT => bridge.result_committed(message),
                _ => {}
            }
        }
    })
}

/// Bridges a runtime signal to an abort signal for the duration of one call.
struct Bridged {
    abort: AbortSignal,
    done: Cancel,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl Bridged {
    fn new(signal: &Cancel) -> Bridged {
        let abort = AbortSignal::new();
        let done = Cancel::new();
        let (watched, finished, target) = (signal.clone(), done.clone(), abort.clone());
        let watcher = std::thread::spawn(move || {
            let linked = Cancel::any(&[&watched, &finished]);
            linked.signal().wait();
            if watched.is_cancelled() {
                target.abort();
            }
        });
        Bridged {
            abort,
            done,
            watcher: Some(watcher),
        }
    }
}

impl Drop for Bridged {
    fn drop(&mut self) {
        self.done.abort();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, HarnessError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| HarnessError::Error(e.to_string()))
}

/// Resolves a pinned model by provider and ID.
pub type ResolveModel = Arc<dyn Fn(&str, &str) -> Option<Model> + Send + Sync>;
/// The API key of a provider.
pub type ProviderKey = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The provider as the durable generation calls it.
pub struct TurnModels {
    pub bridge: Arc<Bridge>,
    pub provider: Arc<dyn agent::StreamFn>,
    /// Resolves a pinned model (`provider`, `id`).
    pub resolve: ResolveModel,
    /// The key a child conversation's request uses for its model's provider.
    pub api_key: ProviderKey,
    /// The root conversation's provider session ID: requests that carry it are the root's turns; every other
    /// conversation is a subagent's.
    pub root_session: Arc<std::sync::OnceLock<String>>,
    /// Charges a response of another conversation (a subagent's, a typed agent's, another root conversation's) to
    /// the usage ledger; the root's own turns are charged by the actor.
    pub charge: Option<ChargeTurn>,
    /// Changes a child conversation's request before it is sent (`transform_context`: what `ctx` kept across a
    /// compaction).
    pub transform_child: Option<TransformChild>,
}

/// Changes the messages of a child conversation's request.
pub type TransformChild = Arc<dyn Fn(durable::ConversationId, &mut Vec<Message>) + Send + Sync>;

/// Charges one response of a conversation to the usage ledger.
pub type ChargeTurn = Arc<dyn Fn(durable::ConversationId, &AssistantMessage) + Send + Sync>;

impl TurnModels {
    /// A subagent's request: its conversation's context as the runtime built it.
    fn stream_child(
        &self,
        model: &Model,
        context: &ai::TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> harness::scheduler::Result<AssistantMessage> {
        let bridged = Bridged::new(signal);
        let mut options = ai::SimpleStreamOptions::default();
        options.base.api_key = (self.api_key)(&model.provider);
        options.base.signal = Some(bridged.abort.clone());
        options.reasoning = request.reasoning.clone();
        let mut sink = |event: &AssistantMessageEvent| on_event(event);
        let transformed;
        let context = match (&self.transform_child, request.conversation_id) {
            (Some(transform), Some(conversation)) => {
                let mut messages = context.messages.clone();
                transform(conversation, &mut messages);
                transformed = ai::TranscriptContext { messages };
                &transformed
            }
            _ => context,
        };
        let message = runtime()?.block_on(self.provider.stream(model, context, &options, &mut sink));
        drop(bridged);
        signal.check()?;
        if let (Some(charge), Some(conversation)) = (&self.charge, request.conversation_id) {
            charge(conversation, &message);
        }
        Ok(message)
    }
}

/// The user messages after the last answer or result: what the boundaries placed for this request.
fn trailing_users(messages: &[Message]) -> Vec<Message> {
    let start = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant(_) | Message::ToolResult(_)))
        .map_or(0, |i| i + 1);
    messages[start..]
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .cloned()
        .collect()
}

impl Models for TurnModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        (self.resolve)(provider, model_id)
    }

    fn stream(
        &self,
        model: &Model,
        context: &ai::TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> harness::scheduler::Result<AssistantMessage> {
        if self.root_session.get().map(String::as_str) != request.session_id.as_deref() {
            return self.stream_child(model, context, request, signal, on_event);
        }
        loop {
            let message = self.stream_root(model, context, request, signal, on_event)?;
            // An overflow compacts the context and asks again (once per overflow; the actor decides).
            if !crate::auto_compaction::overflowed(&message, model.context_window, model.max_tokens) {
                return Ok(message);
            }
            let answer = Answer::default();
            (self.bridge.to_actor)(Request::Overflow {
                message: Box::new(message),
                answer: answer.clone(),
            });
            match answer.wait(signal) {
                Some(Settled::Retry) => continue,
                Some(Settled::Answer(message)) => return Ok(*message),
                Some(Settled::Failed(error)) => return Err(HarnessError::Error(error)),
                None => return Err(signal.reason().unwrap_or_else(|| HarnessError::msg("cancelled"))),
            }
        }
    }
}

impl TurnModels {
    /// One root request: the actor builds it from the transcript; stream events reach the actor as they arrive.
    fn stream_root(
        &self,
        model: &Model,
        context: &ai::TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> harness::scheduler::Result<AssistantMessage> {
        let answer = Answer::default();
        (self.bridge.to_actor)(Request::Generation {
            users: trailing_users(&context.messages),
            answer: answer.clone(),
        });
        let start = match answer.wait(signal) {
            Some(Ok(start)) => start,
            Some(Err(StartError::Request(error))) => return Ok(crate::lane::error_message(model, error)),
            Some(Err(StartError::Checkpoint(error))) => return Err(HarnessError::Error(error)),
            None => return Err(signal.reason().unwrap_or_else(|| HarnessError::msg("cancelled"))),
        };
        let bridged = Bridged::new(signal);
        let mut options = ai::SimpleStreamOptions::default();
        options.base.api_key = start.api_key;
        options.base.signal = Some(bridged.abort.clone());
        options.reasoning = request.reasoning.clone();
        let llm = ai::TranscriptContext {
            messages: start.messages,
        };
        let bridge = self.bridge.clone();
        let mut started = false;
        let mut sink = |event: &AssistantMessageEvent| {
            on_event(event);
            match event {
                AssistantMessageEvent::Start { partial } => {
                    started = true;
                    bridge.send(AgentEvent::MessageStart {
                        message: Message::Assistant(partial.clone()),
                    });
                }
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {}
                AssistantMessageEvent::TextStart { partial, .. }
                | AssistantMessageEvent::TextDelta { partial, .. }
                | AssistantMessageEvent::TextEnd { partial, .. }
                | AssistantMessageEvent::ThinkingStart { partial, .. }
                | AssistantMessageEvent::ThinkingDelta { partial, .. }
                | AssistantMessageEvent::ThinkingEnd { partial, .. }
                | AssistantMessageEvent::ToolCallStart { partial, .. }
                | AssistantMessageEvent::ToolCallDelta { partial, .. }
                | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                    if started {
                        bridge.send(AgentEvent::MessageUpdate {
                            message: Message::Assistant(partial.clone()),
                            assistant_message_event: event.clone(),
                        });
                    }
                }
            }
        };
        let message = runtime()?.block_on(self.provider.stream(model, &llm, &options, &mut sink));
        drop(bridged);
        // A request that failed before its stream started has no `message_start` (the harness reports only its
        // committed answer); an aborted one is announced, so the actor can hide it.
        if !started && message.stop_reason != StopReason::Error {
            self.bridge.send(AgentEvent::MessageStart {
                message: Message::Assistant(message.clone()),
            });
        }
        if signal.is_cancelled() {
            // Only the user's abort ends the turn here; the transcript keeps the aborted response as the agent loop
            // does. A closing worker keeps nothing: the reopened runtime resends the pinned request.
            if *lock(&self.bridge.user_abort) {
                self.bridge.send(AgentEvent::MessageEnd {
                    message: Message::Assistant(message.clone()),
                });
            }
            return Err(signal.reason().unwrap_or_else(|| HarnessError::msg("cancelled")));
        }
        Ok(message)
    }
}

/// The host request runtime.py sends with what a running cell printed (`ULTRON_RLM_STREAM_OUTPUT`); nothing waits
/// for its reply.
pub const CELL_OUTPUT: &str = "cell.output";

/// Append `text` (secrets masked, as the cell's result will be) to the running cell's durable partial output.
pub fn publish_output(progress: &Mutex<Option<CellProgress>>, text: &str) {
    if text.is_empty() {
        return;
    }
    let (masked, _) = host::secrets::OutputMasker::default().mask_counted(text, None);
    if let Some(progress) = lock(progress).as_ref() {
        progress.output(&masked);
    }
}

/// Runs cells through the lane's `rlm` tool: the root's, or a subagent's own lane.
pub struct TurnCells {
    pub bridge: Arc<Bridge>,
    pub tool: Arc<dyn AgentTool>,
    /// Where the cell's host operations report what they print (its durable partial output), while it runs.
    pub progress: Arc<Mutex<Option<CellProgress>>>,
    pub stack: Arc<crate::compose::Stack>,
    pub children: Arc<crate::subagents::DurableChildren>,
}

/// Run one call of `tool`, as the agent loop does: a thrown error becomes an error result with its text.
fn run_tool(tool: &dyn AgentTool, call: &CellCall, abort: &AbortSignal) -> Result<(AgentToolResult, bool), String> {
    let update = |_: AgentToolResult| {};
    let executed = runtime().map_err(|e| e.to_string())?.block_on(tool.execute(
        &call.call_id,
        call.arguments.clone(),
        abort,
        &update,
    ));
    Ok(match executed {
        Ok(result) => (result, false),
        Err(message) => (
            AgentToolResult {
                content: vec![crate::lane::text_block(message)],
                details: json!({}),
                usage: None,
                terminate: None,
            },
            true,
        ),
    })
}

fn cell_result(result: AgentToolResult, is_error: bool) -> CellResult {
    CellResult {
        output: Some(
            result
                .content
                .iter()
                .filter_map(|c| serde_json::to_value(c).ok())
                .collect(),
        ),
        is_error,
        details: Some(result.details).filter(|d| !d.is_null()),
        usage: result.usage.and_then(|u| serde_json::to_value(u).ok()),
        diagnostics: Vec::new(),
        control: None,
    }
}

impl CellExecutor for TurnCells {
    fn tool(&self) -> Tool {
        self.tool.tool().clone()
    }

    fn prepare(&self, arguments: &Value) -> Result<Value, String> {
        // The agent loop passes arguments through unvalidated; the tool reports a missing `code` itself.
        Ok(arguments.clone())
    }

    fn execute(&self, call: &CellCall, progress: &CellProgress, signal: &Cancel) -> Result<CellResult, String> {
        if call.conversation_id != ROOT_CONVERSATION_ID {
            // A subagent's cell runs in its own lane, with its own host handler.
            let lane = self
                .children
                .lane_of(call.conversation_id)
                .ok_or("This conversation has no REPL lane")?;
            let tool = self.stack.rlm(&lane)?;
            let bridged = Bridged::new(signal);
            let (result, is_error) = run_tool(&tool, call, &bridged.abort)?;
            drop(bridged);
            if signal.is_cancelled() {
                return Err(signal
                    .reason()
                    .map_or_else(|| "cancelled".to_string(), |r| r.to_string()));
            }
            return Ok(cell_result(result, is_error));
        }
        let bridged = Bridged::new(signal);
        *lock(&self.progress) = Some(progress.clone());
        // The root cell's start and end, on the cell's own thread: a task that ends while the cell runs is the
        // cell's (collapse on return), whenever the actor gets to the cell's events.
        let context = self.stack.context.get().cloned();
        if let Some(context) = &context {
            context.tool_start(crate::lane::MAIN, &call.name, &call.call_id);
        }
        let executed = run_tool(self.tool.as_ref(), call, &bridged.abort);
        if let Some(context) = &context {
            context.tool_end(crate::lane::MAIN, &call.call_id);
        }
        *lock(&self.progress) = None;
        drop(bridged);
        let (result, is_error) = executed?;
        lock(&self.bridge.results).insert(
            call.call_id.clone(),
            Reported {
                name: call.name.clone(),
                result: result.clone(),
                is_error,
            },
        );
        if signal.is_cancelled() {
            return Err(signal
                .reason()
                .map_or_else(|| "cancelled".to_string(), |r| r.to_string()));
        }
        // The round's last cell: the turn ends now, so a steer it earns is placed with the round's results.
        if let Some((calls, failed)) = self.bridge.round_complete() {
            let answer = Answer::default();
            self.bridge.request(Request::RoundEnded {
                calls,
                failed,
                answer: answer.clone(),
            });
            let _ = answer.wait(signal);
        }
        Ok(cell_result(result, is_error))
    }
}
