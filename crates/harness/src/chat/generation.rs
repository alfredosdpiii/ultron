//! The built-in generation task `pi.generation` (spec §8.3, §8.5): prepare the positional system prompt and the REPL
//! tool, request or poll the model with options pinned at preparation, retry with durable backoff, compact once on a
//! context overflow, classify the response, and run the tool round of REPL cells. Port of upstream
//! `harness/generation.ts`; the tool list is exactly the cell executor's REPL tool.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, Message, StopReason, ToolCall};
use durable::{
    ConversationId, EntryDraft, EntryHead, EntryId, EntryQuery, JoinPolicy, SubmissionId, SubmissionSettlement,
    TaskDefinition, TaskId, TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskRecord, TaskState, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::Services;
use super::cell::{CellResult, append_tool_result, cell_definition};
use super::compaction::{Threshold, create_compaction, estimate_planned, select_cut, threshold_compaction};
use super::context::read_context;
use super::docs::{AGENT, LIVE, PROVIDER};
use super::entries;
use super::inbox::{At, apply_boundary, prepare_boundary, user_entry};
use super::live::{GENERATION_KIND, append_assistant, convert_partial, end_run, hand_over, live, run_task};
use super::models::{ModelRef, StreamRequest, is_context_overflow, is_retryable};
use super::prompt::{plan_system_entries, replay_sections};
use super::throttle::Throttle;
use super::util::{delete, from_value, get, owner, set, strict, to_value};
use crate::scheduler::{HarnessError, NextState, Result, Task, TaskRuntime, completed};

const DEFAULT_POLL_AFTER_MS: u64 = 5000;

/// The generation's checkpoint (spec §8.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GenerationCheckpoint {
    Prepare {
        attempt: u64,
        /// The blocking compaction this generation waited for; it starts no other compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Error text of the overflow that started `compacted`; checked once when `prepare` resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
    },
    Request {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        thinking_level: String,
        /// The settings' request options when preparation committed; a resend after recovery uses them unchanged.
        stream_options: Map<String, Value>,
        /// Newest entry included in the request.
        cutoff: EntryId,
    },
    Retry {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        until: u64,
    },
    Poll {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        cutoff: EntryId,
        handle: ai::DeferredHandle,
        poll_at: u64,
    },
    /// Waiting on the round's cell tasks, which the generation owns.
    Tools {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Cell tasks created so far, in call order.
        tools: Vec<TaskId>,
        /// Calls of a sequential round not started yet, in call order.
        pending: Vec<String>,
    },
}

fn checkpoint(value: GenerationCheckpoint) -> Value {
    to_value(&value)
}

/// The Session-facing definition: version 1, starting at `{ phase: "prepare", attempt: 1 }`.
pub(crate) static GENERATION_DEFINITION: LazyLock<TaskDefinition> = LazyLock::new(|| {
    TaskDefinition::new(GENERATION_KIND, 1, |_| {
        checkpoint(GenerationCheckpoint::Prepare {
            attempt: 1,
            compacted: None,
            overflow: None,
        })
    })
});

/// A generation owned by its conversation.
pub(crate) fn create_generation(tx: &mut Tx<'_>, conversation_id: ConversationId) -> Result<TaskId> {
    Ok(tx.create_task(
        &GENERATION_DEFINITION,
        json!({}),
        TaskOptions {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation_id),
            ..TaskOptions::default()
        },
    )?)
}

/// Start a run for `inputs`, placed input submissions: a new generation takes `pi.live.run`.
pub(crate) fn start_run(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    live: &chord_delta::Draft,
    inputs: Vec<SubmissionId>,
) -> Result<()> {
    let task_id = create_generation(tx, conversation_id)?;
    let inputs: Vec<u64> = inputs.into_iter().map(|id| id.0).collect();
    set(live, &["run"], json!({ "taskId": task_id.0, "inputs": inputs }))
}

/// The agent a phase resolves: the conversation's model and thinking level.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub model: Option<ModelRef>,
    pub thinking_level: String,
}

pub(crate) fn read_agent(runtime: &TaskRuntime) -> Result<Agent> {
    let state = runtime.snapshot(&AGENT, owner(runtime.conversation_id()))?;
    let state = state.map(|state| (*state).clone()).unwrap_or(json!({}));
    let model = match state.get("model") {
        Some(model) if !model.is_null() => Some(from_value::<ModelRef>(model.clone(), "pi.agent model")?),
        _ => None,
    };
    Ok(Agent {
        model,
        thinking_level: state["thinkingLevel"].as_str().unwrap_or("off").to_owned(),
    })
}

/// The persisted provider session ID; a legacy conversation without `pi.provider` gets one in its own commit.
pub(crate) fn ensure_provider_session_id(runtime: &TaskRuntime) -> Result<String> {
    let conversation_id = runtime.conversation_id();
    if let Some(state) = runtime.snapshot(&PROVIDER, owner(conversation_id))? {
        return Ok(state["sessionId"].as_str().unwrap_or_default().to_owned());
    }
    let created = Arc::new(Mutex::new(String::new()));
    let slot = created.clone();
    runtime.commit(&runtime.ctx(), move |tx, _| {
        let provider = tx.doc(&PROVIDER, owner(conversation_id))?;
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = get(&provider, &["sessionId"])?
            .and_then(|id| id.as_str().map(str::to_owned))
            .unwrap_or_default();
        Ok(None)
    })?;
    let id = created.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    Ok(id)
}

fn running(value: GenerationCheckpoint) -> Option<NextState> {
    Some(NextState::Running {
        checkpoint: checkpoint(value),
    })
}

fn failed(message: &str, reason: &str) -> Option<NextState> {
    Some(NextState::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: message.to_owned(),
                detail: Some(json!({ "reason": reason })),
            },
            result: None,
        },
    })
}

/// The built-in generation task over `services`.
pub(crate) fn generation_task(services: Arc<Services>) -> Task {
    let (prepare, request, retry, poll, tools, abort) = (
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services,
    );
    Task::define(GENERATION_KIND, 1, |input| (GENERATION_DEFINITION.initial)(input))
        .phase("prepare", move |task, runtime| prepare_phase(&prepare, task, runtime))
        .phase("request", move |task, runtime| request_phase(&request, task, runtime))
        .phase("retry", move |task, runtime| retry_phase(&retry, task, runtime))
        .phase("poll", move |task, runtime| poll_phase(&poll, task, runtime))
        .phase("tools", move |task, runtime| tools_phase(&tools, task, runtime))
        .abort(move |task, runtime| abort_handler(&abort, task, runtime))
        .build()
}

pub(crate) fn current_checkpoint(task: &TaskRecord) -> Result<GenerationCheckpoint> {
    let value = match &task.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => checkpoint.clone(),
        _ => return Err(HarnessError::msg("A terminal generation has no checkpoint")),
    };
    from_value(value, "generation checkpoint")
}

/// Settle the run's inputs `unanswered` with `model_error` and fail with `text`.
fn fail_model_error(runtime: &TaskRuntime, text: &str) -> Result<()> {
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "model_error".into(),
                detail: Some(json!(text)),
            },
        )?;
        Ok(failed(text, "model_error"))
    })
}

/// Settle the run's inputs `unanswered` with `no_model` and fail.
fn fail_no_model(runtime: &TaskRuntime, model: Option<&ModelRef>) -> Result<()> {
    let message = match model {
        None => "No model is configured".to_owned(),
        Some(model) => format!("Model {}/{} is not available", model.provider, model.model_id),
    };
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "no_model".into(),
                detail: None,
            },
        )?;
        Ok(failed(&message, "no_model"))
    })
}

fn prepare_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Prepare {
        attempt,
        compacted,
        overflow,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("prepare phase without a prepare checkpoint"));
    };
    let conversation_id = runtime.conversation_id();
    let agent = read_agent(runtime)?;
    let settings = (services.settings)();
    let Some(model_ref) = agent.model.clone() else {
        return fail_no_model(runtime, None);
    };
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(runtime, Some(&model_ref));
    };
    if let (Some(compacted), Some(overflow)) = (compacted, overflow.as_deref()) {
        let outcomes = runtime.outcomes(&[compacted])?;
        let summarized =
            matches!(outcomes.first(), Some(TaskOutcome::Completed { result }) if result.get("entryId").is_some());
        if !summarized {
            return fail_model_error(runtime, overflow);
        }
    }
    let view = runtime.read(|tx| read_context(tx, conversation_id, None))?;
    let shown = replay_sections(&view.messages);
    let desired = match &services.sections {
        None => Default::default(),
        Some(sections) => match sections(conversation_id, runtime.signal()) {
            Ok(desired) => desired,
            Err(error) => {
                runtime.signal().check()?;
                runtime.report(&error)?;
                shown
            }
        },
    };
    let tools = vec![services.cells.tool()];
    let planned = plan_system_entries(&view, &desired, &tools, runtime.now()?);
    let policy = services.compaction_policy(conversation_id, &settings);
    let mut threshold = match compacted {
        Some(_) => None,
        None => threshold_compaction(&view, &planned, model.context_window, &policy)?,
    };
    // `before_compaction`: entries the hook gives decline a blocking threshold compaction (placed with this request).
    let mut declined: Vec<EntryDraft> = Vec::new();
    if threshold == Some(Threshold::Blocking)
        && let Some(hook) = &services.hooks.before_compaction
    {
        let tokens = estimate_planned(&view, &planned)?;
        let limit = model.context_window.saturating_sub(policy.reserve_tokens);
        for entry in hook(conversation_id, tokens, limit) {
            declined.push(super::inbox::entry_draft(&entry)?);
        }
        if !declined.is_empty() {
            threshold = None;
        }
    }
    let task_id = runtime.task_id();
    if threshold == Some(Threshold::Blocking) {
        // Compact first and prepare again; the transcript is unchanged until the compaction appends.
        return runtime.commit(&runtime.ctx(), |tx, _| {
            let child = create_compaction(tx, conversation_id, "threshold", None, Some(task_id))?;
            Ok(Some(NextState::Waiting {
                checkpoint: checkpoint(GenerationCheckpoint::Prepare {
                    attempt,
                    compacted: Some(child),
                    overflow: None,
                }),
                on: vec![child],
                policy: JoinPolicy::AllSettled,
            }))
        });
    }
    runtime.commit(&runtime.ctx(), |tx, _| {
        let query = EntryQuery {
            conversation_id,
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        };
        let mut cutoff = tx.scan_entries(&query, 1, None)?.items.first().map(|entry| entry.id);
        for entry in declined {
            cutoff = Some(tx.append_entry(conversation_id, entry)?.id);
        }
        for entry in planned {
            cutoff = Some(tx.append_entry(conversation_id, entry)?.id);
        }
        let Some(cutoff) = cutoff else {
            return Err(HarnessError::Error(format!(
                "Conversation {} has no entries to send",
                conversation_id.0
            )));
        };
        // Checked in this commit, so a compaction admitted during preparation counts.
        if threshold == Some(Threshold::Background) && get(&live(tx, conversation_id)?, &["compactions"])?.is_none() {
            create_compaction(tx, conversation_id, "threshold", None, None)?;
        }
        Ok(running(GenerationCheckpoint::Request {
            attempt,
            compacted,
            model: model_ref.clone(),
            thinking_level: agent.thinking_level.clone(),
            stream_options: settings.stream.clone(),
            cutoff,
        }))
    })
}

/// What classification needs from the request that produced a message.
struct Request {
    attempt: u64,
    compacted: Option<TaskId>,
    model: ModelRef,
    cutoff: EntryId,
    /// Committed model context through `cutoff`, when the phase already derived it.
    messages: Option<Vec<Message>>,
    /// Set when the message came from polling, so a still deferred result polls strictly later.
    poll_at: Option<u64>,
}

fn request_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Request {
        attempt,
        compacted,
        model: model_ref,
        thinking_level,
        stream_options,
        cutoff,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("request phase without a request checkpoint"));
    };
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        convert_partial(tx, &live, conversation_id)?;
        set(&live, &["generation"], json!({ "attempt": attempt }))?;
        Ok(None)
    })?;
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(runtime, Some(&model_ref));
    };
    let view = runtime.read(|tx| read_context(tx, conversation_id, Some(cutoff)))?;
    let mut messages = view.messages.clone();
    if let Some(hook) = &services.hooks.before_request {
        messages = hook(messages);
    }
    let request = StreamRequest {
        options: stream_options,
        session_id: Some(ensure_provider_session_id(runtime)?),
        reasoning: (thinking_level != "off").then_some(thinking_level),
        conversation_id: Some(conversation_id),
    };
    let message = stream_response(services, runtime, &model, messages, &request, attempt)?;
    let request = Request {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        messages: Some(view.messages),
        poll_at: None,
    };
    classify(services, runtime, request, message)
}

fn partial_of(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// Stream one request and return the terminal message. Partials commit as throttled writes with one commit in
/// flight; the throttle stops and awaits that commit before returning, so no stale partial lands after the outcome.
fn stream_response(
    services: &Services,
    runtime: &TaskRuntime,
    model: &ai::Model,
    messages: Vec<Message>,
    request: &StreamRequest,
    attempt: u64,
) -> Result<AssistantMessage> {
    let interval = Duration::from_millis((services.settings)().partial_interval_ms);
    let committer = runtime.clone();
    let throttle = Throttle::new(interval, move |partial: Value| {
        let conversation_id = committer.conversation_id();
        let result = committer.commit(&committer.ctx(), |tx, _| {
            let live = live(tx, conversation_id)?;
            if get(&live, &["generation"])?.is_none() {
                set(&live, &["generation"], json!({ "attempt": attempt }))?;
            }
            set(&live, &["generation", "message"], partial)?;
            Ok(None)
        });
        // Rejections after an abort mark or close are expected; the committed state stays consistent.
        if let Err(error) = result
            && !committer.signal().is_cancelled()
        {
            let _ = committer.report(&error);
        }
    });
    let context = ai::TranscriptContext { messages };
    let mut on_event = |event: &AssistantMessageEvent| {
        // A partial without content, such as the opening `start` event, shows nothing; a deferred response never gets
        // past it, so it never leaves a partial.
        if let Some(partial) = partial_of(event)
            && !partial.content.is_empty()
        {
            throttle.mark(strict(to_value(partial)));
        }
    };
    let result = services
        .models
        .stream(model, &context, request, runtime.signal(), &mut on_event);
    throttle.stop();
    // An abort mark or close: the abort invocation or the reopened run handles the committed state.
    runtime.signal().check()?;
    result
}

fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn stop_reason_name(reason: StopReason) -> String {
    to_value(&reason).as_str().unwrap_or_default().to_owned()
}

/// Classify a terminal provider message in one commit that also clears the partial.
fn classify(services: &Services, runtime: &TaskRuntime, request: Request, message: AssistantMessage) -> Result<()> {
    runtime.signal().check()?;
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let Request {
        attempt,
        compacted,
        cutoff,
        ..
    } = request;
    if message.stop_reason == StopReason::Deferred
        && let Some(handle) = message.deferred.clone()
    {
        let after = handle
            .poll_after_ms
            .and_then(|ms| u64::try_from(ms).ok())
            .unwrap_or(DEFAULT_POLL_AFTER_MS);
        let poll_at = (runtime.now()? + after).max(request.poll_at.map_or(0, |previous| previous + 1));
        let model = request.model.clone();
        return runtime.commit(&runtime.ctx(), |tx, _| {
            let live = live(tx, conversation_id)?;
            set(
                &live,
                &["generation"],
                json!({ "attempt": attempt, "deferred": { "pollAt": poll_at } }),
            )?;
            Ok(running(GenerationCheckpoint::Poll {
                attempt,
                compacted,
                model,
                cutoff,
                handle,
                poll_at,
            }))
        });
    }
    if let Some(hook) = &services.hooks.after_response {
        hook(&message);
    }
    let calls = tool_calls(&message);
    if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
        return start_tool_round(services, runtime, request, message, calls);
    }
    if matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length | StopReason::ToolUse
    ) {
        return answer(services, runtime, message);
    }
    // The retry and compaction policies govern the next attempt, so they are read now.
    let settings = (services.settings)();
    let overflow = message.stop_reason == StopReason::Error && is_context_overflow(&message);
    let stored = strict(to_value(&message));
    let policy = services.compaction_policy(conversation_id, &settings);
    if overflow && compacted.is_none() && policy.enabled {
        let view = runtime.read(|tx| read_context(tx, conversation_id, Some(cutoff)))?;
        if select_cut(&view, policy.keep_recent_tokens).is_some() {
            let text = message
                .error_message
                .clone()
                .unwrap_or_else(|| "Context overflow".into());
            return runtime.commit(&runtime.ctx(), |tx, _| {
                let live = live(tx, conversation_id)?;
                append_assistant(tx, conversation_id, &stored)?;
                delete(&live, &["generation"])?;
                let child = create_compaction(tx, conversation_id, "overflow", None, Some(task_id))?;
                Ok(Some(NextState::Waiting {
                    checkpoint: checkpoint(GenerationCheckpoint::Prepare {
                        attempt,
                        compacted: Some(child),
                        overflow: Some(text.clone()),
                    }),
                    on: vec![child],
                    policy: JoinPolicy::AllSettled,
                }))
            });
        }
    }
    let policy = settings.retry;
    // An overflow is never retried: only a compaction can make the next request fit.
    let retry = message.stop_reason == StopReason::Error
        && !overflow
        && is_retryable(&message)
        && policy.enabled
        && attempt <= policy.max_retries;
    let until = if retry {
        runtime.now()? + policy.delay_ms(attempt)
    } else {
        0
    };
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        append_assistant(tx, conversation_id, &stored)?;
        if retry {
            let error = message.error_message.clone().unwrap_or_default();
            set(
                &live,
                &["generation"],
                json!({ "attempt": attempt, "retry": { "at": until, "error": error } }),
            )?;
            return Ok(running(GenerationCheckpoint::Retry {
                attempt,
                compacted,
                until,
            }));
        }
        let text = message.error_message.clone().unwrap_or_else(|| {
            format!(
                "Model response ended with stop reason {}",
                stop_reason_name(message.stop_reason)
            )
        });
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "model_error".into(),
                detail: Some(json!(text)),
            },
        )?;
        Ok(failed(&text, "model_error"))
    })
}

/// A final answer; the final boundary places queued items (spec §6). The first `on_yield` continuation appends a
/// user message and hands the run to a successor generation, but only when the boundary selected no user item and no
/// reset. Otherwise the run's inputs settle `done`, and selected user items start the next run.
fn answer(services: &Services, runtime: &TaskRuntime, message: AssistantMessage) -> Result<()> {
    let continuation = services.hooks.on_yield.as_ref().and_then(|hook| hook(&message));
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let stored = strict(to_value(&message));
    runtime.commit(&runtime.ctx(), |tx, _| {
        // Queue modes are read on the Session line, when the boundary is decided.
        let mut boundary = prepare_boundary(tx, conversation_id, (services.settings)().queue_modes())?;
        let live = live(tx, conversation_id)?;
        let entry = append_assistant(tx, conversation_id, &stored)?;
        let result = completed(json!({ "entryId": entry.id.0 }));
        let now = runtime.now()?;
        let placed = apply_boundary(tx, &mut boundary, At::Final, now)?;
        if let Some(content) = continuation.as_ref()
            && placed.users.is_empty()
            && !placed.reset
        {
            tx.append_entry(conversation_id, user_entry(content, now))?;
            let successor = create_generation(tx, conversation_id)?;
            hand_over(&live, task_id, successor)?;
            delete(&live, &["generation"])?;
            return Ok(Some(result));
        }
        end_run(tx, &live, task_id, SubmissionSettlement::Done { answer: entry.id })?;
        if !placed.users.is_empty() {
            start_run(tx, conversation_id, &live, placed.users)?;
        }
        Ok(Some(result))
    })
}

/// A cell task for call `call_id`, owned by the generation.
fn create_cell_task(tx: &mut Tx<'_>, generation: TaskId, assistant: EntryId, call_id: &str) -> Result<TaskId> {
    Ok(tx.create_task(
        cell_definition(),
        json!({ "kind": "model", "assistant": assistant.0, "callId": call_id }),
        TaskOptions {
            ownership: TaskOwnership::Task(generation),
            ..TaskOptions::default()
        },
    )?)
}

/// Append the tool-calling answer and start its tool round in one commit (spec §8.3). A call to a tool the request did
/// not offer gets its `tool_unavailable` result here; every other call gets a cell task owned by the generation, only
/// the first one now when the round is sequential. The generation then waits for them in its `tools` phase.
fn start_tool_round(
    services: &Services,
    runtime: &TaskRuntime,
    request: Request,
    message: AssistantMessage,
    calls: Vec<ToolCall>,
) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let messages = match request.messages {
        Some(messages) => messages,
        None => {
            runtime
                .read(|tx| read_context(tx, conversation_id, Some(request.cutoff)))?
                .messages
        }
    };
    let offered: Vec<String> = ai::transcript::current_tools(&messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let sequential = (services.settings)().tool_execution == super::models::ToolExecution::Sequential;
    let stored = strict(to_value(&message));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        let entry = append_assistant(tx, conversation_id, &stored)?;
        let mut slots = Vec::new();
        let mut tools = Vec::new();
        let mut pending = Vec::new();
        for call in &calls {
            if !offered.contains(&call.name) {
                let unavailable =
                    CellResult::error("tool_unavailable", &format!("Tool {} is not available", call.name));
                let result = append_tool_result(
                    tx,
                    conversation_id,
                    &call.id,
                    &call.name,
                    &unavailable,
                    runtime.now()?,
                    None,
                )?;
                slots.push(json!({ "callId": call.id, "name": call.name, "status": "done", "entry": result.id.0 }));
                continue;
            }
            if sequential && !tools.is_empty() {
                pending.push(call.id.clone());
                slots.push(json!({ "callId": call.id, "name": call.name, "status": "pending" }));
                continue;
            }
            let cell = create_cell_task(tx, task_id, entry.id, &call.id)?;
            tools.push(cell);
            slots.push(json!({ "callId": call.id, "name": call.name, "taskId": cell.0, "status": "pending" }));
        }
        delete(&live, &["generation"])?;
        set(&live, &["tools"], Value::Array(slots))?;
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Tools {
                assistant: entry.id,
                tools: tools.clone(),
                pending,
            }),
            on: tools,
            policy: JoinPolicy::AllSettled,
        }))
    })
}

fn tools_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Tools {
        assistant,
        tools,
        pending,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("tools phase without a tools checkpoint"));
    };
    let Some((next, rest)) = pending.split_first() else {
        return finish_tool_round(services, runtime, assistant, tools);
    };
    // Sequential round: start the next call and wait for it.
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        let cell = create_cell_task(tx, task_id, assistant, next)?;
        let slots = get(&live, &["tools"])?.unwrap_or(json!([]));
        if let Some(index) = slots.as_array().and_then(|slots| {
            slots
                .iter()
                .position(|slot| slot["callId"] == json!(next) && slot.get("taskId").is_none())
        }) {
            super::util::d(live.set(&super::live::slot_path(index, Some("taskId")), json!(cell.0)))?;
        }
        let mut started = tools.clone();
        started.push(cell);
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Tools {
                assistant,
                tools: started,
                pending: rest.to_vec(),
            }),
            on: vec![cell],
            policy: JoinPolicy::AllSettled,
        }))
    })
}

/// The control a cell asked for: end the run (`terminate`), or start a fresh context with `handoff` text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Control {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

/// The round's cells are terminal: apply their controls and either end the run at the final boundary (`terminate`,
/// `handoff`, or a queued reset) or hand it to the next generation at the `postTools` boundary (spec §8.5).
fn finish_tool_round(services: &Services, runtime: &TaskRuntime, assistant: EntryId, tools: Vec<TaskId>) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let outcomes = runtime.outcomes(&tools)?;
    let controls: Vec<(TaskId, Option<Control>)> = tools
        .iter()
        .zip(outcomes)
        .map(|(id, outcome)| {
            let control = match outcome {
                TaskOutcome::Completed { result } if result["kind"] != "nested" => result
                    .get("control")
                    .and_then(|control| serde_json::from_value(control.clone()).ok()),
                _ => None,
            };
            (*id, control)
        })
        .collect();
    let slots = runtime
        .snapshot(&LIVE, owner(conversation_id))?
        .and_then(|live| live.get("tools").cloned())
        .unwrap_or(json!([]));
    let slots = slots.as_array().cloned().unwrap_or_default();
    let control_of = |id: TaskId| {
        controls
            .iter()
            .find(|(each, _)| *each == id)
            .and_then(|(_, control)| control.clone())
    };
    let terminate = !slots.is_empty()
        && slots.iter().all(|slot| {
            slot["taskId"]
                .as_u64()
                .is_some_and(|id| control_of(TaskId(id)).is_some_and(|control| control.terminate == Some(true)))
        });
    // The last handoff in call order wins.
    let handoff = controls
        .iter()
        .rev()
        .find_map(|(_, control)| control.as_ref().and_then(|control| control.handoff.clone()));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let mut boundary = prepare_boundary(tx, conversation_id, (services.settings)().queue_modes())?;
        let live = live(tx, conversation_id)?;
        let now = runtime.now()?;
        if terminate || handoff.is_some() {
            if let Some(text) = &handoff {
                let entry = tx.append_entry(
                    conversation_id,
                    EntryDraft {
                        head: Some(EntryHead::SelfId),
                        model: Some(vec![json!({ "role": "user", "content": text, "timestamp": now })]),
                        ..EntryDraft::new(entries::RESET)
                    },
                )?;
                boundary.head = Some(entry.id);
            }
            let placed = apply_boundary(tx, &mut boundary, At::Final, now)?;
            end_run(tx, &live, task_id, SubmissionSettlement::Done { answer: assistant })?;
            if !placed.users.is_empty() {
                start_run(tx, conversation_id, &live, placed.users)?;
            }
        } else {
            let placed = apply_boundary(tx, &mut boundary, At::PostTools, now)?;
            if placed.reset {
                // The queued reset cut the run's context before an answer.
                end_run(
                    tx,
                    &live,
                    task_id,
                    SubmissionSettlement::Unanswered {
                        reason: "reset".into(),
                        detail: None,
                    },
                )?;
                if !placed.users.is_empty() {
                    start_run(tx, conversation_id, &live, placed.users)?;
                }
            } else {
                delete(&live, &["tools"])?;
                delete(&live, &["nestedTools"])?;
                if run_task(&live)? == Some(task_id) {
                    for user in &placed.users {
                        super::util::d(live.push(&super::util::keys(&["run", "inputs"]), vec![json!(user.0)]))?;
                    }
                }
                let successor = create_generation(tx, conversation_id)?;
                hand_over(&live, task_id, successor)?;
            }
        }
        Ok(Some(completed(json!({ "entryId": assistant.0 }))))
    })
}

fn retry_phase(_services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Retry {
        attempt,
        compacted,
        until,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("retry phase without a retry checkpoint"));
    };
    runtime.sleep(until, &runtime.ctx())?;
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        set(&live, &["generation"], json!({ "attempt": attempt + 1 }))?;
        Ok(running(GenerationCheckpoint::Prepare {
            attempt: attempt + 1,
            compacted,
            overflow: None,
        }))
    })
}

fn poll_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Poll {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        handle,
        poll_at,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("poll phase without a poll checkpoint"));
    };
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(runtime, Some(&model_ref));
    };
    runtime.sleep(poll_at, &runtime.ctx())?;
    let message = services.models.fetch_deferred(&model, &handle, runtime.signal())?;
    let request = Request {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        messages: None,
        poll_at: Some(poll_at),
    };
    classify(services, runtime, request, message)
}

/// The calls `call_ids` of the assistant entry, in the given order.
fn read_calls(runtime: &TaskRuntime, assistant: EntryId, call_ids: &[String]) -> Result<Vec<ToolCall>> {
    let entry = runtime.read(|tx| Ok(tx.entry_of(entries::ASSISTANT, assistant)?))?;
    let calls = match entry
        .and_then(|entry| entry.model)
        .and_then(|model| model.into_iter().next())
    {
        Some(message) => match super::context::message_of(message)? {
            Message::Assistant(message) => tool_calls(&message),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    Ok(call_ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| &call.id == id).cloned())
        .collect())
}

/// Runs after the round's cells are terminal: calls never started get `aborted` results; the run's inputs settle
/// `unanswered` with `aborted`.
fn abort_handler(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let checkpoint = current_checkpoint(task)?;
    if let GenerationCheckpoint::Poll { model, handle, .. } = &checkpoint
        && let Some(model) = services.models.get_model(&model.provider, &model.model_id)
        && let Err(error) = services.models.cancel_deferred(&model, handle, runtime.signal())
    {
        runtime.report(&error)?;
    }
    let unstarted = match &checkpoint {
        GenerationCheckpoint::Tools { assistant, pending, .. } => read_calls(runtime, *assistant, pending)?,
        _ => Vec::new(),
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        convert_partial(tx, &live, conversation_id)?;
        for call in &unstarted {
            let aborted = CellResult::error("aborted", &format!("Tool {} was aborted", call.name));
            append_tool_result(
                tx,
                conversation_id,
                &call.id,
                &call.name,
                &aborted,
                runtime.now()?,
                None,
            )?;
        }
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "aborted".into(),
                detail: None,
            },
        )?;
        Ok(Some(NextState::Terminal {
            outcome: TaskOutcome::Aborted {
                reason: None,
                result: None,
            },
        }))
    })
}
