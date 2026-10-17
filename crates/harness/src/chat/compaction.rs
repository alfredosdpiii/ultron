//! The built-in compaction task `pi.compaction` (spec §8.7): select an old prefix of the model context, summarize it
//! with Ultron's summarization request (`crates/compaction`), and place a summary entry whose `head` is the first kept
//! entry. Port of upstream `harness/compaction.ts`. A compaction the generation owns blocks it and appends directly;
//! a conversation-owned one places its summary through a write submission.

use std::sync::Arc;

use ai::{AssistantContent, AssistantMessage, Message, StopReason};
use durable::{
    ConversationId, EntryDraft, EntryHead, EntryId, TaskId, TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership,
    TaskRecord, TaskState, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::Services;
use super::child_summary::SummaryPlan;
use super::context::{ContextView, order_tool_results, read_context};
use super::entries;
use super::generation::{ensure_provider_session_id, read_agent};
use super::live::{COMPACTION_KIND, add_compaction_status, compaction_status_index, live, remove_compaction_status};
use super::models::{CompactionPolicy, ModelRef, StreamRequest, is_retryable};
use super::submissions::{SubmissionDraft, admit_submission};
use super::usage::{Bucket, record_usage};
use super::util::{d, from_value, strict, to_value};
use crate::scheduler::{HarnessError, NextState, Result, Task, TaskRuntime, completed};

/// Which threshold compaction preparation starts before its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threshold {
    Blocking,
    Background,
}

/// The pinned summarization request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    pub attempt: u64,
    pub model: ModelRef,
    pub thinking_level: String,
    pub stream_options: Map<String, Value>,
    /// Newest entry of the context the range was selected from.
    pub tail: EntryId,
    /// First entry kept verbatim; the summary's `head`.
    pub first_kept: EntryId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum CompactionCheckpoint {
    Select,
    Summarize {
        #[serde(flatten)]
        request: SummaryRequest,
    },
    Retry {
        until: u64,
        #[serde(flatten)]
        request: SummaryRequest,
    },
}

static DEFINITION: std::sync::LazyLock<durable::TaskDefinition> = std::sync::LazyLock::new(|| {
    durable::TaskDefinition::new(COMPACTION_KIND, 1, |_| to_value(&CompactionCheckpoint::Select))
});

/// Create a compaction task with its status in this commit. `owner` is the generation that waits for it (a blocking
/// compaction); without one it is conversation-owned, and background unless it is manual.
pub(crate) fn create_compaction(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    reason: &str,
    instructions: Option<&str>,
    owner: Option<TaskId>,
) -> Result<TaskId> {
    let mut input = json!({ "reason": reason });
    if let Some(instructions) = instructions {
        input["instructions"] = json!(instructions);
    }
    let ownership = match owner {
        Some(owner) => TaskOwnership::Task(owner),
        None => TaskOwnership::Conversation,
    };
    let task_id = tx.create_task(
        &DEFINITION,
        input,
        TaskOptions {
            ownership,
            conversation_id: Some(conversation_id),
            background: owner.is_none() && reason != "manual",
            ..TaskOptions::default()
        },
    )?;
    let live = live(tx, conversation_id)?;
    add_compaction_status(
        &live,
        json!({ "taskId": task_id.0, "reason": reason, "blocking": owner.is_some(), "attempt": 1 }),
    )?;
    Ok(task_id)
}

fn context_tokens(usage: &ai::Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

/// Size of a request over `view` followed by `extra`: the usage of the newest assistant appended after the head
/// marker, plus estimates of the messages after it; without one, estimates of every message.
pub fn estimate_context(view: &ContextView, extra: &[Message]) -> u64 {
    let after = view.head.as_ref().map(|head| head.id);
    let mut measured: Option<&AssistantMessage> = None;
    for (index, entry) in view.entries.iter().enumerate().rev() {
        if measured.is_some() {
            break;
        }
        if after.is_some_and(|after| entry.id <= after) {
            continue;
        }
        measured = view.contributions[index]
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) if context_tokens(&assistant.usage) > 0 => Some(assistant),
                _ => None,
            });
    }
    let (from, mut tokens) = match measured {
        None => (0, 0),
        Some(assistant) => {
            let position = view
                .messages
                .iter()
                .rposition(|message| matches!(message, Message::Assistant(each) if each == assistant))
                .map_or(0, |index| index + 1);
            (position, context_tokens(&assistant.usage))
        }
    };
    for message in view.messages.iter().skip(from).chain(extra) {
        tokens += ai::transcript::estimate_message_tokens(message);
    }
    tokens
}

/// [`estimate_context`] of `view` followed by the messages of the `planned` entries.
pub(crate) fn estimate_planned(view: &ContextView, planned: &[EntryDraft]) -> Result<u64> {
    let mut extra = Vec::new();
    for entry in planned {
        for message in entry.model.iter().flatten() {
            extra.push(super::context::message_of(message.clone())?);
        }
    }
    Ok(estimate_context(view, &extra))
}

/// Which threshold compaction preparation starts (spec §8.3): blocking above `contextWindow - reserveTokens`,
/// background above the background threshold, and only when range selection finds a cut.
pub(crate) fn threshold_compaction(
    view: &ContextView,
    planned: &[EntryDraft],
    context_window: u64,
    policy: &CompactionPolicy,
) -> Result<Option<Threshold>> {
    if !policy.enabled || context_window == 0 {
        return Ok(None);
    }
    let tokens = estimate_planned(view, planned)?;
    let blocking = context_window.saturating_sub(policy.reserve_tokens);
    let background = blocking.saturating_sub(policy.background_tokens);
    let over = if tokens > blocking {
        Some(Threshold::Blocking)
    } else if policy.background_tokens > 0 && tokens > background {
        Some(Threshold::Background)
    } else {
        None
    };
    if over.is_none() || select_cut(view, policy.keep_recent_tokens).is_none() {
        return Ok(None);
    }
    Ok(over)
}

/// Index in `view.entries` of the first entry a summary keeps, or `None` when there is nothing to compact. Walks back
/// from the tail until `keep_recent_tokens` are kept, then cuts at the first candidate at or after that entry: an
/// entry whose contribution starts with a user or assistant message, never a tool result, and never a user entry
/// that a result of the preceding assistant's calls still follows.
pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Option<usize> {
    let contributions = &view.contributions;
    let start = usize::from(view.head.is_some());
    let candidates: Vec<usize> = (start..contributions.len())
        .filter(|index| is_candidate(contributions, *index))
        .collect();
    let mut kept = 0;
    let mut cut = None;
    for index in (start..contributions.len()).rev() {
        kept += contributions[index]
            .iter()
            .map(ai::transcript::estimate_message_tokens)
            .sum::<u64>();
        if kept < keep_recent_tokens {
            continue;
        }
        cut = candidates
            .iter()
            .copied()
            .find(|candidate| *candidate >= index)
            .or_else(|| candidates.last().copied());
        break;
    }
    let cut = cut?;
    (start..cut)
        .any(|index| !contributions[index].is_empty())
        .then_some(cut)
}

fn is_candidate(contributions: &[Vec<Message>], index: usize) -> bool {
    match contributions[index].first() {
        Some(Message::Assistant(_)) => return true,
        Some(Message::User(_)) => {}
        _ => return false,
    }
    // A result of the preceding assistant's calls that follows this entry, before the next assistant, belongs before it.
    let mut calls: Vec<String> = Vec::new();
    for before in (0..index).rev() {
        let assistant = contributions[before].iter().rev().find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        if let Some(assistant) = assistant {
            calls = assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::ToolCall(call) => Some(call.id.clone()),
                    _ => None,
                })
                .collect();
            break;
        }
    }
    if calls.is_empty() {
        return true;
    }
    for (after, messages) in contributions.iter().enumerate().skip(index) {
        for (position, message) in messages.iter().enumerate() {
            match message {
                Message::Assistant(_) if after > index || position > 0 => return true,
                Message::ToolResult(result) if calls.contains(&result.tool_call_id) => return false,
                _ => {}
            }
        }
    }
    true
}

/// What a compaction cutting `view` at `cut` summarizes (`prepareCompaction`): the context before the cut (after the
/// previous compaction's summary, which is updated instead), split at the start of the cut's turn when the cut is
/// inside one, and the files the summarized part and the previous compaction touched.
pub fn summary_plan(view: &ContextView, cut: usize) -> SummaryPlan {
    let previous = view.head.as_ref().filter(|head| head.kind == entries::COMPACTION);
    let previous_summary = previous.and_then(|head| {
        let content = &head.model.as_ref()?.first()?["content"];
        let text = content.as_str().or_else(|| content[0]["text"].as_str())?.to_string();
        let text = text
            .strip_prefix(compaction::COMPACTION_SUMMARY_PREFIX)
            .unwrap_or(&text);
        Some(
            text.strip_suffix(compaction::COMPACTION_SUMMARY_SUFFIX)
                .unwrap_or(text)
                .to_string(),
        )
    });
    let start = usize::from(previous.is_some());
    let cut = cut.max(start);
    // `findTurnStartIndex`: a cut at an answer splits its turn, which starts at the user message before it.
    // A turn starts at a user input (`role === "user"` of a message entry): the positional prompt sections, notes
    // and other custom entries do not start one (`findTurnStartIndex`).
    let starts_turn = |index: usize| {
        view.entries
            .get(index)
            .is_some_and(|entry| entry.kind == entries::USER || entry.kind == entries::RESET)
    };
    let turn_start = if cut >= view.contributions.len() || starts_turn(cut) {
        None
    } else {
        (start..cut).rev().find(|index| starts_turn(*index))
    };
    let values = |range: std::ops::Range<usize>| -> Vec<Value> {
        order_tool_results(&view.contributions[range].concat())
            .iter()
            .filter(|message| !matches!(message, Message::System(_)))
            .map(|message| strict(to_value(message)))
            .collect()
    };
    let (history, prefix) = match turn_start {
        Some(turn) => (values(start..turn), values(turn..cut)),
        None => (values(start..cut), Vec::new()),
    };
    let details = previous
        .and_then(|head| head.data.as_ref())
        .map(|data| &data["details"]);
    let file_ops =
        super::child_summary::file_operations(details, &history.iter().chain(prefix.iter()).collect::<Vec<_>>());
    SummaryPlan {
        split: turn_start.is_some(),
        history,
        prefix,
        previous_summary,
        file_ops,
    }
}

/// Model messages of the entries before `cut`, ordered like model context.
pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Message> {
    order_tool_results(&view.contributions[..cut].concat())
}

/// The built-in compaction task over `services`.
pub(crate) fn compaction_task(services: Arc<Services>) -> Task {
    let (select, summarize, retry) = (services.clone(), services.clone(), services);
    Task::define(COMPACTION_KIND, 1, |input| (DEFINITION.initial)(input))
        .phase("select", move |task, runtime| select_phase(&select, task, runtime))
        .phase("summarize", move |task, runtime| {
            summarize_phase(&summarize, task, runtime)
        })
        .phase("retry", move |task, runtime| retry_phase(&retry, task, runtime))
        .abort(|_, runtime| {
            let conversation_id = runtime.conversation_id();
            let task_id = runtime.task_id();
            runtime.commit(&runtime.ctx(), |tx, _| {
                remove_compaction_status(&live(tx, conversation_id)?, task_id)?;
                Ok(Some(NextState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            })
        })
        .build()
}

fn checkpoint_of(task: &TaskRecord) -> Result<CompactionCheckpoint> {
    match &task.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => from_value(checkpoint.clone(), "compaction checkpoint"),
        _ => Err(HarnessError::msg("A terminal compaction has no checkpoint")),
    }
}

fn finish(runtime: &TaskRuntime, outcome: TaskOutcome) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        remove_compaction_status(&live(tx, conversation_id)?, task_id)?;
        Ok(Some(NextState::Terminal { outcome }))
    })
}

fn fail(message: String, reason: &str) -> TaskOutcome {
    TaskOutcome::Failed {
        error: TaskOutcomeError {
            message,
            detail: Some(json!({ "reason": reason })),
        },
        result: None,
    }
}

fn select_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let agent = read_agent(runtime)?;
    let settings = (services.settings)();
    let model = agent
        .model
        .as_ref()
        .and_then(|model| services.models.get_model(&model.provider, &model.model_id));
    let (Some(model_ref), Some(_)) = (agent.model.clone(), model) else {
        let message = match &agent.model {
            None => "No model is configured".to_owned(),
            Some(model) => format!("Model {}/{} is not available", model.provider, model.model_id),
        };
        return finish(runtime, fail(message, "no_model"));
    };
    let view = runtime.read(|tx| read_context(tx, conversation_id, None))?;
    let policy = services.compaction_policy(conversation_id, &settings);
    let Some(cut) = select_cut(&view, policy.keep_recent_tokens) else {
        return finish(runtime, TaskOutcome::Completed { result: json!({}) });
    };
    let first_kept = view.entries[cut].id;
    let tail = view.entries.iter().map(|entry| entry.id).max().unwrap_or(first_kept);
    let request = SummaryRequest {
        attempt: 1,
        model: model_ref,
        thinking_level: agent.thinking_level,
        stream_options: settings.stream.clone(),
        tail,
        first_kept,
    };
    let _ = task;
    runtime.commit(&runtime.ctx(), |_, _| {
        Ok(Some(NextState::Running {
            checkpoint: to_value(&CompactionCheckpoint::Summarize { request }),
        }))
    })
}

/// The summary of a clean `stop` with text and no tool call.
fn summary_text(message: &AssistantMessage) -> Option<String> {
    if message.stop_reason != StopReason::Stop
        || message
            .content
            .iter()
            .any(|content| matches!(content, AssistantContent::ToolCall(_)))
    {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    (!text.is_empty()).then_some(text)
}

fn summary_failure(message: &AssistantMessage, label: &str) -> String {
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => format!(
            "{label} failed: {}",
            message
                .error_message
                .clone()
                .unwrap_or_else(|| to_value(&message.stop_reason).as_str().unwrap_or_default().to_owned())
        ),
        StopReason::Length => format!("{label} hit the token limit; the summary is incomplete"),
        _ if message
            .content
            .iter()
            .any(|content| matches!(content, AssistantContent::ToolCall(_))) =>
        {
            format!("{label} attempted to call a tool")
        }
        _ => format!("{label} produced no text"),
    }
}

fn summarize_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let CompactionCheckpoint::Summarize { request } = checkpoint_of(task)? else {
        return Err(HarnessError::msg("summarize phase without a summary request"));
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let Some(model) = services
        .models
        .get_model(&request.model.provider, &request.model.model_id)
    else {
        let message = format!(
            "Model {}/{} is not available",
            request.model.provider, request.model.model_id
        );
        return finish(runtime, fail(message, "no_model"));
    };
    // The context at `tail` is immutable, so this is the range `select` chose.
    let view = runtime.read(|tx| read_context(tx, conversation_id, Some(request.tail)))?;
    let cut = view
        .entries
        .iter()
        .position(|entry| entry.id == request.first_kept)
        .unwrap_or(0);
    let plan = summary_plan(&view, cut);
    let session_id = ensure_provider_session_id(runtime)?;
    let settings = (services.settings)();
    let instructions = task.input["instructions"].as_str();
    let options = compaction::CompactOptions {
        model: &model,
        custom_instructions: instructions,
        thinking_level: Some(request.thinking_level.as_str()),
        session_id: Some(session_id.as_str()),
        now_ms: runtime.now()? as i64,
    };
    let reserve = services
        .compaction_policy(runtime.conversation_id(), &settings)
        .reserve_tokens;
    // The history summary, then a split turn's prefix summary; the first response that is no summary ends them.
    let mut texts = Vec::new();
    let mut responses = Vec::new();
    let mut message = None;
    let requests = super::child_summary::summary_requests(&plan, reserve, &options);
    // A split turn's last request is its prefix summary (`Turn prefix summarization failed: ...`).
    let prefix_request = (plan.split && !plan.prefix.is_empty()).then(|| requests.len() - 1);
    for summary_request in requests {
        let mut stream_options = request.stream_options.clone();
        stream_options.remove("deferred");
        stream_options.insert("cacheRetention".into(), json!("none"));
        stream_options.insert("maxTokens".into(), json!(summary_request.max_tokens));
        let stream_request = StreamRequest {
            options: stream_options,
            session_id: summary_request.session_id.clone(),
            reasoning: summary_request.reasoning.clone(),
            conversation_id: Some(runtime.conversation_id()),
            final_note: None,
        };
        let response = services.models.stream(
            &model,
            &summary_request.context,
            &stream_request,
            runtime.signal(),
            &mut |_| {},
        )?;
        // An abort mark or close: the abort invocation or the reopened task handles the committed state.
        runtime.signal().check()?;
        let text = summary_text(&response);
        responses.push((
            to_value(&response.usage),
            format!("{}/{}", response.provider, response.model),
        ));
        let failed = text.is_none();
        texts.extend(text);
        message = Some(response);
        if failed {
            break;
        }
    }
    let Some(message) = message else {
        return Err(HarnessError::msg("compaction made no summary request"));
    };
    let summary = (texts.len() == responses.len()).then(|| super::child_summary::compose_summary(&plan, &texts));
    let policy = settings.retry;
    let retry = message.stop_reason == StopReason::Error
        && is_retryable(&message)
        && policy.enabled
        && request.attempt <= policy.max_retries;
    let until = if retry {
        runtime.now()? + policy.delay_ms(request.attempt)
    } else {
        0
    };
    let modes = settings.queue_modes();
    runtime.commit(&runtime.ctx(), |tx, current| {
        for (usage, key) in &responses {
            record_usage(tx, conversation_id, Bucket::Models, key, usage)?;
        }
        let live = live(tx, conversation_id)?;
        if let Some((summary, read, modified)) = &summary {
            let details = json!({ "readFiles": read, "modifiedFiles": modified });
            return place_summary(
                tx,
                runtime,
                current,
                &live,
                request.first_kept,
                (summary, &details),
                modes,
            );
        }
        if retry {
            if let Some(index) = compaction_status_index(&live, task_id)? {
                let path = vec![
                    chord_delta::Seg::from("compactions"),
                    chord_delta::Seg::from(index),
                    chord_delta::Seg::from("retry"),
                ];
                d(live.set(
                    &path,
                    json!({ "at": until, "error": message.error_message.clone().unwrap_or_default() }),
                ))?;
            }
            return Ok(Some(NextState::Running {
                checkpoint: to_value(&CompactionCheckpoint::Retry {
                    until,
                    request: request.clone(),
                }),
            }));
        }
        remove_compaction_status(&live, task_id)?;
        Ok(Some(NextState::Terminal {
            outcome: fail(
                summary_failure(
                    &message,
                    if prefix_request == Some(responses.len() - 1) {
                        "Turn prefix summarization"
                    } else {
                        "Summarization"
                    },
                ),
                "model_error",
            ),
        }))
    })
}

fn retry_phase(_services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let CompactionCheckpoint::Retry { until, request } = checkpoint_of(task)? else {
        return Err(HarnessError::msg("retry phase without a retry checkpoint"));
    };
    runtime.sleep(until, &runtime.ctx())?;
    let attempt = request.attempt + 1;
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        if let Some(index) = compaction_status_index(&live, task_id)? {
            let status = vec![chord_delta::Seg::from("compactions"), chord_delta::Seg::from(index)];
            let mut attempt_path = status.clone();
            attempt_path.push(chord_delta::Seg::from("attempt"));
            d(live.set(&attempt_path, json!(attempt)))?;
            let mut retry_path = status;
            retry_path.push(chord_delta::Seg::from("retry"));
            d(live.delete(&retry_path))?;
        }
        Ok(Some(NextState::Running {
            checkpoint: to_value(&CompactionCheckpoint::Summarize {
                request: SummaryRequest { attempt, ..request },
            }),
        }))
    })
}

/// Place the summary entry and complete. A blocking compaction, owned by its generation, appends it; a
/// conversation-owned one admits it as a write submission, placed at once when idle, otherwise at the next boundary,
/// or settled `stale`.
fn place_summary(
    tx: &mut Tx<'_>,
    runtime: &TaskRuntime,
    current: &TaskRecord,
    live: &chord_delta::Draft,
    first_kept: EntryId,
    (summary, details): (&str, &Value),
    modes: super::inbox::QueueModes,
) -> Result<Option<NextState>> {
    let conversation_id = runtime.conversation_id();
    remove_compaction_status(live, runtime.task_id())?;
    let text = format!(
        "{}{summary}{}",
        compaction::COMPACTION_SUMMARY_PREFIX,
        compaction::COMPACTION_SUMMARY_SUFFIX
    );
    let now = runtime.now()?;
    let reason = current.input["reason"].clone();
    let result = match current.owner {
        None => {
            let entry = json!({
                "kind": entries::COMPACTION,
                "head": first_kept.0,
                "model": [{ "role": "user", "content": [{ "type": "text", "text": text }], "timestamp": now }],
                "data": { "reason": reason, "details": details },
            });
            let draft = SubmissionDraft::write(entry).request_id(&format!("compaction:{}", runtime.task_id().0));
            let id = admit_submission(tx, conversation_id, &draft, now, modes)?;
            json!({ "submissionId": id.0 })
        }
        Some(_) => {
            let entry = tx.append_entry(
                conversation_id,
                EntryDraft {
                    head: Some(EntryHead::Entry(first_kept)),
                    model: Some(vec![
                        json!({ "role": "user", "content": [{ "type": "text", "text": text }], "timestamp": now }),
                    ]),
                    data: Some(json!({ "reason": reason, "details": details })),
                    ..EntryDraft::new(entries::COMPACTION)
                },
            )?;
            json!({ "entryId": entry.id.0 })
        }
    };
    Ok(Some(completed(result)))
}
