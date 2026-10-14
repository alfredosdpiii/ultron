//! The worker side of the extension UI bridge (`ExtensionUIBridge` of experimental/services/extension-ui-provider.ts):
//! dialogs (`select`, `confirm`, `input`, `editor`) and notifications of the session's extensions (`/mcp` and the MCP
//! gateway's notices today) reach whichever client polls `ultron.extension-ui`, in Pi's RPC wire format
//! (`extension_ui_request`). While a client polls, a dialog waits for its answer; with nobody polling, dialogs
//! resolve at once with Pi's defaults and notifications are dropped, as in Pi's print mode.

use serde_json::{Map, Value, json};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long after its last poll a client still counts as serving.
const SERVING_GRACE: Duration = Duration::from_secs(2);
/// Longest single poll; clients poll again.
const MAX_POLL: Duration = Duration::from_secs(30);
/// Notifications kept for a client between polls.
const MAX_BUFFERED_EVENTS: usize = 256;
/// How long a worker started for an interactive client waits for its first poll before running headless.
const EXPECTED_CLIENT_WAIT: Duration = Duration::from_secs(30);
/// The serving check runs this long after the grace ends (`delayMs + 50`).
const CHECK_SLACK: Duration = Duration::from_millis(50);

struct Dialog {
    item: Value,
    answer: Option<Option<Value>>,
}

#[derive(Default)]
struct State {
    seq: u64,
    events: Vec<Value>,
    /// Open dialogs in the order they were raised.
    dialogs: Vec<(String, Dialog)>,
    polls: usize,
    last_poll_end: Option<Instant>,
    polled: bool,
    closed: bool,
    /// When to check whether a client still serves (`#scheduleServingCheck`).
    check_at: Option<Instant>,
}

pub struct ExtensionUiBridge {
    state: Mutex<State>,
    changed: Condvar,
    /// Until then (unless a client polled), a worker started for an interactive client counts as served.
    expected_until: Option<Instant>,
}

fn uuid() -> String {
    crate::paths::uuid_v4()
}

impl ExtensionUiBridge {
    /// `expect_client`: the worker was started for an interactive client (the TUI or RPC) that has not polled yet.
    pub fn new(expect_client: bool) -> Arc<Self> {
        Self::expecting(expect_client.then_some(EXPECTED_CLIENT_WAIT))
    }

    /// [`ExtensionUiBridge::new`] waiting `wait` for the expected client (`expectedClientWaitMs`).
    pub fn expecting(wait: Option<Duration>) -> Arc<Self> {
        let expected_until = wait.map(|wait| Instant::now() + wait);
        let bridge = Arc::new(Self {
            state: Mutex::new(State {
                check_at: expected_until.map(|until| until + CHECK_SLACK),
                ..State::default()
            }),
            changed: Condvar::new(),
            expected_until,
        });
        Self::watch_serving(Arc::downgrade(&bridge));
        bridge
    }

    /// Once nobody serves, buffered notifications are dropped (dialogs settle with their defaults on their own): a
    /// client that comes later starts from the open dialogs only, as with Pi's print mode in between.
    fn watch_serving(bridge: std::sync::Weak<Self>) {
        std::thread::spawn(move || {
            loop {
                let Some(this) = bridge.upgrade() else { return };
                let mut state = this.lock();
                if state.closed {
                    return;
                }
                let now = Instant::now();
                match state.check_at {
                    Some(at) if at <= now => {
                        state.check_at = None;
                        if !this.serving_in(&state) {
                            state.events.clear();
                            this.changed.notify_all();
                        }
                    }
                    other => {
                        // Wakes for a new deadline; looks again at least every second so a dropped bridge ends this.
                        let wait = other.map_or(Duration::from_secs(1), |at| (at - now).min(Duration::from_secs(1)));
                        drop(this.changed.wait_timeout(state, wait));
                    }
                }
            }
        });
    }

    /// Notifications kept for the next poll.
    pub fn buffered_events(&self) -> usize {
        self.lock().events.len()
    }

    /// How many dialogs wait for an answer.
    pub fn open_dialogs(&self) -> usize {
        self.lock().dialogs.len()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn serving_in(&self, state: &State) -> bool {
        let now = Instant::now();
        state.polls > 0
            || state
                .last_poll_end
                .is_some_and(|end| now.duration_since(end) < SERVING_GRACE)
            || (!state.polled && self.expected_until.is_some_and(|until| now < until))
    }

    /// A client polls now, polled within the grace period, or an expected client has not arrived yet.
    pub fn serving(&self) -> bool {
        self.serving_in(&self.lock())
    }

    fn item(state: &mut State, request: Map<String, Value>) -> Value {
        state.seq += 1;
        let mut wire = Map::new();
        wire.insert("type".into(), json!("extension_ui_request"));
        wire.extend(request);
        // Undefined fields are dropped, as in Pi's JSONL output.
        wire.retain(|_, v| !v.is_null());
        json!({ "seq": state.seq, "request": wire })
    }

    /// `poll(after, waitMs)`: requests after `after`; `None` starts serving now and returns only the open dialogs.
    pub fn poll(&self, requested_after: Option<u64>, wait: Duration) -> Value {
        let mut state = self.lock();
        // A cursor from before this worker started (a reattached client) restarts from the beginning.
        let mut after = requested_after.map(|a| if a > state.seq { 0 } else { a });
        // The first client of a worker started for it also receives what was queued before it arrived.
        if after.is_none() && !state.polled && self.expected_until.is_some() {
            after = Some(0);
        }
        state.polled = true;
        state.polls += 1;
        let collect = |state: &State| -> Vec<Value> {
            let dialogs = state.dialogs.iter().map(|(_, d)| d.item.clone());
            let Some(after) = after else { return dialogs.collect() };
            let mut items: Vec<Value> = state.events.iter().cloned().chain(dialogs).collect();
            items.retain(|i| i["seq"].as_u64().unwrap_or(0) > after);
            items.sort_by_key(|i| i["seq"].as_u64().unwrap_or(0));
            items
        };
        let mut requests = collect(&state);
        if requests.is_empty() && after.is_some() && !state.closed {
            let deadline = Instant::now() + wait.min(MAX_POLL);
            while requests.is_empty() && !state.closed {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = self
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
                requests = collect(&state);
            }
        }
        let cursor = match after {
            None => state.seq,
            Some(after) => requests.iter().filter_map(|i| i["seq"].as_u64()).fold(after, u64::max),
        };
        state.polls -= 1;
        state.last_poll_end = Some(Instant::now());
        state.check_at = Some(Instant::now() + SERVING_GRACE + CHECK_SLACK);
        self.changed.notify_all();
        json!({ "cursor": cursor, "requests": requests })
    }

    /// Answer a dialog; unknown or settled ids are ignored, as in Pi.
    pub fn respond(&self, id: &str, answer: Value) {
        let mut state = self.lock();
        if let Some((_, dialog)) = state.dialogs.iter_mut().find(|(d, _)| d == id) {
            dialog.answer = Some(Some(answer));
            self.changed.notify_all();
        }
    }

    /// A notification or other fire-and-forget request (dropped when nobody serves).
    pub fn emit(&self, request: Map<String, Value>) {
        let mut state = self.lock();
        if !self.serving_in(&state) {
            return;
        }
        let mut request = request;
        request.insert("id".into(), json!(uuid()));
        let item = Self::item(&mut state, request);
        state.events.push(item);
        let excess = state.events.len().saturating_sub(MAX_BUFFERED_EVENTS);
        state.events.drain(..excess);
        self.changed.notify_all();
    }

    /// A dialog: the client's answer (`None`: cancelled, timed out, nobody serving, or `cancelled` said so).
    pub fn dialog(
        &self,
        request: Map<String, Value>,
        timeout: Option<Duration>,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<Value> {
        let mut state = self.lock();
        if cancelled() || !self.serving_in(&state) {
            return None;
        }
        let id = uuid();
        let mut request = request;
        request.insert("id".into(), json!(id));
        let item = Self::item(&mut state, request);
        state.dialogs.push((id.clone(), Dialog { item, answer: None }));
        self.changed.notify_all();
        let deadline = timeout.map(|t| Instant::now() + t);
        let answer = loop {
            let index = state.dialogs.iter().position(|(d, _)| *d == id);
            let Some(index) = index else { break None };
            if let Some(answer) = state.dialogs[index].1.answer.clone() {
                break answer;
            }
            // Once nobody serves, open dialogs get Pi's defaults instead of waiting for an answer that cannot come.
            if state.closed || cancelled() || deadline.is_some_and(|d| Instant::now() >= d) || !self.serving_in(&state)
            {
                break None;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        state.dialogs.retain(|(d, _)| *d != id);
        // `{cancelled: true}` is no answer.
        answer.filter(|a| a.get("cancelled").is_none())
    }

    /// Settle every open dialog with its default (the worker is closing).
    pub fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }
}

/// `/mcp`'s UI over the bridge (`createContext`): dialogs and notifications go to the serving client.
pub struct BridgeUi(pub Arc<ExtensionUiBridge>);

fn request(fields: &[(&str, Value)]) -> Map<String, Value> {
    fields.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

impl mcp::gateway::CommandUi for BridgeUi {
    fn has_ui(&self) -> bool {
        self.0.serving()
    }

    fn notify(&self, message: &str, level: &str) {
        self.0.emit(request(&[
            ("method", json!("notify")),
            ("message", json!(message)),
            ("notifyType", json!(level)),
        ]));
    }

    fn select(&self, title: &str, options: &[String]) -> Option<String> {
        let answer = self.0.dialog(
            request(&[
                ("method", json!("select")),
                ("title", json!(title)),
                ("options", json!(options)),
            ]),
            None,
            &|| false,
        )?;
        answer.get("value").and_then(Value::as_str).map(String::from)
    }

    fn input(&self, title: &str, placeholder: &str, cancel: &mcp::CancelToken) -> Option<String> {
        let answer = self.0.dialog(
            request(&[
                ("method", json!("input")),
                ("title", json!(title)),
                ("placeholder", json!(placeholder)),
            ]),
            None,
            &|| cancel.is_cancelled(),
        )?;
        answer.get("value").and_then(Value::as_str).map(String::from)
    }
}
