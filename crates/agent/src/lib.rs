//! Rust port of Ultron's `packages/agent` loop (`agent-loop.ts`) and `Agent` (`agent.ts`): turns over a streaming
//! function, tool execution, steering and follow-up queues, abort, and the emitted [`AgentEvent`] stream.
//!
//! Evidence: `fixtures/generate.mjs` drives the TypeScript `Agent` with the TypeScript faux provider and records every
//! provider call and agent event; `tests/loop.rs` replays the provider events here and compares the agent events.

mod agent;
mod agent_loop;
mod types;

pub use agent::{Agent, AgentControl, AgentOptions, QueueMode};
pub use agent_loop::{agent_loop, agent_loop_continue};
pub use types::*;
