//! A scripted provider (port of pi-ai `providers/faux.ts`) for tests and offline runs: queued responses (fixed
//! messages or functions of the request), streamed in token-sized deltas, optionally paced, with the faux usage
//! estimate, deferred responses, and abort handling.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Cost, DeferredHandle, Message, Model, ModelCost,
    StopReason, TextContent, ThinkingContent, ToolCall, TranscriptContext, Usage, UserContent, UserMessageContent,
};
use serde_json::{Map, Value};

use super::models::{Models, StreamRequest};
use crate::scheduler::cancel::lock;
use crate::scheduler::{Cancel, Result};

/// A response function: the request's context and options, and the provider state.
pub type FauxFactory =
    Arc<dyn Fn(&TranscriptContext, &StreamRequest, &Cancel) -> Result<AssistantMessage> + Send + Sync>;

/// One queued response.
#[derive(Clone)]
pub enum FauxStep {
    Message(Box<AssistantMessage>),
    Factory(FauxFactory),
}

impl From<AssistantMessage> for FauxStep {
    fn from(message: AssistantMessage) -> FauxStep {
        FauxStep::Message(Box::new(message))
    }
}

impl FauxStep {
    pub fn factory(
        factory: impl Fn(&TranscriptContext, &StreamRequest, &Cancel) -> Result<AssistantMessage> + Send + Sync + 'static,
    ) -> FauxStep {
        FauxStep::Factory(Arc::new(factory))
    }
}

struct Deferred {
    handle: DeferredHandle,
    step: FauxStep,
    context: TranscriptContext,
    request: StreamRequest,
    pending_fetches: u64,
    cancelled: bool,
    last: Option<AssistantMessage>,
}

#[derive(Default)]
struct State {
    responses: VecDeque<FauxStep>,
    call_count: usize,
    deferred_fetch_count: usize,
    cancelled_deferred: Vec<DeferredHandle>,
    requests: Vec<TranscriptContext>,
    deferred: HashMap<String, Deferred>,
    next_id: u64,
}

/// Options of [`Faux::new`].
#[derive(Clone, Debug, Default)]
pub struct FauxOptions {
    /// Paces streaming: each delta waits `tokens / tokens_per_second`.
    pub tokens_per_second: Option<f64>,
    /// Tokens per delta (4 characters each); default 3.
    pub token_size: Option<usize>,
    /// Deferred responses: fetches that return the handle again before the response is ready.
    pub pending_fetches: u64,
    pub poll_after_ms: Option<i64>,
    pub context_window: Option<u64>,
}

/// The faux provider: model `faux/faux-1`. Clones share the response queue.
#[derive(Clone)]
pub struct Faux {
    state: Arc<Mutex<State>>,
    options: FauxOptions,
    model: Model,
}

const DEFAULT_TIMESTAMP: i64 = 0;

/// `fauxAssistantMessage` of text.
pub fn text_message(text: &str) -> AssistantMessage {
    message(vec![text_block(text)], StopReason::Stop)
}

pub fn text_block(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_owned(),
        text_signature: None,
    })
}

/// `fauxToolCall(name, arguments, { id })`.
pub fn tool_call(name: &str, arguments: Value, id: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// `fauxAssistantMessage(content, { stopReason })`.
pub fn message(content: Vec<AssistantContent>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        provider_metadata: None,
        timestamp: DEFAULT_TIMESTAMP,
    }
}

/// A faux error message, e.g. `503 Service Unavailable`.
pub fn error_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        error_message: Some(text.to_owned()),
        ..message(Vec::new(), StopReason::Error)
    }
}

/// A call of the REPL tool with `code`.
pub fn rlm_call(code: &str, id: &str) -> AssistantMessage {
    message(
        vec![tool_call("rlm", serde_json::json!({ "code": code }), id)],
        StopReason::ToolUse,
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn content_text(content: &UserMessageContent) -> String {
    match content {
        UserMessageContent::Text(text) => text.clone(),
        UserMessageContent::Blocks(blocks) => user_blocks(blocks),
    }
}

fn user_blocks(blocks: &[UserContent]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            UserContent::Text(text) => text.text.clone(),
            UserContent::Image(image) => format!("[image:{}:{}]", image.mime_type, image.data.len()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_text(content: &[AssistantContent]) -> String {
    content
        .iter()
        .map(|block| match block {
            AssistantContent::Text(text) => text.text.clone(),
            AssistantContent::Thinking(thinking) => thinking.thinking.clone(),
            AssistantContent::ToolCall(call) => format!("{}:{}", call.name, call.arguments),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_text(message: &Message) -> String {
    match message {
        Message::System(system) => {
            let mut parts = vec![ai::transcript::system_message_text(system)];
            for tool in system.tools_removed.iter().flatten() {
                parts.push(format!("tool-:{}", serde_json::to_string(tool).unwrap_or_default()));
            }
            for tool in system.tools_added.iter().flatten() {
                parts.push(format!("tool+:{}", serde_json::to_string(tool).unwrap_or_default()));
            }
            parts
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        }
        Message::User(user) => content_text(&user.content),
        Message::Assistant(assistant) => assistant_text(&assistant.content),
        Message::ToolResult(result) => {
            let mut parts = vec![result.tool_name.clone()];
            parts.push(user_blocks(&result.content));
            parts.join("\n")
        }
    }
}

fn role(message: &Message) -> &'static str {
    match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

fn estimate(text: &str) -> u64 {
    text.encode_utf16().count().div_ceil(4) as u64
}

/// The faux usage estimate: the prompt's characters / 4 as input, the output's as output.
fn with_usage(mut message: AssistantMessage, context: &TranscriptContext) -> AssistantMessage {
    let prompt: Vec<String> = context
        .messages
        .iter()
        .map(|message| format!("{}:{}", role(message), message_text(message)))
        .collect();
    let length: usize =
        prompt.iter().map(|part| part.encode_utf16().count()).sum::<usize>() + prompt.len().saturating_sub(1) * 2;
    let input = length.div_ceil(4) as u64;
    let output = estimate(&assistant_text(&message.content));
    message.usage = Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: Cost::default(),
    };
    message
}

impl Faux {
    pub fn new(options: FauxOptions) -> Faux {
        let model = Model {
            id: "faux-1".into(),
            name: "Faux Model".into(),
            api: "faux".into(),
            provider: "faux".into(),
            base_url: "http://localhost:0".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec!["text".into(), "image".into()],
            input_limits: None,
            cost: ModelCost::default(),
            prompt_cache: None,
            context_window: options.context_window.unwrap_or(128_000),
            max_tokens: 16_384,
            sampling_params: None,
            headers: None,
            compat: None,
            request: None,
            extra: Map::new(),
        };
        Faux {
            state: Arc::new(Mutex::new(State::default())),
            options,
            model,
        }
    }

    pub fn model_ref() -> super::models::ModelRef {
        super::models::ModelRef::new("faux", "faux-1")
    }

    pub fn set_responses(&self, responses: Vec<FauxStep>) {
        lock(&self.state).responses = responses.into();
    }

    pub fn append_responses(&self, responses: Vec<FauxStep>) {
        lock(&self.state).responses.extend(responses);
    }

    pub fn call_count(&self) -> usize {
        lock(&self.state).call_count
    }

    pub fn deferred_fetch_count(&self) -> usize {
        lock(&self.state).deferred_fetch_count
    }

    pub fn cancelled_deferred(&self) -> Vec<DeferredHandle> {
        lock(&self.state).cancelled_deferred.clone()
    }

    pub fn pending(&self) -> usize {
        lock(&self.state).responses.len()
    }

    /// The contexts of every request, in order.
    pub fn requests(&self) -> Vec<TranscriptContext> {
        lock(&self.state).requests.clone()
    }

    fn resolve(
        &self,
        step: &FauxStep,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
    ) -> Result<AssistantMessage> {
        let mut message = match step {
            FauxStep::Message(message) => (**message).clone(),
            FauxStep::Factory(factory) => factory(context, request, signal)?,
        };
        message.api = self.model.api.clone();
        message.provider = self.model.provider.clone();
        message.model = self.model.id.clone();
        if message.timestamp == DEFAULT_TIMESTAMP {
            message.timestamp = now_ms();
        }
        Ok(with_usage(message, context))
    }

    fn aborted(partial: &AssistantMessage) -> AssistantMessage {
        AssistantMessage {
            stop_reason: StopReason::Aborted,
            error_message: Some("Request was aborted".into()),
            timestamp: now_ms(),
            ..partial.clone()
        }
    }

    fn chunks(&self, text: &str) -> Vec<String> {
        let size = self.options.token_size.unwrap_or(3).max(1) * 4;
        let characters: Vec<char> = text.chars().collect();
        if characters.is_empty() {
            return vec![String::new()];
        }
        characters.chunks(size).map(|chunk| chunk.iter().collect()).collect()
    }

    fn pace(&self, chunk: &str, signal: &Cancel) {
        if let Some(rate) = self.options.tokens_per_second.filter(|rate| *rate > 0.0) {
            let delay = estimate(chunk) as f64 / rate;
            signal.wait_timeout(Duration::from_secs_f64(delay));
        }
    }

    /// Stream `message` in deltas; an abort ends it with an aborted message.
    fn stream_deltas(
        &self,
        message: AssistantMessage,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> AssistantMessage {
        let mut partial = AssistantMessage {
            content: Vec::new(),
            stop_reason: StopReason::Pending,
            ..message.clone()
        };
        let abort = |partial: &AssistantMessage, on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send)| {
            let aborted = Faux::aborted(partial);
            on_event(&AssistantMessageEvent::Error {
                reason: StopReason::Aborted,
                error: aborted.clone(),
            });
            aborted
        };
        if signal.is_cancelled() {
            return abort(&partial, on_event);
        }
        on_event(&AssistantMessageEvent::Start {
            partial: partial.clone(),
        });
        for (index, block) in message.content.iter().enumerate() {
            if signal.is_cancelled() {
                return abort(&partial, on_event);
            }
            match block {
                AssistantContent::Thinking(thinking) => {
                    partial.content.push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        ..thinking.clone()
                    }));
                    on_event(&AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    for chunk in self.chunks(&thinking.thinking) {
                        self.pace(&chunk, signal);
                        if signal.is_cancelled() {
                            return abort(&partial, on_event);
                        }
                        if let Some(AssistantContent::Thinking(current)) = partial.content.get_mut(index) {
                            current.thinking.push_str(&chunk);
                        }
                        on_event(&AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.clone(),
                        });
                    }
                    on_event(&AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content: thinking.thinking.clone(),
                        partial: partial.clone(),
                    });
                }
                AssistantContent::Text(text) => {
                    partial.content.push(text_block(""));
                    on_event(&AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    for chunk in self.chunks(&text.text) {
                        self.pace(&chunk, signal);
                        if signal.is_cancelled() {
                            return abort(&partial, on_event);
                        }
                        if let Some(AssistantContent::Text(current)) = partial.content.get_mut(index) {
                            current.text.push_str(&chunk);
                        }
                        on_event(&AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.clone(),
                        });
                    }
                    on_event(&AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content: text.text.clone(),
                        partial: partial.clone(),
                    });
                }
                AssistantContent::ToolCall(call) => {
                    partial.content.push(AssistantContent::ToolCall(ToolCall {
                        arguments: Value::Object(Map::new()),
                        ..call.clone()
                    }));
                    on_event(&AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial: partial.clone(),
                    });
                    for chunk in self.chunks(&call.arguments.to_string()) {
                        self.pace(&chunk, signal);
                        if signal.is_cancelled() {
                            return abort(&partial, on_event);
                        }
                        on_event(&AssistantMessageEvent::ToolCallDelta {
                            content_index: index,
                            delta: chunk,
                            partial: partial.clone(),
                        });
                    }
                    partial.content[index] = block.clone();
                    on_event(&AssistantMessageEvent::ToolCallEnd {
                        content_index: index,
                        tool_call: call.clone(),
                        partial: partial.clone(),
                    });
                }
            }
        }
        match message.stop_reason {
            StopReason::Error | StopReason::Aborted => on_event(&AssistantMessageEvent::Error {
                reason: message.stop_reason,
                error: message.clone(),
            }),
            reason => on_event(&AssistantMessageEvent::Done {
                reason,
                message: message.clone(),
            }),
        }
        message
    }

    fn error(&self, text: &str, context: &TranscriptContext) -> AssistantMessage {
        let mut message = error_message(text);
        message.timestamp = now_ms();
        with_usage(message, context)
    }
}

impl Models for Faux {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        (provider == self.model.provider && model_id == self.model.id).then(|| self.model.clone())
    }

    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> Result<AssistantMessage> {
        let step = {
            let mut state = lock(&self.state);
            state.call_count += 1;
            state.requests.push(context.clone());
            state.responses.pop_front()
        };
        let Some(step) = step else {
            let message = self.error("No more faux responses queued", context);
            on_event(&AssistantMessageEvent::Error {
                reason: StopReason::Error,
                error: message.clone(),
            });
            return Ok(message);
        };
        if request.options.get("deferred").is_some_and(|deferred| deferred == true) {
            let handle = {
                let mut state = lock(&self.state);
                state.next_id += 1;
                let handle = DeferredHandle {
                    provider: model.provider.clone(),
                    model_id: model.id.clone(),
                    api: model.api.clone(),
                    id: format!("deferred:{}", state.next_id),
                    expires_at: None,
                    poll_after_ms: self.options.poll_after_ms,
                    data: None,
                };
                state.deferred.insert(
                    handle.id.clone(),
                    Deferred {
                        handle: handle.clone(),
                        step,
                        context: context.clone(),
                        request: request.clone(),
                        pending_fetches: self.options.pending_fetches,
                        cancelled: false,
                        last: None,
                    },
                );
                handle
            };
            let deferred = AssistantMessage {
                deferred: Some(handle),
                timestamp: now_ms(),
                ..message(Vec::new(), StopReason::Deferred)
            };
            return Ok(self.stream_deltas(deferred, signal, on_event));
        }
        let message = match self.resolve(&step, context, request, signal) {
            Ok(message) => message,
            Err(error) => {
                if signal.is_cancelled() {
                    return Err(error);
                }
                self.error(&error.message(), context)
            }
        };
        Ok(self.stream_deltas(message, signal, on_event))
    }

    fn fetch_deferred(&self, _model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<AssistantMessage> {
        let (step, context, request) = {
            let mut state = lock(&self.state);
            state.deferred_fetch_count += 1;
            let Some(entry) = state.deferred.get_mut(&handle.id) else {
                return Ok(error_message(&format!("Unknown faux deferred response: {}", handle.id)));
            };
            if entry.cancelled {
                return Ok(error_message(&format!(
                    "Faux deferred response was cancelled: {}",
                    handle.id
                )));
            }
            if entry.pending_fetches > 0 {
                entry.pending_fetches -= 1;
                return Ok(AssistantMessage {
                    deferred: Some(entry.handle.clone()),
                    timestamp: now_ms(),
                    ..message(Vec::new(), StopReason::Deferred)
                });
            }
            if let Some(last) = &entry.last {
                return Ok(last.clone());
            }
            let mut request = entry.request.clone();
            request.options.remove("deferred");
            (entry.step.clone(), entry.context.clone(), request)
        };
        let resolved = self
            .resolve(&step, &context, &request, signal)
            .unwrap_or_else(|error| self.error(&error.message(), &context));
        if let Some(entry) = lock(&self.state).deferred.get_mut(&handle.id) {
            entry.last = Some(resolved.clone());
        }
        Ok(resolved)
    }

    fn cancel_deferred(&self, _model: &Model, handle: &DeferredHandle, _signal: &Cancel) -> Result<()> {
        let mut state = lock(&self.state);
        state.cancelled_deferred.push(handle.clone());
        if let Some(entry) = state.deferred.get_mut(&handle.id) {
            entry.cancelled = true;
        }
        Ok(())
    }
}

/// A response that never answers: it blocks until the request's signal is cancelled, then fails with its reason.
/// `reached` is cancelled once the request was sent.
pub fn unanswered() -> (FauxStep, Cancel) {
    let reached = Cancel::new();
    let signal_reached = reached.clone();
    let step = FauxStep::factory(move |_, _, signal| {
        signal_reached.abort();
        Err(signal.wait())
    });
    (step, reached)
}

/// A response held until `release` is cancelled (or the request is aborted); `reached` is cancelled once sent.
pub fn gated(message: AssistantMessage) -> (FauxStep, Cancel, Cancel) {
    let reached = Cancel::new();
    let release = Cancel::new();
    let (signal_reached, gate) = (reached.clone(), release.clone());
    let step = FauxStep::factory(move |_, _, signal| {
        signal_reached.abort();
        let linked = Cancel::any(&[&gate, signal]);
        linked.signal().wait();
        signal.check()?;
        Ok(message.clone())
    });
    (step, reached, release)
}
