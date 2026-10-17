use std::future::poll_fn;
use std::sync::Mutex;
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

use ai::http::BoxFuture;
use ai::transcript::{current_tools, normalize_context};
use ai::{
    AbortSignal, AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, SimpleStreamOptions,
    StopReason, StreamOptions, SystemContent, SystemMessage, Tool, ToolCall, ToolReference, ToolResultMessage,
};
use serde_json::Value;

use std::sync::Arc;

use mcp::validation::validate_tool_arguments;

use crate::types::{
    AfterToolCall, AfterToolCallContext, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, BeforeToolCallContext, StreamFn, ToolExecutionMode,
};

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The event sink, shared by concurrently running tools.
pub(crate) struct Emit<'a>(Mutex<&'a mut (dyn FnMut(AgentEvent) + Send)>);

impl Emit<'_> {
    fn emit(&self, event: AgentEvent) {
        let mut sink = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (*sink)(event)
    }
}

/// `runAgentLoop`: add `prompts` (after declaring tool changes) and run until the agent stops. Returns the new
/// messages.
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: &mut (dyn FnMut(AgentEvent) + Send),
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) -> Vec<AgentMessage> {
    let emit = Emit(Mutex::new(emit));
    let initial = declare_tool_changes(&context, prompts);
    let mut new_messages = initial.clone();
    let mut current = context;
    current.messages.extend(initial.iter().cloned());
    emit.emit(AgentEvent::AgentStart);
    emit.emit(AgentEvent::TurnStart);
    for message in initial {
        emit.emit(AgentEvent::MessageStart {
            message: message.clone(),
        });
        emit.emit(AgentEvent::MessageEnd { message });
    }
    run_loop(current, &mut new_messages, config, signal, &emit, stream_fn).await;
    new_messages
}

/// `runAgentLoopContinue`: run from the current context without a new message.
pub async fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: &mut (dyn FnMut(AgentEvent) + Send),
    signal: &AbortSignal,
    stream_fn: &dyn StreamFn,
) -> Result<Vec<AgentMessage>, String> {
    match context.messages.last() {
        None => return Err("Cannot continue: no messages in context".into()),
        Some(Message::Assistant(_)) => return Err("Cannot continue from message role: assistant".into()),
        Some(_) => {}
    }
    let emit = Emit(Mutex::new(emit));
    let mut new_messages = Vec::new();
    emit.emit(AgentEvent::AgentStart);
    emit.emit(AgentEvent::TurnStart);
    run_loop(context, &mut new_messages, config, signal, &emit, stream_fn).await;
    Ok(new_messages)
}

fn poll_messages(source: &mut Option<crate::types::MessageSource>) -> Vec<AgentMessage> {
    source.as_mut().map(|get| get()).unwrap_or_default()
}

async fn run_loop(
    mut context: AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    signal: &AbortSignal,
    emit: &Emit<'_>,
    stream_fn: &dyn StreamFn,
) {
    let mut first_turn = true;
    let mut pending = poll_messages(&mut config.get_steering_messages);
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            if !first_turn {
                emit.emit(AgentEvent::TurnStart);
            }
            first_turn = false;
            for message in declare_tool_changes(&context, std::mem::take(&mut pending)) {
                emit.emit(AgentEvent::MessageStart {
                    message: message.clone(),
                });
                emit.emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                });
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let message = stream_assistant_response(&mut context, &config, signal, emit, stream_fn).await;
            new_messages.push(Message::Assistant(message.clone()));
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit.emit(AgentEvent::TurnEnd {
                    message: Message::Assistant(message),
                    tool_results: Vec::new(),
                });
                emit.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return;
            }

            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(call) => Some(call.clone()),
                    _ => None,
                })
                .collect();
            has_more_tool_calls = false;
            let mut tool_results = Vec::new();
            if !tool_calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_tool_calls(&tool_calls, emit)
                } else {
                    execute_tool_calls(&context, &message, &tool_calls, &config, signal, emit).await
                };
                has_more_tool_calls = !batch.terminate;
                for result in &batch.messages {
                    context.messages.push(Message::ToolResult(result.clone()));
                    new_messages.push(Message::ToolResult(result.clone()));
                }
                tool_results = batch.messages;
            }
            emit.emit(AgentEvent::TurnEnd {
                message: Message::Assistant(message),
                tool_results,
            });
            pending = poll_messages(&mut config.get_steering_messages);
        }

        let follow_ups = poll_messages(&mut config.get_follow_up_messages);
        if follow_ups.is_empty() {
            break;
        }
        pending = follow_ups;
    }
    emit.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
}

/// `declareToolChanges`: make the transcript's declared tools equal `context.tools` by putting `toolsAdded` /
/// `toolsRemoved` on the last pending system message, or on a new one before the first non-system pending message.
pub(crate) fn declare_tool_changes(context: &AgentContext, pending: Vec<AgentMessage>) -> Vec<AgentMessage> {
    let system_index = pending.iter().rposition(|m| matches!(m, Message::System(_)));
    let baseline: Vec<AgentMessage> = pending
        .iter()
        .enumerate()
        .map(|(i, m)| match m {
            Message::System(s) if Some(i) == system_index => {
                Message::System(with_tool_changes(s, Vec::new(), Vec::new()))
            }
            other => other.clone(),
        })
        .collect();
    let mut transcript = context.messages.clone();
    transcript.extend(baseline.iter().cloned());
    let executable: Vec<Tool> = context.tools.iter().map(|t| t.tool().clone()).collect();
    let (added, removed) = tool_state_changes(&current_tools(&transcript), &executable);
    let unchanged = added.is_empty() && removed.is_empty();

    if let Some(index) = system_index {
        let Message::System(system) = &pending[index] else {
            unreachable!()
        };
        let declares = system.tools_added.as_ref().is_some_and(|t| !t.is_empty())
            || system.tools_removed.as_ref().is_some_and(|t| !t.is_empty());
        if unchanged && !declares {
            return pending;
        }
        let update = with_tool_changes(system, added, removed);
        let mut out = baseline;
        out[index] = Message::System(update);
        return out;
    }
    if unchanged {
        return pending;
    }
    let empty = SystemMessage {
        content: SystemContent::Text(String::new()),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: now_ms(),
    };
    let update = Message::System(with_tool_changes(&empty, added, removed));
    let index = pending
        .iter()
        .position(|m| !matches!(m, Message::System(_)))
        .unwrap_or(pending.len());
    let mut out = pending;
    out.insert(index, update);
    out
}

fn with_tool_changes(message: &SystemMessage, added: Vec<Tool>, removed: Vec<ToolReference>) -> SystemMessage {
    SystemMessage {
        tools_added: (!added.is_empty()).then_some(added),
        tools_removed: (!removed.is_empty()).then_some(removed),
        ..message.clone()
    }
}

/// `getToolStateChanges`: a changed definition is a removal plus an addition. Declarations compare by their JSON
/// text, as upstream's `declarationsEqual`.
fn tool_state_changes(previous: &[Tool], current: &[Tool]) -> (Vec<Tool>, Vec<ToolReference>) {
    let json = |tool: &Tool| serde_json::to_string(tool).unwrap_or_default();
    let same = |a: &Tool, b: &Tool| json(a) == json(b);
    let added = current
        .iter()
        .filter(|tool| !previous.iter().any(|p| p.name == tool.name && same(p, tool)))
        .cloned()
        .collect();
    let removed = previous
        .iter()
        .filter(|tool| !current.iter().any(|c| c.name == tool.name && same(tool, c)))
        .map(|tool| ToolReference {
            name: tool.name.clone(),
        })
        .collect();
    (added, removed)
}

/// `streamAssistantResponse`: the in-progress message is the last context message while streaming.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: &AbortSignal,
    emit: &Emit<'_>,
    stream_fn: &dyn StreamFn,
) -> AssistantMessage {
    let llm_context = normalize_context(&Context {
        system_prompt: None,
        messages: context.messages.clone(),
        tools: None,
    });
    let options = SimpleStreamOptions {
        base: StreamOptions {
            api_key: config.api_key.clone(),
            session_id: config.session_id.clone(),
            signal: Some(signal.clone()),
            ..Default::default()
        },
        reasoning: config.reasoning.clone(),
        ..Default::default()
    };
    let messages = &mut context.messages;
    let mut added_partial = false;
    let mut sink = |event: &AssistantMessageEvent| match event {
        AssistantMessageEvent::Start { partial } => {
            messages.push(Message::Assistant(partial.clone()));
            added_partial = true;
            emit.emit(AgentEvent::MessageStart {
                message: Message::Assistant(partial.clone()),
            });
        }
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {}
        AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
            if added_partial {
                let message = Message::Assistant(partial.clone());
                if let Some(last) = messages.last_mut() {
                    *last = message.clone();
                }
                emit.emit(AgentEvent::MessageUpdate {
                    message,
                    assistant_message_event: event.clone(),
                });
            }
        }
    };
    let final_message = stream_fn.stream(&config.model, &llm_context, &options, &mut sink).await;
    let message = Message::Assistant(final_message.clone());
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = message.clone();
        }
    } else {
        context.messages.push(message.clone());
        emit.emit(AgentEvent::MessageStart {
            message: message.clone(),
        });
    }
    emit.emit(AgentEvent::MessageEnd { message });
    final_message
}

struct Batch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct Finalized {
    call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

/// A `length` stop may have truncated every tool call's arguments: none run.
fn fail_truncated_tool_calls(calls: &[ToolCall], emit: &Emit<'_>) -> Batch {
    let mut messages = Vec::new();
    for call in calls {
        emit_start(call, emit);
        let finalized = Finalized {
            call: call.clone(),
            result: AgentToolResult::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )),
            is_error: true,
        };
        emit_end(&finalized, emit);
        let message = tool_result_message(&finalized);
        emit_result_message(&message, emit);
        messages.push(message);
    }
    Batch {
        messages,
        terminate: false,
    }
}

fn emit_start(call: &ToolCall, emit: &Emit<'_>) {
    emit.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: call.arguments.clone(),
    });
}

fn emit_end(finalized: &Finalized, emit: &Emit<'_>) {
    emit.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.call.id.clone(),
        tool_name: finalized.call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    });
}

fn emit_result_message(message: &ToolResultMessage, emit: &Emit<'_>) {
    emit.emit(AgentEvent::MessageStart {
        message: Message::ToolResult(message.clone()),
    });
    emit.emit(AgentEvent::MessageEnd {
        message: Message::ToolResult(message.clone()),
    });
}

fn tool_result_message(finalized: &Finalized) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.call.id.clone(),
        tool_name: finalized.call.name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()).filter(|d| !d.is_null()),
        usage: finalized.result.usage.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
    }
}

fn should_terminate(finalized: &[Finalized]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate == Some(true))
}

async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: &AbortSignal,
    emit: &Emit<'_>,
) -> Batch {
    let sequential = config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context
                .tools
                .iter()
                .find(|t| t.tool().name == call.name)
                .and_then(|t| t.execution_mode())
                == Some(ToolExecutionMode::Sequential)
        });
    let after = config.after_tool_call.as_ref();
    let mut finalized = Vec::new();
    if sequential {
        let mut messages = Vec::new();
        for call in calls {
            emit_start(call, emit);
            let done = match prepare_tool_call(context, assistant, call, config, signal).await {
                Prepared::Immediate(done) => *done,
                Prepared::Ready { tool, args } => {
                    let executed = execute_prepared(call, tool, &args, signal, emit).await;
                    finalize(context, assistant, call, &args, executed, after, signal).await
                }
            };
            emit_end(&done, emit);
            let message = tool_result_message(&done);
            emit_result_message(&message, emit);
            messages.push(message);
            finalized.push(done);
            if signal.aborted() {
                break;
            }
        }
        return Batch {
            messages,
            terminate: should_terminate(&finalized),
        };
    }

    // Parallel: each call is prepared in order (lookup, argument preparation, validation, `beforeToolCall`), and an
    // immediate outcome ends at once; then every prepared call runs concurrently; result messages follow in call
    // order.
    let mut slots: Vec<Option<Finalized>> = Vec::new();
    let mut running: Vec<BoxFuture<'_, (usize, Finalized)>> = Vec::new();
    for call in calls {
        emit_start(call, emit);
        match prepare_tool_call(context, assistant, call, config, signal).await {
            Prepared::Immediate(done) => {
                emit_end(&done, emit);
                slots.push(Some(*done));
            }
            Prepared::Ready { tool, args } => {
                let index = slots.len();
                running.push(Box::pin(async move {
                    let done = if signal.aborted() {
                        immediate(call, "Operation aborted")
                    } else {
                        let executed = execute_prepared(call, tool, &args, signal, emit).await;
                        finalize(context, assistant, call, &args, executed, after, signal).await
                    };
                    emit_end(&done, emit);
                    (index, done)
                }));
                slots.push(None);
            }
        }
        if signal.aborted() {
            break;
        }
    }
    for (index, done) in join_all(running).await {
        slots[index] = Some(done);
    }
    let finalized: Vec<Finalized> = slots.into_iter().flatten().collect();
    let mut messages = Vec::new();
    for done in &finalized {
        let message = tool_result_message(done);
        emit_result_message(&message, emit);
        messages.push(message);
    }
    Batch {
        terminate: should_terminate(&finalized),
        messages,
    }
}

/// Polls every future until all finish, in completion order.
async fn join_all<T>(mut futures: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    let mut out = Vec::new();
    poll_fn(|cx| {
        let mut i = 0;
        while i < futures.len() {
            if let Poll::Ready(value) = futures[i].as_mut().poll(cx) {
                drop(futures.remove(i));
                out.push(value);
            } else {
                i += 1;
            }
        }
        if futures.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    out
}

enum Prepared<'c> {
    Immediate(Box<Finalized>),
    Ready { tool: &'c Arc<dyn AgentTool>, args: Value },
}

fn immediate(call: &ToolCall, message: impl Into<String>) -> Finalized {
    Finalized {
        call: call.clone(),
        result: AgentToolResult::error(message),
        is_error: true,
    }
}

/// `prepareToolCall`: find the tool, apply its `prepareArguments`, validate and coerce the arguments against its
/// schema (`validateToolArguments`, same error text), then ask `beforeToolCall`. Anything that fails or blocks is an
/// immediate error result.
async fn prepare_tool_call<'c>(
    context: &'c AgentContext,
    assistant: &AssistantMessage,
    call: &ToolCall,
    config: &AgentLoopConfig,
    signal: &AbortSignal,
) -> Prepared<'c> {
    let Some(tool) = context.tools.iter().find(|t| t.tool().name == call.name) else {
        return Prepared::Immediate(Box::new(immediate(call, format!("Tool {} not found", call.name))));
    };
    let args = match tool
        .prepare_arguments(&call.arguments)
        .and_then(|prepared| validate_tool_arguments(&call.name, &tool.tool().parameters, &prepared))
    {
        Ok(args) => args,
        Err(message) => return Prepared::Immediate(Box::new(immediate(call, message))),
    };
    if let Some(before) = &config.before_tool_call {
        let decision = before(
            BeforeToolCallContext {
                assistant_message: assistant,
                tool_call: call,
                args: &args,
                context,
            },
            signal,
        )
        .await;
        let decision = match decision {
            Ok(decision) => decision,
            Err(message) => return Prepared::Immediate(Box::new(immediate(call, message))),
        };
        if signal.aborted() {
            return Prepared::Immediate(Box::new(immediate(call, "Operation aborted")));
        }
        if let Some(decision) = decision.filter(|d| d.block) {
            let mut done = immediate(
                call,
                decision
                    .reason
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "Tool execution was blocked".into()),
            );
            if decision.terminate == Some(true) {
                done.result.terminate = Some(true);
            }
            return Prepared::Immediate(Box::new(done));
        }
    }
    if signal.aborted() {
        return Prepared::Immediate(Box::new(immediate(call, "Operation aborted")));
    }
    Prepared::Ready { tool, args }
}

/// `executePreparedToolCall`: run the tool with the validated arguments; a thrown error is an error result.
async fn execute_prepared(
    call: &ToolCall,
    tool: &Arc<dyn AgentTool>,
    args: &Value,
    signal: &AbortSignal,
    emit: &Emit<'_>,
) -> (AgentToolResult, bool) {
    let on_update = |partial: AgentToolResult| {
        emit.emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            args: call.arguments.clone(),
            partial_result: partial,
        })
    };
    match tool.execute(&call.id, args.clone(), signal, &on_update).await {
        Ok(result) => (result, false),
        Err(message) => (AgentToolResult::error(message), true),
    }
}

/// `finalizeExecutedToolCall`: `afterToolCall` may replace any part of the result; a throw makes it an error.
async fn finalize(
    context: &AgentContext,
    assistant: &AssistantMessage,
    call: &ToolCall,
    args: &Value,
    (mut result, mut is_error): (AgentToolResult, bool),
    after: Option<&AfterToolCall>,
    signal: &AbortSignal,
) -> Finalized {
    if let Some(after) = after {
        let outcome = after(
            AfterToolCallContext {
                assistant_message: assistant,
                tool_call: call,
                args,
                result: &result,
                is_error,
                context,
            },
            signal,
        )
        .await;
        match outcome {
            Ok(Some(replacement)) => {
                result = AgentToolResult {
                    content: replacement.content.unwrap_or(result.content),
                    details: replacement.details.unwrap_or(result.details),
                    usage: replacement.usage.or(result.usage),
                    terminate: replacement.terminate.or(result.terminate),
                };
                is_error = replacement.is_error.unwrap_or(is_error);
            }
            Ok(None) => {}
            Err(message) => {
                result = AgentToolResult::error(message);
                is_error = true;
            }
        }
    }
    Finalized {
        call: call.clone(),
        result,
        is_error,
    }
}
