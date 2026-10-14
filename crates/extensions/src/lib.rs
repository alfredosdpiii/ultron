//! Ultron's extension host. Third-party extensions are separate processes speaking JSON-RPC 2.0 over stdio with MCP
//! framing (see `PROTOCOL.md`); they never run code inside the host or the Python kernel.
//!
//! - [`manifest`]: where extension processes are configured (`extension.json` in the TypeScript extension locations).
//! - [`rpc`]: the symmetric JSON-RPC peer (timeouts, cancellation, requests in both directions).
//! - [`process`]: spawning, the `initialize` handshake, registrations, crash isolation and restart.
//! - [`router`]: event fan-out with the ordering and veto rules of `core/extensions/runner.ts`.
//! - [`kernel_api`]: extension functions as `ext.<extension>.<function>` in the REPL, never model tools.
//! - [`ui`]: the declarative UI extensions contribute and the TUI renders.
//!
//! RLM-first: extension functions are kernel-API functions (`ext.<extension>.<function>(...)` in the REPL), never
//! model tools.

pub mod kernel_api;
pub mod manifest;
pub mod process;
pub mod router;
pub mod rpc;
pub mod ui;

pub use manifest::{Diagnostic, Discovery, Manifest, discover, discover_with_settings, parse_manifest};
pub use process::{
    EVENTS, ExtensionHost, ExtensionProcess, FunctionSpec, HostApi, NoHost, PROTOCOL_VERSION, Registrations,
    SessionInfo,
};
pub use router::{Cancelled, Endpoint, ExtensionError, ResolvedCommand, Router};
pub use rpc::{CancelToken, Peer, RpcError};
pub use ui::{UiModel, UiUpdate};
