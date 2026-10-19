//! Port of `packages/server`: the per-connection protocol state machine (`server.ts`), its
//! bounded error mapping (`errors.ts`), and Chord service-call validation it applies to requests; and the
//! experimental server's Radius relay host (`radius-relay.ts`, `radius-auth.ts`).

pub mod connection;
pub mod radius_relay;
pub mod radius_ws;

pub use connection::{Connection, HostError, Output, ServerConfig, ServerServices};
pub use protocol::{ServiceCall, parse_service_call};
