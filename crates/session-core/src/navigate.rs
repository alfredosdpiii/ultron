//! Tree navigation of the `main` lane (`Lane.navigateTree` in packages/agent harness runtime/lane.ts, the client's
//! `/tree`): move the lane's tip to another entry of the session tree, optionally leaving a branch summary of the
//! branch it leaves (`generateBranchSummary`) as a `branch_summary` entry under the target.
//!
//! The TypeScript harness journals the navigation as an operation (`navigation.ready_to_commit`, or
//! `summary.deciding` with a `commit_navigation` boundary); here the tip move (and the summary entry) commit in one
//! transaction, with the same `navigation_start` / `navigation_end` lane events.

use super::{Core, LANE, event};
use crate::durable_store::lock as lock_session;
use crate::lane::now_ms;
use serde_json::{Map, Value, json};
use session_journal::Write;
use session_journal::session::{BRANCH_TIP, LANE_STATE};

/// Tokens kept free when choosing branch history to summarize (upstream default).
const RESERVE_TOKENS: u64 = 16_384;

impl Core {
    /// Navigate `main` to `target` (`None`: the root, an empty context). Answers `{tipId}`.
    pub(super) fn navigate(
        &mut self,
        target: Option<String>,
        summarize: bool,
        instructions: Option<&str>,
    ) -> Result<Value, String> {
        self.idle()?;
        let tip = self.tip();
        if target == tip {
            return Err("Navigation target must differ from the current tip".into());
        }
        if summarize && (tip.is_none() || target.is_none()) {
            return Err("Summarized navigation requires non-root source and target entries".into());
        }
        if let Some(target) = &target
            && !lock_session(&self.session)
                .state()
                .entries()
                .iter()
                .any(|e| e["id"] == target.as_str())
        {
            return Err(format!("Unknown target: {target}"));
        }
        let run_id = format!("nav-{}", now_ms());
        self.emit(event(vec![
            ("type", json!("navigation_start")),
            ("lane", json!(LANE)),
            ("runId", json!(run_id)),
            ("targetId", json!(target)),
            ("startedAt", json!(now_ms())),
        ]));
        let result = self.commit_navigation(tip.clone(), target, summarize, instructions);
        let mut end = vec![
            ("type", json!("navigation_end")),
            ("lane", json!(LANE)),
            ("runId", json!(run_id)),
            ("status", json!(if result.is_ok() { "completed" } else { "failed" })),
            ("fromTipId", json!(tip)),
        ];
        match &result {
            Ok(new_tip) => end.push(("tipId", json!(new_tip))),
            Err(error) => end.push(("error", json!({"message": error}))),
        }
        end.push(("endedAt", json!(now_ms())));
        self.emit(event(end));
        result.map(|tip| json!({"tipId": tip}))
    }

    fn commit_navigation(
        &mut self,
        tip: Option<String>,
        target: Option<String>,
        summarize: bool,
        instructions: Option<&str>,
    ) -> Result<Option<String>, String> {
        let lane_state = self.lane_state();
        let (Some(target_id), true) = (target.clone(), summarize) else {
            lock_session(&self.session).commit(&[
                Write::set(BRANCH_TIP, LANE, target.clone().map_or(Value::Null, Value::from)),
                Write::set(LANE_STATE, LANE, lane_state),
            ])?;
            return Ok(target);
        };
        let pi = self.pi();
        let (entries, _) = compaction::collect_entries_for_branch_summary(&pi, tip.as_deref(), &target_id);
        let options = compaction::BranchSummaryOptions {
            model: &self.model,
            custom_instructions: instructions,
            replace_instructions: false,
            reserve_tokens: RESERVE_TOKENS,
            now_ms: now_ms(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let summary = runtime
            .block_on(compaction::generate_branch_summary(
                &entries,
                &options,
                async |request| self.summarize(request).await,
            ))
            .map_err(|e| match e {
                compaction::BranchSummaryError::Aborted => "Branch summarization was aborted".to_string(),
                compaction::BranchSummaryError::Failed(message) => message,
            })?;
        let id = session_journal::lock_clock(&lock_session(&self.session).clock()).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), target_id.into());
        entry.insert("type".into(), "branch_summary".into());
        entry.insert("fromId".into(), tip.map_or(Value::Null, Value::from));
        entry.insert("summary".into(), summary.summary.into());
        entry.insert(
            "details".into(),
            json!({"readFiles": summary.read_files, "modifiedFiles": summary.modified_files}),
        );
        if let Some(usage) = summary.usage {
            entry.insert("usage".into(), serde_json::to_value(&usage).map_err(|e| e.to_string())?);
        }
        lock_session(&self.session).commit(&[
            Write::Entry(entry),
            Write::set(BRANCH_TIP, LANE, id.clone().into()),
            Write::set(LANE_STATE, LANE, lane_state),
        ])?;
        self.announce();
        Ok(Some(id))
    }
}

/// Where native sessions keep entry labels (`entryLabel` in harness session/values.ts).
const ENTRY_LABEL: &str = "pi.entry.label";

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
