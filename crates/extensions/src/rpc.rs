//! A symmetric JSON-RPC 2.0 peer over an `mcp::Transport` (newline-delimited JSON on stdio, the MCP framing).
//!
//! Both sides send requests: the host calls the extension (`event`, `functions/call`, ...) and the extension calls
//! the host (`ui/select`, `host/exec`, ...). Each outgoing request waits on its own channel with an optional timeout and
//! cancel token; a timeout or cancellation sends `notifications/cancelled` like MCP. Incoming requests run on their
//! own threads so a handler may itself call back across the connection.

use mcp::jsonrpc::{INTERNAL_ERROR, METHOD_NOT_FOUND, is_notification, is_request, is_response};
use mcp::{Incoming, Sink, Transport};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

type CancelCallback = Box<dyn FnOnce(&str) + Send>;

/// An `AbortSignal`: cancelling runs the registered callbacks once with the reason.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Mutex<(Option<String>, Vec<CancelCallback>)>>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels with `reason` (default "Aborted"); later calls do nothing.
    pub fn cancel(&self, reason: Option<&str>) {
        let (reason, callbacks) = {
            let mut inner = lock(&self.0);
            if inner.0.is_some() {
                return;
            }
            let reason = reason.unwrap_or("Aborted").to_string();
            inner.0 = Some(reason.clone());
            (reason, std::mem::take(&mut inner.1))
        };
        for callback in callbacks {
            callback(&reason);
        }
    }

    pub fn reason(&self) -> Option<String> {
        lock(&self.0).0.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        lock(&self.0).0.is_some()
    }

    /// Runs `callback` on cancellation, or now when already cancelled.
    pub fn on_cancel(&self, callback: impl FnOnce(&str) + Send + 'static) {
        let mut inner = lock(&self.0);
        match inner.0.clone() {
            Some(reason) => {
                drop(inner);
                callback(&reason);
            }
            None => inner.1.push(Box::new(callback)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    /// A JSON-RPC error response.
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// No response within the timeout, in milliseconds.
    Timeout(u64),
    /// The caller's cancel token fired.
    Cancelled(String),
    /// The connection closed (the process exited or was stopped).
    Closed(String),
    Other(String),
}

impl RpcError {
    pub fn remote(code: i64, message: impl Into<String>) -> Self {
        RpcError::Remote {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::Remote { message, .. } | RpcError::Closed(message) | RpcError::Other(message) => {
                f.write_str(message)
            }
            RpcError::Timeout(ms) => write!(f, "request timed out after {ms}ms"),
            RpcError::Cancelled(reason) => write!(f, "request cancelled: {reason}"),
        }
    }
}

impl std::error::Error for RpcError {}

/// Answers a request from the other side: (method, params, cancel token).
pub type RequestHandler = Arc<dyn Fn(&str, Option<&Value>, &CancelToken) -> Result<Value, RpcError> + Send + Sync>;
/// Receives a notification from the other side: (method, params).
pub type NotificationHandler = Arc<dyn Fn(&str, Option<&Value>) + Send + Sync>;

type CloseListener = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct State {
    closed: Option<String>,
    next_id: u64,
    pending: HashMap<String, (Value, Sender<Result<Value, RpcError>>)>,
    incoming: HashMap<String, CancelToken>,
}

struct Inner {
    transport: Arc<dyn Transport>,
    state: Mutex<State>,
    on_request: RequestHandler,
    on_notification: NotificationHandler,
    on_close: Mutex<Vec<CloseListener>>,
}

#[derive(Clone)]
pub struct Peer(Arc<Inner>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(id: &Value) -> String {
    id.to_string()
}

impl Peer {
    /// Starts the transport and begins routing its messages.
    pub fn start(
        transport: Arc<dyn Transport>,
        on_request: RequestHandler,
        on_notification: NotificationHandler,
    ) -> Result<Peer, RpcError> {
        let peer = Peer(Arc::new(Inner {
            transport: transport.clone(),
            state: Mutex::new(State {
                next_id: 1,
                ..State::default()
            }),
            on_request,
            on_notification,
            on_close: Mutex::new(Vec::new()),
        }));
        let weak: Weak<Inner> = Arc::downgrade(&peer.0);
        let sink: Sink = Arc::new(move |event| {
            if let Some(inner) = weak.upgrade() {
                Peer(inner).handle(event);
            }
        });
        transport.start(sink).map_err(|e| RpcError::Other(e.to_string()))?;
        Ok(peer)
    }

    /// Called once when the connection closes, with the reason.
    pub fn on_close(&self, listener: impl Fn(&str) + Send + Sync + 'static) {
        let closed = lock(&self.0.state).closed.clone();
        match closed {
            Some(reason) => listener(&reason),
            None => lock(&self.0.on_close).push(Arc::new(listener)),
        }
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.0.state).closed.is_some()
    }

    pub fn closed_reason(&self) -> Option<String> {
        lock(&self.0.state).closed.clone()
    }

    /// Sends a request and waits for its response. `timeout` of `None` or zero waits indefinitely.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, RpcError> {
        if let Some(reason) = cancel.and_then(CancelToken::reason) {
            return Err(RpcError::Cancelled(reason));
        }
        let (tx, rx) = mpsc::channel();
        let (id, message) = {
            let mut state = lock(&self.0.state);
            if let Some(reason) = &state.closed {
                return Err(RpcError::Closed(reason.clone()));
            }
            let id = json!(state.next_id);
            state.next_id += 1;
            state.pending.insert(key(&id), (id.clone(), tx));
            let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            (id, message)
        };
        let request_key = key(&id);
        if let Some(cancel) = cancel {
            let weak = Arc::downgrade(&self.0);
            let request_key = request_key.clone();
            cancel.on_cancel(move |reason| {
                if let Some(inner) = weak.upgrade() {
                    Peer(inner).abandon(&request_key, RpcError::Cancelled(reason.to_string()), Some(reason));
                }
            });
        }
        if let Err(error) = self.0.transport.send(&message) {
            self.abandon(&request_key, RpcError::Closed(error.to_string()), None);
        }
        let deadline = timeout.filter(|t| !t.is_zero()).map(|t| Instant::now() + t);
        let received = match deadline {
            Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Timeout) => {
                let ms = timeout.map_or(0, |t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX));
                self.abandon(&request_key, RpcError::Timeout(ms), Some("Request timed out"));
                // A response may have raced the timeout; the abandon sent whichever came first.
                rx.recv().unwrap_or(Err(RpcError::Timeout(ms)))
            }
            Err(RecvTimeoutError::Disconnected) => Err(RpcError::Closed("connection closed".into())),
        }
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        if let Some(reason) = self.closed_reason() {
            return Err(RpcError::Closed(reason));
        }
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.0
            .transport
            .send(&message)
            .map_err(|e| RpcError::Closed(e.to_string()))
    }

    /// Fails an outgoing request and, when `reason` is given, tells the other side to stop working on it.
    fn abandon(&self, request_key: &str, error: RpcError, reason: Option<&str>) {
        let Some((id, tx)) = lock(&self.0.state).pending.remove(request_key) else {
            return;
        };
        let _ = tx.send(Err(error));
        if let Some(reason) = reason {
            let _ = self.notify("notifications/cancelled", json!({ "requestId": id, "reason": reason }));
        }
    }

    /// Closes the transport (which stops the process) and fails everything in flight.
    pub fn close(&self, reason: &str) {
        self.mark_closed(reason);
        self.0.transport.close();
    }

    fn mark_closed(&self, reason: &str) {
        let (pending, incoming) = {
            let mut state = lock(&self.0.state);
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(reason.to_string());
            let pending: Vec<_> = state.pending.drain().map(|(_, p)| p).collect();
            let incoming: Vec<_> = state.incoming.drain().map(|(_, c)| c).collect();
            (pending, incoming)
        };
        for (_, tx) in pending {
            let _ = tx.send(Err(RpcError::Closed(reason.to_string())));
        }
        for token in incoming {
            token.cancel(Some(reason));
        }
        let listeners = std::mem::take(&mut *lock(&self.0.on_close));
        for listener in listeners {
            listener(reason);
        }
    }

    fn handle(&self, event: Incoming) {
        match event {
            Incoming::Message(message) => self.handle_message(message),
            // Malformed lines are dropped; the stream itself is still usable.
            Incoming::Error(_) => {}
            Incoming::Closed => self.mark_closed("extension process exited"),
        }
    }

    fn handle_message(&self, message: Value) {
        if is_response(&message) {
            let entry = lock(&self.0.state).pending.remove(&key(&message["id"]));
            if let Some((_, tx)) = entry {
                let outcome = match message.get("error") {
                    Some(error) => Err(RpcError::Remote {
                        code: error["code"].as_i64().unwrap_or(INTERNAL_ERROR),
                        message: error["message"].as_str().unwrap_or_default().to_string(),
                        data: error.get("data").cloned(),
                    }),
                    None => Ok(message["result"].clone()),
                };
                let _ = tx.send(outcome);
            }
        } else if is_request(&message) {
            let peer = self.clone();
            std::thread::spawn(move || peer.handle_request(message));
        } else if is_notification(&message) {
            let method = message["method"].as_str().unwrap_or_default();
            if method == "notifications/cancelled" {
                let id = message
                    .get("params")
                    .and_then(|p| p.get("requestId"))
                    .cloned()
                    .unwrap_or_default();
                let reason = message["params"]["reason"].as_str().unwrap_or("cancelled").to_string();
                let token = lock(&self.0.state).incoming.get(&key(&id)).cloned();
                if let Some(token) = token {
                    token.cancel(Some(&reason));
                }
                return;
            }
            (self.0.on_notification)(method, message.get("params"));
        }
    }

    fn handle_request(&self, message: Value) {
        let id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or_default();
        let token = CancelToken::new();
        lock(&self.0.state).incoming.insert(key(&id), token.clone());
        let outcome = (self.0.on_request)(method, message.get("params"), &token);
        lock(&self.0.state).incoming.remove(&key(&id));
        let reply = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(RpcError::Remote { code, message, data }) => {
                let mut error = json!({ "code": code, "message": message });
                if let Some(data) = data {
                    error["data"] = data;
                }
                json!({ "jsonrpc": "2.0", "id": id, "error": error })
            }
            Err(other) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": INTERNAL_ERROR, "message": other.to_string() } })
            }
        };
        let _ = self.0.transport.send(&reply);
    }
}

/// The error a host returns for a method it does not implement.
pub fn method_not_found(method: &str) -> RpcError {
    RpcError::remote(METHOD_NOT_FOUND, format!("Method not found: {method}"))
}
