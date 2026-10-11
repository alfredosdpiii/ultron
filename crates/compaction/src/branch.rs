//! Branch summarization for tree navigation (`branch-summarization.ts`): when leaving a branch, summarize it so its
//! context is not lost.

use ai::{AssistantMessage, Model, StopReason, Usage};
use serde_json::Value;
use session_file::{SessionManager, js};

use crate::{
    FileOperations, SummaryRequest, add_recorded_files, compute_file_lists, convert_to_llm, entry_type,
    estimate_tokens, extract_file_ops_from_message, format_file_operations, has_tool_call, response_text,
    serialize_conversation, str_of, summarization_failure, summary_context,
};

/// `collectEntriesForBranchSummary`: the entries from `old_leaf_id` back to (not including) its deepest common
/// ancestor with `target_id`, oldest first, and that ancestor's id.
pub fn collect_entries_for_branch_summary(
    session: &SessionManager,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> (Vec<Value>, Option<String>) {
    let Some(old_leaf_id) = old_leaf_id.filter(|id| !id.is_empty()) else {
        return (Vec::new(), None);
    };
    let old_path: Vec<&Value> = session.branch_entries(Some(old_leaf_id));
    let common_ancestor = session
        .branch_entries(Some(target_id))
        .into_iter()
        .rev()
        .find(|e| old_path.iter().any(|o| o.get("id") == e.get("id")))
        .and_then(|e| str_of(e, "id"))
        .map(str::to_string);
    let mut entries = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current.filter(|id| !id.is_empty() && Some(id) != common_ancestor.as_ref()) {
        let Some(entry) = session.entry(&id) else { break };
        entries.push(entry.clone());
        current = str_of(entry, "parentId").map(str::to_string);
    }
    entries.reverse();
    (entries, common_ancestor)
}

/// The message an entry contributes to a branch summary; tool results are skipped (their call carries the context).
fn message_from_entry(entry: &Value) -> Option<Value> {
    let timestamp = || js::date_json(entry.get("timestamp"));
    let object = |pairs: Vec<(&str, Option<Value>)>| {
        Value::Object(
            pairs
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_string(), v?)))
                .collect(),
        )
    };
    match entry_type(entry)? {
        "message" => entry
            .get("message")
            .filter(|m| str_of(m, "role") != Some("toolResult"))
            .cloned(),
        "custom_message" => Some(object(vec![
            ("role", Some("custom".into())),
            ("customType", entry.get("customType").cloned()),
            ("content", entry.get("content").cloned()),
            ("display", entry.get("display").cloned()),
            ("details", entry.get("details").cloned()),
            ("timestamp", Some(timestamp())),
        ])),
        "branch_summary" => Some(object(vec![
            ("role", Some("branchSummary".into())),
            ("summary", entry.get("summary").cloned()),
            ("fromId", entry.get("fromId").cloned()),
            ("timestamp", Some(timestamp())),
        ])),
        "compaction" => Some(object(vec![
            ("role", Some("compactionSummary".into())),
            ("summary", entry.get("summary").cloned()),
            ("tokensBefore", entry.get("tokensBefore").cloned()),
            ("timestamp", Some(timestamp())),
        ])),
        _ => None,
    }
}

/// `BranchPreparation`.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchPreparation {
    /// Messages to summarize, oldest first.
    pub messages: Vec<Value>,
    pub file_ops: FileOperations,
    pub total_tokens: u64,
}

/// `prepareBranchEntries`: the newest messages that fit `token_budget` (0 or less: no limit), plus file operations
/// from every entry, including the lists recorded by earlier pi-generated branch summaries.
pub fn prepare_branch_entries(entries: &[Value], token_budget: i64) -> BranchPreparation {
    let mut file_ops = FileOperations::default();
    for entry in entries.iter().filter(|e| entry_type(e) == Some("branch_summary")) {
        add_recorded_files(entry, &mut file_ops);
    }
    let mut newest_first = Vec::new();
    let mut total_tokens = 0;
    for entry in entries.iter().rev() {
        let Some(message) = message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);
        let tokens = estimate_tokens(&message);
        if token_budget > 0 && (total_tokens + tokens) as i64 > token_budget {
            // A summary is important context: still take it while under 90% of the budget.
            let summary = matches!(entry_type(entry), Some("compaction" | "branch_summary"));
            if summary && (total_tokens as f64) < token_budget as f64 * 0.9 {
                newest_first.push(message);
                total_tokens += tokens;
            }
            break;
        }
        newest_first.push(message);
        total_tokens += tokens;
    }
    newest_first.reverse();
    BranchPreparation {
        messages: newest_first,
        file_ops,
        total_tokens,
    }
}

pub const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str =
    "Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

/// `GenerateBranchSummaryOptions` without transport settings (those belong to the caller's model call).
#[derive(Clone, Copy, Debug)]
pub struct BranchSummaryOptions<'a> {
    pub model: &'a Model,
    pub custom_instructions: Option<&'a str>,
    /// Use `custom_instructions` instead of the default prompt rather than appending them.
    pub replace_instructions: bool,
    /// Tokens reserved when selecting branch history (upstream default 16384).
    pub reserve_tokens: u64,
    /// `Date.now()` for the request's user message.
    pub now_ms: i64,
}

/// A generated branch summary; `usage` is `None` when there was nothing to summarize.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummary {
    pub summary: String,
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Why no branch summary was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchSummaryError {
    Aborted,
    Failed(String),
}

/// `generateBranchSummary`: summarizes abandoned branch `entries` (oldest first) through `call`.
pub async fn generate_branch_summary(
    entries: &[Value],
    options: &BranchSummaryOptions<'_>,
    mut call: impl AsyncFnMut(SummaryRequest) -> AssistantMessage,
) -> Result<BranchSummary, BranchSummaryError> {
    let model = options.model;
    let context_window = if model.context_window > 0 {
        model.context_window
    } else {
        128_000
    };
    let budget = context_window as i64 - options.reserve_tokens as i64;
    let BranchPreparation { messages, file_ops, .. } = prepare_branch_entries(entries, budget);
    if messages.is_empty() {
        return Ok(BranchSummary {
            summary: "No content to summarize".to_string(),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }
    let conversation = serialize_conversation(&convert_to_llm(&messages));
    let custom = options.custom_instructions.filter(|s| !s.is_empty());
    let instructions = match custom {
        Some(custom) if options.replace_instructions => custom.to_string(),
        Some(custom) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}"),
        None => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{instructions}");
    let request = SummaryRequest {
        context: summary_context(prompt, options.now_ms),
        max_tokens: if model.max_tokens > 0 {
            model.max_tokens.min(4096)
        } else {
            4096
        },
        reasoning: None,
        session_id: None,
    };
    let response = call(request).await;
    if response.stop_reason == StopReason::Aborted {
        return Err(BranchSummaryError::Aborted);
    }
    if let Some(failure) = summarization_failure(&response, "Branch summarization") {
        return Err(BranchSummaryError::Failed(failure));
    }
    if has_tool_call(&response) {
        return Err(BranchSummaryError::Failed(
            "Branch summarization attempted to call a tool".to_string(),
        ));
    }
    let (read_files, modified_files) = compute_file_lists(&file_ops);
    let summary = format!(
        "{BRANCH_SUMMARY_PREAMBLE}{}{}",
        response_text(&response),
        format_file_operations(&read_files, &modified_files)
    );
    Ok(BranchSummary {
        summary,
        usage: Some(response.usage),
        read_files,
        modified_files,
    })
}
