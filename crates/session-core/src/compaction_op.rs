//! A manual compaction or a tree navigation as the harness journals its operation (`runtime/lane.ts`
//! `acceptCompaction` / `acceptNavigation`, `runtime/drive/structural.ts`): the preparation and the operation in
//! `summary.deciding`, `summary.ready`, `summary.effect_pending` (before the request, with the request, then with
//! its usage row), and the terminal transaction that commits the compaction entry (or the branch summary), moves
//! the tip and records the result. A navigation without a summary is admitted at `navigation.ready_to_commit` and
//! committed by its own terminal transaction (`commitNavigation`).

use serde_json::{Map, Value, json};
use session_journal::run::{OP_META, OP_RESULT, OP_STATE, default_settings};
use session_journal::session::{BRANCH_TIP, LANE_CONFIG, LANE_STATE};
use session_journal::{Session, Write};

const OP_PREPARATION: &str = "pi.op.preparation";
/// Where native sessions keep entry labels (`entryLabel` in harness session/values.ts).
pub const ENTRY_LABEL: &str = "pi.entry.label";

fn mint(session: &Session) -> String {
    session_journal::lock_clock(&session.clock()).next_id()
}

fn now(session: &Session) -> i64 {
    session_journal::lock_clock(&session.clock()).now()
}

/// A compaction's durable preparation (`pi.op.preparation`): what it summarizes, keeps, and the settings it ran with.
pub fn prepared_value(
    preparation: &compaction::CompactionPreparation,
    retained_tail: &[Value],
    settings: &Value,
) -> Value {
    let file_ops = &preparation.file_ops;
    json!({
        "kind": "compaction",
        "messagesToSummarize": preparation.messages_to_summarize,
        "turnPrefixMessages": preparation.turn_prefix_messages,
        "retainedTail": retained_tail,
        "isSplitTurn": preparation.is_split_turn,
        "tokensBefore": preparation.tokens_before,
        "fileOps": {"read": file_ops.read, "written": file_ops.written, "edited": file_ops.edited},
        "settings": settings,
    })
}

/// The harness's normalized retry policy of a summary (`summaryContext.retryPolicy`, `normalizedRetryPolicy`):
/// `retry.enabled ? retry.maxRetries + 1 : 1` attempts, delays `baseDelayMs * 2^(attempt-1)` capped at
/// `maxAgentDelayMs` (pi-ai `retryDelayMs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SummaryRetry {
    pub max_attempts: u64,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for SummaryRetry {
    fn default() -> Self {
        SummaryRetry {
            max_attempts: 4,
            base_delay_ms: 2000,
            max_delay_ms: 60_000,
        }
    }
}

impl SummaryRetry {
    /// From settings.json's `retry` object (missing members take their defaults); `enabled`: on for this session.
    pub fn from_settings(retry: &Value, enabled: bool) -> SummaryRetry {
        let number = |key: &str, default: u64| retry[key].as_u64().unwrap_or(default);
        let on = enabled && retry["enabled"] != false;
        SummaryRetry {
            max_attempts: if on { number("maxRetries", 3) + 1 } else { 1 },
            base_delay_ms: number("baseDelayMs", 2000),
            max_delay_ms: number("maxAgentDelayMs", 60_000),
        }
    }

    pub fn value(&self) -> Value {
        json!({"maxAttempts": self.max_attempts, "baseDelayMs": self.base_delay_ms, "maxAgentDelayMs": self.max_delay_ms})
    }

    /// `retryDelayMs(policy, attempt)`: the wait after failed attempt `attempt` (1-based).
    pub fn delay_ms(&self, attempt: u64) -> u64 {
        let factor = 1u64
            .checked_shl(attempt.saturating_sub(1).min(63) as u32)
            .unwrap_or(u64::MAX);
        self.base_delay_ms.saturating_mul(factor).min(self.max_delay_ms)
    }
}

/// One structural operation in progress: a compaction, or a navigation (with or without a branch summary).
pub struct CompactionOp {
    /// `compaction` or `navigation` (the intent and the result's kind).
    kind: &'static str,
    /// A navigation's target (`None`: the root) and the label it sets.
    target: Option<String>,
    label: Option<String>,
    /// Where a navigation without a summary puts its label: the selected entry (AgentSession `navigateTree`).
    label_entry: Option<String>,
    operation_id: String,
    task_id: String,
    pub result_entry_id: String,
    usage_id: String,
    /// The summary attempt in progress (1-based) and the policy it retries under.
    attempt: u64,
    retry: SummaryRetry,
    source_tip: Option<String>,
    started_at: i64,
    last_operation_id: Value,
    inbox: Value,
    task: Value,
    summary_context: Value,
}

impl CompactionOp {
    fn preparation_key(&self) -> String {
        format!("{}:{}", self.operation_id, self.task_id)
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    fn state(&self, at: &str, extra: Vec<(&str, Value)>) -> Value {
        let mut state = Map::new();
        state.insert("control".into(), json!({"status": "running"}));
        state.insert("settings".into(), default_settings());
        state.insert("latestAssistantEntryId".into(), Value::Null);
        state.insert("at".into(), at.into());
        if !self.task.is_null() {
            state.insert("task".into(), self.task.clone());
        }
        for (key, value) in extra {
            state.insert(key.into(), value);
        }
        Value::Object(state)
    }

    fn lane_state(&self, running: bool) -> Value {
        if running {
            json!({"currentOperationId": self.operation_id, "lastOperationId": self.last_operation_id, "inbox": self.inbox})
        } else {
            json!({"currentOperationId": null, "lastOperationId": self.operation_id, "inbox": self.inbox})
        }
    }

    /// Admit the operation with its preparation (`retained_tail`: the kept messages): `summary.deciding`, where the
    /// `session_before_compact` hook decides before any request ([`CompactionOp::request`]).
    pub fn begin(
        session: &mut Session,
        preparation: &compaction::CompactionPreparation,
        retained_tail: &[Value],
        custom_instructions: Option<&str>,
    ) -> Result<CompactionOp, String> {
        let operation_id = mint(session);
        let task_id = mint(session);
        let lane = session.value(LANE_STATE, "main").cloned().unwrap_or(Value::Null);
        let mut task = json!({"taskId": task_id, "reason": "manual"});
        let mut intent = json!({"kind": "compaction"});
        if let Some(instructions) = custom_instructions {
            task["customInstructions"] = json!(instructions);
            intent["customInstructions"] = json!(instructions);
        }
        task["boundary"] = json!({"kind": "finish"});
        let op = CompactionOp {
            kind: "compaction",
            target: None,
            label: None,
            label_entry: None,
            source_tip: session.branch_tip("main").flatten(),
            started_at: now(session),
            last_operation_id: lane["lastOperationId"].clone(),
            inbox: lane.get("inbox").cloned().unwrap_or_else(|| json!([])),
            task,
            summary_context: Value::Null,
            result_entry_id: String::new(),
            usage_id: String::new(),
            attempt: 1,
            retry: SummaryRetry::default(),
            operation_id,
            task_id,
        };
        let prepared = prepared_value(preparation, retained_tail, &default_settings()["compaction"]);
        let meta = json!({
            "operationId": op.operation_id, "lane": "main", "sourceTipId": op.source_tip, "startedAt": op.started_at,
            "intent": intent,
        });
        session.commit(&[
            Write::set(OP_PREPARATION, &op.preparation_key(), prepared),
            Write::set(OP_META, &op.operation_id, meta),
            Write::set(OP_STATE, &op.operation_id, op.state("summary.deciding", vec![])),
            Write::set(LANE_STATE, "main", op.lane_state(true)),
        ])?;
        Ok(op)
    }

    fn navigation(session: &Session, target: Option<&str>, label: Option<&str>, task: Value) -> CompactionOp {
        let lane = session.value(LANE_STATE, "main").cloned().unwrap_or(Value::Null);
        CompactionOp {
            kind: "navigation",
            target: target.map(str::to_string),
            label: label.map(str::to_string),
            label_entry: target.map(str::to_string),
            source_tip: session.branch_tip("main").flatten(),
            started_at: now(session),
            last_operation_id: lane["lastOperationId"].clone(),
            inbox: lane.get("inbox").cloned().unwrap_or_else(|| json!([])),
            task,
            summary_context: Value::Null,
            result_entry_id: String::new(),
            usage_id: String::new(),
            attempt: 1,
            retry: SummaryRetry::default(),
            operation_id: String::new(),
            task_id: String::new(),
        }
    }

    fn navigation_meta(&self, summarize: bool, custom_instructions: Option<&str>) -> Value {
        let mut intent = json!({"kind": "navigation", "targetId": self.target, "summarize": summarize});
        if let Some(label) = &self.label {
            intent["label"] = json!(label);
        }
        if let Some(instructions) = custom_instructions {
            intent["customInstructions"] = json!(instructions);
        }
        json!({
            "operationId": self.operation_id, "lane": "main", "sourceTipId": self.source_tip,
            "startedAt": self.started_at, "intent": intent,
        })
    }

    /// The navigation task's `commit_navigation` boundary, with the instructions and label it runs with.
    fn navigation_task(&self, custom_instructions: Option<&str>) -> Value {
        let mut task = json!({"taskId": self.task_id});
        if let Some(instructions) = custom_instructions {
            task["customInstructions"] = json!(instructions);
        }
        let mut boundary = json!({"kind": "commit_navigation", "targetId": self.target});
        if let Some(label) = &self.label {
            boundary["label"] = json!(label);
        }
        task["boundary"] = boundary;
        task
    }

    /// Admit a summarized navigation to `target` with its preparation (`preparation`: the branch left, as
    /// `prepareBranchEntries` reduces it): `summary.deciding` with a `commit_navigation` boundary, where the
    /// `session_before_tree` hook decides before any request.
    pub fn begin_navigation(
        session: &mut Session,
        target: Option<&str>,
        preparation: &compaction::BranchPreparation,
        custom_instructions: Option<&str>,
        label: Option<&str>,
    ) -> Result<CompactionOp, String> {
        let mut op = CompactionOp::navigation(session, target, label, Value::Null);
        op.operation_id = mint(session);
        op.task_id = mint(session);
        op.task = op.navigation_task(custom_instructions);
        let ops = &preparation.file_ops;
        let prepared = json!({
            "kind": "branch_summary", "messages": preparation.messages,
            "fileOps": {"read": ops.read, "written": ops.written, "edited": ops.edited},
            "totalTokens": preparation.total_tokens,
        });
        session.commit(&[
            Write::set(OP_PREPARATION, &op.preparation_key(), prepared),
            Write::set(OP_META, &op.operation_id, op.navigation_meta(true, custom_instructions)),
            Write::set(OP_STATE, &op.operation_id, op.state("summary.deciding", vec![])),
            Write::set(LANE_STATE, "main", op.lane_state(true)),
        ])?;
        Ok(op)
    }

    /// Admit a navigation without a summary: `navigation.ready_to_commit` ([`CompactionOp::commit_navigation`]);
    /// its label goes on `label_entry` (default: the target).
    pub fn begin_direct_navigation(
        session: &mut Session,
        target: Option<&str>,
        label: Option<&str>,
        label_entry: Option<&str>,
    ) -> Result<CompactionOp, String> {
        let mut op = CompactionOp::navigation(session, target, label, Value::Null);
        op.label_entry = label_entry.map(str::to_string).or_else(|| op.target.clone());
        op.operation_id = mint(session);
        let mut extra = vec![("targetId", json!(op.target))];
        if let Some(label) = &op.label {
            extra.push(("label", json!(label)));
            // AgentSession labels the selected entry; the tip goes before it when it is a user message.
            if op.label_entry != op.target {
                extra.push(("labelEntryId", json!(op.label_entry)));
            }
        }
        session.commit(&[
            Write::set(OP_META, &op.operation_id, op.navigation_meta(false, None)),
            Write::set(
                OP_STATE,
                &op.operation_id,
                op.state("navigation.ready_to_commit", extra),
            ),
            Write::set(LANE_STATE, "main", op.lane_state(true)),
        ])?;
        Ok(op)
    }

    /// Commit a navigation without a summary: the tip moves to the target (labelled) and the operation ends.
    pub fn commit_navigation(self, session: &mut Session) -> Result<Option<String>, String> {
        let mut writes = vec![Write::set(
            BRANCH_TIP,
            "main",
            self.target.clone().map_or(Value::Null, Value::from),
        )];
        if let (Some(entry), Some(label)) = (&self.label_entry, &self.label) {
            writes.push(Write::set(ENTRY_LABEL, entry, label.clone().into()));
        }
        writes.extend(self.terminal(session, "completed", self.target.as_deref(), None));
        session.commit(&writes)?;
        Ok(self.target)
    }

    /// Commit the branch summary `entry` (without id, parent and `fromId`) under the target as the new tip, its
    /// label on the summary (AgentSession `navigateTree`), and end the operation; `usage`: a hook's summary usage,
    /// recorded as a usage row (a model summary's row was recorded with its response).
    pub fn complete_navigation(
        mut self,
        session: &mut Session,
        mut entry: Map<String, Value>,
        hook_usage: Option<Value>,
    ) -> Result<String, String> {
        if self.result_entry_id.is_empty() {
            self.result_entry_id = mint(session);
        }
        let mut writes = Vec::new();
        if let Some(usage) = hook_usage {
            let mut row = Map::new();
            row.insert("id".into(), mint(session).into());
            row.insert("usage".into(), usage);
            row.insert("adjustment".into(), false.into());
            writes.push(Write::Usage(row));
        }
        let id = self.result_entry_id.clone();
        let target = self.target.clone().map_or(Value::Null, Value::from);
        let mut full = Map::new();
        full.insert("id".into(), id.clone().into());
        full.insert("parentId".into(), target.clone());
        full.insert("type".into(), "branch_summary".into());
        full.insert(
            "fromId".into(),
            self.source_tip.clone().map_or(Value::Null, Value::from),
        );
        full.append(&mut entry);
        writes.extend([
            Write::set(BRANCH_TIP, "main", target),
            Write::Entry(full),
            Write::set(BRANCH_TIP, "main", id.clone().into()),
        ]);
        if let Some(label) = &self.label {
            writes.push(Write::set(ENTRY_LABEL, &id, label.clone().into()));
        }
        writes.extend(self.terminal(session, "completed", Some(&id), None));
        session.commit(&writes)?;
        Ok(id)
    }

    /// End the operation `declined` (a `session_before_tree` hook cancelled the navigation); the branch is
    /// unchanged.
    pub fn decline(self, session: &mut Session) -> Result<(), String> {
        let writes = self.terminal(session, "declined", self.source_tip.as_deref(), None);
        session.commit(&writes).map(drop)
    }

    /// Prepare the summary request under `retry`: `summary.ready`, `summary.effect_pending`, and the request with
    /// its usage id.
    pub fn request(&mut self, session: &mut Session, retry: SummaryRetry) -> Result<(), String> {
        self.result_entry_id = mint(session);
        self.retry = retry;
        self.attempt = 1;
        self.summary_context = json!({
            "resultEntryId": self.result_entry_id,
            "configuration": session.value(LANE_CONFIG, "main").cloned().unwrap_or(Value::Null),
            "streamOptions": {"timeoutMs": 300000, "maxRetryDelayMs": 60000, "deferred": false},
            "retryPolicy": retry.value(),
        });
        self.attempt_states(session)
    }

    /// The summary task's id (the harness's retry `step`).
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// The summary attempt in progress (1-based).
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// `summary.ready` (`nextAttempt`), `summary.effect_pending` for the attempt, then with its request.
    fn attempt_states(&mut self, session: &mut Session) -> Result<(), String> {
        let context = self.summary_context.clone();
        session.commit(&[Write::set(
            OP_STATE,
            &self.operation_id,
            self.state(
                "summary.ready",
                vec![
                    ("summaryContext", context.clone()),
                    ("nextAttempt", json!(self.attempt)),
                ],
            ),
        )])?;
        let pending = |usage_ids: Value| {
            vec![
                ("summaryContext", context.clone()),
                ("attempt", json!(self.attempt)),
                ("usageIds", usage_ids),
            ]
        };
        session.commit(&[Write::set(
            OP_STATE,
            &self.operation_id,
            self.state("summary.effect_pending", pending(json!([]))),
        )])?;
        self.usage_id = mint(session);
        let mut with_request = pending(json!([]));
        with_request.push(("request", json!({"index": 0, "usageId": self.usage_id})));
        session.commit(&[Write::set(
            OP_STATE,
            &self.operation_id,
            self.state("summary.effect_pending", with_request),
        )])?;
        Ok(())
    }

    /// A retryable failed attempt (`publishAttemptResult`): its response's usage row (when it reported one) and
    /// `summary.retry_wait` until `not_before` with the next attempt and the error.
    pub fn retry_wait(
        &mut self,
        session: &mut Session,
        usage: Option<&Value>,
        error: &str,
        not_before: i64,
    ) -> Result<(), String> {
        let mut writes = Vec::new();
        if let Some(usage) = usage {
            let mut row = Map::new();
            row.insert("id".into(), self.usage_id.clone().into());
            row.insert("usage".into(), usage.clone());
            row.insert("adjustment".into(), false.into());
            writes.push(Write::Usage(row));
        }
        let state = self.state(
            "summary.retry_wait",
            vec![
                ("summaryContext", self.summary_context.clone()),
                ("nextAttempt", json!(self.attempt + 1)),
                ("notBefore", json!(not_before)),
                ("errorMessage", json!(error)),
            ],
        );
        writes.push(Write::set(OP_STATE, &self.operation_id, state));
        session.commit(&writes).map(drop)
    }

    /// The wait ended (`runStructuralRetryWait`): the next attempt's `summary.ready`, `summary.effect_pending` and
    /// request.
    pub fn retry(&mut self, session: &mut Session) -> Result<(), String> {
        self.attempt += 1;
        self.attempt_states(session)
    }

    /// The summary response arrived: its usage row, with the state that lists it.
    pub fn record_usage(&self, session: &mut Session, usage: &Value) -> Result<(), String> {
        let mut row = Map::new();
        row.insert("id".into(), self.usage_id.clone().into());
        row.insert("usage".into(), usage.clone());
        row.insert("adjustment".into(), false.into());
        let state = self.state(
            "summary.effect_pending",
            vec![
                ("summaryContext", self.summary_context.clone()),
                ("attempt", json!(self.attempt)),
                ("usageIds", json!([self.usage_id])),
            ],
        );
        session.commit(&[Write::Usage(row), Write::set(OP_STATE, &self.operation_id, state)])?;
        Ok(())
    }

    fn terminal(&self, session: &Session, status: &str, tip: Option<&str>, error: Option<&str>) -> Vec<Write> {
        let mut result = json!({
            "operationId": self.operation_id, "kind": self.kind, "status": status, "fromTipId": self.source_tip,
            "tipId": tip, "startedAt": self.started_at, "endedAt": now(session),
        });
        if let Some(message) = error {
            let code = if self.kind == "compaction" {
                "compaction_failed"
            } else {
                "summarization_failed"
            };
            result["error"] = json!({"code": code, "message": message});
        }
        let mut writes = vec![
            Write::delete(OP_META, &self.operation_id),
            Write::delete(OP_STATE, &self.operation_id),
        ];
        // A navigation without a summary has no preparation.
        if !self.task.is_null() {
            writes.push(Write::delete(OP_PREPARATION, &self.preparation_key()));
        }
        writes.extend([
            Write::set(OP_RESULT, &self.operation_id, result),
            Write::set(LANE_STATE, "main", self.lane_state(false)),
        ]);
        writes
    }

    /// Commit the compaction entry (`entry` without id and parent) as the new tip and end the operation.
    pub fn complete(self, session: &mut Session, mut entry: Map<String, Value>) -> Result<String, String> {
        let id = self.result_entry_id.clone();
        let mut full = Map::new();
        full.insert("id".into(), id.clone().into());
        full.insert(
            "parentId".into(),
            self.source_tip.clone().map_or(Value::Null, Value::from),
        );
        full.append(&mut entry);
        let mut writes = vec![Write::Entry(full), Write::set(BRANCH_TIP, "main", id.clone().into())];
        writes.extend(self.terminal(session, "completed", Some(&id), None));
        session.commit(&writes)?;
        Ok(id)
    }

    /// End the operation as aborted (a cancel request, or a hook that cancelled it); the branch is unchanged.
    pub fn abort(self, session: &mut Session) -> Result<(), String> {
        let writes = self.terminal(session, "aborted", self.source_tip.as_deref(), None);
        session.commit(&writes).map(drop)
    }

    /// End the operation as failed; the branch is unchanged.
    pub fn fail(self, session: &mut Session, message: &str) -> Result<(), String> {
        let writes = self.terminal(session, "failed", self.source_tip.as_deref(), Some(message));
        session.commit(&writes).map(drop)
    }
}

/// A cancel request for the lane's compaction or navigation operation in progress (`requestOperationAbort`): its state's control
/// becomes `cancel_requested`, so a worker that dies before the terminal transaction is settled `aborted`.
pub fn request_cancel(session: &mut Session, requested_at: i64) -> Result<(), String> {
    let Some(operation) = session
        .value(LANE_STATE, "main")
        .and_then(|lane| lane["currentOperationId"].as_str().map(str::to_string))
    else {
        return Ok(());
    };
    if session
        .value(OP_META, &operation)
        .is_none_or(|meta| !matches!(meta["intent"]["kind"].as_str(), Some("compaction" | "navigation")))
    {
        return Ok(());
    }
    let Some(mut state) = session.value(OP_STATE, &operation).cloned() else {
        return Ok(());
    };
    if state["control"]["status"] == "cancel_requested" {
        return Ok(());
    }
    state["control"] = json!({"status": "cancel_requested", "requestedAt": requested_at});
    session.commit(&[Write::set(OP_STATE, &operation, state)]).map(drop)
}
