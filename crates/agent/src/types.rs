use std::sync::Arc;

use ai::http::BoxFuture;
use ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, EventSink, Message, Model, ToolCall, ToolResultMessage,
    TranscriptContext, Usage, UserContent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A transcript message. Upstream allows app-specific message kinds; none exist in Rust yet.
pub type AgentMessage = Message;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// What a tool returns. `terminate` on every result of a batch ends the run after it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub content: Vec<UserContent>,
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        AgentToolResult {
            content: vec![UserContent::Text(ai::TextContent {
                text: message.into(),
                text_signature: None,
            })],
            details: Value::Object(Default::default()),
            usage: None,
            terminate: None,
        }
    }
}

/// Receives partial results while a tool runs (`tool_execution_update`).
pub type ToolUpdate<'a> = dyn Fn(AgentToolResult) + Send + Sync + 'a;

/// An executable tool. `Err` is a thrown error: it becomes an error result with the message as text.
pub trait AgentTool: Send + Sync {
    /// The declaration the model sees.
    fn tool(&self) -> &ai::Tool;
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }
    /// `prepareArguments`: adapt the raw call arguments before validation (for example an old argument name). `Err`
    /// is a thrown error: the call fails with the message as text.
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        Ok(args.clone())
    }
    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        args: Value,
        signal: &'a AbortSignal,
        on_update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>>;
}

/// `StreamFn`: one model response, events to `sink`, the final message returned (errors are messages).
pub trait StreamFn: Send + Sync {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        context: &'a TranscriptContext,
        options: &'a ai::SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage>;
}

#[derive(Clone, Default)]
pub struct AgentContext {
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

/// Polls queued messages; called between turns.
pub type MessageSource = Box<dyn FnMut() -> Vec<AgentMessage> + Send>;

/// What `beforeToolCall` sees: the call as the model sent it and its validated arguments.
pub struct BeforeToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub context: &'a AgentContext,
}

/// `beforeToolCall`'s decision: `block` fails the call with `reason` (default "Tool execution was blocked");
/// `terminate` marks that error result as ending the run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: Option<bool>,
}

/// What `afterToolCall` sees: the executed call, its validated arguments and its result.
pub struct AfterToolCallContext<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
    pub context: &'a AgentContext,
}

/// `afterToolCall`'s replacement: each field present replaces the result's.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<UserContent>>,
    pub details: Option<Value>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
    pub is_error: Option<bool>,
}

/// Runs after validation, before execution. `Err` is a thrown error: the call fails with the message as text.
pub type BeforeToolCall = Arc<
    dyn for<'a> Fn(
            BeforeToolCallContext<'a>,
            &'a AbortSignal,
        ) -> BoxFuture<'a, Result<Option<BeforeToolCallResult>, String>>
        + Send
        + Sync,
>;

/// Runs after execution. `Err` is a thrown error: the result becomes an error with the message as text.
pub type AfterToolCall = Arc<
    dyn for<'a> Fn(
            AfterToolCallContext<'a>,
            &'a AbortSignal,
        ) -> BoxFuture<'a, Result<Option<AfterToolCallResult>, String>>
        + Send
        + Sync,
>;

pub struct AgentLoopConfig {
    pub model: Model,
    /// `None` is thinking off.
    pub reasoning: Option<String>,
    pub session_id: Option<String>,
    pub api_key: Option<String>,
    pub tool_execution: ToolExecutionMode,
    pub get_steering_messages: Option<MessageSource>,
    pub get_follow_up_messages: Option<MessageSource>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
}

impl AgentLoopConfig {
    pub fn new(model: Model) -> Self {
        AgentLoopConfig {
            model,
            reasoning: None,
            session_id: None,
            api_key: None,
            tool_execution: ToolExecutionMode::Parallel,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // events are short-lived; boxing every message would cost more than it saves
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        #[serde(rename = "toolResults")]
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        #[serde(rename = "assistantMessageEvent")]
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}
