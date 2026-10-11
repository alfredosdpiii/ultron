//! What generation needs from the provider layer, and the settings it reads.
//!
//! [`Models`] is upstream `HarnessOptions.models` reduced to what the generation and compaction tasks call:
//! resolve a model, stream one response under the invocation's signal, and poll or cancel a deferred response. It
//! blocks the calling task thread. [`StreamFnModels`] adapts an `agent::StreamFn` (the provider stack `crates/cli`
//! composes) to it; tests use [`super::faux::Faux`].

use std::sync::{Arc, LazyLock};

use ai::{AssistantMessage, AssistantMessageEvent, DeferredHandle, Model, StopReason, TranscriptContext};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::inbox::{QueueMode, QueueModes};
use crate::scheduler::{Cancel, HarnessError, Result};

/// A model by provider and ID, as `pi.agent` and generation checkpoints pin it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider: &str, model_id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
        }
    }
}

/// One request's options: the pinned stream options (`ConversationStreamOptions` as JSON, e.g. `maxTokens`,
/// `temperature`), the provider session ID, and the reasoning level (`None` for thinking off).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamRequest {
    pub options: Map<String, Value>,
    pub session_id: Option<String>,
    pub reasoning: Option<String>,
}

impl StreamRequest {
    /// The request as `ai::SimpleStreamOptions` (the signal is set by the caller).
    pub fn simple_options(&self) -> ai::SimpleStreamOptions {
        let mut options: ai::SimpleStreamOptions =
            serde_json::from_value(Value::Object(self.options.clone())).unwrap_or_default();
        options.base.session_id = self.session_id.clone();
        options.reasoning = self.reasoning.clone();
        options
    }
}

/// The provider layer as generation uses it. Every call blocks; `signal` is the task invocation's.
pub trait Models: Send + Sync {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model>;

    /// Stream one response. Events go to `on_event` as they arrive; provider failures are messages with stop reason
    /// `error`; `Err` is a transport bug. An aborted signal ends the stream with an `aborted` message or `Err`.
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> Result<AssistantMessage>;

    /// Poll a deferred response; a still deferred result keeps stop reason `deferred`.
    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<AssistantMessage> {
        let _ = (model, handle, signal);
        Err(HarnessError::msg("This provider has no deferred responses"))
    }

    fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<()> {
        let _ = (model, handle, signal);
        Ok(())
    }
}

/// `settings.retry` (spec §7.1): bounded attempts with exponential backoff.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Retries after the first attempt.
    pub max_retries: u64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_delay_ms: 60_000,
        }
    }
}

impl RetryPolicy {
    /// `retryDelayMs`: `baseDelayMs * 2^(attempt-1)`, capped.
    pub fn delay_ms(&self, attempt: u64) -> u64 {
        let factor = 1u64.checked_shl(attempt.saturating_sub(1) as u32).unwrap_or(u64::MAX);
        self.base_delay_ms.saturating_mul(factor).min(self.max_delay_ms)
    }
}

/// `settings.compaction` (spec §8.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionPolicy {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub background_tokens: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        CompactionPolicy {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            background_tokens: 32_768,
        }
    }
}

/// Whether a round's cells run at once or one after another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolExecution {
    #[default]
    Parallel,
    Sequential,
}

/// Harness settings, read at each decision so changes apply to the next one.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Request options pinned at preparation (`ConversationStreamOptions`).
    pub stream: Map<String, Value>,
    pub retry: RetryPolicy,
    pub compaction: CompactionPolicy,
    /// Throttle of committed generation partials.
    pub partial_interval_ms: u64,
    /// Throttle of committed cell output.
    pub output_interval_ms: u64,
    pub tool_execution: ToolExecution,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            stream: Map::new(),
            retry: RetryPolicy::default(),
            compaction: CompactionPolicy::default(),
            partial_interval_ms: 100,
            output_interval_ms: 100,
            tool_execution: ToolExecution::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
        }
    }
}

impl Settings {
    pub fn queue_modes(&self) -> QueueModes {
        QueueModes {
            steering: self.steering_mode,
            follow_up: self.follow_up_mode,
        }
    }
}

/// Where settings are read from: a fixed value or a live source.
pub type SettingsSource = Arc<dyn Fn() -> Settings + Send + Sync>;

fn patterns(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .map(|source| Regex::new(&format!("(?i){source}")).expect("valid pattern"))
        .collect()
}

static OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"prompt (?:is )?too long",
        r"prompt exceeds max length",
        r"request_too_large",
        r"input is too long for requested model",
        r"exceeds the context window",
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum",
        r"maximum prompt length is \d+",
        r"reduce the length of the messages",
        r"maximum context length is \d+ tokens",
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
        r"exceeds the limit of \d+",
        r"exceeds the available context size",
        r"greater than the context length",
        r"context window exceeds limit",
        r"exceeded model token limit",
        r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
        r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length",
        r"range of input length should be",
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
    ])
});

static NON_OVERFLOW: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"^(Throttling error|Service unavailable):",
        r"rate limit",
        r"too many requests",
    ])
});

static CEREBRAS_BODYLESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)").expect("valid pattern"));

/// pi-ai `isContextOverflow` without a context window: an error message a provider uses for an oversized request.
pub fn is_context_overflow(message: &AssistantMessage) -> bool {
    let Some(text) = message
        .error_message
        .as_deref()
        .filter(|_| message.stop_reason == StopReason::Error)
    else {
        return false;
    };
    if NON_OVERFLOW.iter().any(|pattern| pattern.is_match(text)) {
        return false;
    }
    OVERFLOW.iter().any(|pattern| pattern.is_match(text))
        || (message.provider == "cerebras" && CEREBRAS_BODYLESS.is_match(text))
}

static NON_RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|\
         out of budget|quota exceeded|billing|subscription_sharing_usage_limit_exceeded",
    )
    .expect("valid pattern")
});

static RETRYABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "(?i)overloaded|server_busy|servers are currently busy|currently experiencing high demand|model is at capacity|\
         rate.?limit|too many requests|429|500|502|503|504|520|524|service.?unavailable|server.?error|internal.?error|\
         provider.?returned.?error|exceeded request buffer limit while retrying upstream|network.?error|\
         connection.?error|connection.?refused|connection.?lost|other side closed|fetch failed|getaddrinfo|ENOTFOUND|\
         EAI_AGAIN|upstream.?connect|reset before headers|socket hang up|socket connection was closed|timed? out|\
         timeout|terminated|websocket.?closed|websocket.?error|ended without|stream ended before message_stop|\
         stream ended before a terminal response event|http2 request did not get a response|\
         pending stream has been canceled|retry delay|you can retry your request|try your request again|\
         please retry your request|ResourceExhausted|subscription_sharing_usage_unavailable|\
         subscription_sharing_user_unavailable",
    )
    .expect("valid pattern")
});

/// pi-ai `isRetryableAssistantError`: a transient provider or transport error.
pub fn is_retryable(message: &AssistantMessage) -> bool {
    let Some(text) = message
        .error_message
        .as_deref()
        .filter(|text| message.stop_reason == StopReason::Error && !text.is_empty())
    else {
        return false;
    };
    !NON_RETRYABLE.is_match(text) && RETRYABLE.is_match(text)
}

/// [`Models`] over the provider stack the composition root already builds: a model catalog and an
/// `agent::StreamFn`. Each request runs the stream's future on a current-thread Tokio runtime on the calling task
/// thread; cancelling the invocation's signal aborts the request's `AbortSignal`.
#[derive(Clone)]
pub struct StreamFnModels {
    models: Vec<Model>,
    stream: Arc<dyn agent::StreamFn>,
}

impl StreamFnModels {
    pub fn new(models: Vec<Model>, stream: Arc<dyn agent::StreamFn>) -> StreamFnModels {
        StreamFnModels { models, stream }
    }
}

impl Models for StreamFnModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        self.models
            .iter()
            .find(|model| model.provider == provider && model.id == model_id)
            .cloned()
    }

    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> Result<AssistantMessage> {
        let abort = ai::AbortSignal::new();
        let mut options = request.simple_options();
        options.base.signal = Some(abort.clone());
        let bridge = abort.clone();
        let registration = signal.on_cancel(move |_| bridge.abort());
        if registration.is_none() {
            abort.abort();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| HarnessError::Error(error.to_string()));
        let result = runtime.map(|runtime| runtime.block_on(self.stream.stream(model, context, &options, on_event)));
        if let Some(id) = registration {
            signal.off(id);
        }
        result
    }
}
