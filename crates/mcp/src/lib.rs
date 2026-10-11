//! Rust port of `packages/mcp` (the MCP client) and the configuration half of the built-in MCP extension
//! (`packages/coding-agent/src/extensions/mcp/config.ts`, `core/mcp-servers.ts`).
//!
//! Ultron is RLM-first: MCP tools are never model tools. The host owns these connections and the Python
//! kernel's `mcp.*` API reaches them through host requests.
//!
//! The client is blocking and thread-based like `crates/kernel`: a transport delivers incoming frames from its
//! reader thread, and each request waits on its own channel.
//!
//! Translated from `@ultron/mcp` (MIT), which credits the MCP TypeScript SDK (MIT, see packages/mcp/LICENSES).

pub mod client;
pub mod config;
pub mod content;
pub mod edit;
pub mod http;
pub mod jsonrpc;
pub mod resolve;
pub mod results;
pub mod runtime;
pub mod stdio;

pub use client::{CancelToken, Client, ClientOptions, ConnState, RequestOptions};
pub use jsonrpc::Error;
pub use stdio::{StdioOptions, StdioTransport};

use serde_json::Value;
use std::sync::Arc;

/// What a transport reports to the client, from its own threads.
pub enum Incoming {
    Message(Value),
    Error(String),
    Closed,
}

pub type Sink = Arc<dyn Fn(Incoming) + Send + Sync>;

/// A JSON-RPC message channel. `Closed` reaches the sink at most once.
pub trait Transport: Send + Sync {
    fn start(&self, sink: Sink) -> Result<(), Error>;
    fn send(&self, message: &Value) -> Result<(), Error>;
    fn close(&self);
    fn set_protocol_version(&self, _version: &str) {}
}
