//! A layer over every lane's host requests, set once by a composition root whose root agent is not the session
//! actor's (`ultron mcp`, whose root is Claude Code): it may answer a request itself (refuse an API that root cannot
//! serve, such as `schedule.*` with nothing to fire it) or pass it on and see the answer (which root a spawn's work is
//! charged to). Without one set, requests pass through unchanged.

use kernel::{HostHandler, Signal};
use serde_json::{Map, Value};
use std::sync::{Arc, OnceLock};

/// Passes the request (with this payload) on to the session's own handler.
pub type Next<'a> = &'a dyn Fn(Map<String, Value>) -> Result<Value, String>;

/// `(lane, kind, payload, next)`: answer the request, or call `next` and return (or inspect) what it answers.
pub type RequestLayer = Arc<dyn Fn(&str, &str, Map<String, Value>, Next<'_>) -> Result<Value, String> + Send + Sync>;

/// The outermost handler of a lane's requests: the stack's layer when one is set (read at each request, so a layer
/// set after a kernel started still applies), else the inner handler.
pub(crate) struct Layered {
    pub inner: Arc<dyn HostHandler>,
    pub layer: Arc<OnceLock<RequestLayer>>,
    pub lane: String,
}

impl HostHandler for Layered {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        match self.layer.get() {
            Some(layer) => layer(&self.lane, kind, payload, &|payload| {
                self.inner.handle(kind, payload, signal)
            }),
            None => self.inner.handle(kind, payload, signal),
        }
    }
}
