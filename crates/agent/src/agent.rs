use std::sync::{Arc, Mutex, PoisonError};

use ai::transcript::create_initial_system_message;
use ai::{AbortSignal, ImageContent, Message, Model, TextContent, UserContent, UserMessage, UserMessageContent};

use crate::agent_loop::{agent_loop, now_ms};
use crate::types::{
    AfterToolCall, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, BeforeToolCall, StreamFn,
    ToolExecutionMode,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

struct Queue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl Queue {
    fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.clone(),
            QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        let drained = self.peek();
        self.messages.drain(..drained.len());
        drained
    }
}

type Shared<T> = Arc<Mutex<T>>;

fn lock<T>(shared: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Cloneable handle for listeners and other tasks: queue messages and abort the active run.
#[derive(Clone)]
pub struct AgentControl {
    steering: Shared<Queue>,
    follow_up: Shared<Queue>,
    active: Shared<Option<AbortSignal>>,
}

impl AgentControl {
    /// Queue a message delivered after the current turn's tool calls.
    pub fn steer(&self, message: AgentMessage) {
        lock(&self.steering).messages.push(message);
    }

    /// Queue a message delivered when the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        lock(&self.follow_up).messages.push(message);
    }

    pub fn abort(&self) {
        if let Some(signal) = lock(&self.active).as_ref() {
            signal.abort();
        }
    }

    pub fn has_queued_messages(&self) -> bool {
        !lock(&self.steering).messages.is_empty() || !lock(&self.follow_up).messages.is_empty()
    }

    pub fn clear_all_queues(&self) {
        lock(&self.steering).messages.clear();
        lock(&self.follow_up).messages.clear();
    }
}

pub struct AgentOptions {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    /// `"off"` or a reasoning level.
    pub thinking_level: String,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: ToolExecutionMode,
    pub session_id: Option<String>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
}

impl AgentOptions {
    pub fn new(model: Model) -> Self {
        AgentOptions {
            model,
            system_prompt: None,
            tools: Vec::new(),
            messages: Vec::new(),
            thinking_level: "off".into(),
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            tool_execution: ToolExecutionMode::Parallel,
            session_id: None,
            before_tool_call: None,
            after_tool_call: None,
        }
    }
}

type Listener = Box<dyn FnMut(&AgentEvent) + Send>;

/// `Agent`: owns the transcript and tools, runs prompts through the loop, and keeps state from the events.
pub struct Agent {
    pub model: Model,
    pub thinking_level: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    pub tool_execution: ToolExecutionMode,
    pub session_id: Option<String>,
    pub error_message: Option<String>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,
    stream_fn: Box<dyn StreamFn>,
    listeners: Vec<Listener>,
    control: AgentControl,
}

impl Agent {
    pub fn new(options: AgentOptions, stream_fn: Box<dyn StreamFn>) -> Self {
        let mut messages = options.messages;
        let declarations: Vec<ai::Tool> = options.tools.iter().map(|t| t.tool().clone()).collect();
        if !matches!(messages.first(), Some(Message::System(_)))
            && let Some(initial) = create_initial_system_message(options.system_prompt.as_deref(), Some(&declarations))
        {
            messages.insert(0, Message::System(initial));
        }
        let queue = |mode| {
            Arc::new(Mutex::new(Queue {
                messages: Vec::new(),
                mode,
            }))
        };
        Agent {
            model: options.model,
            thinking_level: options.thinking_level,
            tools: options.tools,
            messages,
            tool_execution: options.tool_execution,
            session_id: options.session_id,
            error_message: None,
            before_tool_call: options.before_tool_call,
            after_tool_call: options.after_tool_call,
            stream_fn,
            listeners: Vec::new(),
            control: AgentControl {
                steering: queue(options.steering_mode),
                follow_up: queue(options.follow_up_mode),
                active: Arc::new(Mutex::new(None)),
            },
        }
    }

    pub fn control(&self) -> AgentControl {
        self.control.clone()
    }

    pub fn subscribe(&mut self, listener: impl FnMut(&AgentEvent) + Send + 'static) {
        self.listeners.push(Box::new(listener));
    }

    /// Prompt with text (and images) as one user message.
    pub async fn prompt_text(&mut self, text: &str, images: Vec<ImageContent>) {
        let mut content = vec![UserContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })];
        content.extend(images.into_iter().map(UserContent::Image));
        let message = Message::User(UserMessage {
            content: UserMessageContent::Blocks(content),
            timestamp: now_ms(),
        });
        self.prompt(vec![message]).await;
    }

    /// Run the loop over `messages`; returns when the run ends (`agent_end`).
    pub async fn prompt(&mut self, messages: Vec<AgentMessage>) {
        let signal = AbortSignal::new();
        *lock(&self.control.active) = Some(signal.clone());
        self.error_message = None;

        let steering = self.control.steering.clone();
        let follow_up = self.control.follow_up.clone();
        let config = AgentLoopConfig {
            model: self.model.clone(),
            reasoning: (self.thinking_level != "off").then(|| self.thinking_level.clone()),
            session_id: self.session_id.clone(),
            api_key: None,
            tool_execution: self.tool_execution,
            get_steering_messages: Some(Box::new(move || lock(&steering).drain())),
            get_follow_up_messages: Some(Box::new(move || lock(&follow_up).drain())),
            before_tool_call: self.before_tool_call.clone(),
            after_tool_call: self.after_tool_call.clone(),
        };
        let context = AgentContext {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
        };
        let Agent {
            messages: state,
            error_message,
            listeners,
            stream_fn,
            ..
        } = self;
        let mut process = |event: AgentEvent| {
            match &event {
                AgentEvent::MessageEnd { message } => state.push(message.clone()),
                AgentEvent::TurnEnd {
                    message: Message::Assistant(message),
                    ..
                } if message.error_message.is_some() => {
                    error_message.clone_from(&message.error_message);
                }
                _ => {}
            }
            for listener in listeners.iter_mut() {
                listener(&event);
            }
        };
        agent_loop(messages, context, config, &mut process, &signal, stream_fn.as_ref()).await;

        *lock(&self.control.active) = None;
    }
}
