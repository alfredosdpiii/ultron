//! Subagents run as in-memory agent loops (a session without the durable runtime: `ultron mcp`, whose root is
//! Claude Code, with `--children ultron` or no `claude` installed). The same loop as `delegation::child::agent_runner`
//! (the child's reply is its last assistant message's text; cancelling the child aborts the loop), with what a
//! child lane of the session actor gets from its durable conversation:
//!
//! - agent-hub steering ([`MemoryChildren::steer`], the stack's `lane_steer`): a message for a running child is
//!   queued as a steer of its loop, seen after the current turn's tool calls; a child that is not running is not
//!   steered;
//! - the context-pressure reminder (crate::context_reminder): the child lane's context use is the estimate of its
//!   loop's conversation (`estimateContextTokens`: the newest answer's usage plus estimates after it), read while
//!   the child runs.

use crate::context_reminder::ContextReminders;
use agent::{Agent, AgentControl, AgentEvent};
use ai::{AssistantContent, Message, StopReason, UserMessage, UserMessageContent};
use delegation::child::ChildAgentFactory;
use delegation::host::{ChildRun, ChildRunner};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The running in-memory children, by lane.
#[derive(Default)]
pub struct MemoryChildren {
    running: Mutex<HashMap<String, AgentControl>>,
    reminders: OnceLock<Arc<ContextReminders>>,
}

impl MemoryChildren {
    /// Child lanes' context use goes to `reminders` from the next child on.
    pub fn attach_reminders(&self, reminders: Arc<ContextReminders>) {
        let _ = self.reminders.set(reminders);
    }

    /// Steer the running child on `lane` with `message`; false when no child runs there.
    pub fn steer(&self, lane: &str, message: &str) -> bool {
        let Some(control) = lock(&self.running).get(lane).cloned() else {
            return false;
        };
        control.steer(Message::User(UserMessage {
            content: UserMessageContent::Text(message.to_string()),
            timestamp: crate::lane::now_ms(),
        }));
        true
    }

    /// Whether a child runs on `lane`.
    pub fn is_running(&self, lane: &str) -> bool {
        lock(&self.running).contains_key(lane)
    }

    /// The runner of subagents built by `factory`.
    pub fn runner(self: &Arc<Self>, factory: ChildAgentFactory) -> ChildRunner {
        let children = self.clone();
        Arc::new(move |run: ChildRun| children.run(&factory, run))
    }

    fn run(&self, factory: &ChildAgentFactory, run: ChildRun) -> Result<String, String> {
        let (options, stream) = factory(&run);
        let mut agent = Agent::new(options, stream);
        let control = agent.control();
        let signal = run.signal.clone();
        let abort = control.clone();
        // The child's end also aborts its signal, so this watcher always exits.
        std::thread::spawn(move || {
            while !signal.wait(Duration::from_secs(3600)) {}
            abort.abort();
        });
        // The conversation as the loop has it, for the lane's context meter.
        let conversation = Arc::new(Mutex::new(agent.messages.clone()));
        let seen = conversation.clone();
        agent.subscribe(move |event| {
            if let AgentEvent::MessageEnd { message } = event {
                lock(&seen).push(message.clone());
            }
        });
        let lane = run.lane_name.clone();
        if let Some(reminders) = self.reminders.get() {
            let meter = conversation.clone();
            reminders.set_lane_meter(
                &lane,
                Some(Arc::new(move || {
                    Some(ai::transcript::estimate_context_tokens(&lock(&meter)).tokens)
                })),
            );
        }
        lock(&self.running).insert(lane.clone(), control);
        let ended = |children: &MemoryChildren| {
            lock(&children.running).remove(&lane);
            if let Some(reminders) = children.reminders.get() {
                reminders.set_lane_meter(&lane, None);
            }
        };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                ended(self);
                return Err(error.to_string());
            }
        };
        runtime.block_on(agent.prompt_text(&run.prompt, Vec::new()));
        ended(self);
        let last = agent.messages.iter().rev().find_map(|m| match m {
            Message::Assistant(message) => Some(message),
            _ => None,
        });
        let Some(last) = last else {
            return Err("Agent produced no assistant result at the completed tip".into());
        };
        if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
            return Err(last.error_message.clone().unwrap_or_else(|| "Agent run failed".into()));
        }
        Ok(last
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
