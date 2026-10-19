//! Tree navigation of the `main` lane (`Lane.navigateTree` in packages/agent harness runtime/lane.ts, the client's
//! `/tree`): move the lane's tip to another entry of the session tree, optionally leaving a branch summary of the
//! branch it leaves (`generateBranchSummary`) as a `branch_summary` entry under the target.
//!
//! Journaled as the harness journals it (crate::compaction_op): a summarized navigation is admitted at
//! `summary.deciding` with its preparation and a `commit_navigation` boundary, goes through `summary.ready` /
//! `summary.effect_pending` with its request, and ends with the transaction that commits the summary entry and the
//! tip (`completed`), or ends `aborted`, `declined` or `failed` with the branch unchanged. A navigation without a
//! summary is admitted at `navigation.ready_to_commit` and committed by its own transaction. A worker killed in
//! between is settled on reopen (crate::recovery): a cancel request ends it `aborted`, a navigation ready to commit
//! commits, an interrupted summary ends `failed` (`interrupted`); no request is made again.
//!
//! The summary runs off the actor like a compaction (core::compacting): `state` reports `isCompacting` throughout
//! and `abort` cancels the navigation (AgentSession `abortBranchSummary`). (Pi extensions' `session_before_tree`
//! and `session_tree` hooks are not supported by ultron-rs.)

use super::compacting::{AttemptFn, RetryStep, Summarizer};
use super::{Core, LANE, event};
use crate::compaction_op::{CompactionOp, ENTRY_LABEL};
use crate::durable_store::lock as lock_session;
use crate::lane::now_ms;
use serde_json::{Map, Value, json};
use session_journal::Write;
use std::sync::Arc;

/// Tokens kept free when choosing branch history to summarize (upstream default).
const RESERVE_TOKENS: u64 = 16_384;

/// A navigation whose branch summary was aborted (what the clients show: interactive-mode.ts, client-tui.ts).
pub const BRANCH_SUMMARY_ABORTED: &str = "Branch summarization cancelled";
/// A navigation during a run (AgentSession `navigateTree`).
const NAVIGATE_WHILE_STREAMING: &str = "Wait for the current response to finish before navigating the session tree.";

/// How a navigation ended without moving.
enum Stopped {
    /// Aborted (`abort`), or its summary was.
    Aborted,
    Failed(String),
}

impl Stopped {
    fn message(&self) -> String {
        match self {
            Stopped::Aborted => BRANCH_SUMMARY_ABORTED.into(),
            Stopped::Failed(message) => message.clone(),
        }
    }
}

/// A summarized navigation that stopped, with its operation when it is still open (to end it with the outcome).
type Halted = (Stopped, Option<Box<CompactionOp>>);

/// The navigation as the caller asked for it.
struct Request {
    /// The selected entry (AgentSession `targetId`; `None`: the root).
    target: Option<String>,
    /// Where the tip goes: before a selected user or custom message, else the selected entry.
    leaf: Option<String>,
    /// A selected user or custom message's text, for the editor.
    editor_text: Option<String>,
    summarize: bool,
    instructions: Option<String>,
    label: Option<String>,
}

/// AgentSession's new leaf for a selected entry: before a user or custom message (whose text goes back to the
/// editor), else the entry itself.
fn resolve(entry: &Value) -> (Option<String>, Option<String>) {
    let parent = entry["parentId"].as_str().map(str::to_string);
    match entry["type"].as_str() {
        Some("message") if entry["message"]["role"] == "user" => (parent, Some(text_of(&entry["message"]["content"]))),
        Some("custom_message") => (parent, Some(text_of(&entry["content"]))),
        _ => (entry["id"].as_str().map(str::to_string), None),
    }
}

/// `contentText(content, "")`: a string as is, else its text parts joined.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

impl Core {
    /// Navigate `main` to `target` (`None`: the root, an empty context). Answers `{tipId}`.
    pub(super) fn navigate(
        &mut self,
        target: Option<String>,
        summarize: bool,
        instructions: Option<&str>,
        label: Option<String>,
    ) -> Result<Value, String> {
        if self.run.is_some() {
            return Err(NAVIGATE_WHILE_STREAMING.into());
        }
        let tip = self.tip();
        // AgentSession: already there is a no-op.
        if target == tip {
            return Ok(json!({"tipId": tip}));
        }
        if target.is_none() && label.is_some() {
            return Err("Root navigation cannot set a label".into());
        }
        let selected = match &target {
            Some(id) => Some(
                self.pi()
                    .entry(id)
                    .cloned()
                    .ok_or_else(|| format!("Entry {id} not found"))?,
            ),
            None => None,
        };
        let (leaf, editor_text) = selected.as_ref().map_or((None, None), resolve);
        let request = Request {
            target,
            leaf,
            editor_text,
            summarize,
            instructions: instructions.map(str::to_string),
            label,
        };
        // AgentSession's branch summary state (`isCompacting`) spans the summary.
        let signal = ai::AbortSignal::new();
        self.compacting = Some(signal.clone());
        self.navigating = true;
        let editor_text = request.editor_text.clone();
        let result = self.navigate_now(tip, request, &signal);
        self.navigating = false;
        self.compacting = None;
        let mut answer = json!({"tipId": result?});
        if let Some(text) = editor_text {
            answer["editorText"] = json!(text);
        }
        Ok(answer)
    }

    fn navigate_now(
        &mut self,
        tip: Option<String>,
        request: Request,
        signal: &ai::AbortSignal,
    ) -> Result<Option<String>, String> {
        let pi = self.pi();
        let (entries, _common_ancestor) = match &request.target {
            Some(target) => compaction::collect_entries_for_branch_summary(&pi, tip.as_deref(), target),
            None => (Vec::new(), None),
        };
        // AgentSession summarizes only a branch with entries. A navigation without a summary decides before it is
        // admitted (`navigation.ready_to_commit` is decided).
        if !request.summarize || entries.is_empty() {
            let op = CompactionOp::begin_direct_navigation(
                &mut lock_session(&self.session),
                request.leaf.as_deref(),
                request.label.as_deref(),
                request.target.as_deref(),
            )?;
            let operation = op.operation_id().to_string();
            self.navigation_start(&operation, &request.leaf);
            let committed = op.commit_navigation(&mut lock_session(&self.session));
            let ended = match &committed {
                Ok(new_tip) => Ok(json!(new_tip)),
                Err(error) => Err(Stopped::Failed(error.clone())),
            };
            self.navigation_end(&operation, &tip, ended.as_ref());
            return committed;
        }
        let preparation = compaction::prepare_branch_entries(&entries, 0);
        let op = CompactionOp::begin_navigation(
            &mut lock_session(&self.session),
            request.leaf.as_deref(),
            &preparation,
            request.instructions.as_deref(),
            request.label.as_deref(),
        )?;
        let operation = op.operation_id().to_string();
        self.navigation_start(&operation, &request.leaf);
        let ended = match self.summarized(op, &request, &entries, signal) {
            Ok(done) => Ok(done),
            // The operation still open ends with the outcome; a failed terminal write is the error then.
            Err((stopped, Some(op))) => {
                let mut session = lock_session(&self.session);
                let closed = match &stopped {
                    Stopped::Aborted => (*op).abort(&mut session),
                    Stopped::Failed(message) => (*op).fail(&mut session, message),
                };
                Err(closed.err().map_or(stopped, Stopped::Failed))
            }
            Err((stopped, None)) => Err(stopped),
        };
        self.announce();
        let shown = ended.as_ref().map(|id| json!(id));
        self.navigation_end(&operation, &tip, shown.as_ref().map_err(|e| *e));
        let id = ended.map_err(|stopped| stopped.message())?;
        Ok(Some(id))
    }

    /// The summarized navigation from `summary.deciding`: the model's summary, committed by `op`. Answers the summary
    /// entry.
    fn summarized(
        &mut self,
        mut op: CompactionOp,
        request: &Request,
        entries: &[Value],
        signal: &ai::AbortSignal,
    ) -> Result<String, Halted> {
        if signal.aborted() {
            return Err((Stopped::Aborted, Some(Box::new(op))));
        }
        let session = &self.session.clone(); // held across the off-actor summary
        let retry = self.summary_retry();
        if let Err(error) = op.request(&mut lock_session(session), retry) {
            return Err((Stopped::Failed(error), Some(Box::new(op))));
        }
        let summarizer: Summarizer = self.summarizer();
        let (cancel, instructions, replace) = (signal.clone(), request.instructions.clone(), false);
        let entries = entries.to_vec();
        let attempt: AttemptFn<compaction::BranchSummary> =
            Arc::new(move || summarizer.branch(&entries, instructions.as_deref(), replace, RESERVE_TOKENS, &cancel));
        let ids = (op.operation_id().to_string(), op.task_id().to_string());
        let mut journal = |core: &mut Core, step: RetryStep<'_>| {
            let mut session = lock_session(&core.session);
            match step {
                RetryStep::Wait {
                    usage,
                    error,
                    not_before,
                } => op.retry_wait(&mut session, usage.as_ref(), error, not_before),
                RetryStep::Retry => op.retry(&mut session),
            }
        };
        let (result, _) = self.summary_attempts(
            signal,
            retry,
            (&ids.0, &ids.1),
            json!({"source": "branchSummary"}),
            attempt,
            &mut journal,
        );
        if signal.aborted() {
            return Err((Stopped::Aborted, Some(Box::new(op))));
        }
        let summary = match result {
            Ok(summary) => summary,
            Err(message) => return Err((Stopped::Failed(message), Some(Box::new(op)))),
        };
        let mut entry = Map::new();
        entry.insert("summary".into(), summary.summary.into());
        entry.insert(
            "details".into(),
            json!({"readFiles": summary.read_files, "modifiedFiles": summary.modified_files}),
        );
        if let Some(usage) = summary.usage {
            let usage = serde_json::to_value(&usage).unwrap_or(Value::Null);
            if let Err(error) = op.record_usage(&mut lock_session(session), &usage) {
                return Err((Stopped::Failed(error), Some(Box::new(op))));
            }
            entry.insert("usage".into(), usage);
        }
        entry.insert("fromHook".into(), false.into());
        let id = op
            .complete_navigation(&mut lock_session(session), entry, None)
            .map_err(|e| (Stopped::Failed(e), None))?;
        Ok(id)
    }

    fn navigation_start(&mut self, operation: &str, target: &Option<String>) {
        self.emit(event(vec![
            ("type", json!("navigation_start")),
            ("lane", json!(LANE)),
            ("runId", json!(operation)),
            ("targetId", json!(target)),
            ("startedAt", json!(now_ms())),
        ]));
    }

    /// `navigation_end` with the harness's status: `completed`, `aborted` or `failed` (with its error).
    fn navigation_end(&mut self, operation: &str, from: &Option<String>, ended: Result<&Value, &Stopped>) {
        let (status, tip, error) = match ended {
            Ok(tip) => ("completed", tip.clone(), None),
            Err(Stopped::Aborted) => ("aborted", json!(from), None),
            Err(Stopped::Failed(message)) => ("failed", json!(from), Some(message.clone())),
        };
        let mut fields = vec![
            ("type", json!("navigation_end")),
            ("lane", json!(LANE)),
            ("runId", json!(operation)),
            ("status", json!(status)),
        ];
        if let Some(message) = error {
            fields.push(("error", json!({"message": message})));
        }
        fields.extend([("fromTipId", json!(from)), ("tipId", tip), ("endedAt", json!(now_ms()))]);
        self.emit(event(fields));
    }
}

impl Core {
    /// Pi's `getTree()` with labels from the native session, the authoritative label store
    /// (`PiSessionView.tree`): `{tree, leafId}`.
    pub(super) fn labeled_tree(&self) -> Value {
        let pi = self.pi();
        let labels: std::collections::HashMap<String, Value> = lock_session(&self.session)
            .state()
            .scan_values(ENTRY_LABEL, "")
            .into_iter()
            .map(|v| (v.key.clone(), v.value.clone()))
            .collect();
        let mut tree = crate::view::tree(&pi);
        let mut stack: Vec<&mut Value> = tree.as_array_mut().into_iter().flatten().collect();
        while let Some(node) = stack.pop() {
            let id = node["entry"]["id"].as_str().unwrap_or("").to_string();
            if let Some(map) = node.as_object_mut() {
                match labels.get(&id) {
                    None => {
                        map.remove("label");
                        map.remove("labelTimestamp");
                    }
                    Some(label) if map.get("label") != Some(label) => {
                        map.insert("label".into(), label.clone());
                        map.remove("labelTimestamp");
                    }
                    Some(_) => {}
                }
            }
            if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                stack.extend(children.iter_mut());
            }
        }
        json!({"tree": tree, "leafId": pi.leaf_id().cloned().unwrap_or(Value::Null)})
    }

    /// `setLabel`: label entry `entry_id` (`None` or empty removes the label).
    pub(super) fn set_label(&mut self, entry_id: &str, label: Option<&str>) -> Result<(), String> {
        let write = match label.filter(|l| !l.is_empty()) {
            Some(label) => Write::set(ENTRY_LABEL, entry_id, label.into()),
            None => Write::delete(ENTRY_LABEL, entry_id),
        };
        lock_session(&self.session).commit(&[write]).map(|_| ())
    }
}
