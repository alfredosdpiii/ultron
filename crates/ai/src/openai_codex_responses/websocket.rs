//! The Codex WebSocket transport (`processWebSocketStream` and its helpers in api/openai-codex-responses.ts): one
//! `response.create` message per request on a socket cached per session and ChatGPT account (reused while open,
//! closed after five idle minutes or before the backend's 55-minute connection limit, a busy one never shared), the
//! response events read as SSE events are, and in `websocket-cached`/`auto` mode only the input added since the
//! previous response sent, as `previous_response_id` on the same connection. The debug counters and the sessions
//! that fell back to SSE are kept here too.

use super::{CodexError, aborted_error, map_codex_event};
use crate::http::{AbortSignal, HeaderMap, Transport, cancelled, is_aborted};
use crate::json::parse_json;
use crate::openai_responses::{ConvertOptions, OPENAI_TOOL_CALL_PROVIDERS, ResponsesRun, TierPricing, ToolOptions};
use crate::options::now_ms;
use crate::types::{AssistantMessageEvent, Message, Model, TranscriptContext};
use crate::websocket::{NO_WEBSOCKET, WebSocketConnection, WsEvent};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub(super) const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const SESSION_WEBSOCKET_MAX_AGE_MS: i64 = 55 * 60 * 1000;
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS: u64 = 15_000;
pub(super) const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
pub(super) const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;

/// `OpenAICodexWebSocketDebugStats`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delta_input_items: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub websocket_fallback_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_web_socket_error: Option<String>,
}

/// `CachedWebSocketContinuationState`.
#[derive(Clone)]
struct Continuation {
    last_request_body: Map<String, Value>,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

/// `CachedWebSocketConnection`: the socket is out of the cache while a request uses it (`busy`).
struct Entry {
    id: u64,
    socket: Option<Box<dyn WebSocketConnection>>,
    busy: bool,
    created_at: i64,
    continuation: Option<Continuation>,
    /// Bumped whenever the idle timer is cleared or rescheduled; a timer whose generation is gone does nothing.
    idle_generation: u64,
}

#[derive(Default)]
struct WebSocketState {
    stats: HashMap<String, OpenAICodexWebSocketDebugStats>,
    sse_fallback_sessions: HashSet<String>,
    /// `websocketSessionCache`: session id, then ChatGPT account id.
    cache: HashMap<String, HashMap<String, Entry>>,
    next_id: u64,
}

static WEBSOCKET_STATE: LazyLock<Mutex<WebSocketState>> = LazyLock::new(Mutex::default);
static CLOCK_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

fn state() -> std::sync::MutexGuard<'static, WebSocketState> {
    WEBSOCKET_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `Date.now()` for the connection age limit.
fn clock() -> i64 {
    now_ms() + CLOCK_OFFSET_MS.load(Ordering::SeqCst)
}

/// Moves the clock the connection age limit is measured on (tests: what faking `Date.now` does upstream).
pub fn advance_openai_codex_websocket_clock(ms: i64) {
    CLOCK_OFFSET_MS.fetch_add(ms, Ordering::SeqCst);
}

pub fn get_openai_codex_websocket_debug_stats(session_id: &str) -> Option<OpenAICodexWebSocketDebugStats> {
    state().stats.get(session_id).cloned()
}

pub fn reset_openai_codex_websocket_debug_stats(session_id: Option<&str>) {
    let mut state = state();
    match session_id {
        Some(id) => {
            state.stats.remove(id);
            state.sse_fallback_sessions.remove(id);
        }
        None => {
            state.stats.clear();
            state.sse_fallback_sessions.clear();
        }
    }
}

/// Close a socket in the background (`closeWebSocketSilently` from code that cannot wait); without a runtime the
/// connection is just dropped.
fn close_detached(mut socket: Box<dyn WebSocketConnection>, reason: &'static str) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { socket.close(1000, reason).await });
    }
}

/// `closeOpenAICodexWebSocketSessions`: close the cached sockets of `session_id`, or of every session.
pub fn close_openai_codex_websocket_sessions(session_id: Option<&str>) {
    let removed: Vec<Entry> = {
        let mut state = state();
        match session_id {
            Some(id) => state
                .cache
                .remove(id)
                .map(|accounts| accounts.into_values().collect())
                .unwrap_or_default(),
            None => state
                .cache
                .drain()
                .flat_map(|(_, accounts)| accounts.into_values())
                .collect(),
        }
    };
    for entry in removed {
        if let Some(socket) = entry.socket {
            close_detached(socket, "debug_close");
        }
    }
}

pub(super) fn is_websocket_sse_fallback_active(session_id: Option<&str>) -> bool {
    session_id.is_some_and(|id| state().sse_fallback_sessions.contains(id))
}

pub(super) fn record_websocket_sse_fallback(session_id: Option<&str>) {
    let Some(id) = session_id else { return };
    let mut state = state();
    let active = state.sse_fallback_sessions.contains(id);
    let stats = state.stats.entry(id.to_string()).or_default();
    stats.sse_fallbacks += 1;
    stats.websocket_fallback_active = Some(active);
}

pub(super) fn record_websocket_failure(session_id: Option<&str>, message: &str) {
    let Some(id) = session_id else { return };
    let mut state = state();
    state.sse_fallback_sessions.insert(id.to_string());
    let stats = state.stats.entry(id.to_string()).or_default();
    stats.websocket_failures += 1;
    stats.last_web_socket_error = Some(message.to_string());
    stats.websocket_fallback_active = Some(true);
}

/// `resolveCodexWebSocketUrl`: the SSE URL with `wss:`/`ws:`.
pub(super) fn resolve_codex_websocket_url(base_url: &str) -> String {
    let url = super::resolve_codex_url(base_url);
    let url = reqwest::Url::parse(&url).map_or(url, |parsed| parsed.to_string());
    if let Some(rest) = url.strip_prefix("https:") {
        format!("wss:{rest}")
    } else if let Some(rest) = url.strip_prefix("http:") {
        format!("ws:{rest}")
    } else {
        url
    }
}

/// `uuidv7()`.
pub(super) fn uuid_v7() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    let ms = (now_ms() as u64).to_be_bytes();
    bytes[..6].copy_from_slice(&ms[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// `connectWebSocket`: open, or fail with the connect timeout (default 15 s; 0 waits forever) or the abort.
async fn connect(
    transport: &dyn Transport,
    url: &str,
    headers: &HeaderMap,
    signal: Option<&AbortSignal>,
    connect_timeout_ms: Option<u64>,
) -> Result<Box<dyn WebSocketConnection>, CodexError> {
    if !transport.websocket_available() {
        return Err(CodexError::Plain(NO_WEBSOCKET.into()));
    }
    if is_aborted(signal) {
        return Err(aborted_error());
    }
    let timeout_ms = connect_timeout_ms.unwrap_or(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);
    let deadline = async {
        match timeout_ms {
            0 => std::future::pending().await,
            ms => tokio::time::sleep(Duration::from_millis(ms)).await,
        }
    };
    tokio::select! {
        opened = transport.websocket(url.to_string(), headers.clone()) => opened.map_err(CodexError::Plain),
        () = cancelled(signal) => Err(aborted_error()),
        () = deadline => Err(CodexError::Plain(format!("WebSocket connect timeout after {timeout_ms}ms"))),
    }
}

/// Where an acquired socket goes back to: its cache slot, or nowhere (a one-off socket, closed after use).
struct Slot {
    session: String,
    account: String,
    id: u64,
}

struct Acquired {
    socket: Box<dyn WebSocketConnection>,
    slot: Option<Slot>,
    reused: bool,
    continuation: Option<Continuation>,
}

/// What the cache had for a session and account.
enum Cached {
    None,
    /// Past the connection age limit: closed, then a new one.
    Expired(Box<dyn WebSocketConnection>),
    /// Idle: reused if it is still open.
    Idle(Box<dyn WebSocketConnection>, u64, Option<Continuation>),
    /// In use by another request: this one gets a socket of its own.
    Busy,
}

/// `acquireWebSocket`.
async fn acquire(
    transport: &dyn Transport,
    url: &str,
    headers: &HeaderMap,
    session: Option<&str>,
    account: &str,
    signal: Option<&AbortSignal>,
    connect_timeout_ms: Option<u64>,
) -> Result<Acquired, CodexError> {
    let Some(session) = session else {
        let socket = connect(transport, url, headers, signal, connect_timeout_ms).await?;
        return Ok(Acquired {
            socket,
            slot: None,
            reused: false,
            continuation: None,
        });
    };
    let cached = {
        let mut state = state();
        let accounts = state.cache.get_mut(session);
        match accounts.and_then(|accounts| accounts.get_mut(account)) {
            None => Cached::None,
            Some(entry) => {
                entry.idle_generation += 1;
                if entry.busy {
                    Cached::Busy
                } else if clock() - entry.created_at >= SESSION_WEBSOCKET_MAX_AGE_MS {
                    let socket = entry.socket.take();
                    remove_entry(&mut state, session, account, None);
                    socket.map_or(Cached::None, Cached::Expired)
                } else {
                    entry.busy = true;
                    let id = entry.id;
                    let continuation = entry.continuation.take();
                    match entry.socket.take() {
                        Some(socket) => Cached::Idle(socket, id, continuation),
                        None => Cached::None,
                    }
                }
            }
        }
    };
    match cached {
        Cached::Busy => {
            let socket = connect(transport, url, headers, signal, connect_timeout_ms).await?;
            return Ok(Acquired {
                socket,
                slot: None,
                reused: false,
                continuation: None,
            });
        }
        Cached::Expired(mut socket) => socket.close(1000, "connection_age_limit").await,
        Cached::Idle(mut socket, id, continuation) => {
            if socket.is_open().await {
                return Ok(Acquired {
                    socket,
                    slot: Some(Slot {
                        session: session.to_string(),
                        account: account.to_string(),
                        id,
                    }),
                    reused: true,
                    continuation,
                });
            }
            socket.close(1000, "done").await;
            remove_entry(&mut state(), session, account, Some(id));
        }
        Cached::None => {}
    }
    let socket = connect(transport, url, headers, signal, connect_timeout_ms).await?;
    let mut state = state();
    state.next_id += 1;
    let id = state.next_id;
    state.cache.entry(session.to_string()).or_default().insert(
        account.to_string(),
        Entry {
            id,
            socket: None,
            busy: true,
            created_at: clock(),
            continuation: None,
            idle_generation: 0,
        },
    );
    Ok(Acquired {
        socket,
        slot: Some(Slot {
            session: session.to_string(),
            account: account.to_string(),
            id,
        }),
        reused: false,
        continuation: None,
    })
}

/// Drop the cache entry of `session`/`account` (only entry `id`, when given).
fn remove_entry(state: &mut WebSocketState, session: &str, account: &str, id: Option<u64>) {
    let Some(accounts) = state.cache.get_mut(session) else {
        return;
    };
    if id.is_none_or(|id| accounts.get(account).is_some_and(|e| e.id == id)) {
        accounts.remove(account);
    }
    if accounts.is_empty() {
        state.cache.remove(session);
    }
}

/// `release({ keep })`: a socket kept open goes back to its slot with its continuation, its idle timer started;
/// anything else is closed.
async fn release(mut acquired: Acquired, keep: bool, continuation: Option<Continuation>) {
    let Some(slot) = acquired.slot.take() else {
        acquired.socket.close(1000, "done").await;
        return;
    };
    if !keep || !acquired.socket.is_open().await {
        acquired.socket.close(1000, "done").await;
        remove_entry(&mut state(), &slot.session, &slot.account, Some(slot.id));
        return;
    }
    let socket = acquired.socket;
    let kept: Result<u64, Box<dyn WebSocketConnection>> = {
        let mut state = state();
        match state
            .cache
            .get_mut(&slot.session)
            .and_then(|accounts| accounts.get_mut(&slot.account))
            .filter(|entry| entry.id == slot.id)
        {
            Some(entry) => {
                entry.socket = Some(socket);
                entry.busy = false;
                entry.continuation = continuation;
                entry.idle_generation += 1;
                Ok(entry.idle_generation)
            }
            // The cache was closed while the socket was in use.
            None => Err(socket),
        }
    };
    let generation = match kept {
        Ok(generation) => Some(generation),
        Err(mut socket) => {
            socket.close(1000, "done").await;
            None
        }
    };
    if let (Some(generation), Ok(handle)) = (generation, tokio::runtime::Handle::try_current()) {
        // Five minutes from now, not from when the timer task first runs.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(SESSION_WEBSOCKET_CACHE_TTL_MS);
        handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let socket = {
                let mut state = state();
                let entry = state
                    .cache
                    .get_mut(&slot.session)
                    .and_then(|accounts| accounts.get_mut(&slot.account))
                    .filter(|e| e.id == slot.id && !e.busy && e.idle_generation == generation);
                let socket = entry.and_then(|entry| entry.socket.take());
                if socket.is_some() {
                    remove_entry(&mut state, &slot.session, &slot.account, Some(slot.id));
                }
                socket
            };
            if let Some(mut socket) = socket {
                socket.close(1000, "idle_timeout").await;
            }
        });
    }
}

fn without_input(body: &Map<String, Value>) -> String {
    let mut rest = body.clone();
    rest.shift_remove("input");
    rest.shift_remove("previous_response_id");
    serde_json::to_string(&rest).unwrap_or_default()
}

fn input_of(body: &Map<String, Value>) -> Vec<Value> {
    body.get("input").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// `getCachedWebSocketInputDelta`: the input added since the previous request and its response, if the request
/// is otherwise the same.
fn input_delta(body: &Map<String, Value>, continuation: &Continuation) -> Option<Vec<Value>> {
    if without_input(body) != without_input(&continuation.last_request_body) {
        return None;
    }
    let current = input_of(body);
    let mut baseline = input_of(&continuation.last_request_body);
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current.len() < baseline.len() {
        return None;
    }
    let prefix = serde_json::to_string(&current[..baseline.len()]).unwrap_or_default();
    if prefix != serde_json::to_string(&baseline).unwrap_or_default() {
        return None;
    }
    Some(current[baseline.len()..].to_vec())
}

/// `buildCachedWebSocketRequestBody`: the request to send, and the continuation the entry keeps (none once the
/// request cannot continue from it).
fn cached_request_body(
    body: &Map<String, Value>,
    continuation: Option<Continuation>,
) -> (Map<String, Value>, Option<Continuation>) {
    let Some(continuation) = continuation else {
        return (body.clone(), None);
    };
    match input_delta(body, &continuation) {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut request = body.clone();
            request.insert("previous_response_id".into(), json!(continuation.last_response_id));
            request.insert("input".into(), Value::Array(delta));
            (request, Some(continuation))
        }
        _ => (body.clone(), None),
    }
}

/// The text of a message (`decodeWebSocketData`).
fn message_text(event: WsEvent) -> Result<Option<String>, WsEvent> {
    match event {
        WsEvent::Text(text) => Ok(Some(text)),
        WsEvent::Binary(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        other => Err(other),
    }
}

/// `extractWebSocketCloseError`.
fn close_error(code: u16, reason: &str) -> CodexError {
    let mut reason_text = if reason.is_empty() {
        String::new()
    } else {
        format!(" {reason}")
    };
    if reason_text.is_empty() && code == WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE {
        reason_text = " message too big".into();
    }
    CodexError::Closed {
        message: format!("WebSocket closed {code}{reason_text}").trim().to_string(),
        code,
    }
}

/// What one request needs besides the socket.
pub(super) struct Request<'a> {
    pub url: &'a str,
    pub body: &'a Value,
    pub headers: &'a HeaderMap,
    pub model: &'a Model,
    pub grammar: &'a IndexMap<String, String>,
    pub service_tier: Option<&'a str>,
    pub idle_timeout_ms: Option<u64>,
    pub connect_timeout_ms: Option<u64>,
    pub cache_session_id: Option<&'a str>,
    pub account_id: &'a str,
    pub transport_kind: &'a str,
    pub signal: Option<&'a AbortSignal>,
}

/// `processWebSocketStream`: `started` turns true with the first response event (when `start` is emitted unless
/// `start_emitted` says it was).
pub(super) async fn process_websocket_stream(
    run: &mut ResponsesRun<'_, '_>,
    transport: &dyn Transport,
    request: &Request<'_>,
    started: &mut bool,
    start_emitted: &mut bool,
) -> Result<(), CodexError> {
    let mut acquired = acquire(
        transport,
        request.url,
        request.headers,
        request.cache_session_id,
        request.account_id,
        request.signal,
        request.connect_timeout_ms,
    )
    .await?;
    let use_cached_context = matches!(request.transport_kind, "websocket-cached" | "auto");
    let full_body = request.body.as_object().cloned().unwrap_or_default();
    let previous = acquired.continuation.take();
    let (request_body, kept) = if use_cached_context && acquired.slot.is_some() {
        cached_request_body(&full_body, previous)
    } else {
        (full_body.clone(), previous)
    };
    if let Some(session) = request.cache_session_id {
        let mut state = state();
        let stats = state.stats.entry(session.to_string()).or_default();
        stats.requests += 1;
        if acquired.reused {
            stats.connections_reused += 1;
        } else {
            stats.connections_created += 1;
        }
        if use_cached_context {
            stats.cached_context_requests += 1;
        }
        if request_body.get("store") == Some(&Value::Bool(true)) {
            stats.store_true_requests += 1;
        }
        let items = request_body.get("input").and_then(Value::as_array).map_or(0, Vec::len) as u64;
        stats.last_input_items = items;
        match request_body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            Some(id) => {
                stats.delta_requests += 1;
                stats.last_delta_input_items = Some(items);
                stats.last_previous_response_id = Some(id.to_string());
            }
            None => {
                stats.full_context_requests += 1;
                stats.last_delta_input_items = None;
                stats.last_previous_response_id = None;
            }
        }
    }
    let mut message = Map::new();
    message.insert("type".into(), json!("response.create"));
    message.extend(request_body);
    let outcome = read_response(run, acquired.socket.as_mut(), request, &message, started, start_emitted).await;
    match outcome {
        Ok(()) => {
            let aborted = is_aborted(request.signal);
            let continuation = match (&run.output.response_id, aborted) {
                (Some(response_id), false) if use_cached_context && acquired.slot.is_some() => {
                    let context = TranscriptContext {
                        messages: vec![Message::Assistant(run.output.clone())],
                    };
                    let items = crate::openai_responses::convert_responses_messages(
                        request.model,
                        &context,
                        &ConvertOptions {
                            tool_call_providers: OPENAI_TOOL_CALL_PROVIDERS,
                            include_system_prompt: false,
                            grammar: request.grammar,
                            supports_mid_convo_system_messages: false,
                            supports_additional_tools: false,
                            supports_tool_search: false,
                            tool_options: ToolOptions::default(),
                        },
                        now_ms(),
                    )
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|item| {
                        !matches!(
                            item.get("type").and_then(Value::as_str),
                            Some("function_call_output" | "custom_tool_call_output")
                        )
                    })
                    .collect();
                    Some(Continuation {
                        last_request_body: full_body,
                        last_response_id: response_id.clone(),
                        last_response_items: items,
                    })
                }
                _ => kept,
            };
            release(acquired, !aborted, continuation).await;
            Ok(())
        }
        Err(error) => {
            release(acquired, false, None).await;
            Err(error)
        }
    }
}

/// Send the request and read its events (`parseWebSocket` through `mapCodexEvents` into the shared Responses
/// event handling) until the response completes.
async fn read_response(
    run: &mut ResponsesRun<'_, '_>,
    socket: &mut dyn WebSocketConnection,
    request: &Request<'_>,
    message: &Map<String, Value>,
    started: &mut bool,
    start_emitted: &mut bool,
) -> Result<(), CodexError> {
    let text = serde_json::to_string(message).unwrap_or_default();
    socket.send_text(&text).await;
    let signal = request.signal;
    loop {
        if is_aborted(signal) {
            return Err(aborted_error());
        }
        let idle = async {
            match request.idle_timeout_ms.filter(|ms| *ms > 0) {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending().await,
            }
        };
        let event = tokio::select! {
            event = socket.next_event() => event,
            () = cancelled(signal) => return Err(aborted_error()),
            () = idle => {
                socket.close(1000, "idle_timeout").await;
                return Err(CodexError::Plain(format!(
                    "WebSocket idle timeout after {}ms",
                    request.idle_timeout_ms.unwrap_or_default()
                )));
            }
        };
        let text = match message_text(event) {
            Ok(Some(text)) if text.is_empty() => continue,
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(WsEvent::Error(message)) => return Err(CodexError::Plain(message)),
            Err(WsEvent::Closed { code, reason, .. }) => return Err(close_error(code, &reason)),
            Err(_) => continue,
        };
        let event = parse_json(&text).map_err(|e| {
            let message = crate::jsutil::json_parse_error(&text).unwrap_or_else(|| e.to_string());
            CodexError::Protocol(format!("Invalid Codex WebSocket JSON: {message}"))
        })?;
        let Some((event, terminal)) = map_codex_event(event, &mut run.output)? else {
            continue;
        };
        if !*started {
            *started = true;
            if !*start_emitted {
                *start_emitted = true;
                run.emit(|partial| AssistantMessageEvent::Start { partial });
            }
        }
        run.handle(
            &event,
            request.model,
            request.grammar,
            TierPricing::Codex(request.service_tier),
        )
        .map_err(CodexError::Plain)?;
        if terminal {
            break;
        }
    }
    run.end().map_err(CodexError::Plain)
}
