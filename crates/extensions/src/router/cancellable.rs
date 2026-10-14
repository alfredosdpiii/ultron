//! The cancellable `session_before_*` dispatch. TypeScript hands `session_before_compact` and `session_before_tree`
//! handlers the operation's `signal` (`event.signal`); over the wire the signal is `notifications/cancelled` for
//! the event request. When it fires the host gives the handler in flight up at once (MCP: the sender ignores a
//! late response), calls no further handler, and answers [`Cancelled`]: AgentSession ends a compaction aborted
//! whatever a hook returns after the abort (`signal.aborted` is checked after the hooks and the summary).

use super::{Router, SESSION_BEFORE_EVENTS, event_type};
use crate::rpc::CancelToken;
use serde_json::Value;
use session_file::js::truthy;

/// A dispatch given up because its signal fired, with the signal's reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancelled(pub String);

impl Router {
    /// [`Router::emit`] with the event's signal: `cancel` stops the dispatch (see the module docs).
    pub fn emit_cancellable(&self, event: &Value, cancel: &CancelToken) -> Result<Option<Value>, Cancelled> {
        self.dispatch_all(event, Some(cancel))
    }

    /// The `emit` rules: every subscriber in load order; for a `session_before_*` event the first truthy `cancel`
    /// stops the dispatch and the last truthy result is returned. Errors are reported and the dispatch moves on.
    pub(super) fn dispatch_all(&self, event: &Value, cancel: Option<&CancelToken>) -> Result<Option<Value>, Cancelled> {
        let kind = event_type(event);
        let cancellable = SESSION_BEFORE_EVENTS.contains(&kind);
        let cancelled = || cancel.and_then(CancelToken::reason).map(Cancelled);
        let mut result = None;
        for endpoint in self.subscribers(kind) {
            if let Some(cancelled) = cancelled() {
                return Err(cancelled);
            }
            let outcome = match cancel {
                Some(cancel) => endpoint.dispatch_cancellable(event, cancel),
                None => endpoint.dispatch(event),
            };
            match outcome {
                Ok(value) => {
                    if cancellable && truthy(value.as_ref()) {
                        let value = value.unwrap_or_default();
                        let cancel = truthy(value.get("cancel"));
                        result = Some(value);
                        if cancel {
                            return Ok(result);
                        }
                    }
                }
                Err(error) => match cancelled() {
                    Some(cancelled) => return Err(cancelled),
                    None => self.report(&*endpoint, kind, error),
                },
            }
        }
        Ok(result)
    }
}
