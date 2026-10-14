//! The MCP client (`client.ts`): initialize handshake, requests with timeouts, progress and cancellation,
//! paginated lists, tool calls, resources, and the server-to-client requests and notifications.

use crate::jsonrpc::{self, Error, INTERNAL_ERROR, METHOD_NOT_FOUND, id_text, is_id};
use crate::{Incoming, Sink, Transport};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [LATEST_PROTOCOL_VERSION, "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LIST_PAGES: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Idle,
    Connecting,
    Connected,
    Closed,
}

impl ConnState {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnState::Idle => "idle",
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::Closed => "closed",
        }
    }
}

#[derive(Default, Clone)]
pub struct ClientOptions {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
    pub capabilities: Map<String, Value>,
    pub protocol_version: Option<String>,
    /// `Duration::ZERO` disables the timeout. Default: 30 s.
    pub request_timeout: Option<Duration>,
    /// Answered to `roots/list`; also announces the `roots` capability.
    pub roots: Option<Vec<Value>>,
}

pub type ProgressListener = Arc<dyn Fn(&Value) + Send + Sync>;

#[derive(Default, Clone)]
pub struct RequestOptions {
    /// `Duration::ZERO` disables the timeout. Default: the client's.
    pub timeout: Option<Duration>,
    pub cancel: Option<CancelToken>,
    /// Asks the server for progress (`_meta.progressToken`); each notification renews the timeout.
    pub on_progress: Option<ProgressListener>,
}

type CancelCallback = Box<dyn FnOnce(&str) + Send>;

/// An `AbortSignal`: cancelling runs the registered callbacks once with the reason.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Mutex<(Option<String>, Vec<CancelCallback>)>>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels with `reason` (default `Aborted`); later calls do nothing.
    pub fn cancel(&self, reason: Option<&str>) {
        let callbacks = {
            let mut inner = lock(&self.0);
            if inner.0.is_some() {
                return;
            }
            let reason = reason.unwrap_or("Aborted").to_string();
            inner.0 = Some(reason);
            std::mem::take(&mut inner.1)
        };
        let reason = self.reason().unwrap_or_default();
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

    fn on_cancel(&self, callback: CancelCallback) {
        let mut inner = lock(&self.0);
        match inner.0.clone() {
            Some(reason) => {
                drop(inner);
                callback(&reason);
            }
            None => inner.1.push(callback),
        }
    }
}

/// Handles a server-to-client request; `Ok(Value::Null)` answers `{}`.
pub type RequestHandler = Arc<dyn Fn(Option<&Value>, &CancelToken) -> Result<Value, Error> + Send + Sync>;
type NotificationListener = Arc<dyn Fn(Option<&Value>) + Send + Sync>;
type ErrorListener = Arc<dyn Fn(&str) + Send + Sync>;

struct Pending {
    id: Value,
    tx: Sender<Result<Value, Error>>,
    timeout: Duration,
    deadline: Option<Instant>,
    on_progress: Option<ProgressListener>,
    progress_token: Option<String>,
}

struct State {
    conn: ConnState,
    transport: Option<Arc<dyn Transport>>,
    next_id: u64,
    server_info: Option<Value>,
    capabilities: Option<Value>,
    instructions: Option<String>,
    protocol_version: Option<String>,
    pending: HashMap<String, Pending>,
    progress: HashMap<String, String>,
    incoming: HashMap<String, CancelToken>,
}

struct Inner {
    options: ClientOptions,
    state: Mutex<State>,
    handlers: Mutex<HashMap<String, RequestHandler>>,
    notification_listeners: Mutex<HashMap<String, Vec<NotificationListener>>>,
    error_listeners: Mutex<Vec<ErrorListener>>,
    close_listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Clone)]
pub struct Client(Arc<Inner>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(id: &Value) -> String {
    id.to_string()
}

fn deadline(timeout: Duration) -> Option<Instant> {
    (!timeout.is_zero()).then(|| Instant::now() + timeout)
}

fn validate_initialize_result(value: &Value) -> Result<(), Error> {
    let info = value.get("serverInfo");
    let ok = value.get("protocolVersion").is_some_and(Value::is_string)
        && value.get("capabilities").is_some_and(Value::is_object)
        && info.is_some_and(Value::is_object)
        && info.and_then(|i| i.get("name")).is_some_and(Value::is_string)
        && info.and_then(|i| i.get("version")).is_some_and(Value::is_string)
        && value.get("instructions").is_none_or(Value::is_string);
    if ok {
        Ok(())
    } else {
        Err(Error::invalid("Invalid MCP initialize result"))
    }
}

type Page = (Vec<Value>, Option<String>);

fn validate_list_page(
    method: &str,
    key: &str,
    value: &Value,
    is_item: fn(&Map<String, Value>) -> bool,
) -> Result<Page, Error> {
    let Some(items) = value.get(key).and_then(Value::as_array) else {
        return Err(Error::invalid(format!("Invalid MCP {method} result")));
    };
    if !items.iter().all(|item| item.as_object().is_some_and(is_item)) {
        return Err(Error::invalid(format!("Invalid entry in MCP {method} result")));
    }
    // Some servers end pagination with `null` or `""` instead of omitting the cursor.
    let cursor = match value.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(Error::invalid(format!("Invalid MCP {method} cursor"))),
    };
    Ok((items.clone(), cursor))
}

fn optional_string(item: &Map<String, Value>, key: &str) -> bool {
    item.get(key).is_none_or(Value::is_string)
}

fn is_tool(tool: &Map<String, Value>) -> bool {
    tool.get("name").is_some_and(Value::is_string) && tool.get("inputSchema").is_some_and(Value::is_object)
}

// `name` is required by the spec, but some servers omit it; the URI stands in.
fn is_resource(resource: &Map<String, Value>) -> bool {
    resource.get("uri").is_some_and(Value::is_string) && optional_string(resource, "name")
}

fn is_resource_template(template: &Map<String, Value>) -> bool {
    template.get("uriTemplate").is_some_and(Value::is_string) && optional_string(template, "name")
}

/// `{ ...item, name: item.name ?? item[fallback] }`.
fn with_name(mut item: Value, fallback: &str) -> Value {
    if let Some(map) = item.as_object_mut() {
        let name = match map.get("name") {
            Some(name) if !name.is_null() => name.clone(),
            _ => map.get(fallback).cloned().unwrap_or(Value::Null),
        };
        map.insert("name".into(), name);
    }
    item
}

fn page_value(list_key: &str, items: Vec<Value>, cursor: Option<String>) -> Value {
    let mut page = Map::new();
    page.insert(list_key.into(), Value::Array(items));
    if let Some(cursor) = cursor {
        page.insert("nextCursor".into(), Value::String(cursor));
    }
    Value::Object(page)
}

fn validate_read_resource_result(value: Value) -> Result<Value, Error> {
    let Some(contents) = value.get("contents").and_then(Value::as_array) else {
        return Err(Error::invalid("Invalid MCP resources/read result"));
    };
    let valid = |c: &Value| {
        c.get("uri").is_some_and(Value::is_string)
            && (c.get("text").is_some_and(Value::is_string) || c.get("blob").is_some_and(Value::is_string))
    };
    if !contents.iter().all(valid) {
        return Err(Error::invalid("Invalid contents in MCP resources/read result"));
    }
    Ok(value)
}

/// `content` is required by the spec, but servers that only return `structuredContent` omit it.
fn validate_call_tool_result(value: Value) -> Result<Value, Error> {
    let Value::Object(mut map) = value else {
        return Err(Error::invalid("Invalid MCP tools/call result"));
    };
    if map.get("content").is_some_and(|c| !c.is_array()) {
        return Err(Error::invalid("Invalid MCP tools/call result"));
    }
    if map.get("structuredContent").is_some_and(|c| !c.is_object()) {
        return Err(Error::invalid("Invalid MCP tools/call structured content"));
    }
    if !map.contains_key("content") {
        map.insert("content".into(), json!([]));
    }
    Ok(Value::Object(map))
}

impl Client {
    pub fn new(options: ClientOptions) -> Self {
        let mut handlers: HashMap<String, RequestHandler> = HashMap::new();
        handlers.insert("ping".into(), Arc::new(|_, _| Ok(json!({}))));
        if let Some(roots) = options.roots.clone() {
            handlers.insert("roots/list".into(), Arc::new(move |_, _| Ok(json!({ "roots": roots }))));
        }
        Client(Arc::new(Inner {
            options,
            state: Mutex::new(State {
                conn: ConnState::Idle,
                transport: None,
                next_id: 1,
                server_info: None,
                capabilities: None,
                instructions: None,
                protocol_version: None,
                pending: HashMap::new(),
                progress: HashMap::new(),
                incoming: HashMap::new(),
            }),
            handlers: Mutex::new(handlers),
            notification_listeners: Mutex::new(HashMap::new()),
            error_listeners: Mutex::new(Vec::new()),
            close_listeners: Mutex::new(Vec::new()),
        }))
    }

    pub fn connection_state(&self) -> ConnState {
        lock(&self.0.state).conn
    }

    pub fn server_info(&self) -> Option<Value> {
        lock(&self.0.state).server_info.clone()
    }

    pub fn server_capabilities(&self) -> Option<Value> {
        lock(&self.0.state).capabilities.clone()
    }

    pub fn instructions(&self) -> Option<String> {
        lock(&self.0.state).instructions.clone()
    }

    pub fn protocol_version(&self) -> Option<String> {
        lock(&self.0.state).protocol_version.clone()
    }

    pub fn set_request_handler(&self, method: &str, handler: RequestHandler) {
        lock(&self.0.handlers).insert(method.into(), handler);
    }

    pub fn on_notification(&self, method: &str, listener: impl Fn(Option<&Value>) + Send + Sync + 'static) {
        lock(&self.0.notification_listeners)
            .entry(method.into())
            .or_default()
            .push(Arc::new(listener));
    }

    pub fn on_error(&self, listener: impl Fn(&str) + Send + Sync + 'static) {
        lock(&self.0.error_listeners).push(Arc::new(listener));
    }

    /// Called once when the connection closes, whether the transport dropped or `close()` was called.
    pub fn on_close(&self, listener: impl Fn() + Send + Sync + 'static) {
        lock(&self.0.close_listeners).push(Arc::new(listener));
    }

    /// Starts the transport, runs the `initialize` handshake, and returns the server's initialize result.
    pub fn connect(&self, transport: Arc<dyn Transport>) -> Result<Value, Error> {
        {
            let mut state = lock(&self.0.state);
            if state.conn != ConnState::Idle {
                return Err(Error::Other(format!(
                    "Cannot connect MCP client in {} state",
                    state.conn.as_str()
                )));
            }
            state.conn = ConnState::Connecting;
            state.transport = Some(transport.clone());
        }
        let weak: Weak<Inner> = Arc::downgrade(&self.0);
        let sink: Sink = Arc::new(move |event| {
            if let Some(inner) = weak.upgrade() {
                Client(inner).handle_incoming(event);
            }
        });
        let result = self.handshake(&transport, sink);
        if result.is_err() {
            self.close();
        }
        result
    }

    fn handshake(&self, transport: &Arc<dyn Transport>, sink: Sink) -> Result<Value, Error> {
        transport.start(sink)?;
        let options = &self.0.options;
        let mut capabilities = options.capabilities.clone();
        if options.roots.is_some() && !capabilities.contains_key("roots") {
            capabilities.insert("roots".into(), json!({}));
        }
        let mut client_info = json!({ "name": options.name, "version": options.version });
        if let Some(title) = &options.title {
            client_info["title"] = json!(title);
        }
        let params = json!({
            "protocolVersion": options.protocol_version.as_deref().unwrap_or(LATEST_PROTOCOL_VERSION),
            "capabilities": capabilities,
            "clientInfo": client_info,
        });
        let result = self.request_internal("initialize", Some(params), RequestOptions::default(), true)?;
        validate_initialize_result(&result)?;
        let version = result["protocolVersion"].as_str().unwrap_or_default().to_string();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version.as_str()) {
            return Err(Error::Other(format!(
                "MCP server selected unsupported protocol version {version}"
            )));
        }
        {
            let mut state = lock(&self.0.state);
            state.protocol_version = Some(version.clone());
            state.server_info = result.get("serverInfo").cloned();
            state.capabilities = result.get("capabilities").cloned();
            state.instructions = result.get("instructions").and_then(Value::as_str).map(str::to_string);
        }
        transport.set_protocol_version(&version);
        self.notify_internal("notifications/initialized", None, true)?;
        lock(&self.0.state).conn = ConnState::Connected;
        Ok(result)
    }

    pub fn request(&self, method: &str, params: Option<Value>, options: RequestOptions) -> Result<Value, Error> {
        self.request_internal(method, params, options, false)
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), Error> {
        self.notify_internal(method, params, false)
    }

    pub fn ping(&self, options: RequestOptions) -> Result<(), Error> {
        self.request("ping", None, options).map(drop)
    }

    pub fn list_tools(&self, options: RequestOptions) -> Result<Vec<Value>, Error> {
        self.list_all("tools/list", "tools", is_tool, options)
    }

    /// Every resource, following `nextCursor` through all pages.
    pub fn list_resources(&self, options: RequestOptions) -> Result<Vec<Value>, Error> {
        let items = self.list_all("resources/list", "resources", is_resource, options)?;
        Ok(items.into_iter().map(|item| with_name(item, "uri")).collect())
    }

    /// One page of resources: `{ resources, nextCursor? }`.
    pub fn list_resources_page(&self, cursor: Option<&str>, options: RequestOptions) -> Result<Value, Error> {
        let (items, next) = self.list_page("resources/list", "resources", is_resource, cursor, options)?;
        let items = items.into_iter().map(|item| with_name(item, "uri")).collect();
        Ok(page_value("resources", items, next))
    }

    pub fn list_resource_templates(&self, options: RequestOptions) -> Result<Vec<Value>, Error> {
        let items = self.list_all(
            "resources/templates/list",
            "resourceTemplates",
            is_resource_template,
            options,
        )?;
        Ok(items.into_iter().map(|item| with_name(item, "uriTemplate")).collect())
    }

    /// One page of resource templates: `{ resourceTemplates, nextCursor? }`.
    pub fn list_resource_templates_page(&self, cursor: Option<&str>, options: RequestOptions) -> Result<Value, Error> {
        let (items, next) = self.list_page(
            "resources/templates/list",
            "resourceTemplates",
            is_resource_template,
            cursor,
            options,
        )?;
        let items = items.into_iter().map(|item| with_name(item, "uriTemplate")).collect();
        Ok(page_value("resourceTemplates", items, next))
    }

    pub fn read_resource(&self, uri: &str, options: RequestOptions) -> Result<Value, Error> {
        validate_read_resource_result(self.request("resources/read", Some(json!({ "uri": uri })), options)?)
    }

    pub fn call_tool(&self, name: &str, args: Option<Value>, options: RequestOptions) -> Result<Value, Error> {
        let mut params = json!({ "name": name });
        if let Some(args) = args {
            params["arguments"] = args;
        }
        validate_call_tool_result(self.request("tools/call", Some(params), options)?)
    }

    fn list_page(
        &self,
        method: &str,
        list_key: &str,
        is_item: fn(&Map<String, Value>) -> bool,
        cursor: Option<&str>,
        options: RequestOptions,
    ) -> Result<Page, Error> {
        let params = cursor.map(|cursor| json!({ "cursor": cursor }));
        validate_list_page(method, list_key, &self.request(method, params, options)?, is_item)
    }

    fn list_all(
        &self,
        method: &str,
        list_key: &str,
        is_item: fn(&Map<String, Value>) -> bool,
        options: RequestOptions,
    ) -> Result<Vec<Value>, Error> {
        let mut items = Vec::new();
        let mut cursors = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let (page, next) = self.list_page(method, list_key, is_item, cursor.as_deref(), options.clone())?;
            items.extend(page);
            let Some(next) = next else { return Ok(items) };
            if !cursors.insert(next.clone()) {
                return Err(Error::Other(format!("MCP {method} returned duplicate cursor: {next}")));
            }
            cursor = Some(next);
        }
        Err(Error::Other(format!("MCP {method} exceeded {MAX_LIST_PAGES} pages")))
    }

    /// Closes the connection: in-flight requests fail with `McpConnectionClosedError`, then the transport closes.
    pub fn close(&self) {
        let transport = lock(&self.0.state).transport.take();
        self.mark_closed(Error::closed());
        if let Some(transport) = transport {
            transport.close();
        }
    }

    fn require_transport(&self, allow_connecting: bool) -> Result<Arc<dyn Transport>, Error> {
        let state = lock(&self.0.state);
        match &state.transport {
            Some(t)
                if state.conn == ConnState::Connected || (allow_connecting && state.conn == ConnState::Connecting) =>
            {
                Ok(t.clone())
            }
            _ => Err(Error::ConnectionClosed(format!(
                "MCP client is {}",
                state.conn.as_str()
            ))),
        }
    }

    fn request_internal(
        &self,
        method: &str,
        params: Option<Value>,
        options: RequestOptions,
        allow_connecting: bool,
    ) -> Result<Value, Error> {
        let transport = self.require_transport(allow_connecting)?;
        if options.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(Error::Aborted);
        }
        let timeout = options
            .timeout
            .or(self.0.options.request_timeout)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        // The spec forbids cancelling `initialize`.
        let cancellable = method != "initialize";
        let (tx, rx) = mpsc::channel();
        let (id, message) = {
            let mut state = lock(&self.0.state);
            let id = json!(state.next_id);
            state.next_id += 1;
            let progress_token = options.on_progress.is_some().then(|| key(&id));
            let params = match &progress_token {
                None => params,
                Some(_) => {
                    let mut map = match params {
                        Some(Value::Object(map)) => map,
                        _ => Map::new(),
                    };
                    let mut meta = match map.get("_meta") {
                        Some(Value::Object(meta)) => meta.clone(),
                        _ => Map::new(),
                    };
                    meta.insert("progressToken".into(), id.clone());
                    map.insert("_meta".into(), Value::Object(meta));
                    Some(Value::Object(map))
                }
            };
            let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
            if let Some(params) = params {
                message["params"] = params;
            }
            if let Some(token) = &progress_token {
                state.progress.insert(token.clone(), key(&id));
            }
            state.pending.insert(
                key(&id),
                Pending {
                    id: id.clone(),
                    tx,
                    timeout,
                    deadline: deadline(timeout),
                    on_progress: options.on_progress.clone(),
                    progress_token,
                },
            );
            (id, message)
        };
        let request_key = key(&id);
        if let Some(cancel) = &options.cancel {
            let weak = Arc::downgrade(&self.0);
            let request_key = request_key.clone();
            cancel.on_cancel(Box::new(move |reason| {
                if let Some(inner) = weak.upgrade() {
                    Client(inner).cancel_pending(&request_key, Error::Aborted, cancellable, Some(reason));
                }
            }));
        }
        if let Err(error) = transport.send(&message) {
            self.cancel_pending(&request_key, error, false, None);
        }
        loop {
            let current = lock(&self.0.state).pending.get(&request_key).map(|p| p.deadline);
            let received = match current {
                Some(Some(deadline)) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                _ => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Timeout) => {
                    let expired = lock(&self.0.state)
                        .pending
                        .get(&request_key)
                        .and_then(|p| p.deadline)
                        .is_some_and(|d| d <= Instant::now());
                    if expired {
                        let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                        self.cancel_pending(&request_key, Error::Timeout(ms), cancellable, Some("Request timed out"));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(Error::closed()),
            }
        }
    }

    fn notify_internal(&self, method: &str, params: Option<Value>, allow_connecting: bool) -> Result<(), Error> {
        let transport = self.require_transport(allow_connecting)?;
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        transport.send(&message)
    }

    fn remove_pending(state: &mut State, request_key: &str) -> Option<Pending> {
        let entry = state.pending.remove(request_key)?;
        if let Some(token) = &entry.progress_token {
            state.progress.remove(token);
        }
        Some(entry)
    }

    fn cancel_pending(&self, request_key: &str, error: Error, notify_server: bool, reason: Option<&str>) {
        let (entry, transport) = {
            let mut state = lock(&self.0.state);
            let Some(entry) = Self::remove_pending(&mut state, request_key) else {
                return;
            };
            (entry, state.transport.clone())
        };
        let _ = entry.tx.send(Err(error));
        if let (true, Some(transport)) = (notify_server, transport) {
            let mut params = json!({ "requestId": entry.id });
            if let Some(reason) = reason.filter(|r| !r.is_empty()) {
                params["reason"] = json!(reason);
            }
            let notice = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": params });
            if let Err(error) = transport.send(&notice) {
                self.emit_error(&error.to_string());
            }
        }
    }

    /// Idempotent: fails in-flight requests, cancels server requests being served, and flips the state.
    fn mark_closed(&self, error: Error) {
        let (was_closed, pending, incoming) = {
            let mut state = lock(&self.0.state);
            let was_closed = state.conn == ConnState::Closed;
            state.conn = ConnState::Closed;
            state.progress.clear();
            let pending: Vec<Pending> = state.pending.drain().map(|(_, p)| p).collect();
            let incoming: Vec<CancelToken> = state.incoming.drain().map(|(_, c)| c).collect();
            (was_closed, pending, incoming)
        };
        // Close listeners run before the waiting requests wake, as in TypeScript, where they run synchronously and
        // the rejected requests continue in later microtasks: a caller that sees its request fail also sees the close.
        if !was_closed {
            let listeners = lock(&self.0.close_listeners).clone();
            for listener in listeners {
                listener();
            }
        }
        for entry in pending {
            let _ = entry.tx.send(Err(error.clone()));
        }
        for token in incoming {
            token.cancel(Some(&error.to_string()));
        }
    }

    fn emit_error(&self, message: &str) {
        let listeners = lock(&self.0.error_listeners).clone();
        for listener in listeners {
            listener(message);
        }
    }

    fn handle_incoming(&self, event: Incoming) {
        // A closed client has dropped its transport; like the TypeScript client's disposed listeners, it hears nothing.
        if lock(&self.0.state).transport.is_none() {
            return;
        }
        match event {
            Incoming::Message(message) => self.handle_message(message),
            Incoming::Error(error) => self.emit_error(&error),
            Incoming::Closed => self.mark_closed(Error::closed()),
        }
    }

    fn handle_message(&self, message: Value) {
        if jsonrpc::is_response(&message) {
            self.handle_response(&message);
        } else if jsonrpc::is_request(&message) {
            let client = self.clone();
            std::thread::spawn(move || client.handle_request(message));
        } else if jsonrpc::is_notification(&message) {
            self.handle_notification(message["method"].as_str().unwrap_or_default(), message.get("params"));
        } else {
            self.emit_error("Received invalid JSON-RPC message");
        }
    }

    fn handle_response(&self, message: &Value) {
        let id = &message["id"];
        let entry = Self::remove_pending(&mut lock(&self.0.state), &key(id));
        let Some(entry) = entry else {
            self.emit_error(&format!("Received response for unknown MCP request {}", id_text(id)));
            return;
        };
        let outcome = match message.get("error") {
            Some(error) => Err(Error::Mcp {
                code: error["code"].as_i64().unwrap_or_default(),
                message: error["message"].as_str().unwrap_or_default().to_string(),
                data: error.get("data").cloned(),
            }),
            None => Ok(message["result"].clone()),
        };
        let _ = entry.tx.send(outcome);
    }

    fn handle_request(&self, message: Value) {
        let Some(transport) = lock(&self.0.state).transport.clone() else {
            return;
        };
        let id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or_default();
        let handler = lock(&self.0.handlers).get(method).cloned();
        let Some(handler) = handler else {
            let error = json!({ "code": METHOD_NOT_FOUND, "message": format!("Method not found: {method}") });
            if let Err(e) = transport.send(&json!({ "jsonrpc": "2.0", "id": id, "error": error })) {
                self.emit_error(&e.to_string());
            }
            return;
        };
        let token = CancelToken::new();
        lock(&self.0.state).incoming.insert(key(&id), token.clone());
        let reply = match handler(message.get("params"), &token) {
            Ok(Value::Null) => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(Error::Mcp { code, message, data }) => {
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
        if let Err(e) = transport.send(&reply) {
            self.emit_error(&e.to_string());
        }
        lock(&self.0.state).incoming.remove(&key(&id));
    }

    fn handle_notification(&self, method: &str, params: Option<&Value>) {
        match method {
            "notifications/progress" => self.handle_progress(params),
            "notifications/cancelled" => {
                if let Some(id) = params.and_then(|p| p.get("requestId")).filter(|id| is_id(id)) {
                    let token = lock(&self.0.state).incoming.get(&key(id)).cloned();
                    let reason = params.and_then(|p| p.get("reason")).map(id_text);
                    if let Some(token) = token {
                        token.cancel(reason.as_deref());
                    }
                }
            }
            _ => {}
        }
        let listeners = lock(&self.0.notification_listeners)
            .get(method)
            .cloned()
            .unwrap_or_default();
        for listener in listeners {
            listener(params);
        }
    }

    fn handle_progress(&self, params: Option<&Value>) {
        let Some(params) = params.filter(|p| p.is_object()) else {
            return;
        };
        let Some(token) = params.get("progressToken").filter(|t| is_id(t)) else {
            return;
        };
        if !params.get("progress").is_some_and(Value::is_number) {
            return;
        }
        let listener = {
            let mut state = lock(&self.0.state);
            let Some(request_key) = state.progress.get(&key(token)).cloned() else {
                return;
            };
            let Some(entry) = state.pending.get_mut(&request_key) else {
                return;
            };
            entry.deadline = deadline(entry.timeout);
            entry.on_progress.clone()
        };
        if let Some(listener) = listener {
            listener(params);
        }
    }
}
