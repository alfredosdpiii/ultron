//! Host operations the REPL reaches through `rlm.host_request` for durable delegation: `rlm.spawn`, `rlm.collect`,
//! `rlm.results`, `rlm.list_subagents`, `rlm.cancel` / `rlm.delete_subagent`, and `rlm.ask` for persistent children.
//! Each request is served for one cell ([`CellCall`]): children it spawns are owned by that cell's task.
//!
//! Waits run in the host (on the request's thread, bounded by the cell's signal), never as a busy cell loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use durable::{SubmissionStatus, TaskId, TaskQuery, TaskRecord, TaskStatus};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};

use super::children::{CHILD_KIND, ChildCheckpoint, ChildInput, child_done, child_report, spawn_child};
use super::{CellCall, ChatHarness, ModelRef, SubmissionDraft, WhenBusy};
use crate::scheduler::{Cancel, Ctx, HarnessError};

/// How often a wait rechecks children that are not terminal tasks (persistent children going idle).
const POLL: Duration = Duration::from_millis(20);

/// How long `rlm.ask` waits for an answer when the call gives no `timeout_ms`: a kept child that stops making
/// progress must not hold the caller's cell forever (a DeepSWE run sat 67 minutes in one ask).
pub const ASK_TIMEOUT: Duration = Duration::from_secs(20 * 60);

type OpResult = Result<Value, String>;

fn error(error: HarnessError) -> String {
    error.message()
}

/// The durable delegation operations over one ChatHarness.
#[derive(Clone)]
pub struct HostOps {
    chat: ChatHarness,
}

/// A child ID as the REPL sees it (`SpawnHandle.rlm_child_id`).
pub fn child_id(task: TaskId) -> String {
    task.0.to_string()
}

fn parse_child(selector: &Value) -> Result<TaskId, String> {
    let text = match selector {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => return Err(format!("Unknown subagent selector {other}")),
    };
    text.parse::<u64>()
        .map(TaskId)
        .map_err(|_| format!("Unknown subagent {text}"))
}

fn parse_model(value: Option<&Value>) -> Result<Option<ModelRef>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => match text.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                Ok(Some(ModelRef::new(provider, model)))
            }
            _ => Err(format!("rlm.spawn model must be provider/model, got {text}")),
        },
        Some(other) => Err(format!("rlm.spawn model must be a string, got {other}")),
    }
}

impl HostOps {
    pub fn new(chat: ChatHarness) -> HostOps {
        HostOps { chat }
    }

    /// Serve `kind` for the cell `call`; `None` when the request is not a delegation operation.
    pub fn handle(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        call: &CellCall,
        signal: &Cancel,
    ) -> Option<OpResult> {
        Some(match kind {
            "rlm.spawn" => self.spawn(payload, call),
            "rlm.collect" => self.collect(payload, call, signal),
            "rlm.results" => self.results(payload, call),
            "rlm.list_subagents" => self.list(call),
            "rlm.cancel" | "rlm.delete_subagent" => self.cancel(payload, signal),
            "rlm.ask" => self.ask(payload, call, signal),
            "swarm.register" | "swarm.agents" | "swarm.route" | "swarm.send" | "swarm.inbox" | "board.add"
            | "board.claim" | "board.update" | "board.list" | "notes.read" | "notes.append" | "notes.write" => {
                self.swarm(kind, payload, call)
            }
            _ => return None,
        })
    }

    /// Whether `kind` is one of these operations.
    pub fn serves(kind: &str) -> bool {
        matches!(
            kind,
            "rlm.spawn"
                | "rlm.collect"
                | "rlm.results"
                | "rlm.list_subagents"
                | "rlm.cancel"
                | "rlm.delete_subagent"
                | "rlm.ask"
                | "swarm.register"
                | "swarm.agents"
                | "swarm.route"
                | "swarm.send"
                | "swarm.inbox"
                | "board.add"
                | "board.claim"
                | "board.update"
                | "board.list"
                | "notes.read"
                | "notes.append"
                | "notes.write"
        )
    }

    /// A [`HostHandler`] for one cell: delegation operations here, everything else to `fallback`.
    pub fn handler(&self, call: CellCall, fallback: Option<Arc<dyn HostHandler>>) -> Arc<dyn HostHandler> {
        Arc::new(CellOps {
            ops: self.clone(),
            call,
            fallback,
        })
    }

    fn spawn(&self, payload: &Map<String, Value>, call: &CellCall) -> OpResult {
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or("rlm.spawn prompt must be a non-empty string")?;
        let empty = Map::new();
        let options = payload.get("kwargs").and_then(Value::as_object).unwrap_or(&empty);
        let name = options
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or("rlm.spawn name must be a non-empty string")?;
        let input = ChildInput {
            name: name.to_owned(),
            prompt: prompt.to_owned(),
            model: parse_model(options.get("model"))?,
            timeout_ms: options
                .get("timeout_ms")
                .and_then(Value::as_f64)
                .map(|ms| ms.ceil() as u64),
            keep: options.get("keep").and_then(Value::as_bool).unwrap_or(false)
                || payload.get("keep").and_then(Value::as_bool).unwrap_or(false),
            thinking: options.get("thinking").and_then(Value::as_str).map(str::to_owned),
            ..ChildInput::default()
        };
        let conversation = call.conversation_id;
        let cell = call.task_id;
        let task = self
            .chat
            .commit(|tx| spawn_child(tx, conversation, Some(cell), &input))
            .map_err(error)?;
        let _ = self.chat.resume();
        Ok(json!({
            "rlm_child_id": child_id(task),
            "name": input.name,
            "session_dir": "",
            "model": input.model.map(|model| format!("{}/{}", model.provider, model.model_id)).unwrap_or_default(),
            "timeout_ms": input.timeout_ms.unwrap_or(0),
            "parent_branch_anchor": "",
            "worktree": Value::Null,
        }))
    }

    /// The children of the calling conversation, oldest first.
    fn children_of(&self, call: &CellCall) -> Result<Vec<TaskRecord>, String> {
        let conversation = call.conversation_id;
        self.chat
            .commit(|tx| {
                let query = TaskQuery {
                    conversation_id: Some(conversation),
                    kind: Some(CHILD_KIND.into()),
                    ..TaskQuery::default()
                };
                let mut records = Vec::new();
                let mut cursor = None;
                loop {
                    let page = tx.committed_scan_tasks(&query, 256, cursor.as_ref())?;
                    records.extend(page.items);
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                records.sort_by_key(|record| record.id);
                Ok(records)
            })
            .map_err(error)
    }

    /// The selected children (all of the calling conversation's when none are named).
    fn selected(&self, payload: &Map<String, Value>, call: &CellCall) -> Result<Vec<TaskId>, String> {
        let selectors = payload
            .get("selectors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if selectors.is_empty() {
            return Ok(self.children_of(call)?.iter().map(|record| record.id).collect());
        }
        selectors.iter().map(parse_child).collect()
    }

    fn record(&self, id: TaskId) -> Result<TaskRecord, String> {
        match self.chat.get_task(id).map_err(error)? {
            Some(record) if record.kind == CHILD_KIND => Ok(record),
            _ => Err(format!("Unknown subagent {}", child_id(id))),
        }
    }

    /// Wait for every selected child to have an answer, then return the results.
    fn collect(&self, payload: &Map<String, Value>, call: &CellCall, signal: &Cancel) -> OpResult {
        let ids = self.selected(payload, call)?;
        let timeout = payload
            .get("timeout_ms")
            .and_then(Value::as_f64)
            .filter(|ms| *ms > 0.0)
            .map(|ms| Instant::now() + Duration::from_millis(ms as u64));
        let mut results = Vec::new();
        for id in ids {
            loop {
                let record = self.record(id)?;
                if child_done(&record) {
                    results.extend(child_report(&record));
                    break;
                }
                if record.state.status() != TaskStatus::Terminal && !is_keep(&record) {
                    // An ordinary child becomes terminal: wait for its record, bounded by the cell and the timeout.
                    let (done, expired) = (Cancel::new(), Cancel::new());
                    let limit = timeout.map(|deadline| deadline.saturating_duration_since(Instant::now()));
                    let timer = limit.map(|limit| {
                        let (done, expired) = (done.clone(), expired.clone());
                        std::thread::spawn(move || {
                            if done.wait_timeout(limit).is_none() {
                                expired.cancel(HarnessError::msg("timeout"));
                            }
                        })
                    });
                    let linked = Cancel::any(&[signal, &expired]);
                    let waited = self.chat.wait_for_task(id, &Ctx::with_signal(linked.signal()));
                    done.abort();
                    if let Some(timer) = timer {
                        let _ = timer.join();
                    }
                    if waited.is_ok() {
                        continue;
                    }
                }
                if let Some(reason) = signal.reason() {
                    return Err(reason.message());
                }
                if timeout.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(format!("Timed out waiting for subagent {}", child_id(id)));
                }
                signal.wait_timeout(POLL);
            }
        }
        Ok(json!({ "results": results }))
    }

    /// Never waits: the results of children with an answer and the IDs of those still running.
    fn results(&self, payload: &Map<String, Value>, call: &CellCall) -> OpResult {
        let mut results = Vec::new();
        let mut running = Vec::new();
        for id in self.selected(payload, call)? {
            let record = self.record(id)?;
            match child_report(&record) {
                Some(report) => results.push(report),
                None => running.push(json!(child_id(id))),
            }
        }
        Ok(json!({ "results": results, "running": running }))
    }

    fn list(&self, call: &CellCall) -> OpResult {
        let children: Vec<Value> = self
            .children_of(call)?
            .iter()
            .map(|record| {
                let status = match record.state.status() {
                    TaskStatus::Terminal => "ended",
                    _ if is_keep(record) => "kept",
                    _ => "running",
                };
                json!({
                    "id": child_id(record.id),
                    "name": record.input["name"],
                    "keep": is_keep(record),
                    "status": status,
                })
            })
            .collect();
        Ok(json!({ "subagents": children }))
    }

    fn cancel(&self, payload: &Map<String, Value>, signal: &Cancel) -> OpResult {
        let selector = payload
            .get("selector")
            .or_else(|| payload.get("id"))
            .ok_or("rlm.cancel needs a subagent id")?;
        let id = parse_child(selector)?;
        self.record(id)?;
        let result = self.chat.abort_task(id, &Ctx::with_signal(signal)).map_err(error)?;
        Ok(json!({ "cancelled": result == crate::scheduler::AbortResult::Marked }))
    }

    /// Ask a persistent child a follow-up question: an input submission to its conversation, answered there; the
    /// caller's context grows only by the question and the answer. The wait is bounded by `timeout_ms` (default
    /// [`ASK_TIMEOUT`]); when it runs out the answer is `status: "timeout"` and the question stays queued in the
    /// child, which keeps working on it.
    fn ask(&self, payload: &Map<String, Value>, call: &CellCall, signal: &Cancel) -> OpResult {
        let id = parse_child(payload.get("child").ok_or("rlm.ask needs a child")?)?;
        let question = payload
            .get("question")
            .and_then(Value::as_str)
            .filter(|question| !question.trim().is_empty())
            .ok_or("rlm.ask question must be a non-empty string")?;
        let record = self.record(id)?;
        if !is_keep(&record) {
            return Err(format!(
                "Subagent {} is not persistent; spawn it with keep=True to ask it follow-ups",
                child_id(id)
            ));
        }
        let conversation = loop {
            let record = self.record(id)?;
            if record.state.status() == TaskStatus::Terminal {
                return Err(format!("Subagent {} has ended", child_id(id)));
            }
            if let Some(conversation) = child_conversation(&record) {
                break conversation;
            }
            if let Some(reason) = signal.reason() {
                return Err(reason.message());
            }
            signal.wait_timeout(POLL);
        };
        let _ = call;
        let request = payload.get("request_id").and_then(Value::as_str);
        let mut draft = SubmissionDraft::input(question).when_busy(WhenBusy::FollowUp);
        if let Some(request) = request {
            draft = draft.request_id(request);
        }
        let handle = self
            .chat
            .conversation(conversation)
            .map_err(error)?
            .ok_or_else(|| format!("Subagent {} has no conversation", child_id(id)))?;
        let limit = payload
            .get("timeout_ms")
            .and_then(Value::as_f64)
            .filter(|ms| *ms > 0.0)
            .map_or(ASK_TIMEOUT, |ms| Duration::from_millis(ms as u64));
        let submission = handle.submit(draft).map_err(error)?;
        let (done, expired) = (Cancel::new(), Cancel::new());
        let timer = {
            let (done, expired) = (done.clone(), expired.clone());
            std::thread::spawn(move || {
                if done.wait_timeout(limit).is_none() {
                    expired.cancel(HarnessError::msg("timeout"));
                }
            })
        };
        let linked = Cancel::any(&[signal, &expired]);
        let waited = submission.wait(&Ctx::with_signal(linked.signal()));
        done.abort();
        let _ = timer.join();
        if waited.is_err() && signal.reason().is_none() && expired.reason().is_some() {
            return Ok(json!({
                "id": child_id(id),
                "status": "timeout",
                "error": format!(
                    "No answer from subagent {} within {} s; it is still working on the question. Carry on and \
                     rlm.ask it again later (a new question queues behind this one), or rlm.cancel it.",
                    child_id(id),
                    limit.as_secs_f64().round()
                ),
            }));
        }
        let settled = waited.map_err(error)?;
        match (settled.status, settled.answer) {
            (SubmissionStatus::Done, Some(answer)) => {
                let text = self
                    .chat
                    .commit(|tx| {
                        let entry = tx.entry(answer)?;
                        Ok(entry
                            .and_then(|entry| entry.model)
                            .and_then(|model| model.into_iter().next()))
                    })
                    .map_err(error)?
                    .map(|message| {
                        message["content"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|block| block["text"].as_str().map(str::to_owned))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                Ok(json!({ "id": child_id(id), "status": "succeeded", "value": text }))
            }
            _ => Ok(json!({
                "id": child_id(id),
                "status": "failed",
                "error": settled.detail.or(settled.reason.map(Value::String)).unwrap_or(Value::Null),
            })),
        }
    }
}

impl HostOps {
    /// The calling agent's name: what it registered, else `root` for the root conversation.
    fn agent_name(&self, call: &CellCall) -> Result<String, String> {
        let conversation = call.conversation_id;
        let named = self
            .chat
            .commit(|tx| super::swarm::name_of(tx, conversation))
            .map_err(error)?;
        Ok(named.unwrap_or_else(|| {
            if conversation == durable::ROOT_CONVERSATION_ID {
                "root".into()
            } else {
                format!("conversation-{}", conversation.0)
            }
        }))
    }

    fn swarm(&self, kind: &str, payload: &Map<String, Value>, call: &CellCall) -> OpResult {
        use super::swarm;
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let me = self.agent_name(call)?;
        let now = self.chat.now();
        let conversation = call.conversation_id;
        let result = match kind {
            "swarm.register" => {
                let name = text("name").map(str::to_owned).unwrap_or(me);
                self.chat
                    .commit(|tx| swarm::register(tx, &name, Some(conversation), None, payload, now))
            }
            "swarm.agents" => self.chat.commit(swarm::agents),
            "swarm.route" => {
                let query = text("query").ok_or("swarm.route needs a query")?.to_owned();
                self.chat.commit(|tx| swarm::route(tx, &query))
            }
            "swarm.send" => {
                let to = text("to").ok_or("swarm.send needs a recipient")?.to_owned();
                let body = text("text").ok_or("swarm.send needs text")?.to_owned();
                let id = self
                    .chat
                    .commit(|tx| swarm::send(tx, &me, &to, &body, now))
                    .map_err(error)?;
                let mut woke = false;
                if payload.get("wake").and_then(Value::as_bool).unwrap_or(false) {
                    let agents = self.chat.commit(swarm::agents).map_err(error)?;
                    if let Some(target) = agents[&to]["conversation"].as_u64()
                        && let Some(handle) = self.chat.conversation(durable::ConversationId(target)).map_err(error)?
                    {
                        let draft =
                            SubmissionDraft::input(format!("Message from {me}: {body}")).when_busy(WhenBusy::FollowUp);
                        handle.submit(draft).map_err(error)?;
                        woke = true;
                    }
                }
                return Ok(json!({ "id": id, "woke": woke }));
            }
            "swarm.inbox" => {
                let all = payload.get("all").and_then(Value::as_bool).unwrap_or(false);
                let mark = payload.get("mark").and_then(Value::as_bool).unwrap_or(true);
                let name = text("name").map(str::to_owned).unwrap_or(me);
                self.chat.commit(|tx| swarm::inbox(tx, &name, all, mark))
            }
            "board.add" => {
                let title = text("title").ok_or("board.add needs a title")?.to_owned();
                let detail = text("detail").map(str::to_owned);
                self.chat
                    .commit(|tx| swarm::board_add(tx, &me, &title, detail.as_deref(), now))
            }
            "board.claim" | "board.update" => {
                let id = payload
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or("A board task id is a number")?;
                if kind == "board.claim" {
                    self.chat.commit(|tx| swarm::board_claim(tx, &me, id, now))
                } else {
                    let (status, note) = (text("status").map(str::to_owned), text("note").map(str::to_owned));
                    self.chat
                        .commit(|tx| swarm::board_update(tx, &me, id, status.as_deref(), note.as_deref(), now))
                }
            }
            "board.list" => {
                let status = text("status").map(str::to_owned);
                self.chat.commit(|tx| swarm::board_list(tx, status.as_deref()))
            }
            "notes.read" => self.chat.commit(swarm::notes_read),
            "notes.append" => {
                let line = text("text").ok_or("notes.append needs text")?.to_owned();
                self.chat.commit(|tx| swarm::notes_append(tx, &me, &line))
            }
            _ => {
                let body = text("text").ok_or("notes.write needs text")?.to_owned();
                let expect = payload.get("expect_version").and_then(Value::as_u64);
                self.chat.commit(|tx| swarm::notes_write(tx, &body, expect))
            }
        };
        result.map_err(error)
    }
}

fn is_keep(record: &TaskRecord) -> bool {
    record.input["keep"] == true
}

fn child_conversation(record: &TaskRecord) -> Option<durable::ConversationId> {
    let checkpoint = match &record.state {
        durable::TaskState::Pending { checkpoint }
        | durable::TaskState::Running { checkpoint }
        | durable::TaskState::Waiting { checkpoint, .. } => checkpoint.clone(),
        _ => return None,
    };
    match serde_json::from_value::<ChildCheckpoint>(checkpoint).ok()? {
        ChildCheckpoint::Wait { conversation, .. } | ChildCheckpoint::Idle { conversation, .. } => Some(conversation),
        ChildCheckpoint::Start => None,
    }
}

struct CellOps {
    ops: HostOps,
    call: CellCall,
    fallback: Option<Arc<dyn HostHandler>>,
}

impl HostHandler for CellOps {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if !HostOps::serves(kind) {
            return match &self.fallback {
                Some(fallback) => fallback.handle(kind, payload, signal),
                None => Err(format!("Unknown host request {kind}")),
            };
        }
        // The kernel's signal ends with the cell; mirror it for host-side waits.
        let cancel = Cancel::new();
        let watcher = {
            let (cancel, signal) = (cancel.clone(), signal.clone());
            std::thread::spawn(move || {
                while !signal.wait(Duration::from_millis(50)) {
                    if cancel.is_cancelled() {
                        return;
                    }
                }
                cancel.cancel(HarnessError::msg(
                    signal.reason().unwrap_or_else(|| "cell ended".into()),
                ));
            })
        };
        let result = match self.ops.handle(kind, &payload, &self.call, &cancel) {
            Some(result) => result,
            None => match &self.fallback {
                Some(fallback) => fallback.handle(kind, payload, signal),
                None => Err(format!("Unknown host request {kind}")),
            },
        };
        // The watcher sees the cancellation within one poll and exits; nothing waits for it.
        cancel.abort();
        drop(watcher);
        result
    }
}
