//! Whether a failed subscription call means the Claude plan is OUT OF USAGE (its five-hour or weekly window is
//! exhausted until a reset), as opposed to a transient per-minute 429 or an overload. Only the former may route
//! the call to API credits ([`super::fallback`]); a transient 429 or 529 keeps the existing retry behaviour.
//!
//! Signals, strongest first:
//! - Claude Code's own `rate_limit_event` with `rate_limit_info.status == "rejected"` (and `resetsAt`, epoch
//!   seconds or ms). Recorded shape: `packages/ai/test/fixtures/fake-claude.mjs` (`rate_limit` scenario) and
//!   `packages/coding-agent/test/fixtures/fake-claude-agent.mjs` (`USAGE_LIMIT`), read by `classifyFailure` in
//!   `packages/ai/src/api/claude-code-cli.ts`.
//! - The upstream's `anthropic-ratelimit-unified-status: rejected` header on the relayed answer, with
//!   `anthropic-ratelimit-unified-reset` (epoch seconds). ASSUMPTION: not recorded anywhere in this repository; it is
//!   the header Claude Code derives its `rate_limit_event` from. `rejected` is unambiguous, so it is matched as is.
//! - A 429 (`rate_limit` native code or relayed status 429) whose text says the plan's limit is used up:
//!   `usage limit`, `limit reached`, `out of (extra) usage` (the TypeScript `classifyFailure` pattern; the
//!   recorded CLI text `Claude AI usage limit reached|<epoch>`), or `hit your ... limit` (the recorded native text
//!   `You've hit your session limit · resets 3:45pm` of the Hermes plugin tests). Without a 429 no text counts, so a
//!   tool or prompt that merely mentions a limit never flips routing.

use super::ClaudeError;
use serde_json::Value;

/// What the failed call reported besides its error text.
#[derive(Clone, Debug, Default)]
pub struct Signals {
    /// `rate_limit_info` of the newest `rate_limit_event` native emitted.
    pub event: Option<Value>,
    /// `anthropic-ratelimit-unified-status` of the relayed answer.
    pub header_status: Option<String>,
    /// `anthropic-ratelimit-unified-reset` of the relayed answer.
    pub header_reset: Option<String>,
    /// Native's error code (`rate_limit`, `overloaded`, ...).
    pub native_code: Option<String>,
}

/// Epoch seconds or milliseconds as milliseconds (the `resetText` rule of `claude-code-cli.ts`).
fn epoch_ms(value: f64) -> Option<i64> {
    (value.is_finite() && value > 0.0).then_some(if value < 1e12 { value * 1000.0 } else { value } as i64)
}

fn worded(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("usage limit")
        || text.contains("limit reached")
        || text.contains("out of usage")
        || text.contains("out of extra usage")
        || text.find("hit your").is_some_and(|at| text[at..].contains("limit"))
}

/// `Claude AI usage limit reached|1790700600`: the reset after the pipe.
fn text_reset(text: &str) -> Option<i64> {
    let (_, rest) = text.split_once("limit reached|")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    epoch_ms(digits.parse().ok()?)
}

/// `Some(reset)` when `error` is the plan's usage limit (`reset` in epoch ms when reported), else `None`.
pub fn usage_limit(error: &ClaudeError, signals: &Signals) -> Option<Option<i64>> {
    let ClaudeError::Api { message, status } = error else {
        return None;
    };
    let rejected = signals.event.as_ref().is_some_and(|e| e["status"] == "rejected")
        || signals
            .header_status
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("rejected"));
    let throttled = *status == Some(429) || signals.native_code.as_deref() == Some("rate_limit");
    if !rejected && !(throttled && worded(message)) {
        return None;
    }
    let from_event = signals
        .event
        .as_ref()
        .and_then(|e| e["resetsAt"].as_f64())
        .and_then(epoch_ms);
    let from_header = signals
        .header_reset
        .as_deref()
        .and_then(|r| r.trim().parse::<f64>().ok())
        .and_then(epoch_ms);
    Some(from_event.or(from_header).or_else(|| text_reset(message)))
}

/// `error` as [`ClaudeError::UsageLimit`] when it is the plan's usage limit; unchanged otherwise.
pub fn classify(error: ClaudeError, signals: &Signals) -> ClaudeError {
    match usage_limit(&error, signals) {
        Some(resets_at) => ClaudeError::UsageLimit {
            message: error.to_string(),
            resets_at,
        },
        None => error,
    }
}
