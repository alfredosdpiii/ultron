//! The model-written bug report summary (`generateBugReportSummary` of core/bug-report.ts): the newest messages
//! that fit 60% of the context window, the user's description, and the report instructions, sent with the bug
//! report system prompt; the answer must be plain report text.

use ai::transcript::normalize_context;
use ai::{
    AssistantMessage, Context, Message, Model, StopReason, TextContent, UserContent, UserMessage, UserMessageContent,
};
use serde_json::Value;

use crate::{SummaryRequest, convert_to_llm, estimate_tokens, has_tool_call, response_text, serialize_conversation};

pub const BUG_SUMMARY_SYSTEM_PROMPT: &str = "You are helping a user file a bug report about pi, the coding agent they are talking to. You will be shown the conversation transcript. Write a report for the pi developers describing what the user was doing and what went wrong.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the report.";

pub const BUG_SUMMARY_INSTRUCTIONS: &str = "Write the bug report in Markdown with these sections:

## What the user was doing
One short paragraph.

## What went wrong
Concrete description of the failure: wrong output, errors, hangs, tool failures, unexpected behavior. Quote error messages and tool output verbatim where they exist.

## Steps to reproduce
Numbered list, as specific as the transcript allows.

## Relevant details
Tool calls involved, files touched, model behavior, anything else that helps a developer reproduce or locate the problem.

Do not include file contents, secrets, or credentials from the transcript; refer to files by path only. Keep the report factual and concise.";

/// `selectMessages`: the newest messages whose estimates fit `budget` (always at least the newest one).
fn select_messages(messages: &[Value], budget: u64) -> &[Value] {
    let mut tokens = 0u64;
    let mut start = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        let next = estimate_tokens(message);
        if start < messages.len() && tokens + next > budget {
            break;
        }
        start = index;
        tokens += next;
    }
    &messages[start..]
}

/// The summary request for the session's `messages`, with the user's `hint`.
pub fn bug_summary_request(
    messages: &[Value],
    hint: Option<&str>,
    model: &Model,
    thinking_level: Option<&str>,
    session_id: Option<&str>,
    now_ms: i64,
) -> SummaryRequest {
    let window = if model.context_window > 0 {
        model.context_window
    } else {
        128_000
    };
    let selected = select_messages(messages, (window as f64 * 0.6).floor() as u64);
    let hint = hint.map(session_file::js::trim).filter(|h| !h.is_empty());
    let mut parts = Vec::new();
    if selected.len() < messages.len() {
        parts.push(format!(
            "Note: only the last {} of {} messages are shown.",
            selected.len(),
            messages.len()
        ));
    }
    parts.push(format!(
        "<conversation>\n{}\n</conversation>",
        serialize_conversation(&convert_to_llm(selected))
    ));
    if let Some(hint) = hint {
        parts.push(format!("<user-report>\n{hint}\n</user-report>"));
    }
    parts.push(BUG_SUMMARY_INSTRUCTIONS.to_string());
    let context = normalize_context(&Context {
        system_prompt: Some(BUG_SUMMARY_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent {
                text: parts.join("\n\n"),
                text_signature: None,
            })]),
            timestamp: now_ms,
        })],
        tools: None,
    });
    let reasoning = thinking_level
        .filter(|level| model.reasoning && !level.is_empty() && *level != "off")
        .map(str::to_string);
    SummaryRequest {
        context,
        max_tokens: if model.max_tokens > 0 {
            model.max_tokens.min(4096)
        } else {
            4096
        },
        reasoning,
        session_id: session_id.map(str::to_string),
    }
}

/// The report text of a summary response, or why there is none.
pub fn bug_summary_text(response: &AssistantMessage) -> Result<String, String> {
    if response.stop_reason == StopReason::Aborted {
        return Err("Bug report summary was cancelled".into());
    }
    if let Some(failure) = crate::summarization_failure(response, "Bug report summary") {
        return Err(failure);
    }
    if has_tool_call(response) {
        return Err("Bug report summary attempted to call a tool".into());
    }
    let text = session_file::js::trim(&response_text(response)).to_string();
    if text.is_empty() {
        return Err("Bug report summary was empty".into());
    }
    Ok(text)
}
