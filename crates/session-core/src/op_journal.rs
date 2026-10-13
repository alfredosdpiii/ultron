//! The harness operation journal of one run on lane `main` in the format-4 session (a copy of
//! `session_journal::run::Run`, which writes exactly what the TypeScript drive commits), plus [`Run::reopen`]: the
//! durable runtime resumes an operation a dead worker left open, so its journal continues from the stored state
//! instead of being settled as interrupted.

use serde_json::{Map, Value, json};
use session_journal::run::{
    FrameEncoder, OP_META, OP_RESULT, OP_STATE, OP_TOOL_ARGS, PENDING_ENTRY, PENDING_FRAMES, PENDING_TOOL_OUTPUT,
    ULTRON_USAGE,
};
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
            inbox: json!([]),
            generation_trigger: None,
        };
        let meta = json!({
            "operationId": operation_id, "lane": "main", "sourceTipId": source_tip, "startedAt": started_at,
            "intent": {"kind": "run", "promptEntryIds": [prompt_id]},
        });
        let starting = json!({
            "at": "starting", "control": {"status": "running"}, "settings": run.settings, "latestAssistantEntryId": null,
        });
        session.commit(&[
            entry(&prompt_id, source_tip.as_deref(), prompt),
            Write::set(BRANCH_TIP, "main", prompt_id.clone().into()),
            Write::set(OP_META, &operation_id, meta),
            Write::set(OP_STATE, &operation_id, starting),
            Write::set(LANE_STATE, "main", run.lane_state(true)),
        ])?;
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
        let trigger = text(&state["triggerEntryId"])
            .or_else(|| text(&context["triggerEntryId"]))
            .unwrap_or(prompt);
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
            json!({"currentOperationId": null, "lastOperationId": self.operation_id, "inbox": []})
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
            "streamOptions": stream_options(), "retryPolicy": retry_policy(), "overflowRecoveryUsed": false,
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
        self.generation_trigger = trigger;
        self.step_id = mint(session);
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
        self.response_entry_id = mint(session);
        self.usage_id = mint(session);
        self.encoder = FrameEncoder::default();
        let mut pending = self.scope("assistant.effect_pending");
        pending.insert("generationContext".into(), self.generation_context());
        pending.insert("attempt".into(), 1.into());
        pending.insert("responseEntryId".into(), self.response_entry_id.clone().into());
        pending.insert("usageId".into(), self.usage_id.clone().into());
        pending.insert("intendedOutputLimit".into(), max_tokens.into());
        pending.insert("contextWindow".into(), context_window.into());
        session.commit(&[Write::set(OP_STATE, &self.operation_id, Value::Object(pending))])?;
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
        let next = if calls.is_empty() || message["stopReason"] != "toolUse" {
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
        session.commit(&[
            entry(&id, tip.as_deref(), message),
            Write::Usage(usage_row),
            Write::set(BRANCH_TIP, "main", id.clone().into()),
            Write::delete_list(PENDING_FRAMES, &self.frames_key()),
            Write::set(OP_STATE, &self.operation_id, next),
        ])?;
        Ok(!self.calls.is_empty())
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

    fn finish_as(self, session: &mut Session, status: &str, error: Option<(&str, &str)>) -> Result<(), String> {
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
        result.insert(
            "tipId".into(),
            session.branch_tip("main").flatten().map_or(Value::Null, Value::from),
        );
        result.insert("startedAt".into(), self.started_at.into());
        result.insert("endedAt".into(), ended_at.into());
        session.commit(&[
            Write::delete(OP_META, &self.operation_id),
            Write::delete(OP_STATE, &self.operation_id),
            Write::set(OP_RESULT, &self.operation_id, Value::Object(result)),
            Write::set(LANE_STATE, "main", self.lane_state(false)),
        ])?;
        Ok(())
    }
}
