//! The summary a conversation's compaction makes, as the TypeScript harness makes it for every lane but the root's
//! (packages/agent/src/harness/compaction/compaction.ts `compactWithRequest`): the history before the cut is
//! summarized, as an update of the previous compaction's summary when there is one; a cut inside a turn also
//! summarizes that turn's prefix on its own; the files read and modified (from `read`/`write`/`edit` calls and the
//! previous compaction's lists) end the summary.

use ai::{Message, UserContent, UserMessageContent};
use compaction::{CompactOptions, FileOperations, SummaryRequest};
use serde_json::Value;

/// `TURN_PREFIX_SUMMARIZATION_PROMPT` of the harness compaction.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str =
    "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix.";

/// What one compaction summarizes (`CompactionPreparation`, the parts the summary needs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SummaryPlan {
    /// Model messages summarized into the history summary.
    pub history: Vec<Value>,
    /// The split turn's prefix, summarized separately (empty unless `split`).
    pub prefix: Vec<Value>,
    pub split: bool,
    /// The previous compaction's summary, updated rather than rewritten.
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
}

/// The requests of a plan, in order: the history summary (absent for a split turn with no history before it), then
/// the turn prefix summary of a split turn.
pub fn summary_requests(plan: &SummaryPlan, reserve_tokens: u64, options: &CompactOptions<'_>) -> Vec<SummaryRequest> {
    let previous = plan.previous_summary.as_deref();
    let mut requests = Vec::new();
    let split = plan.split && !plan.prefix.is_empty();
    if !split || !plan.history.is_empty() {
        requests.push(compaction::history_summary_request(
            &plan.history,
            reserve_tokens,
            previous,
            options,
        ));
    }
    if split {
        requests.push(turn_prefix_request(&plan.prefix, reserve_tokens, options));
    }
    requests
}

/// `generateTurnPrefixSummary`'s request: the prefix as a conversation, half the reserve.
fn turn_prefix_request(messages: &[Value], reserve_tokens: u64, options: &CompactOptions<'_>) -> SummaryRequest {
    let conversation = compaction::serialize_conversation(&compaction::convert_to_llm(messages));
    let prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}");
    // The shape (system prompt, budget, reasoning, session) is the crate's turn-prefix request; only the text differs.
    let mut request = compaction::turn_prefix_request(messages, reserve_tokens, options);
    for message in &mut request.context.messages {
        if let Message::User(user) = message {
            user.content = UserMessageContent::Blocks(vec![UserContent::Text(ai::TextContent {
                text: prompt.clone(),
                text_signature: None,
            })]);
        }
    }
    request
}

/// The summary from the responses' texts, in request order, with the file lists appended; and those lists.
pub fn compose_summary(plan: &SummaryPlan, texts: &[String]) -> (String, Vec<String>, Vec<String>) {
    let split = plan.split && !plan.prefix.is_empty();
    let mut summary = if split {
        let (history, prefix) = if plan.history.is_empty() {
            (
                "No prior history.".to_string(),
                texts.first().cloned().unwrap_or_default(),
            )
        } else {
            (
                texts.first().cloned().unwrap_or_default(),
                texts.get(1).cloned().unwrap_or_default(),
            )
        };
        format!("{history}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix}")
    } else {
        texts.first().cloned().unwrap_or_default()
    };
    let (read, modified) = compaction::compute_file_lists(&plan.file_ops);
    summary.push_str(&compaction::format_file_operations(&read, &modified));
    (summary, read, modified)
}

/// `extractFileOperations`: the previous compaction's lists (`details.readFiles` read, `details.modifiedFiles`
/// edited), then the `read`/`write`/`edit` calls of the summarized messages and the split turn's prefix.
pub fn file_operations(previous_details: Option<&Value>, messages: &[&Value]) -> FileOperations {
    let mut ops = FileOperations::default();
    if let Some(details) = previous_details.filter(|d| d.is_object()) {
        for path in details["readFiles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            ops.read.insert(path.to_string());
        }
        for path in details["modifiedFiles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            ops.edited.insert(path.to_string());
        }
    }
    for message in messages {
        compaction::extract_file_ops_from_message(message, &mut ops);
    }
    ops
}
