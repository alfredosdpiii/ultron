//! Automatic compaction of the root lane: the settings it runs with and when it starts. The session worker's
//! harness (packages/agent/src/harness/runtime/drive) compacts at a run's checkpoints, before the run ends or before
//! its next request, when the context crosses `contextWindow - reserveTokens` (`checkpoint.ts`,
//! `prepareCompactionThreshold`), and on a response that overflowed the window, compacting and retrying the request
//! once (`response.ts`, `publishResponse`). The session actor runs both ([`crate::core`]); this module is the
//! arithmetic and the classification, ported from pi-ai `isContextOverflow` / `isRecoverableLength` and the settings
//! manager's `getCompactionSettings`.

use ai::{AssistantMessage, StopReason};
use compaction::CompactionSettings;
use serde_json::Value;

/// Why an automatic compaction runs (`compaction_start.reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Threshold,
    Overflow,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::Threshold => "threshold",
            Reason::Overflow => "overflow",
        }
    }

    pub fn parse(name: &str) -> Option<Reason> {
        match name {
            "threshold" => Some(Reason::Threshold),
            "overflow" => Some(Reason::Overflow),
            _ => None,
        }
    }
}

/// `getCompactionTokenSetting`: a token setting must be a non-negative safe integer.
fn token_setting(value: Option<&Value>, what: &str) -> Result<Option<u64>, String> {
    let Some(value) = value else { return Ok(None) };
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    match value.as_f64() {
        Some(n) if n.fract() == 0.0 && (0.0..=MAX_SAFE).contains(&n) && value.is_number() => Ok(Some(n as u64)),
        _ => Err(format!(
            "Invalid {what} setting: {}. Expected a non-negative safe integer.",
            js_string(value)
        )),
    }
}

/// `String(value)` for the error messages.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

/// `getCompactionSettings(model)`: `compaction.enabled` (default on), and each token setting from the model's
/// `compaction.modelOverrides["provider/id"]`, else the ordinary setting, else the default (16384 reserved, 20000
/// kept). `enabled` overrides the setting (`set_auto_compaction` on a live session).
pub fn settings(
    settings: &Value,
    provider: &str,
    model_id: &str,
    enabled: Option<bool>,
) -> Result<CompactionSettings, String> {
    let defaults = CompactionSettings::default();
    let compaction = settings.get("compaction");
    let key = format!("{provider}/{model_id}");
    // Each token setting: the ordinary value checked, then the model's override entry, then its value.
    let resolve = |field: &str, fallback: u64| -> Result<u64, String> {
        let ordinary = token_setting(compaction.and_then(|c| c.get(field)), &format!("compaction.{field}"))?;
        let entry = compaction
            .and_then(|c| c.get("modelOverrides"))
            .and_then(|o| o.get(&key));
        if let Some(entry) = entry
            && !entry.is_object()
        {
            return Err(format!(
                "Invalid compaction.modelOverrides[\"{key}\"] setting: {}. Expected an object.",
                js_string(entry)
            ));
        }
        let overridden = token_setting(
            entry.and_then(|e| e.get(field)),
            &format!("compaction.modelOverrides[\"{key}\"].{field}"),
        )?;
        Ok(overridden.or(ordinary).unwrap_or(fallback))
    };
    let reserve_tokens = resolve("reserveTokens", defaults.reserve_tokens)?;
    let keep_recent_tokens = resolve("keepRecentTokens", defaults.keep_recent_tokens)?;
    // `compaction.enabled ?? true`, as JavaScript reads it.
    let configured = match compaction.and_then(|c| c.get("enabled")) {
        None | Some(Value::Null) => true,
        Some(Value::Bool(on)) => *on,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    };
    Ok(CompactionSettings {
        enabled: enabled.unwrap_or(configured),
        reserve_tokens,
        keep_recent_tokens,
    })
}

/// pi-ai `isContextOverflow(message, contextWindow)`: an error a provider uses for an oversized request, a successful
/// response whose input exceeds the window (silent overflow), or a length stop with no output that filled it.
pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    if harness::chat::models::is_context_overflow(message) {
        return true;
    }
    let input = message.usage.input + message.usage.cache_read;
    if context_window > 0 && message.stop_reason == StopReason::Stop && input > context_window {
        return true;
    }
    context_window > 0
        && message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input as f64 >= context_window as f64 * 0.99
}

/// pi-ai `isRecoverableLength`: a length stop below the output limit the request asked for.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length && desired_max_output > 0 && message.usage.output < desired_max_output
}

/// `publishResponse`'s overflow: the response overflowed the window, or stopped for length below the requested
/// limit; either one compacts and retries once.
pub fn overflowed(message: &AssistantMessage, context_window: u64, intended_output_limit: u64) -> bool {
    is_context_overflow(message, context_window) || is_recoverable_length(message, intended_output_limit)
}

/// `normalizeError`: an overflowed response as the error it is committed as.
pub fn as_error(message: &AssistantMessage) -> AssistantMessage {
    let mut error = message.clone();
    error.error_message = Some(
        message
            .error_message
            .clone()
            .unwrap_or_else(|| "Assistant request exceeded the context window".into()),
    );
    error.stop_reason = StopReason::Error;
    error
}

/// `prepareCompactionThreshold`: a threshold compaction is due at a checkpoint when compaction is on, the branch has
/// no compaction at or after the checkpoint's trigger entry, and the context leaves less than `reserveTokens` of the
/// window. `path` is the branch, oldest first; `tokens_before` the preparation's context estimate.
pub fn threshold_due(
    settings: &CompactionSettings,
    path: &[Value],
    trigger: Option<&str>,
    tokens_before: u64,
    context_window: u64,
) -> bool {
    if !settings.enabled || context_window == 0 {
        return false;
    }
    let newest_compaction = path.iter().rposition(|e| e["type"] == "compaction");
    let trigger_index = trigger.and_then(|t| path.iter().position(|e| e["id"] == t));
    if let Some(compaction) = newest_compaction
        && trigger_index.is_none_or(|trigger| compaction >= trigger)
    {
        return false;
    }
    compaction::should_compact(tokens_before, context_window, settings)
}
