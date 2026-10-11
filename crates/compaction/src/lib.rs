//! Context compaction and branch summarization, ported from
//! `packages/coding-agent/src/core/compaction/` (`compaction.ts`, `branch-summarization.ts`, `utils.ts`).
//!
//! Session entries and agent messages are JSON values in the session file format (see `session-file`), so custom
//! roles (`bashExecution`, `custom`, `branchSummary`, `compactionSummary`) and unknown fields pass through untouched.
//! Summaries are produced by a caller-supplied async model call that receives a [`SummaryRequest`] built from
//! `ai` types and returns the provider's [`AssistantMessage`]; this crate does no I/O.

mod branch;

use std::collections::HashSet;

use ai::transcript::{ContextUsageEstimate, normalize_context};
use ai::{
    AssistantContent, AssistantMessage, Context, Message, Model, StopReason, TextContent, TranscriptContext, Usage,
    UserContent, UserMessage, UserMessageContent,
};
use indexmap::IndexSet;
use serde_json::Value;
use session_file::{Leaf, ProjectedEntry, SessionProjection, build_session_projection, current_system_message, js};

pub use branch::{
    BRANCH_SUMMARY_PREAMBLE, BranchPreparation, BranchSummary, BranchSummaryError, BranchSummaryOptions,
    collect_entries_for_branch_summary, generate_branch_summary, prepare_branch_entries,
};

// ============================================================================
// Shared utilities (utils.ts)
// ============================================================================

/// `text.length` in JavaScript: UTF-16 code units.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn role(message: &Value) -> Option<&str> {
    str_of(message, "role")
}

fn entry_type(entry: &Value) -> Option<&str> {
    str_of(entry, "type")
}

/// `contentText(content, separator)`: a string as is, else the text blocks joined.
fn content_text(content: Option<&Value>, separator: &str) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| str_of(b, "type") == Some("text"))
            .map(|b| js::join_string(b.get("text")))
            .collect::<Vec<_>>()
            .join(separator),
        _ => String::new(),
    }
}

/// Files touched by `read`, `write` and `edit` tool calls, in first-seen order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOperations {
    pub read: IndexSet<String>,
    pub written: IndexSet<String>,
    pub edited: IndexSet<String>,
}

/// `extractFileOpsFromMessage`: records the `path` argument of assistant `read`/`write`/`edit` tool calls.
pub fn extract_file_ops_from_message(message: &Value, ops: &mut FileOperations) {
    if role(message) != Some("assistant") {
        return;
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if str_of(block, "type") != Some("toolCall") || block.get("name").is_none() {
            continue;
        }
        let Some(path) = block.get("arguments").and_then(|args| str_of(args, "path")) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let set = match str_of(block, "name") {
            Some("read") => &mut ops.read,
            Some("write") => &mut ops.written,
            Some("edit") => &mut ops.edited,
            _ => continue,
        };
        set.insert(path.to_string());
    }
}

/// JavaScript's default `Array.prototype.sort`: by UTF-16 code units.
fn js_sort(items: &mut [String]) {
    items.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
}

/// `computeFileLists`: `(read-only files, modified files)`, each sorted.
pub fn compute_file_lists(ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: IndexSet<&String> = ops.edited.iter().chain(&ops.written).collect();
    let mut read_files: Vec<String> = ops.read.iter().filter(|f| !modified.contains(f)).cloned().collect();
    let mut modified_files: Vec<String> = modified.into_iter().cloned().collect();
    js_sort(&mut read_files);
    js_sort(&mut modified_files);
    (read_files, modified_files)
}

/// `formatFileOperations`: the `<read-files>`/`<modified-files>` sections appended to a summary.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read_files.join("\n")));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// `text.slice(0, max)` over UTF-16 code units; never splits a surrogate pair (upstream can leave half of one).
fn js_prefix(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..i];
        }
    }
    text
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let len = js_len(text);
    if len <= max_chars {
        return text.to_string();
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        js_prefix(text, max_chars),
        len - max_chars
    )
}

/// `serializeConversation`: LLM messages (see [`convert_to_llm`]) as `[Role]: ...` text, so the summarizer reads the
/// transcript instead of continuing it. Tool results are cut to 2000 characters.
pub fn serialize_conversation(messages: &[Value]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match role(message) {
            Some("user") => {
                let content = content_text(message.get("content"), "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Some("assistant") => {
                let blocks = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                let mut thinking = Vec::new();
                let mut calls = Vec::new();
                for block in blocks {
                    match str_of(block, "type") {
                        Some("thinking") => thinking.push(js::join_string(block.get("thinking"))),
                        Some("toolCall") => {
                            let args = match block.get("arguments") {
                                Some(Value::Object(map)) => js::object_entries(map)
                                    .into_iter()
                                    .map(|(k, v)| format!("{k}={}", js::stringify(v)))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                _ => String::new(),
                            };
                            calls.push(format!("{}({args})", js::join_string(block.get("name"))));
                        }
                        _ => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if blocks.iter().any(|b| str_of(b, "type") == Some("text")) {
                    parts.push(format!("[Assistant]: {}", content_text(message.get("content"), "\n")));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Some("toolResult") => {
                let content = content_text(message.get("content"), "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// `bashExecutionToText`.
fn bash_execution_to_text(message: &Value) -> String {
    let output = js::join_string(message.get("output"));
    let mut text = format!("Ran `{}`\n", js::join_string(message.get("command")));
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    let exit_code = message.get("exitCode").filter(|c| !c.is_null());
    if js::truthy(message.get("cancelled")) {
        text.push_str("\n\n(command cancelled)");
    } else if let Some(code) = exit_code.filter(|c| c.as_f64() != Some(0.0)) {
        text.push_str(&format!("\n\nCommand exited with code {}", js::stringify(code)));
    }
    if js::truthy(message.get("truncated")) && js::truthy(message.get("fullOutputPath")) {
        text.push_str(&format!(
            "\n\n[Output truncated. Full output: {}]",
            js::join_string(message.get("fullOutputPath"))
        ));
    }
    text
}

fn text_user(text: String, timestamp: Option<&Value>) -> Value {
    let mut message = serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": text }] });
    if let Some(timestamp) = timestamp {
        message["timestamp"] = timestamp.clone();
    }
    message
}

/// `convertToLlm`: custom agent messages become user messages; `!!` bash executions are dropped.
pub fn convert_to_llm(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| match role(m) {
            Some("bashExecution") if js::truthy(m.get("excludeFromContext")) => None,
            Some("bashExecution") => Some(text_user(bash_execution_to_text(m), m.get("timestamp"))),
            Some("custom") => {
                let content = match m.get("content") {
                    Some(Value::String(text)) => serde_json::json!([{ "type": "text", "text": text }]),
                    other => other.cloned().unwrap_or(Value::Null),
                };
                let mut message = serde_json::json!({ "role": "user", "content": content });
                if let Some(timestamp) = m.get("timestamp") {
                    message["timestamp"] = timestamp.clone();
                }
                Some(message)
            }
            Some("branchSummary") => Some(text_user(
                format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    js::join_string(m.get("summary"))
                ),
                m.get("timestamp"),
            )),
            Some("compactionSummary") => Some(text_user(
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    js::join_string(m.get("summary"))
                ),
                m.get("timestamp"),
            )),
            Some("system" | "user" | "assistant" | "toolResult") => Some(m.clone()),
            _ => None,
        })
        .collect()
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

// ============================================================================
// Token estimation
// ============================================================================

/// `CompactionSettings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
        }
    }
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_f64).map_or(0, |n| n as u64)
}

/// `calculateContextTokens`: `totalTokens`, else the sum of input, output and cache tokens.
pub fn calculate_context_tokens(usage: &Value) -> u64 {
    match number(usage, "totalTokens") {
        0 => {
            number(usage, "input") + number(usage, "output") + number(usage, "cacheRead") + number(usage, "cacheWrite")
        }
        total => total,
    }
}

/// Usage of an assistant message that completed and reported a non-zero count.
fn assistant_usage(message: &Value) -> Option<&Value> {
    if role(message) != Some("assistant") || matches!(str_of(message, "stopReason"), Some("aborted" | "error")) {
        return None;
    }
    message
        .get("usage")
        .filter(|u| js::truthy(Some(u)) && calculate_context_tokens(u) > 0)
}

/// `getLastAssistantUsage`: usage of the newest valid assistant message entry.
pub fn last_assistant_usage(entries: &[Value]) -> Option<&Value> {
    entries
        .iter()
        .rev()
        .filter(|e| entry_type(e) == Some("message"))
        .find_map(|e| e.get("message").and_then(assistant_usage))
}

const ESTIMATED_IMAGE_CHARS: usize = 4800;

fn text_and_image_chars(content: Option<&Value>) -> usize {
    match content {
        Some(Value::String(text)) => js_len(text),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match str_of(block, "type") {
                Some("text") => str_of(block, "text").map_or(0, js_len),
                Some("image") => ESTIMATED_IMAGE_CHARS,
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// `estimateTokens`: characters / 4, rounded up (an overestimate); images count 4800 characters.
pub fn estimate_tokens(message: &Value) -> u64 {
    let len = |key: &str| str_of(message, key).map_or(0, js_len);
    let chars = match role(message) {
        Some("system") => {
            let sections: usize = match message.get("sections") {
                Some(Value::Object(map)) => map.values().filter_map(Value::as_str).map(js_len).sum(),
                _ => 0,
            };
            let tools = message
                .get("toolsAdded")
                .filter(|t| js::truthy(Some(t)))
                .map_or(0, |t| js_len(&js::stringify(t)));
            text_and_image_chars(message.get("content")) + sections + tools
        }
        Some("user" | "custom" | "toolResult") => text_and_image_chars(message.get("content")),
        Some("assistant") => message
            .get("content")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|block| match str_of(block, "type") {
                Some("text") => str_of(block, "text").map_or(0, js_len),
                Some("thinking") => str_of(block, "thinking").map_or(0, js_len),
                Some("toolCall") => {
                    str_of(block, "name").map_or(0, js_len)
                        + block.get("arguments").map_or(0, |a| js_len(&js::stringify(a)))
                }
                _ => 0,
            })
            .sum(),
        Some("bashExecution") => len("command") + len("output"),
        Some("branchSummary" | "compactionSummary") => len("summary"),
        _ => 0,
    };
    chars.div_ceil(4) as u64
}

/// `estimateContextTokens`: the newest valid assistant usage plus estimates for the messages after it.
pub fn estimate_context_tokens(messages: &[Value]) -> ContextUsageEstimate {
    let last = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| assistant_usage(m).map(|u| (calculate_context_tokens(u), i)));
    match last {
        None => {
            let tokens = messages.iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens,
                usage_tokens: 0,
                trailing_tokens: tokens,
                last_usage_index: None,
            }
        }
        Some((usage_tokens, index)) => {
            let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

/// `estimateProjectedContextTokens`: like [`estimate_context_tokens`], but usage recorded before the latest context
/// edit or compaction on the branch is stale, so then every projected message is estimated instead.
pub fn estimate_projected_context_tokens(
    projection: &SessionProjection<'_>,
    branch_entries: &[Value],
) -> ContextUsageEstimate {
    let estimate = estimate_context_tokens(&projection.messages);
    if let Some(last_usage) = estimate.last_usage_index {
        let mut start = 0;
        let mut usage_entry_id = None;
        for entry in &projection.entries {
            let next = start + entry.messages.len();
            if last_usage < next {
                usage_entry_id = entry.source.get("id").filter(|id| js::truthy(Some(id)));
                break;
            }
            start = next;
        }
        let usage_entry_index =
            usage_entry_id.and_then(|id| branch_entries.iter().position(|e| e.get("id") == Some(id)));
        let invalidating = branch_entries
            .iter()
            .rposition(|e| matches!(entry_type(e), Some("context_edit" | "compaction")));
        if usage_entry_index.is_some_and(|u| invalidating.is_none_or(|i| u > i)) {
            return estimate;
        }
    }
    let system = current_system_message(&projection.messages).map_or(0, |m| estimate_tokens(&m));
    let tokens = system
        + projection
            .messages
            .iter()
            .filter(|m| role(m) != Some("system"))
            .map(estimate_tokens)
            .sum::<u64>();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

/// `shouldCompact`: whether the context leaves less than `reserve_tokens` of the window.
pub fn should_compact(context_tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool {
    settings.enabled && i128::from(context_tokens) > i128::from(context_window) - i128::from(settings.reserve_tokens)
}

// ============================================================================
// Cut points
// ============================================================================

fn is_cut_point_message(message: &Value) -> bool {
    matches!(
        role(message),
        Some("user" | "assistant" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary")
    )
}

fn is_turn_start_message(message: &Value) -> bool {
    matches!(
        role(message),
        Some("user" | "bashExecution" | "custom" | "branchSummary" | "compactionSummary")
    )
}

fn is_turn_start_entry(entry: &Value) -> bool {
    entry_type(entry) != Some("compaction") && session_file::entry_messages(entry).iter().any(is_turn_start_message)
}

/// `CutPointResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutPoint {
    /// Index of the first entry to keep.
    pub first_kept_entry_index: usize,
    /// The entry that starts the turn being split, when the cut is mid-turn.
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

/// `findTurnStartIndex`: the nearest entry at or before `entry_index` (not before `start_index`) that starts a turn.
pub fn find_turn_start_index(entries: &[Value], entry_index: usize, start_index: usize) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&i| is_turn_start_entry(&entries[i]))
}

/// Walks back from `end` summing `tokens(i)`; once `keep` is reached, cuts at the nearest cut point at or after it.
fn budget_cut(
    cut_points: &[usize],
    start: usize,
    end: usize,
    keep: u64,
    tokens: impl Fn(usize) -> u64,
) -> (usize, bool) {
    let mut accumulated = 0;
    for i in (start..end).rev() {
        let message_tokens = tokens(i);
        if message_tokens == 0 {
            continue;
        }
        accumulated += message_tokens;
        if accumulated >= keep {
            let cut = cut_points
                .iter()
                .copied()
                .find(|&c| c >= i)
                .unwrap_or(cut_points[cut_points.len() - 1]);
            return (cut, true);
        }
    }
    (cut_points[0], false)
}

/// `findCutPoint`: the first entry to keep so that roughly `keep_recent_tokens` of recent context survive. Cuts only
/// at user-like or assistant messages, never at tool results.
pub fn find_cut_point(entries: &[Value], start_index: usize, end_index: usize, keep_recent_tokens: u64) -> CutPoint {
    let cut_points: Vec<usize> = (start_index..end_index)
        .filter(|&i| {
            entry_type(&entries[i]) != Some("compaction")
                && session_file::entry_messages(&entries[i])
                    .iter()
                    .any(is_cut_point_message)
        })
        .collect();
    if cut_points.is_empty() {
        return CutPoint {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }
    let (mut cut, _) = budget_cut(&cut_points, start_index, end_index, keep_recent_tokens, |i| {
        session_file::entry_messages(&entries[i])
            .iter()
            .map(estimate_tokens)
            .sum()
    });
    while cut > start_index {
        let previous = &entries[cut - 1];
        if entry_type(previous) == Some("compaction") || !session_file::entry_messages(previous).is_empty() {
            break;
        }
        cut -= 1;
    }
    let starts_turn = is_turn_start_entry(&entries[cut]);
    let turn_start_index = if starts_turn {
        None
    } else {
        find_turn_start_index(entries, cut, start_index)
    };
    CutPoint {
        first_kept_entry_index: cut,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

fn source_type<'a>(entry: &ProjectedEntry<'a>) -> Option<&'a str> {
    entry_type(entry.source)
}

fn is_projected_turn_start(entry: &ProjectedEntry<'_>) -> bool {
    source_type(entry) != Some("compaction") && entry.messages.iter().any(is_turn_start_message)
}

/// `findProjectedCutPoint`: [`find_cut_point`] over context-edited entries.
fn find_projected_cut_point(entries: &[ProjectedEntry<'_>], start: usize, end: usize, keep: u64) -> CutPoint {
    let cut_points: Vec<usize> = (start..end)
        .filter(|&i| {
            source_type(&entries[i]) != Some("compaction") && entries[i].messages.iter().any(is_cut_point_message)
        })
        .collect();
    if cut_points.is_empty() {
        return CutPoint {
            first_kept_entry_index: start,
            turn_start_index: None,
            is_split_turn: false,
        };
    }
    let (mut cut, exceeded) = budget_cut(&cut_points, start, end, keep, |i| {
        entries[i].messages.iter().map(estimate_tokens).sum()
    });

    // A recovery attempt and its omission edits are context-invisible after the last visible input. Advance only for
    // a closed suffix containing an omitted assistant attempt; arbitrary metadata must not move the cut past input.
    let suffix = entries.get(cut + 1..end).unwrap_or_default();
    let visible = |e: &ProjectedEntry<'_>| {
        source_type(e) != Some("context_edit") && !session_file::entry_messages(e.source).is_empty()
    };
    let omitted = |e: &ProjectedEntry<'_>| visible(e) && e.messages.is_empty();
    let omitted_ids: HashSet<String> = suffix
        .iter()
        .filter(|e| omitted(e))
        .map(|e| id_key(e.source.get("id")))
        .collect();
    let external_replacement = suffix.iter().any(|e| {
        source_type(e) == Some("context_edit")
            && e.source.get("replacement") != Some(&Value::Null)
            && !omitted_ids.contains(&id_key(e.source.get("targetId")))
    });
    let recovery_suffix = exceeded
        && !external_replacement
        && suffix.iter().any(|e| {
            source_type(e) == Some("message")
                && e.source.get("message").and_then(role) == Some("assistant")
                && omitted(e)
        })
        && suffix
            .iter()
            .all(|e| source_type(e) != Some("compaction") && (!visible(e) || omitted(e)));
    if recovery_suffix {
        cut += 1;
    }

    while cut > start {
        let previous = &entries[cut - 1];
        if source_type(previous) == Some("compaction") || !previous.messages.is_empty() {
            break;
        }
        cut -= 1;
    }
    let starts_turn = is_projected_turn_start(&entries[cut]);
    let turn_start_index = if starts_turn {
        None
    } else {
        (start..=cut).rev().find(|&i| is_projected_turn_start(&entries[i]))
    };
    CutPoint {
        first_kept_entry_index: cut,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

/// JavaScript `Set`/`Map` key for an id: `undefined` is distinct from every string.
fn id_key(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), js::stringify)
}

// ============================================================================
// Preparation
// ============================================================================

/// `CompactionPreparation`: what [`compact`] summarizes and keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    /// Messages summarized and dropped.
    pub messages_to_summarize: Vec<Value>,
    /// The start of a split turn, summarized separately.
    pub turn_prefix_messages: Vec<Value>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    /// The previous compaction's summary, for an iterative update.
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// Messages an entry contributes to a summary: none for compactions, and never system messages (prompt state).
fn summary_messages(entries: &[ProjectedEntry<'_>]) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| source_type(e) != Some("compaction"))
        .flat_map(|e| e.messages.iter().filter(|m| role(m) != Some("system")).cloned())
        .collect()
}

/// Adds the read/modified lists a pi-generated (not extension) summary entry recorded in `details`.
fn add_recorded_files(entry: &Value, ops: &mut FileOperations) {
    if js::truthy(entry.get("fromHook")) {
        return;
    }
    let Some(details) = entry.get("details").filter(|d| js::truthy(Some(d))) else {
        return;
    };
    let files = |key: &str| {
        details
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
    };
    ops.read.extend(files("readFiles"));
    ops.edited.extend(files("modifiedFiles"));
}

/// `prepareCompaction`: picks the cut on the session path and collects what to summarize. `None` when the path
/// ends in a compaction or nothing would be summarized.
pub fn prepare_compaction(path_entries: &[Value], settings: CompactionSettings) -> Option<CompactionPreparation> {
    if path_entries.last().is_some_and(|e| entry_type(e) == Some("compaction")) {
        return None;
    }
    let projection = build_session_projection(path_entries, Leaf::Last);
    let entries = &projection.entries;
    // The newest compaction is projected first; older ones in its kept range contribute nothing.
    let prev_compaction = entries
        .iter()
        .position(|e| source_type(e) == Some("compaction") && !e.messages.is_empty());
    let previous_summary = prev_compaction
        .and_then(|i| str_of(entries[i].source, "summary"))
        .map(str::to_string);
    let boundary_start = prev_compaction.map_or(0, |i| i + 1);
    let tokens_before = estimate_projected_context_tokens(&projection, path_entries).tokens;
    let cut = find_projected_cut_point(entries, boundary_start, entries.len(), settings.keep_recent_tokens);

    let first_kept_entry_id = entries
        .get(cut.first_kept_entry_index)
        .and_then(|e| str_of(e.source, "id"))
        .filter(|id| !id.is_empty())?
        .to_string();
    let history_end = match cut.turn_start_index {
        Some(turn_start) if cut.is_split_turn => turn_start,
        _ => cut.first_kept_entry_index,
    };
    let messages_to_summarize = summary_messages(entries.get(boundary_start..history_end).unwrap_or_default());
    let turn_prefix_messages = match cut.turn_start_index {
        Some(turn_start) if cut.is_split_turn => summary_messages(&entries[turn_start..cut.first_kept_entry_index]),
        _ => Vec::new(),
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let mut file_ops = FileOperations::default();
    if let Some(i) = prev_compaction {
        add_recorded_files(entries[i].source, &mut file_ops);
    }
    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        extract_file_ops_from_message(message, &mut file_ops);
    }
    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

// ============================================================================
// Summarization requests
// ============================================================================

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed
- UPDATE \"Next Steps\" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages.";

/// One summarization call. The caller adds its credentials, headers and abort signal, always sends it with
/// `cacheRetention: "none"` (one-off summaries must not write the cache), and mints a fresh routing session id when
/// `session_id` is `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct SummaryRequest {
    pub context: TranscriptContext,
    pub max_tokens: u64,
    /// The thinking level, for reasoning models when one other than `off` was requested.
    pub reasoning: Option<String>,
    pub session_id: Option<String>,
}

/// The summarization transcript: the summarizer system prompt and one user message holding `prompt`.
pub(crate) fn summary_context(prompt: String, now_ms: i64) -> TranscriptContext {
    normalize_context(&Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent {
                text: prompt,
                text_signature: None,
            })]),
            timestamp: now_ms,
        })],
        tools: None,
    })
}

/// `min(floor(fraction * reserve), model.maxTokens || Infinity)`.
fn summary_max_tokens(fraction: f64, reserve_tokens: u64, model: &Model) -> u64 {
    let budget = (fraction * reserve_tokens as f64).floor() as u64;
    if model.max_tokens > 0 {
        budget.min(model.max_tokens)
    } else {
        budget
    }
}

/// `getSummarizationFailure`: why a response must not become a checkpoint (an error, or a length stop that left
/// the summary incomplete).
pub fn summarization_failure(response: &AssistantMessage, label: &str) -> Option<String> {
    match response.stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            response
                .error_message
                .as_deref()
                .filter(|m| !m.is_empty())
                .unwrap_or("Unknown error")
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

fn has_tool_call(response: &AssistantMessage) -> bool {
    response
        .content
        .iter()
        .any(|b| matches!(b, AssistantContent::ToolCall(_)))
}

/// `contentText` of a response: its text blocks joined by newlines.
fn response_text(response: &AssistantMessage) -> String {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Settings of one compaction run (`compact`'s model, focus, thinking level and routing session).
#[derive(Clone, Copy, Debug)]
pub struct CompactOptions<'a> {
    pub model: &'a Model,
    /// Extra focus appended to the summary prompt.
    pub custom_instructions: Option<&'a str>,
    pub thinking_level: Option<&'a str>,
    /// Routing session id forwarded to the provider (without enabling prompt caching).
    pub session_id: Option<&'a str>,
    /// `Date.now()` for the request's user message.
    pub now_ms: i64,
}

impl CompactOptions<'_> {
    fn request(&self, prompt: String, max_tokens: u64) -> SummaryRequest {
        let reasoning = self
            .thinking_level
            .filter(|level| self.model.reasoning && !level.is_empty() && *level != "off")
            .map(str::to_string);
        SummaryRequest {
            context: summary_context(prompt, self.now_ms),
            max_tokens,
            reasoning,
            session_id: self.session_id.map(str::to_string),
        }
    }
}

/// `generateSummaryWithUsage`: the history summary request (an update when `previous_summary` is set).
pub fn history_summary_request(
    messages: &[Value],
    reserve_tokens: u64,
    previous_summary: Option<&str>,
    options: &CompactOptions<'_>,
) -> SummaryRequest {
    let previous = previous_summary.filter(|s| !s.is_empty());
    let mut instructions = if previous.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    }
    .to_string();
    if let Some(focus) = options.custom_instructions.filter(|s| !s.is_empty()) {
        instructions = format!("{instructions}\n\nAdditional focus: {focus}");
    }
    let conversation = serialize_conversation(&convert_to_llm(messages));
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous {
        prompt.push_str(&format!("<previous-summary>\n{previous}\n</previous-summary>\n\n"));
    }
    prompt.push_str(&instructions);
    options.request(prompt, summary_max_tokens(0.8, reserve_tokens, options.model))
}

/// `generateTurnPrefixSummary`'s request: a checkpoint of the start of a split turn, with half the budget.
pub fn turn_prefix_request(messages: &[Value], reserve_tokens: u64, options: &CompactOptions<'_>) -> SummaryRequest {
    let conversation = serialize_conversation(&convert_to_llm(messages));
    let prompt = format!("# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}");
    options.request(prompt, summary_max_tokens(0.5, reserve_tokens, options.model))
}

/// `CompactionResult`; `details` holds the file lists.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Usage,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let optional =
        |a: Option<u64>, b: Option<u64>| (a.is_some() || b.is_some()).then(|| a.unwrap_or(0) + b.unwrap_or(0));
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: optional(first.cache_write_1h, second.cache_write_1h),
        reasoning: optional(first.reasoning, second.reasoning),
        total_tokens: first.total_tokens + second.total_tokens,
        cost: ai::Cost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

/// Sends one request and checks the response; the error text is upstream's thrown message.
async fn summarize(
    request: SummaryRequest,
    label: &str,
    call: &mut impl AsyncFnMut(SummaryRequest) -> AssistantMessage,
) -> Result<(String, Usage), String> {
    let response = call(request).await;
    if let Some(failure) = summarization_failure(&response, label) {
        return Err(failure);
    }
    if has_tool_call(&response) {
        return Err(format!("{label} attempted to call a tool"));
    }
    Ok((response_text(&response), response.usage))
}

/// `compact`: summarizes a prepared compaction through `call` (history, then the split turn's prefix when cutting
/// mid-turn) and appends the file lists.
pub async fn compact(
    preparation: &CompactionPreparation,
    options: &CompactOptions<'_>,
    mut call: impl AsyncFnMut(SummaryRequest) -> AssistantMessage,
) -> Result<CompactionResult, String> {
    let prep = preparation;
    let reserve = prep.settings.reserve_tokens;
    let history = || {
        history_summary_request(
            &prep.messages_to_summarize,
            reserve,
            prep.previous_summary.as_deref(),
            options,
        )
    };
    let (mut summary, usage) = if prep.is_split_turn && !prep.turn_prefix_messages.is_empty() {
        let (history_text, history_usage) = if prep.messages_to_summarize.is_empty() {
            (
                prep.previous_summary
                    .clone()
                    .unwrap_or_else(|| "No prior history.".to_string()),
                None,
            )
        } else {
            let (text, usage) = summarize(history(), "Summarization", &mut call).await?;
            (text, Some(usage))
        };
        let request = turn_prefix_request(&prep.turn_prefix_messages, reserve, options);
        let (prefix_text, prefix_usage) = summarize(request, "Turn prefix summarization", &mut call).await?;
        let usage = match history_usage {
            Some(history_usage) => combine_usage(&history_usage, &prefix_usage),
            None => prefix_usage,
        };
        (
            format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_text}"),
            usage,
        )
    } else {
        summarize(history(), "Summarization", &mut call).await?
    };
    let (read_files, modified_files) = compute_file_lists(&prep.file_ops);
    summary.push_str(&format_file_operations(&read_files, &modified_files));
    Ok(CompactionResult {
        summary,
        first_kept_entry_id: prep.first_kept_entry_id.clone(),
        tokens_before: prep.tokens_before,
        usage,
        read_files,
        modified_files,
    })
}
