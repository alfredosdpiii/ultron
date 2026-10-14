//! A tree navigation's journal (crate::compaction_op), as the harness journals it (`runtime/lane.ts`
//! `acceptNavigation`, `runtime/drive/structural.ts`): a summarized navigation is admitted at `summary.deciding` with
//! its `branch_summary` preparation and a `commit_navigation` boundary; one without a summary at
//! `navigation.ready_to_commit`. The kill points settle on reopen (crate::recovery): an admitted navigation without a
//! summary commits (`commitNavigation` makes no external call), a cancel request ends either `aborted`, and an
//! interrupted summary ends `failed` (`interrupted`) with its preparation removed; the branch only moves when the
//! navigation commits.

use serde_json::{Map, Value, json};
use session_core::compaction_op::{CompactionOp, ENTRY_LABEL, request_cancel};
use session_core::op_journal::OP_PREPARATION;
use session_core::recovery::settle;
use session_journal::run::{OP_META, OP_RESULT, OP_STATE};
use session_journal::session::{BRANCH_TIP, LANE_STATE};
use session_journal::{Session, SessionRepo, TestClock, Write};

fn repo(dir: &std::path::Path) -> SessionRepo {
    SessionRepo::new(dir, TestClock::shared(1_790_000_000_000, 1))
}

/// `first` -> `answer` -> `second`: returns the session, the first answer (the target) and the tip.
fn session(dir: &std::path::Path) -> (Session, String, String) {
    let mut session = repo(dir).create("/w", None, None).unwrap();
    session.commit(&[Write::set(BRANCH_TIP, "main", Value::Null)]).unwrap();
    session
        .append_message("main", json!({"role": "user", "content": "first", "timestamp": 1}))
        .unwrap();
    let target = session
        .append_message(
            "main",
            json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}], "timestamp": 2}),
        )
        .unwrap();
    let tip = session
        .append_message("main", json!({"role": "user", "content": "second", "timestamp": 3}))
        .unwrap();
    (session, target, tip)
}

fn preparation() -> compaction::BranchPreparation {
    compaction::prepare_branch_entries(
        &[json!({"type": "message", "id": "x", "message": {"role": "user", "content": "second", "timestamp": 3}})],
        0,
    )
}

fn current_operation(session: &Session) -> Value {
    session.value(LANE_STATE, "main").unwrap()["currentOperationId"].clone()
}

fn reopen(dir: &std::path::Path, session: Session) -> Session {
    let metadata = session.metadata().clone();
    drop(session);
    repo(dir).open(&metadata).unwrap()
}

/// Admission of a summarized navigation: the `branch_summary` preparation, the intent and `summary.deciding` with a
/// `commit_navigation` boundary carrying the label, the task the custom instructions.
#[test]
fn session_core_navigation_journal_admits_a_summarized_navigation_at_deciding() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = session(dir.path());
    let op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), Some("focus"), Some("here"))
        .unwrap();
    let operation = op.operation_id().to_string();
    assert_eq!(current_operation(&session), json!(operation));
    let meta = session.value(OP_META, &operation).unwrap();
    assert_eq!(
        meta["intent"],
        json!({"kind": "navigation", "targetId": target, "summarize": true, "label": "here", "customInstructions": "focus"})
    );
    assert_eq!(meta["sourceTipId"], tip.as_str());
    let state = session.value(OP_STATE, &operation).unwrap();
    assert_eq!(state["at"], "summary.deciding");
    assert_eq!(state["task"]["customInstructions"], "focus");
    assert_eq!(
        state["task"]["boundary"],
        json!({"kind": "commit_navigation", "targetId": target, "label": "here"})
    );
    let prepared = session.state().scan_values(OP_PREPARATION, &format!("{operation}:"));
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].value["kind"], "branch_summary");
    assert_eq!(prepared[0].value["messages"][0]["content"], "second");
    assert!(prepared[0].value["totalTokens"].as_u64().unwrap() > 0);
}

/// The summary commits in one transaction: the tip passes the target, the `branch_summary` entry (from the old
/// tip) becomes the tip, its label is set, the operation ends `completed` and its preparation goes.
#[test]
fn session_core_navigation_journal_commits_the_summary_under_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = session(dir.path());
    let mut op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
    let operation = op.operation_id().to_string();
    op.override_navigation(Some("names"), Some("checkpoint"));
    op.request(&mut session, session_core::compaction_op::SummaryRetry::default())
        .unwrap();
    let state = session.value(OP_STATE, &operation).unwrap();
    assert_eq!(state["at"], "summary.effect_pending");
    assert_eq!(state["task"]["customInstructions"], "names");
    assert_eq!(state["task"]["boundary"]["label"], "checkpoint");
    let usage = json!({"input": 3, "output": 2, "totalTokens": 5});
    op.record_usage(&mut session, &usage).unwrap();
    let mut entry = Map::new();
    entry.insert("summary".into(), "Left a branch.".into());
    entry.insert("fromHook".into(), false.into());
    let id = op.complete_navigation(&mut session, entry, None).unwrap();
    let committed = session.state().entry(&id).unwrap().clone();
    assert_eq!(
        (&committed["type"], &committed["parentId"], &committed["fromId"]),
        (&json!("branch_summary"), &json!(target), &json!(tip))
    );
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(id.as_str()));
    assert_eq!(session.value(ENTRY_LABEL, &id), Some(&json!("checkpoint")));
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["status"], &result["tipId"]),
        (&json!("navigation"), &json!("completed"), &json!(id))
    );
    assert_eq!(session.state().usage_rows().len(), 1);
    assert!(session.state().scan_values(OP_PREPARATION, "").is_empty());
    assert_eq!(current_operation(&session), Value::Null);
}

/// A hook's summary commits from `summary.deciding` with its usage row; a hook's cancel ends the operation
/// `declined` with the branch unchanged.
#[test]
fn session_core_navigation_journal_hook_summary_and_decline() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = session(dir.path());
    let op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
    let mut entry = Map::new();
    entry.insert("summary".into(), "From the hook.".into());
    entry.insert("fromHook".into(), true.into());
    let usage = json!({"input": 4, "output": 2, "totalTokens": 6});
    let id = op
        .complete_navigation(&mut session, entry, Some(usage.clone()))
        .unwrap();
    assert_eq!(session.state().entry(&id).unwrap()["fromHook"], true);
    assert_eq!(session.state().usage_rows()[0]["usage"], usage);
    let text = std::fs::read_to_string(&session.metadata().path).unwrap();
    assert!(!text.contains("summary.ready"), "no request state for a hook's summary");

    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, _) = self::session(dir.path());
    let op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
    let operation = op.operation_id().to_string();
    op.decline(&mut session).unwrap();
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!((&result["status"], &result["tipId"]), (&json!("declined"), &json!(tip)));
    assert!(session.state().entries().iter().all(|e| e["type"] != "branch_summary"));
    assert!(session.state().scan_values(OP_PREPARATION, "").is_empty());
}

/// Kill point after admission of a navigation without a summary: on reopen it commits (the harness drives
/// `navigation.ready_to_commit` to `commitNavigation`), its label set; with a cancel request it ends `aborted`.
#[test]
fn session_core_navigation_journal_ready_to_commit_then_a_kill_commits() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, _) = session(dir.path());
    let op = CompactionOp::begin_direct_navigation(&mut session, Some(&target), Some("here"), None).unwrap();
    let operation = op.operation_id().to_string();
    let state = session.value(OP_STATE, &operation).unwrap();
    assert_eq!(
        (&state["at"], &state["targetId"], &state["label"]),
        (&json!("navigation.ready_to_commit"), &json!(target), &json!("here"))
    );
    assert!(state.get("task").is_none());
    drop(op);
    let mut session = reopen(dir.path(), session);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("completed"));
    assert!(!recovered.resume);
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(target.as_str()));
    assert_eq!(session.value(ENTRY_LABEL, &target), Some(&json!("here")));
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["tipId"]),
        (&json!("navigation"), &json!(target))
    );
    assert_eq!(current_operation(&session), Value::Null);

    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = self::session(dir.path());
    let op = CompactionOp::begin_direct_navigation(&mut session, Some(&target), None, None).unwrap();
    request_cancel(&mut session, 5).unwrap();
    drop(op);
    let mut session = reopen(dir.path(), session);
    assert_eq!(settle(&mut session).unwrap().status.as_deref(), Some("aborted"));
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
}

/// The direct navigation's own commit: the tip moves, the label is set, and the operation ends `completed`.
#[test]
fn session_core_navigation_journal_direct_navigation_commits_live() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, _) = session(dir.path());
    let op = CompactionOp::begin_direct_navigation(&mut session, Some(&target), Some("here"), None).unwrap();
    let operation = op.operation_id().to_string();
    assert_eq!(
        op.commit_navigation(&mut session).unwrap().as_deref(),
        Some(target.as_str())
    );
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(target.as_str()));
    assert_eq!(session.value(ENTRY_LABEL, &target), Some(&json!("here")));
    assert_eq!(session.value(OP_RESULT, &operation).unwrap()["status"], "completed");
    assert_eq!(current_operation(&session), Value::Null);
}

/// Kill points of a summarized navigation: during the summary request it ends `failed` (`interrupted`), with a
/// cancel request (during the hook or the summary) `aborted`; either way the branch is unchanged, no summary entry
/// exists and the preparation is gone.
#[test]
fn session_core_navigation_journal_interrupted_summary_settles_without_moving() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, target, tip) = session(dir.path());
        let mut op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
        let operation = op.operation_id().to_string();
        if cancel {
            request_cancel(&mut session, 5).unwrap();
            assert_eq!(
                session.value(OP_STATE, &operation).unwrap()["control"]["status"],
                "cancel_requested"
            );
        } else {
            op.request(&mut session, session_core::compaction_op::SummaryRetry::default())
                .unwrap();
        }
        drop(op);
        let mut session = reopen(dir.path(), session);
        let recovered = settle(&mut session).unwrap();
        let result = session.value(OP_RESULT, &operation).unwrap();
        if cancel {
            assert_eq!(recovered.status.as_deref(), Some("aborted"));
        } else {
            assert_eq!(recovered.status.as_deref(), Some("failed"));
            assert_eq!(result["error"]["code"], "interrupted");
        }
        assert!(!recovered.resume);
        assert_eq!(result["kind"], "navigation");
        assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
        assert!(session.state().entries().iter().all(|e| e["type"] != "branch_summary"));
        assert!(session.state().scan_values(OP_PREPARATION, "").is_empty());
        assert_eq!(current_operation(&session), Value::Null);
    }
}

/// A summarized navigation retries like a compaction: `summary.retry_wait` then attempt 2, which commits the
/// summary; killed during the wait it settles `failed` (`interrupted`) without moving.
#[test]
fn session_core_navigation_journal_retry_wait_then_retry_or_kill() {
    use session_core::compaction_op::SummaryRetry;
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, _) = session(dir.path());
    let mut op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
    let operation = op.operation_id().to_string();
    op.request(&mut session, SummaryRetry::default()).unwrap();
    op.retry_wait(&mut session, None, "Overloaded", 99).unwrap();
    assert_eq!(session.value(OP_STATE, &operation).unwrap()["at"], "summary.retry_wait");
    assert_eq!(
        session.value(OP_STATE, &operation).unwrap()["task"]["boundary"]["kind"],
        "commit_navigation"
    );
    op.retry(&mut session).unwrap();
    let mut entry = Map::new();
    entry.insert("summary".into(), "Second attempt.".into());
    let id = op.complete_navigation(&mut session, entry, None).unwrap();
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(id.as_str()));
    assert_eq!(session.value(OP_RESULT, &operation).unwrap()["status"], "completed");

    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = self::session(dir.path());
    let mut op = CompactionOp::begin_navigation(&mut session, Some(&target), &preparation(), None, None).unwrap();
    let operation = op.operation_id().to_string();
    op.request(&mut session, SummaryRetry::default()).unwrap();
    op.retry_wait(&mut session, None, "Overloaded", 99).unwrap();
    drop(op);
    let mut session = reopen(dir.path(), session);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["error"]["code"]),
        (&json!("navigation"), &json!("interrupted"))
    );
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
    assert!(session.state().scan_values(OP_PREPARATION, "").is_empty());
}

/// A selected user message's navigation without a summary goes to its parent and labels the message itself
/// (AgentSession `navigateTree`): `navigation.ready_to_commit` records `labelEntryId`, and a reopened session
/// commits the label there.
#[test]
fn session_core_navigation_journal_labels_the_selected_entry_on_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, target, tip) = session(dir.path());
    let op = CompactionOp::begin_direct_navigation(&mut session, Some(&target), Some("here"), Some(&tip)).unwrap();
    let operation = op.operation_id().to_string();
    assert_eq!(
        session.value(OP_STATE, &operation).unwrap()["labelEntryId"],
        tip.as_str()
    );
    drop(op);
    let mut session = reopen(dir.path(), session);
    assert_eq!(settle(&mut session).unwrap().status.as_deref(), Some("completed"));
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(target.as_str()));
    assert_eq!(session.value(ENTRY_LABEL, &tip), Some(&json!("here")));
    assert_eq!(session.value(ENTRY_LABEL, &target), None);
}
