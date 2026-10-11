//! A [`ChildRunner`] that runs each subagent as a `crates/agent` loop: the caller supplies the agent options and the
//! model stream for a run (its tools, normally the child's own REPL bound to `run.host.lane_handler(&run.lane_name)`),
//! and the child's reply is its last assistant message's text. Cancelling the child aborts the loop.

use crate::host::{ChildRun, ChildRunner};
use agent::{Agent, AgentOptions, StreamFn};
use ai::{AssistantContent, Message, StopReason};
use std::sync::Arc;
use std::time::Duration;

/// Builds a child's agent: options (model, system prompt, tools) and its model stream.
pub type ChildAgentFactory = Arc<dyn Fn(&ChildRun) -> (AgentOptions, Box<dyn StreamFn>) + Send + Sync>;

pub fn agent_runner(factory: ChildAgentFactory) -> ChildRunner {
    Arc::new(move |run: ChildRun| {
        let (options, stream) = factory(&run);
        let mut agent = Agent::new(options, stream);
        let control = agent.control();
        let signal = run.signal.clone();
        // The child's end also aborts its signal, so this watcher always exits.
        std::thread::spawn(move || {
            while !signal.wait(Duration::from_secs(3600)) {}
            control.abort();
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(agent.prompt_text(&run.prompt, Vec::new()));
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
    })
}
