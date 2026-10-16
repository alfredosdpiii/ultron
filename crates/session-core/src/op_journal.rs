//! The harness operation journal of one run on lane `main` in the format-4 session (a copy of
//! `session_journal::run::Run`, which writes exactly what the TypeScript drive commits), plus [`Run::reopen`]: the
//! durable runtime resumes an operation a dead worker left open, so its journal continues from the stored state
//! instead of being settled as interrupted.

use serde_json::{Map, Value, json};
use session_journal::run::{
    FrameEncoder, OP_META, OP_RESULT, OP_STATE, OP_TOOL_ARGS, PENDING_ENTRY, PENDING_FRAMES, PENDING_TOOL_OUTPUT,
    ULTRON_USAGE,
};

/// A compaction's durable preparation (`pi.op.preparation`, keyed `<operation>:<task>`).
pub const OP_PREPARATION: &str = "pi.op.preparation";
use session_journal::session::{BRANCH_TIP, LANE_STATE};
use session_journal::usage::num;
use session_journal::{Session, Write, lock_clock};

/// Stream options and retry policy of a generation (`timeoutMs` 300000, 4 attempts from 2000 ms).
fn stream_options() -> Value {
    json!({"timeoutMs": 300000, "maxRetryDelayMs": 60000})
}

fn retry_policy() -> Value {
    json!({"maxAttempts": 4, "baseDelayMs": 2000, "maxAgentDelayMs": 60000})
}

#[derive(Clone, Debug)]
struct ToolCallState {
    source_index: usize,
    call_id: String,
    result_entry_id: String,
    status: &'static str,
    result: Option<Value>,
}

/// One run in progress.
pub struct Run {
    operation_id: String,
    config: Value,
    settings: Value,
    source_tip: Option<String>,
    started_at: i64,
    last_operation_id: Value,
    latest_assistant: Option<String>,
    trigger: String,
    step_id: String,
    response_entry_id: String,
    usage_id: String,
    calls: Vec<ToolCallState>,
    assistant_entry_id: String,
    encoder: FrameEncoder,
    /// The operation's control state: running, or cancel requested (`{status, requestedAt}`).
    control: Value,
    /// The lane's queued messages (`pi.lane.state` `inbox`) while the run is current.
    inbox: Value,
    /// The entry the current generation answers when it took queued messages (else the run's trigger).
    generation_trigger: Option<String>,
    /// The generation follows an overflow compaction: another overflow fails the run (`overflowRecoveryUsed`).
    overflow_recovery_used: bool,
    /// The compaction running at this checkpoint (the operation's `summary.*` states).
    compaction: Option<Compaction>,
    /// An overflowed answer commits with the compaction it starts: `(task, preparation)`.
    compaction_with_answer: Option<(Value, Value)>,
    /// `pi.op.preparation` keys this operation wrote; its terminal transaction deletes them.
    preparations: Vec<String>,
    /// The next generation's `assistant.ready` was committed with the compaction before it (its step id is set).
    ready_written: bool,
    /// An error answer the run ends with commits in the run's terminal transaction (`providerError`): its entry,
    /// usage row and tip, until then.
    error_answer: Vec<Write>,
    /// The generation's retry policy (`generationContext.retryPolicy`, `normalizedRetryPolicy` of settings.json's
    /// `retry`).
    retry_policy: Value,
    /// The generation's attempt in progress (1-based).
    attempt: u64,
    /// The next error answer is retried: it commits with `assistant.retry_wait` until `(notBefore, error)`.
    retry_next: Option<(i64, String)>,
    /// The operation waits to retry: the attempt that comes next.
    retry_waiting: Option<u64>,
}

/// A compaction inside a run (packages/agent/src/harness/runtime/drive/structural.ts): its task (id, reason and the
/// checkpoint it resumes), the summary request's context and its usage row.
struct Compaction {
    task: Value,
    summary_context: Value,
    usage_ids: Vec<String>,
    /// The summary attempt in progress (1-based).
    attempt: u64,
}

/// The compaction a checkpoint left in progress when its worker died: the task as its state stored it.
pub struct OpenCompaction {
    pub task_id: String,
    pub reason: String,
    /// `{continuation, triggerEntryId}`: the checkpoint the compaction resumes.
    pub resume_after: Value,
}

fn entry(id: &str, parent: Option<&str>, message: Value) -> Write {
    let mut map = Map::new();
    map.insert("id".into(), id.into());
    map.insert("parentId".into(), parent.map_or(Value::Null, Value::from));
    map.insert("type".into(), "message".into());
    map.insert("message".into(), message);
    Write::Entry(map)
}

fn now(session: &Session) -> i64 {
    lock_clock(&session.clock()).now()
}

fn mint(session: &Session) -> String {
    lock_clock(&session.clock()).next_id()
}

impl Run {
    /// Admit a run: the prompt entry, the operation and the lane's current operation, then the first checkpoint.
    pub fn admit(session: &mut Session, config: Value, settings: Value, prompt: Value) -> Result<Run, String> {
        Run::admit_with(session, config, settings, prompt, &[], json!([]))
    }

    /// [`Run::admit`] that also places `captured` (`(entryId, message)` of the inbox items the admission takes) on
    /// the branch before the prompt, in the admission transaction, deletes their pending entries and leaves `inbox`
    /// (the items it did not take) in the lane's state (`selectAcceptedInbox` then the acceptance commit of lane.ts).
    pub fn admit_with(
        session: &mut Session,
        config: Value,
        settings: Value,
        prompt: Value,
        captured: &[(String, Value)],
        inbox: Value,
    ) -> Result<Run, String> {
        let started_at = prompt
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| now(session));
        let operation_id = mint(session);
        let prompt_id = mint(session);
        let source_tip = session.branch_tip("main").flatten();
        let last_operation_id = session
            .value(LANE_STATE, "main")
            .and_then(|s| s.get("lastOperationId").cloned())
            .unwrap_or(Value::Null);
        let run = Run {
            operation_id: operation_id.clone(),
            config,
            settings,
            source_tip: source_tip.clone(),
            started_at,
            last_operation_id,
            latest_assistant: None,
            trigger: prompt_id.clone(),
            step_id: String::new(),
            response_entry_id: String::new(),
            usage_id: String::new(),
            calls: Vec::new(),
            assistant_entry_id: String::new(),
            encoder: FrameEncoder::default(),
            control: json!({"status": "running"}),
            inbox,
            generation_trigger: None,
            overflow_recovery_used: false,
            compaction: None,
            compaction_with_answer: None,
            preparations: Vec::new(),
            ready_written: false,
            error_answer: Vec::new(),
            retry_policy: retry_policy(),
            attempt: 1,
            retry_next: None,
            retry_waiting: None,
        };
        let meta = json!({
            "operationId": operation_id, "lane": "main", "sourceTipId": source_tip, "startedAt": started_at,
            "intent": {"kind": "run", "promptEntryIds": [prompt_id]},
        });
        let starting = json!({
            "at": "starting", "control": {"status": "running"}, "settings": run.settings, "latestAssistantEntryId": null,
        });
        let mut writes = Vec::new();
        let mut parent = source_tip.clone();
        for (id, message) in captured {
            writes.push(entry(id, parent.as_deref(), message.clone()));
            parent = Some(id.clone());
        }
        writes.push(entry(&prompt_id, parent.as_deref(), prompt));
        writes.extend(captured.iter().map(|(id, _)| Write::delete(PENDING_ENTRY, id)));
        writes.extend([
            Write::set(BRANCH_TIP, "main", prompt_id.clone().into()),
            Write::set(OP_META, &operation_id, meta),
            Write::set(OP_STATE, &operation_id, starting),
            Write::set(LANE_STATE, "main", run.lane_state(true)),
        ]);
        session.commit(&writes)?;
        let checkpoint = run.checkpoint(json!({"kind": "need_assistant", "overflowRecoveryUsed": false}));
        session.commit(&[Write::set(OP_STATE, &run.operation_id, checkpoint)])?;
        Ok(run)
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// The open operation `operation_id` as its stored state left it: meta, state and lane state give the source
    /// tip, the control, the step and response ids, and the tool batch (each call's id from the assistant entry,
    /// each reported result from its pending entry).
    pub fn reopen(session: &Session, operation_id: &str) -> Result<Run, String> {
        let meta = session
            .value(OP_META, operation_id)
            .cloned()
            .ok_or_else(|| format!("Operation {operation_id} is not open"))?;
        let state = session.value(OP_STATE, operation_id).cloned().unwrap_or(Value::Null);
        let lane = session
            .value(LANE_STATE, "main")
            .cloned()
            .unwrap_or_else(|| json!({"inbox": []}));
        let text = |value: &Value| value.as_str().map(str::to_string);
        let context = &state["generationContext"];
        let prompt = meta["intent"]["promptEntryIds"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let config = match &context["configuration"] {
            Value::Null => session
                .value(session_journal::session::LANE_CONFIG, "main")
                .cloned()
                .unwrap_or(Value::Null),
            config => config.clone(),
        };
        let batch = &state["batch"];
        let assistant_entry_id = text(&batch["assistantEntryId"]).unwrap_or_default();
        let content = session
            .state()
            .entry(&assistant_entry_id)
            .map(|entry| entry["message"]["content"].clone())
            .unwrap_or(Value::Null);
        let calls = batch["calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|call| {
                let source_index = call["sourceIndex"].as_u64().unwrap_or(0) as usize;
                let result_entry_id = text(&call["resultEntryId"]).unwrap_or_default();
                let result = session
                    .value(PENDING_ENTRY, &result_entry_id)
                    .map(|pending| pending["payload"].clone());
                let status = match (call["status"].as_str(), &result) {
                    (_, Some(_)) => "outcome_ready",
                    (Some("effect_pending"), None) => "effect_pending",
                    _ => "planned",
                };
                ToolCallState {
                    source_index,
                    call_id: content[source_index]["id"].as_str().unwrap_or_default().to_string(),
                    result_entry_id,
                    status,
                    result,
                }
            })
            .collect();
        let resume_after = &state["task"]["boundary"]["resumeAfter"];
        let trigger = text(&state["triggerEntryId"])
            .or_else(|| text(&context["triggerEntryId"]))
            .or_else(|| text(&resume_after["triggerEntryId"]))
            .unwrap_or(prompt);
        // A compaction a dead worker left is not resumed; its preparation goes with the operation.
        let preparations = text(&state["task"]["taskId"])
            .map(|task| vec![format!("{operation_id}:{task}")])
            .unwrap_or_default();
        Ok(Run {
            operation_id: operation_id.to_string(),
            config,
            settings: state
                .get("settings")
                .cloned()
                .unwrap_or_else(session_journal::run::default_settings),
            source_tip: text(&meta["sourceTipId"]),
            started_at: meta["startedAt"].as_i64().unwrap_or(0),
            last_operation_id: lane.get("lastOperationId").cloned().unwrap_or(Value::Null),
            latest_assistant: text(&state["latestAssistantEntryId"]),
            generation_trigger: text(&context["triggerEntryId"]).filter(|t| *t != trigger),
            trigger,
            step_id: text(&context["stepId"])
                .or_else(|| text(&batch["turnId"]))
                .unwrap_or_default(),
            response_entry_id: text(&state["responseEntryId"]).unwrap_or_default(),
            usage_id: text(&state["usageId"]).unwrap_or_default(),
            calls,
            assistant_entry_id,
            encoder: FrameEncoder::default(),
            control: state.get("control").cloned().unwrap_or(json!({"status": "running"})),
            inbox: lane.get("inbox").cloned().unwrap_or(json!([])),
            overflow_recovery_used: context["overflowRecoveryUsed"] == true
                || state["continuation"]["overflowRecoveryUsed"] == true,
            compaction: None,
            compaction_with_answer: None,
            preparations,
            ready_written: false,
            error_answer: Vec::new(),
            retry_policy: match &context["retryPolicy"] {
                Value::Null => retry_policy(),
                policy => policy.clone(),
            },
            attempt: state["attempt"]
                .as_u64()
                .or_else(|| state["nextAttempt"].as_u64())
                .unwrap_or(1),
            retry_next: None,
            retry_waiting: (state["at"] == "assistant.retry_wait")
                .then(|| state["nextAttempt"].as_u64())
                .flatten(),
        })
    }

    /// The generation's retry policy from now on (`{maxAttempts, baseDelayMs, maxAgentDelayMs}`).
    pub fn set_retry_policy(&mut self, policy: Value) {
        self.retry_policy = policy;
    }

    /// The generation's attempt in progress (1-based).
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// The generation step (`generationContext.stepId`).
    pub fn step_id(&self) -> &str {
        &self.step_id
    }

    /// The operation waits to retry its generation: the attempt that comes next.
    pub fn retry_waiting(&self) -> Option<u64> {
        self.retry_waiting
    }

    /// The next error answer is retried (`assistant.retry_wait` until `not_before`, with `error`).
    pub fn schedule_retry(&mut self, not_before: i64, error: String) {
        self.retry_next = Some((not_before, error));
    }

    /// The compaction the stored state is in (`summary.*`), if the worker died during one.
    pub fn open_compaction(session: &Session, operation_id: &str) -> Option<OpenCompaction> {
        let state = session.value(OP_STATE, operation_id)?;
        let task = &state["task"];
        if !state["at"].as_str().is_some_and(|at| at.starts_with("summary.")) {
            return None;
        }
        Some(OpenCompaction {
            task_id: task["taskId"].as_str()?.to_string(),
            reason: task["reason"].as_str()?.to_string(),
            resume_after: task["boundary"]["resumeAfter"].clone(),
        })
    }

    /// The operation's state is at `assistant.effect_pending`: a response was requested and has not committed.
    pub fn awaiting_response(&self, session: &Session) -> bool {
        session
            .value(OP_STATE, &self.operation_id)
            .and_then(|s| s["at"].as_str())
            == Some("assistant.effect_pending")
    }

    /// Calls of the current batch whose result has not been reported.
    pub fn unreported_calls(&self) -> Vec<String> {
        self.calls
            .iter()
            .filter(|c| c.result.is_none())
            .map(|c| c.call_id.clone())
            .collect()
    }

    /// The prompt entry the operation started from.
    pub fn source_tip(&self) -> Option<&str> {
        self.source_tip.as_deref()
    }

    fn lane_state(&self, running: bool) -> Value {
        if running {
            json!({"currentOperationId": self.operation_id, "lastOperationId": self.last_operation_id, "inbox": self.inbox})
        } else {
            // Messages queued for the next run wait in the inbox past this run's end.
            let next_run: Vec<Value> = self
                .inbox
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["kind"] == "nextRun")
                .cloned()
                .collect();
            json!({"currentOperationId": null, "lastOperationId": self.operation_id, "inbox": next_run})
        }
    }

    fn scope(&self, at: &str) -> Map<String, Value> {
        let mut state = Map::new();
        state.insert("control".into(), self.control.clone());
        state.insert("settings".into(), self.settings.clone());
        state.insert(
            "latestAssistantEntryId".into(),
            self.latest_assistant.clone().map_or(Value::Null, Value::from),
        );
        state.insert("at".into(), at.into());
        state
    }

    fn checkpoint(&self, continuation: Value) -> Value {
        let mut state = self.scope("checkpoint");
        state.insert("continuation".into(), continuation);
        state.insert("triggerEntryId".into(), self.trigger.clone().into());
        Value::Object(state)
    }

    fn generation_context(&self) -> Value {
        json!({
            "stepId": self.step_id,
            "triggerEntryId": self.generation_trigger.as_ref().unwrap_or(&self.trigger),
            "configuration": self.config,
            "streamOptions": stream_options(), "retryPolicy": self.retry_policy,
            "overflowRecoveryUsed": self.overflow_recovery_used,
        })
    }

    /// `assistant.ready` then `assistant.effect_pending` before the model request.
    pub fn begin_assistant(
        &mut self,
        session: &mut Session,
        max_tokens: u64,
        context_window: u64,
    ) -> Result<(), String> {
        self.begin_assistant_with(session, max_tokens, context_window, Vec::new(), None)
    }

    /// The lane's queued messages as the run's lane state lists them (`inbox`).
    pub fn set_inbox(&mut self, inbox: Value) {
        self.inbox = inbox;
    }

    /// [`Run::begin_assistant`] with `before` committed in the `assistant.ready` transaction ahead of it: the
    /// queued (steering or follow-up) messages the generation takes, as the harness delivers its inbox; `trigger`
    /// is the last of them, the entry the generation answers.
    pub fn begin_assistant_with(
        &mut self,
        session: &mut Session,
        max_tokens: u64,
        context_window: u64,
        before: Vec<Write>,
        trigger: Option<String>,
    ) -> Result<(), String> {
        // An error answer the harness retries commits on its own first.
        let error_answer = std::mem::take(&mut self.error_answer);
        if !error_answer.is_empty() {
            session.commit(&error_answer)?;
        }
        if let Some(next) = self.retry_waiting.take() {
            // The retry wait ended (`runRetryWait`): the same step again, as the next attempt.
            let mut ready = self.scope("assistant.ready");
            ready.insert("generationContext".into(), self.generation_context());
            ready.insert("nextAttempt".into(), next.into());
            let mut writes = before;
            writes.push(Write::set(OP_STATE, &self.operation_id, Value::Object(ready)));
            session.commit(&writes)?;
            self.attempt = next;
        } else if std::mem::take(&mut self.ready_written) {
            self.attempt = 1;
            // The compaction before this generation committed its `assistant.ready`.
            if !before.is_empty() {
                session.commit(&before)?;
            }
        } else {
            self.attempt = 1;
            self.generation_trigger = trigger;
            self.step_id = mint(session);
            self.commit_ready(session, before)?;
        }
        self.response_entry_id = mint(session);
        self.usage_id = mint(session);
        self.encoder = FrameEncoder::default();
        let mut pending = self.scope("assistant.effect_pending");
        pending.insert("generationContext".into(), self.generation_context());
        pending.insert("attempt".into(), self.attempt.into());
        pending.insert("responseEntryId".into(), self.response_entry_id.clone().into());
        pending.insert("usageId".into(), self.usage_id.clone().into());
        pending.insert("intendedOutputLimit".into(), max_tokens.into());
        pending.insert("contextWindow".into(), context_window.into());
        session.commit(&[Write::set(OP_STATE, &self.operation_id, Value::Object(pending))])?;
        Ok(())
    }

    fn commit_ready(&mut self, session: &mut Session, before: Vec<Write>) -> Result<(), String> {
        let mut ready = self.scope("assistant.ready");
        // The checkpoint's continuation carries over (`may_finish` when a follow-up reopens a finished turn).
        let continuation = session
            .value(OP_STATE, &self.operation_id)
            .and_then(|state| state.get("continuation").cloned())
            .unwrap_or_else(|| json!({"kind": "need_assistant", "overflowRecoveryUsed": false}));
        ready.insert("continuation".into(), continuation);
        ready.insert("triggerEntryId".into(), self.trigger.clone().into());
        ready.insert("generationContext".into(), self.generation_context());
        ready.insert("nextAttempt".into(), 1.into());
        let mut writes = before;
        writes.push(Write::set(OP_STATE, &self.operation_id, Value::Object(ready)));
        writes.push(Write::set(LANE_STATE, "main", self.lane_state(true)));
        session.commit(&writes)?;
        Ok(())
    }

    fn frames_key(&self) -> String {
        format!("{}:{}", self.operation_id, self.response_entry_id)
    }

    /// One stream event (`AssistantMessageEvent` JSON) as a progress frame, when it carries progress.
    pub fn stream_event(&mut self, session: &mut Session, event: &Value) -> Result<(), String> {
        if let Some(frame) = self.encoder.encode(event) {
            session.commit(&[Write::append(PENDING_FRAMES, &self.frames_key(), frame)])?;
        }
        Ok(())
    }

    /// Count the response in the Ultron usage ledger (`ultron.usage/root`, root `turn:<operation>`).
    pub fn record_turn(&mut self, session: &mut Session, message: &Value) -> Result<(), String> {
        let mut document = session
            .value(ULTRON_USAGE, "root")
            .cloned()
            .unwrap_or_else(|| json!({"version": 1, "roots": {}}));
        let root_id = format!("turn:{}", self.operation_id);
        let mut root = document["roots"].get(&root_id).cloned().unwrap_or_else(
            || json!({"rootId": root_id, "startedAt": null, "deadlineAt": null, "reservations": [], "calls": []}),
        );
        let turns = root
            .get("turns")
            .cloned()
            .unwrap_or(json!({"count": 0, "tokens": 0, "lastAt": 0}));
        let usage = &message["usage"];
        let known = |v: &Value| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0);
        let tokens = known(&usage["totalTokens"]);
        let cost = known(&usage["cost"]["total"]);
        let f = |key: &str| turns.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        root["turns"] = json!({
            "count": num(f("count") + 1.0),
            "tokens": num(f("tokens") + tokens.unwrap_or(0.0)),
            "lastAt": now(session),
            "cost": num(f("cost") + cost.unwrap_or(0.0)),
            "unpriced": num(f("unpriced") + if cost.is_some() { 0.0 } else { 1.0 }),
        });
        if !document["roots"].is_object() {
            document["roots"] = json!({});
        }
        document["roots"][&root_id] = root;
        session.commit(&[Write::set(ULTRON_USAGE, "root", document)])?;
        Ok(())
    }

    /// Commit the settled response with its usage row; the next state is the tool batch when it calls tools,
    /// else a checkpoint that may finish. Returns whether tools follow.
    pub fn commit_assistant(&mut self, session: &mut Session, message: Value) -> Result<bool, String> {
        let tip = session.branch_tip("main").flatten();
        let id = self.response_entry_id.clone();
        let mut usage_row = Map::new();
        usage_row.insert("id".into(), self.usage_id.clone().into());
        usage_row.insert("usage".into(), message["usage"].clone());
        usage_row.insert("entryId".into(), id.clone().into());
        usage_row.insert("adjustment".into(), false.into());
        let calls: Vec<(usize, String)> = message["content"]
            .as_array()
            .map(|content| {
                content
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c["type"] == "toolCall")
                    .map(|(i, c)| (i, c["id"].as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        self.latest_assistant = Some(id.clone());
        self.assistant_entry_id = id.clone();
        let mut writes = Vec::new();
        let next = if let Some((task, prepared)) = self.compaction_with_answer.take() {
            // An overflowed answer: the compaction starts in the same transaction (`summary.deciding`).
            self.calls.clear();
            writes.push(self.prepare_compaction(&task, prepared));
            self.compaction = Some(Compaction {
                task: task.clone(),
                summary_context: Value::Null,
                usage_ids: Vec::new(),
                attempt: 1,
            });
            self.summary_state("summary.deciding", vec![])
        } else if calls.is_empty() || message["stopReason"] != "toolUse" {
            self.trigger = id.clone();
            self.calls.clear();
            self.checkpoint(json!({"kind": "may_finish", "includeFinalAssistant": true}))
        } else {
            self.calls = calls
                .into_iter()
                .map(|(source_index, call_id)| ToolCallState {
                    source_index,
                    call_id,
                    result_entry_id: mint(session),
                    status: "planned",
                    result: None,
                })
                .collect();
            self.tools_state()
        };
        // An answer that did not overflow ends any recovery: the next overflow may compact again.
        if self.compaction.is_none() {
            self.overflow_recovery_used = false;
        }
        let error = message["stopReason"] == "error" && writes.is_empty();
        let retry = if error { self.retry_next.take() } else { None };
        let mut all = vec![
            entry(&id, tip.as_deref(), message),
            Write::Usage(usage_row),
            Write::set(BRANCH_TIP, "main", id.clone().into()),
            Write::delete_list(PENDING_FRAMES, &self.frames_key()),
        ];
        if let Some((not_before, error_message)) = retry {
            // `assistant.retry_wait`: the answer commits now and the step is tried again after the wait.
            let next = self.attempt + 1;
            let mut wait = self.scope("assistant.retry_wait");
            wait.insert("generationContext".into(), self.generation_context());
            wait.insert("nextAttempt".into(), next.into());
            wait.insert("notBefore".into(), not_before.into());
            wait.insert("errorMessage".into(), error_message.into());
            all.push(Write::set(OP_STATE, &self.operation_id, Value::Object(wait)));
            session.commit(&all)?;
            self.retry_waiting = Some(next);
            return Ok(false);
        }
        if error {
            // The run ends with it: the answer commits with the terminal transaction.
            self.error_answer = all;
            return Ok(false);
        }
        all.append(&mut writes);
        all.push(Write::set(OP_STATE, &self.operation_id, next));
        session.commit(&all)?;
        Ok(!self.calls.is_empty())
    }

    /// The checkpoint the operation is at: its continuation and trigger entry (`resumeAfter` of a compaction).
    pub fn checkpoint_resume(&self, session: &Session) -> Value {
        let state = session.value(OP_STATE, &self.operation_id);
        // A compaction a dead worker left resumes the checkpoint it started at.
        if let Some(resume_after) = state
            .map(|state| &state["task"]["boundary"]["resumeAfter"])
            .filter(|resume_after| resume_after.is_object())
        {
            return resume_after.clone();
        }
        let continuation = state
            .and_then(|state| state.get("continuation").cloned())
            .unwrap_or_else(|| json!({"kind": "need_assistant", "overflowRecoveryUsed": false}));
        json!({"continuation": continuation, "triggerEntryId": self.trigger})
    }

    /// The entry the current checkpoint follows.
    pub fn trigger(&self) -> &str {
        &self.trigger
    }

    /// The generation follows an overflow compaction (another overflow fails the run).
    pub fn overflow_recovery_used(&self) -> bool {
        self.overflow_recovery_used
    }

    fn task(&self, session: &Session, reason: &str, resume_after: Value) -> Value {
        json!({
            "taskId": mint(session), "reason": reason,
            "boundary": {"kind": "resume_checkpoint", "resumeAfter": resume_after},
        })
    }

    fn prepare_compaction(&mut self, task: &Value, prepared: Value) -> Write {
        let key = format!("{}:{}", self.operation_id, task["taskId"].as_str().unwrap_or_default());
        self.preparations.push(key.clone());
        Write::set(OP_PREPARATION, &key, prepared)
    }

    fn summary_state(&self, at: &str, extra: Vec<(&str, Value)>) -> Value {
        let mut state = self.scope(at);
        if let Some(compaction) = &self.compaction {
            state.insert("task".into(), compaction.task.clone());
        }
        for (key, value) in extra {
            state.insert(key.into(), value);
        }
        Value::Object(state)
    }

    /// The overflowed answer the next [`Run::commit_assistant`] commits starts a compaction (`reason` overflow)
    /// with `prepared` (`pi.op.preparation`): the request is retried once it completes.
    pub fn compact_with_answer(&mut self, session: &Session, prepared: Value) {
        let resume_after = json!({
            "continuation": {"kind": "need_assistant", "overflowRecoveryUsed": true},
            "triggerEntryId": self.generation_trigger.as_ref().unwrap_or(&self.trigger),
        });
        let task = self.task(session, "overflow", resume_after);
        self.compaction_with_answer = Some((task, prepared));
    }

    /// A compaction at the current checkpoint (`reason` threshold, or an overflow compaction a dead worker left,
    /// resuming its `resume_after`): its preparation and `summary.deciding`.
    pub fn begin_compaction(
        &mut self,
        session: &mut Session,
        reason: &str,
        prepared: Value,
        resume_after: Option<Value>,
    ) -> Result<(), String> {
        let resume_after = resume_after.unwrap_or_else(|| self.checkpoint_resume(session));
        let task = self.task(session, reason, resume_after);
        let preparation = self.prepare_compaction(&task, prepared);
        self.compaction = Some(Compaction {
            task,
            summary_context: Value::Null,
            usage_ids: Vec::new(),
            attempt: 1,
        });
        let deciding = self.summary_state("summary.deciding", vec![]);
        session
            .commit(&[
                preparation,
                Write::set(OP_STATE, &self.operation_id, deciding),
                Write::set(LANE_STATE, "main", self.lane_state(true)),
            ])
            .map(drop)
    }

    /// The summary request is prepared: `summary.ready`, `summary.effect_pending`, then the request with its usage
    /// id. Returns the compaction entry's id.
    pub fn compaction_request(
        &mut self,
        session: &mut Session,
        retry: &crate::compaction_op::SummaryRetry,
    ) -> Result<String, String> {
        let result_entry_id = mint(session);
        let context = json!({
            "resultEntryId": result_entry_id,
            "configuration": self.config,
            "streamOptions": {"timeoutMs": 300000, "maxRetryDelayMs": 60000, "deferred": false},
            "retryPolicy": retry.value(),
        });
        if let Some(compaction) = self.compaction.as_mut() {
            compaction.summary_context = context;
            compaction.attempt = 1;
        }
        self.compaction_attempt(session)?;
        Ok(result_entry_id)
    }

    /// The attempt's `summary.ready` (`nextAttempt`), `summary.effect_pending`, then with its request.
    fn compaction_attempt(&mut self, session: &mut Session) -> Result<(), String> {
        let (context, attempt) = self
            .compaction
            .as_ref()
            .map_or((Value::Null, 1), |c| (c.summary_context.clone(), c.attempt));
        let ready = self.summary_state(
            "summary.ready",
            vec![("summaryContext", context), ("nextAttempt", json!(attempt))],
        );
        session.commit(&[Write::set(OP_STATE, &self.operation_id, ready)])?;
        if let Some(compaction) = self.compaction.as_mut() {
            compaction.usage_ids.clear();
        }
        let pending = self.effect_pending(None);
        session.commit(&[Write::set(OP_STATE, &self.operation_id, pending)])?;
        let usage_id = mint(session);
        let requested = self.effect_pending(Some(&usage_id));
        session.commit(&[Write::set(OP_STATE, &self.operation_id, requested)])?;
        if let Some(compaction) = self.compaction.as_mut() {
            compaction.usage_ids.push(usage_id);
        }
        Ok(())
    }

    /// The task id of the compaction in progress.
    pub fn compaction_task_id(&self) -> Option<String> {
        self.compaction.as_ref()?.task["taskId"].as_str().map(str::to_string)
    }

    /// A retryable failed attempt: its response's usage row (when it reported one) and `summary.retry_wait` until
    /// `not_before` with the next attempt and the error (`publishAttemptResult`).
    pub fn compaction_retry_wait(
        &mut self,
        session: &mut Session,
        usage: Option<&Value>,
        error: &str,
        not_before: i64,
    ) -> Result<(), String> {
        let Some(compaction) = self.compaction.as_ref() else {
            return Ok(());
        };
        let mut writes = Vec::new();
        if let (Some(usage), Some(usage_id)) = (usage, compaction.usage_ids.last()) {
            let mut row = Map::new();
            row.insert("id".into(), usage_id.clone().into());
            row.insert("usage".into(), usage.clone());
            row.insert("adjustment".into(), false.into());
            writes.push(Write::Usage(row));
        }
        let state = self.summary_state(
            "summary.retry_wait",
            vec![
                ("summaryContext", compaction.summary_context.clone()),
                ("nextAttempt", json!(compaction.attempt + 1)),
                ("notBefore", json!(not_before)),
                ("errorMessage", json!(error)),
            ],
        );
        writes.push(Write::set(OP_STATE, &self.operation_id, state));
        session.commit(&writes).map(drop)
    }

    /// The wait ended: the next attempt (`runStructuralRetryWait`).
    pub fn compaction_retry(&mut self, session: &mut Session) -> Result<(), String> {
        if let Some(compaction) = self.compaction.as_mut() {
            compaction.attempt += 1;
        }
        self.compaction_attempt(session)
    }

    /// `summary.effect_pending`: with the request in flight (`request`), else with the usage rows recorded so far.
    fn effect_pending(&self, request: Option<&str>) -> Value {
        let context = self
            .compaction
            .as_ref()
            .map_or(Value::Null, |c| c.summary_context.clone());
        let recorded: Vec<String> = match (request, &self.compaction) {
            (Some(_), _) | (None, None) => Vec::new(),
            (None, Some(compaction)) => compaction.usage_ids.clone(),
        };
        let attempt = self.compaction.as_ref().map_or(1, |c| c.attempt);
        let mut extra = vec![
            ("summaryContext", context),
            ("attempt", json!(attempt)),
            ("usageIds", json!(recorded)),
        ];
        if let Some(usage_id) = request {
            extra.push(("request", json!({"index": 0, "usageId": usage_id})));
        }
        self.summary_state("summary.effect_pending", extra)
    }

    /// The summary response arrived: its usage row, with the state that lists it.
    pub fn compaction_usage(&mut self, session: &mut Session, usage: &Value) -> Result<(), String> {
        let Some(usage_id) = self.compaction.as_ref().and_then(|c| c.usage_ids.last().cloned()) else {
            return Ok(());
        };
        let mut row = Map::new();
        row.insert("id".into(), usage_id.into());
        row.insert("usage".into(), usage.clone());
        row.insert("adjustment".into(), false.into());
        let state = self.effect_pending(None);
        session.commit(&[Write::Usage(row), Write::set(OP_STATE, &self.operation_id, state)])?;
        Ok(())
    }

    /// The state a compaction resumes its checkpoint with: the next generation's `assistant.ready` (its state
    /// carries the summary's fields, as `assistantReadyAtBoundary` spreads the scope it is given) when the
    /// checkpoint needs an answer, else the checkpoint itself (the run may finish).
    fn resumed_state(&mut self, session: &Session, current: Value) -> Value {
        let task = self.compaction.as_ref().map_or(Value::Null, |c| c.task.clone());
        let resume_after = &task["boundary"]["resumeAfter"];
        let continuation = &resume_after["continuation"];
        if continuation["kind"] == "need_assistant" {
            self.overflow_recovery_used = continuation["overflowRecoveryUsed"] == true;
            self.generation_trigger = resume_after["triggerEntryId"]
                .as_str()
                .map(str::to_string)
                .filter(|t| *t != self.trigger);
            self.step_id = mint(session);
            self.ready_written = true;
            let mut ready = current.as_object().cloned().unwrap_or_default();
            ready.insert("at".into(), "assistant.ready".into());
            ready.insert("generationContext".into(), self.generation_context());
            ready.insert("nextAttempt".into(), 1.into());
            return Value::Object(ready);
        }
        let mut state = self.scope("checkpoint");
        state.insert("continuation".into(), continuation.clone());
        state.insert("triggerEntryId".into(), resume_after["triggerEntryId"].clone());
        Value::Object(state)
    }

    /// Commit the compaction entry (`entry` without id and parent) as the new tip and resume the checkpoint.
    pub fn complete_compaction(
        &mut self,
        session: &mut Session,
        result_entry_id: &str,
        mut entry: Map<String, Value>,
    ) -> Result<(), String> {
        let tip = session.branch_tip("main").flatten();
        let mut full = Map::new();
        full.insert("id".into(), result_entry_id.into());
        full.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
        full.append(&mut entry);
        let current = self.effect_pending(None);
        let resumed = self.resumed_state(session, current);
        self.compaction = None;
        session
            .commit(&[
                Write::Entry(full),
                Write::set(BRANCH_TIP, "main", result_entry_id.into()),
                Write::set(OP_STATE, &self.operation_id, resumed),
                Write::set(LANE_STATE, "main", self.lane_state(true)),
            ])
            .map(drop)
    }

    /// Commit the compaction a `session_before_compact` hook supplied at `summary.deciding` (no summary request):
    /// its usage row when it reported usage, the entry as the new tip, and the resumed checkpoint. Returns the
    /// entry's id.
    pub fn complete_compaction_from_hook(
        &mut self,
        session: &mut Session,
        mut entry: Map<String, Value>,
        usage: Option<Value>,
    ) -> Result<String, String> {
        let result_entry_id = mint(session);
        let mut writes = Vec::new();
        if let Some(usage) = usage {
            let mut row = Map::new();
            row.insert("id".into(), mint(session).into());
            row.insert("usage".into(), usage);
            row.insert("adjustment".into(), false.into());
            writes.push(Write::Usage(row));
        }
        let tip = session.branch_tip("main").flatten();
        let mut full = Map::new();
        full.insert("id".into(), result_entry_id.clone().into());
        full.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
        full.append(&mut entry);
        let current = self.summary_state("summary.deciding", vec![]);
        let resumed = self.resumed_state(session, current);
        self.compaction = None;
        writes.extend([
            Write::Entry(full),
            Write::set(BRANCH_TIP, "main", result_entry_id.clone().into()),
            Write::set(OP_STATE, &self.operation_id, resumed),
            Write::set(LANE_STATE, "main", self.lane_state(true)),
        ]);
        session.commit(&writes)?;
        Ok(result_entry_id)
    }

    /// A `before_compaction` hook declined the threshold compaction: the checkpoint resumes without one (a run that
    /// may finish just finishes).
    /// Returns whether the checkpoint resumed (its boundary: `placement`, the entries waiting for it, commits with
    /// the next generation's `assistant.ready`).
    pub fn decline_compaction(&mut self, session: &mut Session, placement: Vec<Write>) -> Result<bool, String> {
        let needs_answer = self
            .compaction
            .as_ref()
            .is_some_and(|c| c.task["boundary"]["resumeAfter"]["continuation"]["kind"] == "need_assistant");
        if !needs_answer {
            self.compaction = None;
            return Ok(false);
        }
        let current = self.summary_state("summary.deciding", vec![]);
        let resumed = self.resumed_state(session, current);
        self.compaction = None;
        let mut writes = placement;
        writes.extend([
            Write::set(OP_STATE, &self.operation_id, resumed),
            Write::set(LANE_STATE, "main", self.lane_state(true)),
        ]);
        session.commit(&writes)?;
        Ok(true)
    }

    /// The compaction failed: nothing more is written until the run ends (failed).
    pub fn abandon_compaction(&mut self) {
        self.compaction = None;
    }

    fn tools_state(&self) -> Value {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|c| {
                let mut call = Map::new();
                call.insert("status".into(), c.status.into());
                call.insert("sourceIndex".into(), c.source_index.into());
                call.insert("resultEntryId".into(), c.result_entry_id.clone().into());
                match c.status {
                    "effect_pending" => {
                        call.insert("replay".into(), "never".into());
                    }
                    "outcome_ready" | "completed" => {
                        call.insert("terminate".into(), false.into());
                    }
                    _ => {}
                }
                Value::Object(call)
            })
            .collect();
        let mut state = self.scope("tools");
        state.insert(
            "batch".into(),
            json!({
                "assistantEntryId": self.assistant_entry_id, "configuration": self.config, "turnId": self.step_id,
                "calls": calls,
            }),
        );
        Value::Object(state)
    }

    fn call_index(&self, call_id: &str) -> Option<usize> {
        self.calls.iter().position(|c| c.call_id == call_id)
    }

    /// A tool call starts: its arguments and `effect_pending` (never replayed).
    pub fn tool_started(&mut self, session: &mut Session, call_id: &str, args: Value) -> Result<(), String> {
        let Some(i) = self.call_index(call_id) else {
            return Ok(());
        };
        self.calls[i].status = "effect_pending";
        let key = format!("{}:{}:{}", self.operation_id, self.step_id, self.calls[i].source_index);
        session.commit(&[
            Write::set(OP_TOOL_ARGS, &key, args),
            Write::set(OP_STATE, &self.operation_id, self.tools_state()),
        ])?;
        Ok(())
    }

    /// A tool call's result message: pending until every call of the batch has one, then the results are
    /// committed as entries in source order and the run checkpoints for the next response.
    pub fn tool_finished(&mut self, session: &mut Session, call_id: &str, mut result: Value) -> Result<(), String> {
        let Some(i) = self.call_index(call_id) else {
            return Ok(());
        };
        // The harness writes a failed tool's result without `details`.
        if result["isError"] == true && result.get("details") == Some(&json!({})) {
            result.as_object_mut().map(|m| m.shift_remove("details"));
        }
        self.calls[i].status = "outcome_ready";
        self.calls[i].result = Some(result.clone());
        let result_id = self.calls[i].result_entry_id.clone();
        session.commit(&[
            Write::set(PENDING_ENTRY, &result_id, json!({"type": "message", "payload": result})),
            Write::delete(PENDING_TOOL_OUTPUT, &format!("{}:{}", self.operation_id, result_id)),
            Write::set(OP_STATE, &self.operation_id, self.tools_state()),
        ])?;
        if self.calls.iter().any(|c| c.result.is_none()) {
            return Ok(());
        }
        let mut writes = Vec::new();
        let mut tip = session.branch_tip("main").flatten();
        for call in &self.calls {
            writes.push(entry(
                &call.result_entry_id,
                tip.as_deref(),
                call.result.clone().unwrap_or_default(),
            ));
            writes.push(Write::delete(PENDING_ENTRY, &call.result_entry_id));
            tip = Some(call.result_entry_id.clone());
        }
        let last = tip.unwrap_or_default();
        writes.push(Write::set(BRANCH_TIP, "main", last.clone().into()));
        for call in &self.calls {
            let key = format!("{}:{}:{}", self.operation_id, self.step_id, call.source_index);
            writes.push(Write::delete(OP_TOOL_ARGS, &key));
        }
        self.trigger = last;
        self.calls.clear();
        writes.push(Write::set(
            OP_STATE,
            &self.operation_id,
            self.checkpoint(json!({"kind": "need_assistant", "overflowRecoveryUsed": false})),
        ));
        session.commit(&writes)?;
        Ok(())
    }

    /// A cancel request (`requestCancel`): the operation's control becomes `cancel_requested` in its current state
    /// and every later state; the lane's state is rewritten with it.
    pub fn request_cancel(&mut self, session: &mut Session, requested_at: i64) -> Result<(), String> {
        self.control = json!({"status": "cancel_requested", "requestedAt": requested_at});
        let mut state = session
            .value(OP_STATE, &self.operation_id)
            .cloned()
            .unwrap_or_else(|| Value::Object(self.scope("checkpoint")));
        state["control"] = self.control.clone();
        session.commit(&[
            Write::set(OP_STATE, &self.operation_id, state),
            Write::set(LANE_STATE, "main", self.lane_state(true)),
        ])?;
        Ok(())
    }

    /// The terminal transaction of a run that ended because it was cancelled: status `aborted`.
    pub fn finish_aborted(self, session: &mut Session) -> Result<(), String> {
        self.finish_as(session, "aborted", None)
    }

    /// The terminal transaction: the operation is removed, its result recorded and the lane idle again.
    /// `error` is `(code, message)` for a failed run.
    pub fn finish(self, session: &mut Session, error: Option<(&str, &str)>) -> Result<(), String> {
        let status = if error.is_some() { "failed" } else { "completed" };
        self.finish_as(session, status, error)
    }

    fn finish_as(mut self, session: &mut Session, status: &str, error: Option<(&str, &str)>) -> Result<(), String> {
        let ended_at = now(session);
        let mut result = Map::new();
        result.insert("operationId".into(), self.operation_id.clone().into());
        result.insert("kind".into(), "run".into());
        result.insert("status".into(), status.into());
        if let Some((code, message)) = error {
            result.insert("error".into(), json!({"code": code, "message": message}));
        }
        result.insert(
            "fromTipId".into(),
            self.source_tip.clone().map_or(Value::Null, Value::from),
        );
        // The error answer committing with this transaction is the tip it leaves.
        let tip = match self.error_answer.is_empty() {
            true => session.branch_tip("main").flatten(),
            false => Some(self.assistant_entry_id.clone()),
        };
        result.insert("tipId".into(), tip.map_or(Value::Null, Value::from));
        result.insert("startedAt".into(), self.started_at.into());
        result.insert("endedAt".into(), ended_at.into());
        let mut writes = std::mem::take(&mut self.error_answer);
        writes.extend([
            Write::delete(OP_META, &self.operation_id),
            Write::delete(OP_STATE, &self.operation_id),
        ]);
        writes.extend(self.preparations.iter().map(|key| Write::delete(OP_PREPARATION, key)));
        writes.extend([
            Write::set(OP_RESULT, &self.operation_id, Value::Object(result)),
            Write::set(LANE_STATE, "main", self.lane_state(false)),
        ]);
        session.commit(&writes)?;
        Ok(())
    }
}
