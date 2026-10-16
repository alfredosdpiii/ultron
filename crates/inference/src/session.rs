//! A session's frames (`InferenceRuntime` of inference.ts): admission into the shared budget ledger, the frames
//! still running, their traces, and how a frame runs.
//!
//! With a task host ([`FrameTasks`], the session's typed-agent host) a frame is admitted as an `rlm-frame@1` task
//! under its caller: the calling thread waits for the task's terminal result (cancelling it when the calling cell is
//! aborted) while the task host runs [`FrameSession::execute`] on the task's own thread, which reserves and settles
//! each provider request in the usage ledger. A frame with `budget.depth > 1` runs its conversation as an agent on
//! the task's lane with the `rlm` cell as its only tool; requests from that lane's kernel are nested frames, which
//! draw on the frame's pool with one level less depth. Without a task host (module tests) frames run on the
//! calling thread and cannot run code.

use super::{
    Engine, Kind, LAST_OUTPUTS, Ledger, MAX_DEPTH, MIN_TRANCHE, OUTPUT_PREVIEW_CHARS, Options, Outcome,
    REQUEST_OVERHEAD_TOKENS, View, bounded, charged_tokens, frame_prompt, frame_system, js_len, js_trim, lock,
    parse_reply, repair_prompt, trace,
};
use crate::trace::{Budget, Frame, Spec, TraceStore, Traces};
use agent::{Agent, AgentOptions, AgentTool, StreamFn};
use ai::{AssistantContent, AssistantMessage, Message, StopReason};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const FOLLOW_UP_EXHAUSTED: &str = "budget exhausted before a follow-up request (a retry or tool round)";

/// The task host frames are admitted through (`NativeHostApi`): `rlm-frame@1` tasks in the session's task journal,
/// the usage ledger they charge, and the lanes that frames with depth run their `rlm` cells on.
pub trait FrameTasks: Send + Sync {
    /// The task `lane` acts for (`callerTaskId`); none for the root lane.
    fn caller_task(&self, lane: &str) -> Option<String>;
    /// Admit an `rlm-frame@1` task with `input` (`{frame, task}`) for `lane`; returns its task id.
    fn spawn(&self, lane: &str, input: Value, model: Option<&str>, timeout_ms: u64) -> Result<String, String>;
    /// Wait for the task's terminal result (`{status, value?, error?}`); once `signal` aborts the task is cancelled
    /// ("the calling cell was aborted") and its cancelled result returned.
    fn result(&self, lane: &str, task_id: &str, signal: &Signal) -> Value;
    /// Reserve one model request in the usage ledger (`{kind: "model", rootId?, taskId, parentTaskId, requestKey,
    /// budgetId, timeoutMs | deadlineAt}`); `None` without a ledger.
    fn reserve(&self, request: &Value) -> Result<Option<Value>, String>;
    /// Settle a reservation with the request's status and provider-reported usage (`{inputTokens, outputTokens,
    /// totalTokens, cost}`).
    fn settle(&self, reservation: &Value, status: &str, usage: Option<&Value>);
    /// The `rlm` tool on `lane` (its own kernel) for a frame that runs code.
    fn code_tool(&self, lane: &str) -> Result<Arc<dyn AgentTool>, String>;
    /// Close `lane` once its frame ended.
    fn close_lane(&self, lane: &str);
}

/// One `rlm-frame@1` task run, as the task host hands it over (`NativeFrameRun`).
pub struct FrameExecution {
    pub task_id: String,
    pub input: Value,
    pub lane: String,
    pub usage_root: Option<String>,
    pub deadline_at: Option<f64>,
    pub timeout_ms: u64,
    /// Aborted when the task is cancelled or times out.
    pub signal: Signal,
}

/// Where a new call's budget sits: its depth, the frame it was made from, and its pool limits.
pub(crate) struct Admission {
    pub depth: u64,
    parent: Option<Arc<Live>>,
    caller_task: Option<String>,
    calls: Option<u64>,
    tokens: Option<u64>,
    default_tokens: bool,
}

/// A frame that has not finished: its state, the call it belongs to, and its outcome once known.
pub(crate) struct Live {
    id: String,
    node: usize,
    frame: Mutex<Frame>,
    budget: Arc<Budget>,
    options: Arc<Options>,
    engine: Engine,
    outcome: Mutex<Option<Outcome>>,
    finalized: AtomicBool,
}

impl Live {
    fn outcome(&self) -> Option<Outcome> {
        lock(&self.outcome).clone()
    }

    fn set_outcome(&self, outcome: Outcome) {
        *lock(&self.outcome) = Some(outcome);
    }

    fn spec(&self) -> Spec<'_> {
        Spec {
            contract: self.options.contract.as_ref(),
            max_repairs: self.options.max_repairs,
            depth: self.options.depth,
            model: self.options.model.as_deref(),
        }
    }
}

/// The frames of one session; clones of the `Arc` share them.
pub struct FrameSession {
    ledger: Mutex<Ledger>,
    labels: Mutex<HashMap<String, String>>,
    traces: Mutex<Traces>,
    /// Frames admitted and not yet finished, in admission order.
    live: Mutex<Vec<Arc<Live>>>,
    /// The frame each running frame task's lane acts for.
    lanes: Mutex<HashMap<String, Arc<Live>>>,
    tasks: OnceLock<Arc<dyn FrameTasks>>,
}

impl FrameSession {
    /// A session whose traces live in `store`; frames run on their callers' threads until [`Self::set_tasks`].
    pub fn new(store: Arc<dyn TraceStore>) -> Arc<Self> {
        Arc::new(FrameSession {
            ledger: Mutex::new(Ledger::default()),
            labels: Mutex::new(HashMap::new()),
            traces: Mutex::new(Traces::new(store)),
            live: Mutex::new(Vec::new()),
            lanes: Mutex::new(HashMap::new()),
            tasks: OnceLock::new(),
        })
    }

    /// Admit every later frame through `tasks` (set once).
    pub fn set_tasks(&self, tasks: Arc<dyn FrameTasks>) {
        let _ = self.tasks.set(tasks);
    }

    pub(crate) fn has_tasks(&self) -> bool {
        self.tasks.get().is_some()
    }

    pub(crate) fn label(&self, digest: &str) -> Option<String> {
        lock(&self.labels).get(digest).cloned()
    }

    pub(crate) fn set_label(&self, digest: &str, label: String) {
        lock(&self.labels).insert(digest.to_string(), label);
    }

    /// `options`' budget placement: a request from a running frame's lane is nested under that frame, with at
    /// most one level less depth (default: all of it); a root request defaults to depth 1. A top-level map without
    /// a token limit takes `default_tokens`.
    pub(crate) fn admission(
        &self,
        lane: &str,
        requested_depth: Option<u64>,
        calls: Option<u64>,
        tokens: Option<u64>,
        default_tokens: Option<u64>,
    ) -> Admission {
        let parent = lock(&self.lanes).get(lane).cloned();
        let ceiling = parent
            .as_ref()
            .map_or(MAX_DEPTH, |frame| frame.options.depth.saturating_sub(1));
        let depth = requested_depth
            .unwrap_or(if parent.is_some() { ceiling } else { 1 })
            .min(ceiling);
        let caller_task = match &parent {
            Some(frame) => lock(&frame.frame).task_id.clone(),
            None => self.tasks.get().and_then(|tasks| tasks.caller_task(lane)),
        };
        let use_default = tokens.is_none() && parent.is_none() && default_tokens.is_some();
        Admission {
            depth,
            calls,
            tokens: if use_default { default_tokens } else { tokens },
            default_tokens: use_default,
            parent,
            caller_task,
        }
    }

    /// The call's budget node, under its parent frame's node when nested.
    pub(crate) fn budget(&self, admission: &Admission) -> Budget {
        let parent_node = admission.parent.as_ref().map(|frame| frame.node);
        let node = lock(&self.ledger).add(parent_node, admission.calls, admission.tokens);
        Budget {
            id: trace::uuid("budget"),
            node,
            depth: admission.depth,
            parent: admission.parent.as_ref().map(|frame| frame.id.clone()),
            default_tokens: admission.default_tokens,
        }
    }

    /// `newFrame`: a frame under `budget`. Its first request's call is claimed now, in admission order, so a shared
    /// pool decides deterministically which frames run; without depth or a call it is already incomplete.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit(
        &self,
        budget: &Arc<Budget>,
        options: &Arc<Options>,
        engine: &Engine,
        admission: &Admission,
        task: &str,
        views: Vec<View>,
        kind: &'static str,
        batch: usize,
    ) -> Arc<Live> {
        let mut ledger = lock(&self.ledger);
        let mut frame = Frame::new(&mut ledger, budget, task, views);
        frame.kind = kind;
        frame.batch = batch;
        frame.caller_task = admission.caller_task.clone();
        frame.parent_frame = admission.parent.as_ref().map(|frame| frame.id.clone());
        let mut outcome = None;
        if options.depth < 1 {
            outcome = Some(Outcome::Incomplete(
                "depth_exhausted",
                "this frame's budget allows no nested frames".into(),
            ));
        } else if ledger.hold(budget.node, Kind::Calls, 1) {
            frame.call_held = true;
        } else {
            outcome = Some(Outcome::Incomplete(
                "budget_exhausted",
                "no call left in the budget for this frame".into(),
            ));
        }
        drop(ledger);
        let live = Arc::new(Live {
            id: frame.id.clone(),
            node: frame.node,
            frame: Mutex::new(frame),
            budget: budget.clone(),
            options: options.clone(),
            engine: engine.clone(),
            outcome: Mutex::new(outcome),
            finalized: AtomicBool::new(false),
        });
        lock(&self.live).push(live.clone());
        live
    }

    /// Release the call claimed at admission if no request used it.
    fn release_call(&self, live: &Live) {
        let mut ledger = lock(&self.ledger);
        let mut frame = lock(&live.frame);
        if frame.call_held {
            frame.call_held = false;
            ledger.release(live.node, Kind::Calls, 1);
        }
    }

    /// `runFrame`: admit the frame as an `rlm-frame@1` task under the caller, wait for its result, and cancel it with
    /// the calling cell (without a task host: run it here). Always ends with the frame finalized.
    pub(crate) fn run_frame(&self, live: &Arc<Live>, lane: &str, cell: &Signal) {
        if live.outcome().is_none() {
            let outcome = self.start(live, lane, cell);
            if let Some(outcome) = outcome
                && live.outcome().is_none()
            {
                live.set_outcome(outcome);
            }
        }
        self.release_call(live);
        self.finalize(live);
    }

    fn start(&self, live: &Arc<Live>, lane: &str, cell: &Signal) -> Option<Outcome> {
        if cell.is_aborted() {
            return Some(Outcome::Error("cancelled: the calling cell was aborted".into()));
        }
        let own = live.engine.own_model();
        if let Some(model) = live.options.model.as_deref().filter(|model| *model != own) {
            return Some(Outcome::Error(format!(
                "Admission refused: model {model} is not available to this host"
            )));
        }
        let Some(tasks) = self.tasks.get() else {
            return Some(self.converse(live, cell, None));
        };
        let task = lock(&live.frame).task.clone();
        let input = json!({"frame": live.id, "task": bounded(&task, 200)});
        let timeout_ms = live.options.timeout.as_millis() as u64;
        let task_id = match tasks.spawn(lane, input, live.options.model.as_deref(), timeout_ms) {
            Ok(id) => id,
            Err(error) => return Some(Outcome::Error(format!("Admission refused: {error}"))),
        };
        lock(&live.frame).task_id = Some(task_id.clone());
        let result = tasks.result(lane, &task_id, cell);
        Some(if result["status"] == "succeeded" {
            Outcome::Complete(result.get("value").cloned().unwrap_or(Value::Null))
        } else {
            let status = result["status"].as_str().unwrap_or("failed");
            let error = result["error"].as_str().unwrap_or("frame did not finish");
            Outcome::Error(format!("{status}: {error}"))
        })
    }

    /// The task host's frame runner (`execute`): run the frame task `run` on this thread. The frame's value
    /// succeeds the task; an `Incomplete` or an error fails it, and an aborted run is cancelled.
    pub fn execute(self: &Arc<Self>, run: FrameExecution) -> Result<Value, String> {
        let id = run.input["frame"].as_str().unwrap_or_default();
        let live = lock(&self.live).iter().find(|live| live.id == id).cloned();
        let Some(live) = live else {
            return Err("rlm-frame tasks are started only by rlm.infer and rlm.map".into());
        };
        {
            let mut frame = lock(&live.frame);
            frame.task_id = Some(run.task_id.clone());
            frame.lane = Some(run.lane.clone());
        }
        lock(&self.lanes).insert(run.lane.clone(), live.clone());
        let outcome = if live.options.depth > 1 {
            let outcome = self.converse_with_code(&live, &run);
            if let Some(tasks) = self.tasks.get() {
                tasks.close_lane(&run.lane);
            }
            outcome
        } else {
            self.converse(&live, &run.signal, Some(&run))
        };
        lock(&self.lanes).remove(&run.lane);
        self.release_call(&live);
        live.set_outcome(outcome.clone());
        match outcome {
            Outcome::Complete(value) => Ok(value),
            _ if run.signal.is_aborted() => Err("cancelled".into()),
            Outcome::Incomplete(reason, detail) => Err(format!("Incomplete ({reason}): {detail}")),
            Outcome::Error(error) => Err(error),
        }
    }

    /// Reserve attempt `attempt` of a task-run frame in the usage ledger.
    fn reserve(&self, live: &Live, run: Option<&FrameExecution>, attempt: u64) -> Result<Option<Value>, String> {
        let (Some(run), Some(tasks)) = (run, self.tasks.get()) else {
            return Ok(None);
        };
        let mut request = json!({
            "kind": "model",
            "taskId": run.task_id,
            "parentTaskId": run.task_id,
            "requestKey": format!("{}:frame:{attempt}", run.task_id),
            "budgetId": live.budget.id,
        });
        if let Some(root) = &run.usage_root {
            request["rootId"] = json!(root);
        }
        match run.deadline_at {
            Some(deadline) => request["deadlineAt"] = json!(deadline),
            None => request["timeoutMs"] = json!(run.timeout_ms),
        }
        tasks.reserve(&request)
    }

    fn settle(&self, reservation: Option<Value>, status: &str, usage: Option<Value>) {
        if let (Some(reservation), Some(tasks)) = (reservation, self.tasks.get()) {
            tasks.settle(&reservation, status, usage.as_ref());
        }
    }

    /// The frame's code-free conversation: the seeded prompt, then re-asks within budget until the contract holds.
    /// A request the provider does not answer before the deadline (or before the frame is cancelled) is refunded.
    fn converse(&self, live: &Live, signal: &Signal, run: Option<&FrameExecution>) -> Outcome {
        let options = &live.options;
        let engine = &live.engine;
        let deadline = Instant::now() + options.timeout;
        let system = frame_system(options);
        let mut conversation_chars = js_len(&system) as u64;
        let mut message = {
            let mut frame = lock(&live.frame);
            frame.model = Some(engine.own_model());
            frame_prompt(&frame.task, &frame.views, options.contract.as_ref())
        };
        let mut history: Vec<Message> = Vec::new();
        for attempt in 0u64.. {
            if signal.is_aborted() || Instant::now() >= deadline {
                return Outcome::Error("cancelled".into());
            }
            let estimate = (conversation_chars + js_len(&message) as u64).div_ceil(3) + REQUEST_OVERHEAD_TOKENS;
            if let Some(error) = self.window_error(engine, estimate) {
                return Outcome::Error(error);
            }
            let Some((held, tranche)) = self.claim(live, estimate) else {
                return Outcome::Incomplete("budget_exhausted", exhausted_detail(attempt));
            };
            conversation_chars += js_len(&message) as u64;
            history.push(user_message(&message));
            let reservation = match self.reserve(live, run, attempt) {
                Ok(reservation) => reservation,
                Err(error) => {
                    self.release(live, held);
                    return Outcome::Error(error);
                }
            };
            let max_tokens = cap(engine, tranche);
            let response = engine.request(
                &system,
                &history,
                max_tokens,
                options.thinking.as_deref(),
                deadline,
                signal,
            );
            let charged = match &response {
                Ok(Some(reply)) => charged_tokens(&reply.usage),
                _ => 0,
            };
            self.settle_request(
                live,
                held,
                tranche,
                response.as_ref().ok().and_then(Option::as_ref),
                charged,
            );
            let (status, usage) = match &response {
                Ok(Some(reply)) => ("succeeded", measured(std::slice::from_ref(reply))),
                Ok(None) => ("cancelled", None),
                Err(_) if signal.is_aborted() => ("cancelled", None),
                Err(_) => ("failed", None),
            };
            self.settle(reservation, status, usage);
            let reply = match response {
                Ok(Some(reply)) => reply,
                Ok(None) => return Outcome::Error("cancelled".into()),
                Err(error) => return Outcome::Error(format!("frame request failed: {}", Value::String(error))),
            };
            add_measured(live, std::slice::from_ref(&reply));
            if signal.is_aborted() {
                return Outcome::Error("cancelled".into());
            }
            if let Some(outcome) = failed_reply(&reply) {
                return outcome;
            }
            let text = reply_text(&reply);
            history.push(Message::Assistant(reply));
            conversation_chars += js_len(&text) as u64;
            match self.judge(live, attempt, text) {
                Ok(outcome) => return outcome,
                Err(next) => message = next,
            }
        }
        unreachable!("the attempt counter is unbounded")
    }

    /// A frame with depth: its conversation is an agent on the task's lane with the `rlm` cell as its only tool.
    /// Each attempt is one prompt; its first request uses the hold claimed for it, every further request (a tool
    /// round) claims its own or is refused unsent, and each response is charged as it arrives.
    fn converse_with_code(self: &Arc<Self>, live: &Arc<Live>, run: &FrameExecution) -> Outcome {
        let Some(tasks) = self.tasks.get() else {
            return Outcome::Error("frames that run code need a task host".into());
        };
        let options = &live.options;
        let engine = &live.engine;
        let tool = match tasks.code_tool(&run.lane) {
            Ok(tool) => tool,
            Err(error) => return Outcome::Error(error),
        };
        let deadline = Instant::now() + options.timeout;
        let system = frame_system(options);
        let mut message = {
            let mut frame = lock(&live.frame);
            frame.model = Some(engine.own_model());
            frame_prompt(&frame.task, &frame.views, options.contract.as_ref())
        };
        let pending: Arc<Mutex<Option<(u64, u64)>>> = Arc::default();
        let stream = BudgetedStream {
            session: self.clone(),
            live: live.clone(),
            pending: pending.clone(),
            deadline,
            signal: run.signal.clone(),
        };
        let mut agent_options = AgentOptions::new(engine.model.clone());
        agent_options.system_prompt = Some(system.clone());
        agent_options.tools = vec![tool];
        agent_options.thinking_level = options
            .thinking
            .clone()
            .filter(|_| engine.model.reasoning)
            .unwrap_or_else(|| "off".into());
        let mut agent = Agent::new(agent_options, Box::new(stream));
        // The frame's cancellation aborts the agent's run; the watcher ends with the frame.
        let (control, signal, done) = (agent.control(), run.signal.clone(), Signal::new());
        let finished = done.clone();
        let watcher = std::thread::spawn(move || {
            while !finished.is_aborted() {
                if signal.wait(Duration::from_millis(50)) {
                    control.abort();
                    return;
                }
            }
        });
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                done.abort("done");
                let _ = watcher.join();
                return Outcome::Error(error.to_string());
            }
        };
        let mut conversation_chars = js_len(&system) as u64;
        let outcome = 'attempts: {
            for attempt in 0u64.. {
                if run.signal.is_aborted() || Instant::now() >= deadline {
                    break 'attempts Outcome::Error("cancelled".into());
                }
                let estimate = (conversation_chars + js_len(&message) as u64).div_ceil(3) + REQUEST_OVERHEAD_TOKENS;
                if let Some(error) = self.window_error(engine, estimate) {
                    break 'attempts Outcome::Error(error);
                }
                let Some(hold) = self.claim(live, estimate) else {
                    break 'attempts Outcome::Incomplete("budget_exhausted", exhausted_detail(attempt));
                };
                *lock(&pending) = Some(hold);
                conversation_chars += js_len(&message) as u64;
                let reservation = match self.reserve(live, Some(run), attempt) {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        if let Some((held, _)) = lock(&pending).take() {
                            self.release(live, held);
                        }
                        break 'attempts Outcome::Error(error);
                    }
                };
                let before = agent.messages.len();
                runtime.block_on(agent.prompt_text(&message, Vec::new()));
                // A hold the prompt never used is released (`settlePrompt`).
                if let Some((held, _)) = lock(&pending).take() {
                    self.release(live, held);
                }
                let replies: Vec<AssistantMessage> = agent.messages[before..]
                    .iter()
                    .filter_map(|message| match message {
                        Message::Assistant(reply) => Some(reply.clone()),
                        _ => None,
                    })
                    .collect();
                let status = if run.signal.is_aborted() {
                    "cancelled"
                } else {
                    "succeeded"
                };
                self.settle(reservation, status, measured(&replies));
                add_measured(live, &replies);
                if run.signal.is_aborted() {
                    break 'attempts Outcome::Error("cancelled".into());
                }
                if let Some(exhausted) = lock(&live.frame).exhausted.clone() {
                    break 'attempts Outcome::Incomplete("budget_exhausted", exhausted);
                }
                let Some(reply) = replies.last() else {
                    break 'attempts Outcome::Error("frame run did not complete: failed".into());
                };
                if let Some(outcome) = failed_reply(reply) {
                    break 'attempts outcome;
                }
                let text = reply_text(reply);
                conversation_chars += js_len(&text) as u64;
                match self.judge(live, attempt, text) {
                    Ok(outcome) => break 'attempts outcome,
                    Err(next) => message = next,
                }
            }
            unreachable!("the attempt counter is unbounded")
        };
        done.abort("done");
        let _ = watcher.join();
        outcome
    }

    /// The request's window check: an estimate the model's context window cannot hold is an error.
    fn window_error(&self, engine: &Engine, estimate: u64) -> Option<String> {
        let window = engine.model.context_window;
        (window > 0 && estimate + MIN_TRANCHE > window).then(|| {
            format!(
                "context too large for {}/{}: about {estimate} tokens against a {window}-token window; pass smaller slices",
                engine.model.provider, engine.model.id
            )
        })
    }

    /// Claim the next request's call (the admission call first) and tokens.
    fn claim(&self, live: &Live, estimate: u64) -> Option<(u64, u64)> {
        let mut ledger = lock(&self.ledger);
        let mut frame = lock(&live.frame);
        let held = frame.call_held;
        frame.call_held = false;
        ledger.claim(live.node, estimate, live.engine.model.max_tokens, held)
    }

    /// Release a hold no request used.
    fn release(&self, live: &Live, held: u64) {
        let mut ledger = lock(&self.ledger);
        ledger.release(live.node, Kind::Calls, 1);
        ledger.release(live.node, Kind::Tokens, held);
    }

    /// Settle a sent request: release its hold and charge what the provider reported (a failed or timed-out request
    /// is refunded). One lock, so a concurrent frame never sees the hold released before the charge lands.
    fn settle_request(&self, live: &Live, held: u64, tranche: u64, reply: Option<&AssistantMessage>, charged: u64) {
        let mut ledger = lock(&self.ledger);
        ledger.release(live.node, Kind::Calls, 1);
        ledger.release(live.node, Kind::Tokens, held);
        if reply.is_some() {
            ledger.charge(live.node, Kind::Calls, 1);
            ledger.charge(live.node, Kind::Tokens, charged);
        }
        drop(ledger);
        if let Some(reply) = reply {
            let mut frame = lock(&live.frame);
            if frame.requests.len() < trace::TRACE_REQUEST_LIMIT {
                frame.requests.push(json!({
                    "at": trace::now_ms(), "tranche": tranche, "held": held, "charged": charged,
                    "status": serde_json::to_value(reply.stop_reason).unwrap_or(Value::Null),
                }));
            }
        }
    }

    /// Record an answer and judge it against the contract: the outcome, or the re-ask message.
    fn judge(&self, live: &Live, attempt: u64, text: String) -> Result<Outcome, String> {
        let options = &live.options;
        let output = bounded(&text, OUTPUT_PREVIEW_CHARS);
        let mut frame = lock(&live.frame);
        frame.outputs.push(output.clone());
        if frame.outputs.len() > LAST_OUTPUTS {
            frame.outputs.remove(0);
        }
        let record = |error: Option<&str>| {
            let mut record = json!({"attempt": attempt, "at": trace::now_ms(), "output": output});
            if let Some(error) = error {
                record["error"] = error.into();
            }
            record
        };
        let error = if js_trim(&text).is_empty() {
            "the reply is empty".to_string()
        } else {
            let parsed = match &options.contract {
                None => Ok(Value::String(text)),
                Some(contract) => parse_reply(&text, contract),
            };
            match parsed {
                Ok(value) => {
                    frame.attempts.push(record(None));
                    return Ok(Outcome::Complete(value));
                }
                Err(error) => error,
            }
        };
        frame.attempts.push(record(Some(&error)));
        if attempt >= options.max_repairs {
            return Ok(Outcome::Incomplete(
                "contract_unmet",
                format!("no valid answer after {} attempt(s): {error}", attempt + 1),
            ));
        }
        let fallback = json!({"type": "string", "minLength": 1});
        Err(repair_prompt(&error, options.contract.as_ref().unwrap_or(&fallback)))
    }

    /// `finalize`: the frame ends; its trace and summary are written and it leaves the running frames.
    fn finalize(&self, live: &Arc<Live>) {
        if live.finalized.swap(true, Ordering::SeqCst) {
            return;
        }
        lock(&live.frame).ended_at = Some(trace::now_ms());
        let outcome = live.outcome();
        let (document, summary) = {
            let mut ledger = lock(&self.ledger);
            let frame = lock(&live.frame);
            let spec = live.spec();
            (
                trace::trace(&mut ledger, &live.budget, &frame, &spec, outcome.as_ref()),
                trace::summary(&ledger, &live.budget, &frame, outcome.as_ref()),
            )
        };
        lock(&self.traces).write(&live.id, &document, summary);
        lock(&self.live).retain(|other| !Arc::ptr_eq(other, live));
        // Keep the metadata for the observation; drop the materialized text.
        for view in &mut lock(&live.frame).views {
            view.text = String::new();
        }
    }

    /// The frame's observation: its outcome, trace id, task id, what it spent and what remains.
    pub(crate) fn observation(&self, live: &Live) -> Value {
        let outcome = live
            .outcome()
            .unwrap_or_else(|| Outcome::Error("frame did not run".into()));
        let mut ledger = lock(&self.ledger);
        let frame = lock(&live.frame);
        let mut out = Map::new();
        let complete = matches!(outcome, Outcome::Complete(_));
        match outcome {
            Outcome::Complete(value) => {
                out.insert("status".into(), "complete".into());
                out.insert("value".into(), value);
            }
            Outcome::Incomplete(reason, detail) => {
                out.insert("status".into(), "incomplete".into());
                out.insert("reason".into(), reason.into());
                let detail = if reason == "budget_exhausted" && live.budget.default_tokens {
                    format!(
                        "{detail} (rlm.map's default budget is {} tokens; pass budget=Budget(tokens=...) to allow more)",
                        ledger.nodes[live.budget.node].tokens.limit.unwrap_or_default()
                    )
                } else {
                    detail
                };
                out.insert("detail".into(), detail.into());
            }
            Outcome::Error(error) => {
                out.insert("status".into(), "error".into());
                out.insert("error".into(), error.into());
            }
        }
        out.insert("trace_id".into(), frame.id.clone().into());
        if let Some(task) = &frame.task_id {
            out.insert("task_id".into(), task.clone().into());
        }
        out.insert("spent".into(), frame.spent(&ledger));
        out.insert("remaining".into(), frame.remaining(&mut ledger, live.options.depth));
        if !complete {
            out.insert("last_outputs".into(), frame.outputs.clone().into());
        }
        Value::Object(out)
    }

    /// The budget reply of an `rlm.map`: the shared node's snapshot and what remains of it.
    pub(crate) fn budget_reply(&self, budget: &Budget) -> Value {
        let mut ledger = lock(&self.ledger);
        let mut reply = budget.snapshot(&ledger);
        reply["remaining"] = json!({
            "calls": ledger.available(budget.node, Kind::Calls),
            "tokens": ledger.available(budget.node, Kind::Tokens),
            "depth": budget.depth,
        });
        reply
    }

    pub(crate) fn measured(&self, live: &Live) -> (u64, u64, f64) {
        lock(&live.frame).measured
    }

    /// `rlm.frames`: a trace by id or the newest summaries, running frames included.
    pub(crate) fn list(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        let live: Vec<Arc<Live>> = lock(&self.live).clone();
        let view = |live: &Live, full: bool| {
            let outcome = live.outcome();
            let mut ledger = lock(&self.ledger);
            let frame = lock(&live.frame);
            if full {
                trace::trace(&mut ledger, &live.budget, &frame, &live.spec(), outcome.as_ref())
            } else {
                trace::summary(&ledger, &live.budget, &frame, outcome.as_ref())
            }
        };
        let summaries: Vec<Value> = live.iter().map(|frame| view(frame, false)).collect();
        let by_id = |id: &str| live.iter().find(|frame| frame.id == id).map(|frame| view(frame, true));
        lock(&self.traces).list(payload, &by_id, &summaries)
    }
}

fn exhausted_detail(attempt: u64) -> String {
    if attempt == 0 {
        "the budget cannot cover the first request".to_string()
    } else {
        format!("the budget ran out after {attempt} attempt(s) without a valid answer")
    }
}

/// The request's output cap: the tranche, within the model's own maximum.
fn cap(engine: &Engine, tranche: u64) -> u64 {
    if engine.model.max_tokens > 0 {
        engine.model.max_tokens.min(tranche)
    } else {
        tranche
    }
}

fn user_message(text: &str) -> Message {
    Message::User(ai::UserMessage {
        content: ai::UserMessageContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn reply_text(reply: &AssistantMessage) -> String {
    reply
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A reply that ends the frame: the provider's usage limit (Incomplete), or a run that failed or was aborted.
fn failed_reply(reply: &AssistantMessage) -> Option<Outcome> {
    let limited = reply
        .diagnostics
        .iter()
        .flatten()
        .any(|d| d.kind == "provider_usage_limit");
    if limited {
        let detail = reply
            .error_message
            .clone()
            .unwrap_or("the provider's usage limit is reached".into());
        return Some(Outcome::Incomplete("usage_limit", detail));
    }
    if matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
        let status = if reply.stop_reason == StopReason::Aborted {
            "aborted"
        } else {
            "failed"
        };
        let error = reply.error_message.clone().unwrap_or_default();
        return Some(Outcome::Error(format!(
            "frame run did not complete: {status} ({})",
            json!({"message": error})
        )));
    }
    None
}

/// `measurement`: the provider-reported usage of a prompt's replies (none when a reply carries none, e.g. a
/// request that failed before any response).
fn measured(replies: &[AssistantMessage]) -> Option<Value> {
    if replies.is_empty() || replies.iter().any(|reply| reply.stop_reason == StopReason::Error) {
        return None;
    }
    let (mut input, mut output, mut total, mut cost) = (0u64, 0u64, 0u64, 0f64);
    for reply in replies {
        input += reply.usage.input;
        output += reply.usage.output;
        total += reply.usage.total_tokens;
        cost += reply.usage.cost.total;
    }
    Some(json!({"inputTokens": input, "outputTokens": output, "totalTokens": total, "cost": cost}))
}

fn add_measured(live: &Live, replies: &[AssistantMessage]) {
    let mut frame = lock(&live.frame);
    for reply in replies {
        frame.measured.0 += reply.usage.input;
        frame.measured.1 += reply.usage.output;
        frame.measured.2 += reply.usage.cost.total;
    }
}

/// An assistant message the run records without sending anything (a refused or cancelled request).
fn unsent(engine: &Engine, stop_reason: StopReason, error: String) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: engine.model.api.clone(),
        provider: engine.model.provider.clone(),
        model: engine.model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        diagnostics: None,
        usage: ai::Usage::default(),
        stop_reason,
        deferred: None,
        error_message: Some(error),
        raw_stop_reason: None,
        end_turn: None,
        provider_metadata: None,
        timestamp: trace::now_ms() as i64,
    }
}

/// A code frame's model requests (`before_request`, `before_payload`, `after_response` of inference.ts): each one
/// uses the prompt's pre-claimed hold or claims its own (refused unsent when the budget cannot cover it), is capped
/// at its tranche, and is charged what the provider reports.
struct BudgetedStream {
    session: Arc<FrameSession>,
    live: Arc<Live>,
    pending: Arc<Mutex<Option<(u64, u64)>>>,
    deadline: Instant,
    signal: Signal,
}

impl StreamFn for BudgetedStream {
    fn stream<'a>(
        &'a self,
        _model: &'a ai::Model,
        context: &'a ai::TranscriptContext,
        options: &'a ai::SimpleStreamOptions,
        sink: &'a mut ai::EventSink<'_>,
    ) -> ai::http::BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            let session = &self.session;
            let live = &self.live;
            let engine = &live.engine;
            let hold = lock(&self.pending).take().or_else(|| {
                let estimate = serde_json::to_string(context)
                    .map_or(0, |text| text.len() as u64)
                    .div_ceil(4)
                    + REQUEST_OVERHEAD_TOKENS;
                session.claim(live, estimate)
            });
            let Some((held, tranche)) = hold else {
                lock(&live.frame).exhausted = Some(FOLLOW_UP_EXHAUSTED.into());
                return unsent(
                    engine,
                    StopReason::Error,
                    format!("Inference frame {}: {FOLLOW_UP_EXHAUSTED}", live.id),
                );
            };
            let mut options = options.clone();
            options.base.api_key = engine.api_key.clone();
            options.base.max_tokens = Some(cap(engine, tranche));
            let response = engine
                .stream(context, &options, sink, self.deadline, &self.signal)
                .await;
            let charged = match &response {
                Ok(Some(reply)) => charged_tokens(&reply.usage),
                _ => 0,
            };
            session.settle_request(
                live,
                held,
                tranche,
                response.as_ref().ok().and_then(Option::as_ref),
                charged,
            );
            match response {
                Ok(Some(reply)) => reply,
                Ok(None) => unsent(engine, StopReason::Aborted, "cancelled".into()),
                Err(error) => unsent(engine, StopReason::Error, error),
            }
        })
    }
}
