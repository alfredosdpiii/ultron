//! Ultron's local session daemon (packages/coding-agent/src/experimental: coordinator.ts, server.ts,
//! session-worker-manager.ts, session-worker.ts, process.ts, lost-server.ts) on the protocol of crates/protocol.

pub mod catalog;
pub mod chord;
pub mod control;
pub mod coord_client;
pub mod coordinator;
pub mod diff;
pub mod extension_ui;
pub mod instance;
pub mod lane;
pub mod lifecycle;
pub mod lock;
pub mod lost;
pub mod paths;
pub mod process;
pub mod routed;
pub mod server;
pub mod session;
pub mod session_control;
pub mod signals;
pub mod sockets;
pub mod watchdog;
pub mod wire;
pub mod worker;
pub mod worker_settings;
pub mod workers;
