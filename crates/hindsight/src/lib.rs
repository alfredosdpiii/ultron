//! Ultron's Hindsight memory client (`createHindsightBackend` of `ultron/memory.ts`), the backend of `memory.*` that
//! the session composition root uses by default, autoreview's `HindsightLessons`, and the resolution of the server
//! and bank every Ultron instance shares. HTTP goes through [`transport::Transport`]; [`ReqwestTransport`] is real.

pub mod backend;
pub mod lessons;
pub mod resolve;
pub mod transport;

pub use backend::{HindsightBackend, HindsightOptions, encode_uri_component};
pub use lessons::HindsightLessons;
pub use resolve::{DEFAULT_HINDSIGHT_URL, Endpoint, hindsight_url, worker_scope_tags};
pub use transport::{HttpRequest, HttpResponse, ReqwestTransport, Transport, TransportError};
