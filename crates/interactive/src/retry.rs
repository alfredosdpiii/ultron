//! Retry of the last failed turn (omp `app.retry`, F5): whether the transcript tail is retryable and the idle
//! "F5 to retry" row that advertises the live binding (oh-my-pi interactive-mode.ts `syncRetryHintRow` and
//! turn-recovery.ts `hasAbortedToolCallTail`). The retry itself is a session request the client sends.

use serde_json::Value;
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

use crate::components::{format_key_text, key_hint};

/// The tail is a failed or aborted assistant turn, possibly followed by the placeholder tool results an aborted
/// tool call leaves (or by an errored tool result when Esc landed during execution). A settled successful
/// assistant message leaves nothing to retry.
pub fn failed_turn_tail(messages: &[Value]) -> bool {
    let mut i = messages.len();
    let mut errored_result = false;
    while i > 0 && messages[i - 1]["role"] == "toolResult" {
        errored_result |= messages[i - 1]["isError"] == true;
        i -= 1;
    }
    let Some(last) = i.checked_sub(1).map(|i| &messages[i]) else {
        return false;
    };
    if last["role"] != "assistant" {
        return false;
    }
    match last["stopReason"].as_str() {
        Some("error") | Some("aborted") => true,
        // A completed tool call whose result failed because the user aborted it mid-run.
        Some("toolUse") => errored_result && i < messages.len(),
        _ => false,
    }
}

/// The status row shown while idle with a retryable tail: a blank row, then ` <key> to retry` with the first
/// `app.retry` key (F5 by default). `None` when nothing is retryable or retry is unbound.
pub fn retry_hint_rows(
    theme: &Theme,
    keys: &KeybindingsManager,
    messages: &[Value],
    streaming: bool,
) -> Option<Vec<String>> {
    if streaming || !failed_turn_tail(messages) {
        return None;
    }
    let key = keys.get_keys("app.retry").into_iter().next()?;
    Some(vec![
        String::new(),
        format!(" {}", key_hint(theme, &format_key_text(&key, false, false), "to retry")),
    ])
}
