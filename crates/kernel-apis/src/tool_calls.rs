//! `tools.*`: extension tool calls from the REPL (`ultron/rlm/extension-tools.ts` `ExtensionToolCalls`). The kernel
//! calls a tool with `await tools.call(name, params)` / `await tools.<name>(...)` / `await mcp.call(...)` (the `mcp`
//! gateway is a tool like any other); the host runs it on its own thread through a [`ToolRunner`] with a cancel
//! signal. Every call is a record (name, label, status, elapsed time, input and result previews), journaled as the
//! module document `{version: 1, calls: [...]}`.
//!
//! A slow call detaches like a plain `bash`: after `yield_after` seconds the kernel gets a running snapshot, the call
//! keeps running in the host, and its end is reported to the [`ToolCallEnd`] observer (the session publishes it as a
//! `<runtime_event kind="tool_done">`). A cell that stops while waiting on a call it did not ask a handle for cancels
//! the call (its end then counts as awaited); an Esc abort of the root turn cancels the calls started under it
//! ([`ToolCalls::cancel_root`], `rootAborted`). A call does not outlive the worker: a journaled running call reads as
//! `interrupted` after a restart.
//!
//! Calls on a lane honor the tool's `executionMode` as the harness does for a model's tool calls: a "sequential" tool
//! waits for the lane's calls in flight and runs alone, and calls started after it wait for it (first come, first
//! served; [`LaneGate`]). A running call's progress (`onUpdate`) becomes its record's preview.
//!
//! Not ported: the records of native tool calls (the model's only tool is `rlm`).

use crate::store::{DocumentStore, read_versioned};
use crate::util::{Clock, fields, lock, stringify, system_clock};
use crate::{ApiModule, Request};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const TOOL_CALL_MAX_ACTIVE: usize = 32;
pub const TOOL_CALL_MAX_RETAINED: usize = 64;
pub const TOOL_CALL_MAX_WAIT_SECONDS: f64 = 3600.0;
pub const DEFAULT_TOOL_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 512 * 1024;
const PREVIEW_CHARS: usize = 300;
const DETAILS_MAX_BYTES: usize = 64 * 1024;
const REPLY_TEXT_MAX_BYTES: usize = 768 * 1024;
const JOURNAL_VERSION: i64 = 1;

/// What the REPL learns about one extension tool (`ExtensionToolInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub label: Option<String>,
    pub description: String,
    pub parameters: Value,
    /// `"sequential"`: the tool runs alone among its lane's calls (Pi's `executionMode`).
    pub execution_mode: Option<String>,
}

/// A running call's partial result (`onUpdate(partial)`: `{content, details?}`).
pub type ToolUpdate<'a> = &'a (dyn Fn(&Value) + Send + Sync);

/// The session's tools and how to run one (`ExtensionToolRunner`).
pub trait ToolRunner: Send + Sync {
    fn tools(&self) -> Vec<ToolInfo>;
    /// Runs tool `name` to its end (blocking); `cancel` aborts it. Ok is the tool result `{content, details}`.
    fn execute(&self, name: &str, call_id: &str, params: &Map<String, Value>, cancel: &Signal)
    -> Result<Value, String>;
    /// [`ToolRunner::execute`] with the tool's progress reported to `on_update` (`onUpdate`); a runner whose tools
    /// report none runs them as `execute`.
    fn execute_with_updates(
        &self,
        name: &str,
        call_id: &str,
        params: &Map<String, Value>,
        cancel: &Signal,
        on_update: ToolUpdate<'_>,
    ) -> Result<Value, String> {
        let _ = on_update;
        self.execute(name, call_id, params, cancel)
    }
}

/// The preview a call shows while it waits for its lane's gate.
pub const QUEUED_PREVIEW: &str = "queued: waiting for a sequential tool call on this lane";

/// A first-come-first-served readers-writer gate (`LaneGate`): parallel calls share it, an exclusive call holds it
/// alone, and a waiting exclusive call keeps later calls from overtaking it. Tickets are granted in [`LaneGate::pump`];
/// the caller holds the lock that guards the gate and waits on its condition variable.
#[derive(Default, Debug)]
pub struct LaneGate {
    queue: std::collections::VecDeque<(u64, bool)>,
    granted: std::collections::HashSet<u64>,
    shared: usize,
    exclusive: bool,
}

impl LaneGate {
    /// Queues `ticket`; true when it is granted at once.
    pub fn enqueue(&mut self, ticket: u64, exclusive: bool) -> bool {
        self.queue.push_back((ticket, exclusive));
        self.pump();
        self.granted.contains(&ticket)
    }

    pub fn is_granted(&self, ticket: u64) -> bool {
        self.granted.contains(&ticket)
    }

    /// A queued ticket gives up (its call was cancelled); later tickets may now run.
    pub fn withdraw(&mut self, ticket: u64) {
        self.queue.retain(|(t, _)| *t != ticket);
        self.pump();
    }

    /// A granted ticket's call ended.
    pub fn release(&mut self, ticket: u64, exclusive: bool) {
        if !self.granted.remove(&ticket) {
            return;
        }
        if exclusive {
            self.exclusive = false;
        } else {
            self.shared = self.shared.saturating_sub(1);
        }
        self.pump();
    }

    fn pump(&mut self) {
        while let Some(&(ticket, exclusive)) = self.queue.front() {
            if self.exclusive || (exclusive && self.shared > 0) {
                return;
            }
            self.queue.pop_front();
            if exclusive {
                self.exclusive = true;
            } else {
                self.shared += 1;
            }
            self.granted.insert(ticket);
        }
    }
}

/// A REPL call's end, as reported to the completion-event dispatcher (`ToolCallEnd`).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallEnd {
    /// The journaled record (`ToolCallRecord`).
    pub call: Value,
    /// A cell was waiting on this call when it ended (the model already has the result).
    pub awaited: bool,
    /// Cancelled because its root turn was aborted: no event.
    pub root_aborted: bool,
}

/// The event-relevant view of a call's end: `None` when awaited, root-aborted, or not a REPL call (`toolEvent`).
pub fn tool_event(end: &ToolCallEnd) -> Option<(String, String, String, String)> {
    if end.awaited || end.root_aborted || end.call["source"] != "repl" {
        return None;
    }
    let id = end.call["id"].as_str().unwrap_or_default().to_string();
    Some((
        id.clone(),
        end.call["status"].as_str().unwrap_or_default().to_string(),
        tool_call_summary(&end.call, 240),
        format!("await tools.result(\"{id}\")"),
    ))
}

pub type ToolCallObserver = Arc<dyn Fn(ToolCallEnd) + Send + Sync>;
/// The usage root (root turn) a lane's call starts under.
pub type RootOf = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
struct Outcome {
    text: String,
    details: Value,
    truncated: bool,
}

struct Live {
    cancel: Signal,
    waiters: usize,
    reason: Option<&'static str>,
}

#[derive(Default)]
struct State {
    loaded: bool,
    closed: bool,
    /// Journaled records in start order (`ToolCallRecord` objects).
    records: Vec<Value>,
    live: HashMap<String, Live>,
    outcomes: HashMap<String, Outcome>,
    /// Per-lane gates that run "sequential" tools alone.
    gates: HashMap<String, LaneGate>,
    next_ticket: u64,
}

struct Inner {
    runner: Option<Arc<dyn ToolRunner>>,
    store: Arc<dyn DocumentStore>,
    output_bytes: usize,
    max_retained: usize,
    now: Clock,
    on_end: Mutex<Option<ToolCallObserver>>,
    roots: Mutex<Option<RootOf>>,
    state: Mutex<State>,
    changed: Condvar,
}

/// The `tools.*` host module and the session's REPL tool calls.
#[derive(Clone)]
pub struct ToolCalls {
    inner: Arc<Inner>,
}

/// `toolResultBudget`: ULTRON_TOOL_RESULT_BYTES within 1 KiB..512 KiB, default 256 KiB.
pub fn tool_result_budget(value: Option<&str>) -> usize {
    match value.map(str::trim).filter(|v| !v.is_empty()).map(str::parse::<i64>) {
        Some(Ok(bytes)) if bytes > 0 => (bytes as usize).clamp(1024, MAX_TOOL_RESULT_BYTES),
        _ => DEFAULT_TOOL_RESULT_BYTES,
    }
}

impl ToolCalls {
    /// `runner` is `None` in a session without extension tools: every call then fails with a clear message.
    pub fn new(runner: Option<Arc<dyn ToolRunner>>, store: Arc<dyn DocumentStore>) -> Self {
        Self {
            inner: Arc::new(Inner {
                runner,
                store,
                output_bytes: tool_result_budget(std::env::var("ULTRON_TOOL_RESULT_BYTES").ok().as_deref()),
                max_retained: TOOL_CALL_MAX_RETAINED,
                now: system_clock(),
                on_end: Mutex::new(None),
                roots: Mutex::new(None),
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        }
    }

    /// Reports each REPL call's end (`onEnd`).
    pub fn on_end(&self, observer: ToolCallObserver) {
        *lock(&self.inner.on_end) = Some(observer);
    }

    /// The root a lane's calls start under (`NativeHostApi.rootOf`).
    pub fn set_roots(&self, roots: RootOf) {
        *lock(&self.inner.roots) = Some(roots);
    }

    fn runner(&self) -> Result<Arc<dyn ToolRunner>, String> {
        self.inner
            .runner
            .clone()
            .ok_or_else(|| "Extension tools are not available in this session".to_string())
    }

    fn now(&self) -> i64 {
        (self.inner.now)()
    }

    /// Loads the journal once: a call that was running when its worker stopped is `interrupted`.
    fn load(&self) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        if state.loaded {
            return Ok(());
        }
        let saved = read_versioned("ultron.module/tool-calls", self.inner.store.read()?, "version")?;
        let calls = saved
            .as_ref()
            .and_then(|saved| saved.get("calls"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pending: Vec<String> = state.records.iter().map(|r| id_of(r).to_string()).collect();
        let now = self.now();
        let mut interrupted = false;
        let mut journaled: Vec<Value> = calls
            .into_iter()
            .filter(|call| !pending.iter().any(|id| id == id_of(call)))
            .map(|mut call| {
                if call["status"] == "running" {
                    interrupted = true;
                    call["status"] = "interrupted".into();
                    if call["endedAt"].is_null() {
                        call["endedAt"] = now.into();
                    }
                    call["error"] = "worker restarted".into();
                }
                call
            })
            .collect();
        journaled.append(&mut state.records);
        state.records = journaled;
        state.loaded = true;
        if interrupted {
            self.persist(&state);
        }
        Ok(())
    }

    fn persist(&self, state: &State) {
        let document = json!({"version": JOURNAL_VERSION, "calls": state.records});
        let _ = self.inner.store.write(&document);
    }

    fn tool(&self, name: Option<&Value>) -> Result<ToolInfo, String> {
        let name = name
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or("tool name must be a non-empty string")?;
        let tools = self.runner()?.tools();
        let normalized = |value: &str| value.replace('-', "_");
        tools
            .iter()
            .find(|t| t.name == name)
            .or_else(|| tools.iter().find(|t| normalized(&t.name) == normalized(name)))
            .cloned()
            .ok_or_else(|| {
                let names: Vec<&str> = tools.iter().take(20).map(|t| t.name.as_str()).collect();
                if names.is_empty() {
                    format!("Unknown extension tool \"{name}\" (no extension tools are registered)")
                } else {
                    format!("Unknown extension tool \"{name}\"; available: {}", names.join(", "))
                }
            })
    }

    /// Starts a REPL call of tool `name` on `lane` under `root`; returns its id.
    pub fn start(
        &self,
        lane: &str,
        root: Option<String>,
        name: &str,
        params: Map<String, Value>,
    ) -> Result<String, String> {
        let runner = self.runner()?;
        let id = format!("call-{}", random_hex(4));
        let cancel = Signal::new();
        let exclusive = runner
            .tools()
            .iter()
            .find(|t| t.name == name)
            .is_some_and(|t| t.execution_mode.as_deref() == Some("sequential"));
        let ticket;
        {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err("Extension tool calls are closed".into());
            }
            if state.live.len() >= TOOL_CALL_MAX_ACTIVE {
                return Err(format!(
                    "At most {TOOL_CALL_MAX_ACTIVE} extension tool calls can run at once"
                ));
            }
            let mut record = json!({
                "id": id,
                "lane": lane,
                "rootId": root,
                "source": "repl",
                "name": name,
                "label": tool_call_label(name, &params),
                "input": preview(&Value::Object(params.clone()), PREVIEW_CHARS),
                "status": "running",
                "startedAt": self.now(),
                "endedAt": null,
            });
            // The gate is taken in start order: a call queued behind a sequential one says so.
            state.next_ticket += 1;
            ticket = state.next_ticket;
            if !state
                .gates
                .entry(lane.to_string())
                .or_default()
                .enqueue(ticket, exclusive)
            {
                record["preview"] = QUEUED_PREVIEW.into();
            }
            state.live.insert(
                id.clone(),
                Live {
                    cancel: cancel.clone(),
                    waiters: 0,
                    reason: None,
                },
            );
            state.records.push(record);
            self.prune(&mut state);
            self.persist(&state);
        }
        let (calls, call_id, name, lane) = (self.clone(), id.clone(), name.to_string(), lane.to_string());
        std::thread::spawn(move || {
            let result = match calls.acquire(&lane, &call_id, ticket, &cancel) {
                Ok(()) => {
                    let (updates, update_id) = (calls.clone(), call_id.clone());
                    let on_update = move |partial: &Value| updates.progress(&update_id, partial);
                    let result =
                        runner.execute_with_updates(&name, &format!("repl-{call_id}"), &params, &cancel, &on_update);
                    let mut state = lock(&calls.inner.state);
                    if let Some(gate) = state.gates.get_mut(&lane) {
                        gate.release(ticket, exclusive);
                    }
                    calls.inner.changed.notify_all();
                    result
                }
                Err(error) => Err(error),
            };
            calls.finish(&call_id, result);
        });
        Ok(id)
    }

    /// Waits for the lane's gate; a call cancelled while queued never starts.
    fn acquire(&self, lane: &str, id: &str, ticket: u64, cancel: &Signal) -> Result<(), String> {
        let mut state = lock(&self.inner.state);
        loop {
            let granted = state.gates.get(lane).is_some_and(|g| g.is_granted(ticket));
            if granted {
                if let Some(record) = state.records.iter_mut().find(|r| id_of(r) == id)
                    && record["preview"].as_str().is_some_and(|p| p.starts_with("queued:"))
                {
                    record.as_object_mut().map(|r| r.shift_remove("preview"));
                }
                return Ok(());
            }
            if cancel.is_aborted() {
                if let Some(gate) = state.gates.get_mut(lane) {
                    gate.withdraw(ticket);
                }
                self.inner.changed.notify_all();
                return Err(cancel.reason().unwrap_or_else(|| "tool call cancelled".into()));
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// `onUpdate(partial)`: a running call's latest progress text is its preview.
    fn progress(&self, id: &str, partial: &Value) {
        let text = result_text(partial);
        if text.is_empty() {
            return;
        }
        let mut state = lock(&self.inner.state);
        if !state.live.contains_key(id) {
            return;
        }
        if let Some(record) = state.records.iter_mut().find(|r| id_of(r) == id) {
            record["preview"] = preview(&Value::from(text), PREVIEW_CHARS).into();
        }
    }

    fn finish(&self, id: &str, result: Result<Value, String>) {
        let end = {
            let mut state = lock(&self.inner.state);
            let Some(live) = state.live.remove(id) else {
                return;
            };
            let now = self.now();
            let outcome = match (&result, live.cancel.is_aborted()) {
                (Ok(result), false) => Some(self.outcome(result)),
                _ => None,
            };
            let Some(index) = state.records.iter().position(|r| id_of(r) == id) else {
                return;
            };
            {
                let record = state.records[index].as_object_mut().expect("records are objects");
                record.insert("endedAt".into(), now.into());
                if live.cancel.is_aborted() {
                    record.insert("status".into(), "cancelled".into());
                    record.shift_remove("preview");
                } else if let Err(error) = &result {
                    record.insert("status".into(), "failed".into());
                    let shown = if error.chars().count() > 2000 {
                        format!("{}…", error.chars().take(1999).collect::<String>())
                    } else {
                        error.clone()
                    };
                    record.insert("error".into(), shown.into());
                    record.insert(
                        "preview".into(),
                        preview(&Value::from(error.as_str()), PREVIEW_CHARS).into(),
                    );
                } else if let Some(outcome) = &outcome {
                    record.insert("status".into(), "completed".into());
                    record.insert(
                        "preview".into(),
                        preview(&Value::from(outcome.text.as_str()), PREVIEW_CHARS).into(),
                    );
                }
            }
            if let Some(outcome) = outcome {
                state.outcomes.insert(id.to_string(), outcome);
            }
            self.persist(&state);
            let call = state.records[index].clone();
            self.inner.changed.notify_all();
            (live.reason != Some("closed")).then(|| ToolCallEnd {
                call,
                awaited: live.waiters > 0,
                root_aborted: live.reason == Some("root_aborted"),
            })
        };
        if let Some(end) = end {
            let observer = lock(&self.inner.on_end).clone();
            if let Some(observer) = observer {
                // Observers never affect a call.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(end)));
            }
        }
    }

    /// The result text bounded to the output budget (and the kernel frame), and the details bounded to 64 KiB.
    fn outcome(&self, result: &Value) -> Outcome {
        let raw = result_text(result);
        let mut budget = self.inner.output_bytes;
        let mut text = host::cell::truncate_tool_output(&raw, budget, None);
        // JSON escaping can grow text (control characters six-fold): keep the reply within the kernel's frame.
        while stringify(&Value::from(text.as_str())).len() > REPLY_TEXT_MAX_BYTES && budget > 1024 {
            budget /= 2;
            text = host::cell::truncate_tool_output(&raw, budget, None);
        }
        let mut details = result.get("details").cloned().unwrap_or(Value::Null);
        if !details.is_null() && stringify(&details).len() > DETAILS_MAX_BYTES {
            details = json!({"truncated": true, "preview": preview(&details, 2000)});
        }
        Outcome {
            truncated: text != raw,
            text,
            details,
        }
    }

    /// Waits for call `id` at most `seconds` (`None`: until it ends), or until `signal` (the calling cell) aborts.
    fn wait(&self, id: &str, seconds: Option<f64>, signal: &Signal) {
        let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64((s * 1000.0).max(20.0) / 1000.0));
        let mut state = lock(&self.inner.state);
        match state.live.get_mut(id) {
            Some(live) => live.waiters += 1,
            None => return,
        }
        loop {
            if !state.live.contains_key(id) || signal.is_aborted() {
                break;
            }
            let mut slice = Duration::from_millis(50);
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                slice = slice.min(deadline - now);
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, slice)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        if let Some(live) = state.live.get_mut(id) {
            live.waiters -= 1;
        }
    }

    /// Stops call `id` and waits for its end (`reason`: `cancelled`, `root_aborted` or `closed`).
    pub fn cancel(&self, id: &str, reason: &'static str) {
        let mut state = lock(&self.inner.state);
        let Some(live) = state.live.get_mut(id) else {
            return;
        };
        live.reason.get_or_insert(reason);
        live.cancel.abort(if reason == "root_aborted" {
            "turn aborted"
        } else {
            "tool call cancelled"
        });
        self.inner.changed.notify_all();
        while state.live.contains_key(id) {
            state = self
                .inner
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Esc aborted a root turn: stop every REPL call started under it.
    pub fn cancel_root(&self, root: &str) {
        let ids: Vec<String> = {
            let state = lock(&self.inner.state);
            state
                .records
                .iter()
                .filter(|r| state.live.contains_key(id_of(r)) && r["rootId"].as_str() == Some(root))
                .map(|r| id_of(r).to_string())
                .collect()
        };
        for id in ids {
            self.cancel(&id, "root_aborted");
        }
    }

    /// Running REPL calls of `lane` whose root is not `excluded`.
    pub fn running(&self, lane: &str, excluded: &dyn Fn(Option<&str>) -> bool) -> usize {
        let state = lock(&self.inner.state);
        state
            .records
            .iter()
            .filter(|r| state.live.contains_key(id_of(r)) && r["lane"] == lane && !excluded(r["rootId"].as_str()))
            .count()
    }

    /// Running and recent calls (`agents.status`), newest first.
    pub fn list(&self) -> Vec<Value> {
        lock(&self.inner.state).records.iter().rev().cloned().collect()
    }

    /// Stops every running call without reporting their ends.
    pub fn close(&self) {
        let ids: Vec<String> = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state.live.keys().cloned().collect()
        };
        for id in ids {
            self.cancel(&id, "closed");
        }
    }

    /// The wire form of a call for the kernel's ToolCall / ToolResult (`ToolCallSnapshot`).
    fn snapshot(&self, state: &State, record: &Value) -> Value {
        let id = id_of(record);
        let outcome = state.outcomes.get(id);
        let status = record["status"].as_str().unwrap_or_default();
        let ended = record["endedAt"].as_i64().unwrap_or_else(|| self.now());
        let elapsed = (((ended - record["startedAt"].as_i64().unwrap_or(ended)) as f64) / 100.0).round() / 10.0;
        let error = match record.get("error") {
            Some(error) => error.clone(),
            None if status == "completed" && outcome.is_none() => {
                "the result is no longer held (the worker restarted or the call is too old)".into()
            }
            None => Value::Null,
        };
        json!({
            "id": id,
            "name": record["name"],
            "label": record["label"],
            "lane": record["lane"],
            "status": status,
            "running": status == "running",
            "ok": status == "completed",
            "text": outcome.map(|o| Value::from(o.text.as_str())).unwrap_or(Value::Null),
            "details": outcome.map(|o| o.details.clone()).unwrap_or(Value::Null),
            "truncated": outcome.is_some_and(|o| o.truncated),
            "elapsed_seconds": elapsed,
            "error": error,
        })
    }

    fn snapshot_of(&self, id: &str) -> Result<Value, String> {
        let state = lock(&self.inner.state);
        let record = state
            .records
            .iter()
            .find(|r| id_of(r) == id)
            .ok_or_else(|| format!("Unknown tool call {id}"))?;
        Ok(self.snapshot(&state, record))
    }

    /// The root lane sees every REPL call; a child lane sees its own.
    fn visible(&self, id: Option<&Value>, lane: &str) -> Result<String, String> {
        let id = id
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("call id must be a nonempty string")?;
        let state = lock(&self.inner.state);
        match state.records.iter().find(|r| id_of(r) == id) {
            Some(record) if lane == "main" || record["lane"] == lane => Ok(id.to_string()),
            _ => Err(format!("Unknown tool call {id}")),
        }
    }

    fn is_live(&self, id: &str) -> bool {
        lock(&self.inner.state).live.contains_key(id)
    }

    /// Bounded retention: drop the oldest finished calls beyond the cap.
    fn prune(&self, state: &mut State) {
        let excess = state.records.len().saturating_sub(self.inner.max_retained);
        if excess == 0 {
            return;
        }
        let drop: Vec<String> = state
            .records
            .iter()
            .filter(|r| r["status"] != "running")
            .take(excess)
            .map(|r| id_of(r).to_string())
            .collect();
        for id in &drop {
            state.outcomes.remove(id);
        }
        state.records.retain(|r| !drop.iter().any(|id| id == id_of(r)));
    }

    fn handle_request(&self, request: &Request<'_>) -> Result<Value, String> {
        self.load()?;
        let payload = request.payload;
        match request.kind {
            "tools.list" => {
                fields(payload, &[])?;
                Ok(Value::Array(
                    self.runner()?
                        .tools()
                        .into_iter()
                        .map(|tool| {
                            let mut item = Map::new();
                            item.insert("name".into(), tool.name.into());
                            if let Some(label) = tool.label {
                                item.insert("label".into(), label.into());
                            }
                            item.insert("description".into(), first_paragraph(&tool.description, 400).into());
                            Value::Object(item)
                        })
                        .collect(),
                ))
            }
            "tools.describe" => {
                fields(payload, &["name"])?;
                let tool = self.tool(payload.get("name"))?;
                let mut item = Map::new();
                item.insert("name".into(), tool.name.into());
                if let Some(label) = tool.label {
                    item.insert("label".into(), label.into());
                }
                item.insert("description".into(), tool.description.into());
                item.insert("parameters".into(), tool.parameters);
                Ok(Value::Object(item))
            }
            "tools.call" => {
                // Wait up to `yield_after` seconds (absent: until the call ends). A call still running when a
                // waiting cell is stopped is cancelled, unless the kernel asked for a handle (`detach`).
                fields(payload, &["name", "params", "yield_after", "detach"])?;
                let tool = self.tool(payload.get("name"))?;
                let params = match payload.get("params") {
                    None | Some(Value::Null) => Map::new(),
                    Some(Value::Object(params)) => params.clone(),
                    Some(_) => return Err("tool params must be a dict (a JSON object)".into()),
                };
                let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?;
                let root = lock(&self.inner.roots).clone().and_then(|roots| roots(request.lane));
                let id = self.start(request.lane, root, &tool.name, params)?;
                self.wait(&id, yield_after, request.signal);
                if self.is_live(&id) && request.signal.is_aborted() && payload.get("detach") != Some(&Value::Bool(true))
                {
                    {
                        let mut state = lock(&self.inner.state);
                        if let Some(live) = state.live.get_mut(&id) {
                            live.waiters += 1;
                        }
                    }
                    self.cancel(&id, "cancelled");
                }
                self.snapshot_of(&id)
            }
            "tools.result" => {
                fields(payload, &["id", "wait"])?;
                let id = self.visible(payload.get("id"), request.lane)?;
                if self.is_live(&id) {
                    let wait = wait_seconds(payload.get("wait"), "wait")?;
                    self.wait(&id, wait, request.signal);
                }
                self.snapshot_of(&id)
            }
            "tools.get" => {
                fields(payload, &["id"])?;
                let id = self.visible(payload.get("id"), request.lane)?;
                self.snapshot_of(&id)
            }
            "tools.cancel" => {
                fields(payload, &["id"])?;
                let id = self.visible(payload.get("id"), request.lane)?;
                self.cancel(&id, "cancelled");
                self.snapshot_of(&id)
            }
            "tools.calls" => {
                fields(payload, &[])?;
                let state = lock(&self.inner.state);
                Ok(Value::Array(
                    state
                        .records
                        .iter()
                        .rev()
                        .filter(|r| request.lane == "main" || r["lane"] == request.lane)
                        .map(|r| {
                            let mut snapshot = self.snapshot(&state, r);
                            snapshot["text"] = Value::Null;
                            snapshot["details"] = Value::Null;
                            snapshot["source"] = r["source"].clone();
                            snapshot
                        })
                        .collect(),
                ))
            }
            other => Err(format!("Ultron RLM host request is not wired: {other}")),
        }
    }
}

impl ApiModule for ToolCalls {
    fn prefixes(&self) -> &[&str] {
        &["tools."]
    }

    fn start(&self, _host: &dyn crate::TaskHost) -> Result<(), String> {
        self.load()
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.handle_request(request)
    }
}

fn id_of(record: &Value) -> &str {
    record["id"].as_str().unwrap_or_default()
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("operating system random source");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn wait_seconds(value: Option<&Value>, name: &str) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) if n.as_f64().is_some_and(|s| s.is_finite() && s >= 0.0) => {
            Ok(n.as_f64().map(|s| s.min(TOOL_CALL_MAX_WAIT_SECONDS)))
        }
        Some(_) => Err(format!("{name} must be a non-negative number of seconds")),
    }
}

/// `preview`: a string as is, anything else as JSON; whitespace collapsed; at most `limit` characters.
pub fn preview(value: &Value, limit: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => stringify(other),
    };
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > limit {
        format!("{}…", flat.chars().take(limit - 1).collect::<String>())
    } else {
        flat
    }
}

fn first_paragraph(text: &str, limit: usize) -> String {
    let trimmed = crate::util::js_trim(text);
    let paragraph = fancy_regex::Regex::new(r"\n\s*\n")
        .ok()
        .and_then(|re| re.find(trimmed).ok().flatten())
        .map_or(trimmed, |found| &trimmed[..found.start()]);
    preview(&Value::from(paragraph), limit)
}

/// The text of a tool result: text parts joined; other parts named (`resultText`).
pub fn result_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| {
            let part = part.as_object()?;
            let kind = part.get("type").and_then(Value::as_str);
            match (kind, part.get("text").and_then(Value::as_str)) {
                (Some("text"), Some(text)) => Some(text.to_string()),
                (Some("image"), _) => Some(match part.get("mimeType").and_then(Value::as_str) {
                    Some(mime) => format!("[image {mime}]"),
                    None => "[image]".into(),
                }),
                (Some(kind), _) => Some(format!("[{kind}]")),
                (None, _) => None,
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A short label for the graph; the `mcp` gateway is labelled by its mode (`toolCallLabel`).
pub fn tool_call_label(name: &str, params: &Map<String, Value>) -> String {
    if name != "mcp" {
        return name.to_string();
    }
    let text = |key: &str| params.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(tool) = text("tool") {
        return format!("mcp {tool}");
    }
    if let Some(action) = text("action") {
        return match text("server") {
            Some(server) => format!("mcp {action} {server}"),
            None => format!("mcp {action}"),
        };
    }
    if let Some(server) = text("connect") {
        return format!("mcp connect {server}");
    }
    if let Some(name) = text("describe") {
        return format!("mcp describe {name}");
    }
    if let Some(server) = text("instructions") {
        return format!("mcp instructions {server}");
    }
    if let Some(search) = params.get("search") {
        return format!("mcp search {}", stringify(search));
    }
    if let Some(uri) = text("read") {
        return format!("mcp read {uri}");
    }
    if params
        .get("resources")
        .is_some_and(|r| !matches!(r, Value::Null | Value::Bool(false)) && r != "")
    {
        return match text("resources").or(text("server")) {
            Some(server) => format!("mcp resources {server}"),
            None => "mcp resources".into(),
        };
    }
    if let Some(server) = text("server") {
        return format!("mcp list {server}");
    }
    "mcp status".into()
}

/// One-line completion summary for a runtime event (`toolCallSummary`).
pub fn tool_call_summary(call: &Value, max_chars: usize) -> String {
    let status = call["status"].as_str().unwrap_or_default();
    let shown = match call.get("error").and_then(Value::as_str) {
        _ if status == "completed" => "ok".to_string(),
        Some(error) if !error.is_empty() => format!("{status}: {error}"),
        _ => status.to_string(),
    };
    let body = match call.get("preview").and_then(Value::as_str) {
        Some(preview) if status == "completed" && !preview.is_empty() => format!("; {preview}"),
        _ => String::new(),
    };
    let text = format!("{}: {shown}{body}", call["label"].as_str().unwrap_or_default());
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > max_chars {
        format!("{}…", text.chars().take(max_chars - 1).collect::<String>())
    } else {
        text
    }
}
