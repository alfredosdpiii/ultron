//! Port of `packages/server`: the per-connection protocol state machine (`server.ts`), its
//! bounded error mapping (`errors.ts`), and Chord service-call validation it applies to requests.

pub mod connection;
pub mod unix;

pub use connection::{Connection, HostError, Output, ServerConfig, ServerServices};
pub use protocol::{ServiceCall, parse_service_call};
pub use unix::{DEFAULT_HANDSHAKE_TIMEOUT, UnixServer, unix_socket_path};
