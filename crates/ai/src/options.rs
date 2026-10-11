//! Request options shared by every provider (`StreamOptions`, `SimpleStreamOptions`) and the simple-option helpers of
//! `api/simple-options.ts`.

use crate::env::{ProviderEnv, provider_env_value};
use crate::http::AbortSignal;
use crate::transcript::estimate_context_tokens;
use crate::types::{CacheRetention, Model, TranscriptContext};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::sync::Arc;

/// Inspect or replace the provider payload before it is sent; `None` keeps it.
pub type PayloadHook = Arc<dyn Fn(&Value, &Model) -> Option<Value> + Send + Sync>;

/// Header overrides; a `None` value suppresses a default header of that name.
pub type ProviderHeaders = IndexMap<String, Option<String>>;

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub env: Option<ProviderEnv>,
    pub temperature: Option<f64>,
    pub sampling_params: Option<Map<String, Value>>,
    pub max_tokens: Option<u64>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub metadata: Option<Map<String, Value>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<f64>,
    /// Preferred transport where a provider has several (`"sse" | "websocket" | "websocket-cached" | "auto"`).
    pub transport: Option<String>,
    pub websocket_connect_timeout_ms: Option<f64>,
    #[serde(skip)]
    pub signal: Option<AbortSignal>,
    #[serde(skip)]
    pub on_payload: Option<PayloadHook>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

/// Options of `streamSimple()`: provider-neutral reasoning and tool choice.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimpleStreamOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    /// `"auto"` or `"none"`.
    pub tool_choice: Option<String>,
    /// `"minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
    pub reasoning: Option<String>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl StreamOptions {
    pub(crate) fn signal(&self) -> Option<&AbortSignal> {
        self.signal.as_ref()
    }

    pub(crate) fn retry(&self) -> crate::http::RetryOptions {
        crate::http::RetryOptions {
            max_retries: self.max_retries,
            max_retry_delay_ms: self.max_retry_delay_ms,
        }
    }

    /// `cacheRetention`, else `long` when `PI_CACHE_RETENTION=long`, else `short`.
    pub(crate) fn cache_retention(&self) -> CacheRetention {
        if let Some(retention) = self.cache_retention {
            return retention;
        }
        if provider_env_value("PI_CACHE_RETENTION", self.env.as_ref()).as_deref() == Some("long") {
            return CacheRetention::Long;
        }
        CacheRetention::Short
    }
}

const CONTEXT_SAFETY_TOKENS: i64 = 4096;
const MIN_MAX_TOKENS: i64 = 1;

pub fn clamp_max_tokens_to_context(model: &Model, context: &TranscriptContext, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS as u64);
    }
    let available =
        model.context_window as i64 - estimate_context_tokens(&context.messages).tokens as i64 - CONTEXT_SAFETY_TOKENS;
    (max_tokens as i64).min(available.max(MIN_MAX_TOKENS)) as u64
}

/// `buildBaseOptions`: the simple options as stream options, `maxTokens` clamped to the remaining context.
pub fn build_base_options(model: &Model, context: &TranscriptContext, options: &SimpleStreamOptions) -> StreamOptions {
    let base = &options.base;
    let sampling_params = if model.sampling_params.is_some() || base.sampling_params.is_some() {
        let mut merged = model.sampling_params.clone().unwrap_or_default();
        merged.extend(base.sampling_params.clone().unwrap_or_default());
        Some(merged)
    } else {
        None
    };
    StreamOptions {
        sampling_params,
        max_tokens: Some(clamp_max_tokens_to_context(
            model,
            context,
            base.max_tokens.unwrap_or(model.max_tokens),
        )),
        api_key: base.api_key.clone().filter(|k| !k.is_empty()),
        ..base.clone()
    }
}

pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// `minimal`/`low`/`medium`/`high` (with `xhigh`/`max` clamped to `high`).
pub fn clamp_reasoning(level: &str) -> &str {
    if level == "xhigh" || level == "max" {
        "high"
    } else {
        level
    }
}

pub fn thinking_budget_for_level(level: &str, custom: Option<&ThinkingBudgets>) -> u64 {
    let custom = custom.copied().unwrap_or_default();
    match clamp_reasoning(level) {
        "minimal" => custom.minimal.unwrap_or(1024),
        "low" => custom.low.unwrap_or(2048),
        "medium" => custom.medium.unwrap_or(8192),
        _ => custom.high.unwrap_or(16384),
    }
}

pub fn clamp_thinking_budget_to_answer_room(budget: u64, ceiling: u64) -> u64 {
    budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// `(maxTokens, thinkingBudget)`: thinking added to the caller's cap (or fitted inside the model cap).
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    level: &str,
    custom: Option<&ThinkingBudgets>,
) -> (u64, u64) {
    let mut budget = thinking_budget_for_level(level, custom);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + budget).min(model_max_tokens),
    };
    if max_tokens <= budget {
        budget = clamp_thinking_budget_to_answer_room(budget, max_tokens);
    }
    (max_tokens, budget)
}

/// `pi (<platform> <release>; <arch>)`, with Node's platform and arch names.
pub fn pi_user_agent() -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|r| r.trim().to_string())
        .unwrap_or_default();
    format!("pi ({platform} {release}; {arch})")
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
