//! A manual compaction as the harness journals its operation (`runtime/drive/structural.ts`): the preparation and
//! the operation in `summary.deciding`, `summary.ready`, `summary.effect_pending` (before the request, with the
//! request, then with its usage row), and the terminal transaction that commits the compaction entry, moves the tip
//! and records the result.

use serde_json::{Map, Value, json};
use session_journal::run::{OP_META, OP_RESULT, OP_STATE, default_settings};
use session_journal::session::{BRANCH_TIP, LANE_CONFIG, LANE_STATE};
use session_journal::{Session, Write};

const OP_PREPARATION: &str = "pi.op.preparation";

fn mint(session: &Session) -> String {
    session_journal::lock_clock(&session.clock()).next_id()
}

fn now(session: &Session) -> i64 {
    session_journal::lock_clock(&session.clock()).now()
}

/// One compaction operation in progress.
pub struct CompactionOp {
    operation_id: String,
    task_id: String,
    pub result_entry_id: String,
    usage_id: String,
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

    fn state(&self, at: &str, extra: Vec<(&str, Value)>) -> Value {
        let mut state = Map::new();
        state.insert("control".into(), json!({"status": "running"}));
        state.insert("settings".into(), default_settings());
        state.insert("latestAssistantEntryId".into(), Value::Null);
        state.insert("at".into(), at.into());
        state.insert("task".into(), self.task.clone());
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

    /// Admit the operation with its preparation (`retained_tail`: the kept messages), then prepare the summary
    /// request: `summary.ready`, `summary.effect_pending`, and the request with its usage id.
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
        let mut op = CompactionOp {
            source_tip: session.branch_tip("main").flatten(),
            started_at: now(session),
            last_operation_id: lane["lastOperationId"].clone(),
            inbox: lane.get("inbox").cloned().unwrap_or_else(|| json!([])),
            task,
            summary_context: Value::Null,
            result_entry_id: String::new(),
            usage_id: String::new(),
            operation_id,
            task_id,
        };
        let file_ops = &preparation.file_ops;
        let prepared = json!({
            "kind": "compaction",
            "messagesToSummarize": preparation.messages_to_summarize,
            "turnPrefixMessages": preparation.turn_prefix_messages,
            "retainedTail": retained_tail,
            "isSplitTurn": preparation.is_split_turn,
            "tokensBefore": preparation.tokens_before,
            "fileOps": {"read": file_ops.read, "written": file_ops.written, "edited": file_ops.edited},
            "settings": default_settings()["compaction"],
        });
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
        op.result_entry_id = mint(session);
        op.summary_context = json!({
            "resultEntryId": op.result_entry_id,
            "configuration": session.value(LANE_CONFIG, "main").cloned().unwrap_or(Value::Null),
            "streamOptions": {"timeoutMs": 300000, "maxRetryDelayMs": 60000, "deferred": false},
            "retryPolicy": {"maxAttempts": 4, "baseDelayMs": 2000, "maxAgentDelayMs": 60000},
        });
        let context = op.summary_context.clone();
        session.commit(&[Write::set(
            OP_STATE,
            &op.operation_id,
            op.state(
                "summary.ready",
                vec![("summaryContext", context.clone()), ("nextAttempt", json!(1))],
            ),
        )])?;
        let pending = |usage_ids: Value| {
            vec![
                ("summaryContext", context.clone()),
                ("attempt", json!(1)),
                ("usageIds", usage_ids),
            ]
        };
        session.commit(&[Write::set(
            OP_STATE,
            &op.operation_id,
            op.state("summary.effect_pending", pending(json!([]))),
        )])?;
        op.usage_id = mint(session);
        let mut with_request = pending(json!([]));
        with_request.push(("request", json!({"index": 0, "usageId": op.usage_id})));
        session.commit(&[Write::set(
            OP_STATE,
            &op.operation_id,
            op.state("summary.effect_pending", with_request),
        )])?;
        Ok(op)
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
                ("attempt", json!(1)),
                ("usageIds", json!([self.usage_id])),
            ],
        );
        session.commit(&[Write::Usage(row), Write::set(OP_STATE, &self.operation_id, state)])?;
        Ok(())
    }

    fn terminal(&self, session: &Session, status: &str, tip: Option<&str>, error: Option<&str>) -> Vec<Write> {
        let mut result = json!({
            "operationId": self.operation_id, "kind": "compaction", "status": status, "fromTipId": self.source_tip,
            "tipId": tip, "startedAt": self.started_at, "endedAt": now(session),
        });
        if let Some(message) = error {
            result["error"] = json!({"code": "compaction_failed", "message": message});
        }
        vec![
            Write::delete(OP_META, &self.operation_id),
            Write::delete(OP_STATE, &self.operation_id),
            Write::delete(OP_PREPARATION, &self.preparation_key()),
            Write::set(OP_RESULT, &self.operation_id, result),
            Write::set(LANE_STATE, "main", self.lane_state(false)),
        ]
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

    /// End the operation as failed; the branch is unchanged.
    pub fn fail(self, session: &mut Session, message: &str) -> Result<(), String> {
        let writes = self.terminal(session, "failed", self.source_tip.as_deref(), Some(message));
        session.commit(&writes).map(drop)
    }
}
