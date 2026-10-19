//! A manual compaction's journal when it is cancelled (crate::compaction_op): the cancel request is journaled in the
//! operation's state (`control: cancel_requested`, harness `requestOperationAbort`) before its terminal transaction,
//! so a worker killed in between is settled `aborted` on reopen (`drive/reconcile.ts` `publishAbortedTerminal`),
//! with the branch unchanged and the preparation removed; a live cancel ends the operation `aborted` the same way.

use serde_json::{Map, Value, json};
use session_core::compaction_op::{self, CompactionOp};
use session_core::op_journal::OP_PREPARATION;
use session_core::recovery::settle;
use session_journal::run::{OP_META, OP_RESULT, OP_STATE};
use session_journal::session::{BRANCH_TIP, LANE_STATE};
use session_journal::{Session, SessionRepo, TestClock, Write};

fn repo(dir: &std::path::Path) -> SessionRepo {
    SessionRepo::new(dir, TestClock::shared(1_790_000_000_000, 1))
}

fn preparation(first: &str) -> compaction::CompactionPreparation {
    compaction::CompactionPreparation {
        first_kept_entry_id: first.into(),
        messages_to_summarize: vec![json!({"role": "user", "content": "old", "timestamp": 1})],
        turn_prefix_messages: Vec::new(),
        is_split_turn: false,
        tokens_before: 1234,
        previous_summary: None,
        file_ops: Default::default(),
        settings: Default::default(),
    }
}

/// A session with two messages; the compaction keeps the second. Returns it and its tip.
fn session(dir: &std::path::Path) -> (Session, String) {
    let mut session = repo(dir).create("/w", None, None).unwrap();
    session.commit(&[Write::set(BRANCH_TIP, "main", Value::Null)]).unwrap();
    session
        .append_message("main", json!({"role": "user", "content": "old", "timestamp": 1}))
        .unwrap();
    let tip = session
        .append_message("main", json!({"role": "user", "content": "new", "timestamp": 2}))
        .unwrap();
    (session, tip)
}

fn preparations(session: &Session) -> usize {
    session.state().scan_values(OP_PREPARATION, "").len()
}

fn current_operation(session: &Session) -> Value {
    session.value(LANE_STATE, "main").unwrap()["currentOperationId"].clone()
}

/// Kill point: after the cancel request was journaled during the summary request, before the terminal
/// transaction. The reopened session settles the operation `aborted`, keeps the branch, and leaves nothing open.
#[test]
fn session_core_compaction_cancel_journaled_before_a_kill_settles_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, tip) = session(dir.path());
    let mut op = CompactionOp::begin(&mut session, &preparation(&tip), &[json!({"role": "user"})], None).unwrap();
    op.request(&mut session, session_core::compaction_op::SummaryRetry::default())
        .unwrap();
    let operation = current_operation(&session).as_str().unwrap().to_string();
    compaction_op::request_cancel(&mut session, 5).unwrap();
    let state = session.value(OP_STATE, &operation).unwrap().clone();
    assert_eq!(state["at"], "summary.effect_pending");
    assert_eq!(
        state["control"],
        json!({"status": "cancel_requested", "requestedAt": 5})
    );
    // A second request changes nothing.
    compaction_op::request_cancel(&mut session, 9).unwrap();
    assert_eq!(
        session.value(OP_STATE, &operation).unwrap()["control"]["requestedAt"],
        5
    );
    // The worker died here (SIGKILL: nothing else is written).
    drop(op);
    let mut session = repo(dir.path()).open(session.metadata()).unwrap();
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("aborted"));
    assert!(!recovered.resume);
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(result["kind"], "compaction");
    assert_eq!(result["status"], "aborted");
    assert_eq!(result["tipId"], tip.as_str());
    assert!(result.get("error").is_none());
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
    assert_eq!(preparations(&session), 0);
    assert!(session.value(OP_META, &operation).is_none() && session.value(OP_STATE, &operation).is_none());
    assert_eq!(current_operation(&session), Value::Null);
}

/// Kill point without a cancel request: the compaction ends `failed` (`interrupted`), its preparation removed too.
#[test]
fn session_core_compaction_killed_without_a_cancel_settles_failed_and_drops_its_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, tip) = session(dir.path());
    let mut op = CompactionOp::begin(&mut session, &preparation(&tip), &[], Some("short")).unwrap();
    op.request(&mut session, session_core::compaction_op::SummaryRetry::default())
        .unwrap();
    let operation = current_operation(&session).as_str().unwrap().to_string();
    assert_eq!(preparations(&session), 1);
    drop(op);
    let mut session = repo(dir.path()).open(session.metadata()).unwrap();
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(result["error"]["code"], "interrupted");
    assert_eq!(preparations(&session), 0);
    assert_eq!(current_operation(&session), Value::Null);
}

/// A live cancel (`abort` during the hook or the summary): the terminal transaction ends the operation `aborted`;
/// the branch is unchanged and nothing stays open. A cancel request for a run's operation is not a compaction's.
#[test]
fn session_core_compaction_cancelled_live_ends_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, tip) = session(dir.path());
    let op = CompactionOp::begin(&mut session, &preparation(&tip), &[], None).unwrap();
    let operation = current_operation(&session).as_str().unwrap().to_string();
    assert_eq!(session.value(OP_STATE, &operation).unwrap()["at"], "summary.deciding");
    compaction_op::request_cancel(&mut session, 5).unwrap();
    op.abort(&mut session).unwrap();
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["status"]),
        (&json!("compaction"), &json!("aborted"))
    );
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
    assert_eq!(preparations(&session), 0);
    assert_eq!(current_operation(&session), Value::Null);
    assert!(session.state().entries().iter().all(|e| e["type"] != "compaction"));
}

/// Kill point during the `session_before_compact` hook: `abort` journals the cancel request at
/// `summary.deciding` and gives the hook up; a worker killed before the terminal transaction is settled
/// `aborted` on reopen, the branch unchanged and nothing open.
#[test]
fn session_core_compaction_cancel_during_the_hook_then_a_kill_settles_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut session, tip) = session(dir.path());
    let op = CompactionOp::begin(&mut session, &preparation(&tip), &[], None).unwrap();
    let operation = current_operation(&session).as_str().unwrap().to_string();
    compaction_op::request_cancel(&mut session, 5).unwrap();
    let state = session.value(OP_STATE, &operation).unwrap().clone();
    assert_eq!(state["at"], "summary.deciding");
    assert_eq!(state["control"]["status"], "cancel_requested");
    drop(op);
    let mut session = repo(dir.path()).open(session.metadata()).unwrap();
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("aborted"));
    assert!(!recovered.resume);
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["status"]),
        (&json!("compaction"), &json!("aborted"))
    );
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
    assert_eq!(preparations(&session), 0);
    assert_eq!(current_operation(&session), Value::Null);
}

/// pi-ai `retryDelayMs` and the harness's `normalizedRetryPolicy` from settings.json's `retry`: `baseDelayMs *
/// 2^(attempt-1)` capped at `maxAgentDelayMs`; `maxRetries + 1` attempts, one when retries are off.
#[test]
fn session_core_compaction_summary_retry_policy_matches_the_harness() {
    use session_core::compaction_op::SummaryRetry;
    let policy = SummaryRetry::default();
    assert_eq!(
        (
            policy.delay_ms(1),
            policy.delay_ms(2),
            policy.delay_ms(3),
            policy.delay_ms(6),
            policy.delay_ms(200)
        ),
        (2000, 4000, 8000, 60000, 60000)
    );
    assert_eq!(
        policy.value(),
        json!({"maxAttempts": 4, "baseDelayMs": 2000, "maxAgentDelayMs": 60000})
    );
    assert_eq!(SummaryRetry::from_settings(&Value::Null, true), policy);
    let custom = SummaryRetry::from_settings(&json!({"maxRetries": 1, "baseDelayMs": 5, "maxAgentDelayMs": 7}), true);
    assert_eq!((custom.max_attempts, custom.delay_ms(1), custom.delay_ms(2)), (2, 5, 7));
    assert_eq!(
        SummaryRetry::from_settings(&json!({"enabled": false}), true).max_attempts,
        1
    );
    assert_eq!(SummaryRetry::from_settings(&json!({}), false).max_attempts, 1);
}

/// A retryable failed attempt journals its usage row and `summary.retry_wait` (`nextAttempt`, `notBefore`,
/// `errorMessage`); the next attempt is `summary.ready` (`nextAttempt` 2) and `summary.effect_pending` (`attempt`
/// 2, no usage yet, a fresh request), which completes. A worker killed during the wait settles `failed`
/// (`interrupted`), or `aborted` after a cancel request, with nothing moved and the preparation removed.
#[test]
fn session_core_compaction_retry_wait_journal_and_kill() {
    use session_core::compaction_op::SummaryRetry;
    let dir = tempfile::tempdir().unwrap();
    let (mut session, tip) = session(dir.path());
    let mut op = CompactionOp::begin(&mut session, &preparation(&tip), &[], None).unwrap();
    let operation = current_operation(&session).as_str().unwrap().to_string();
    op.request(&mut session, SummaryRetry::default()).unwrap();
    let first_usage = session.value(OP_STATE, &operation).unwrap()["request"]["usageId"].clone();
    op.retry_wait(&mut session, Some(&json!({"totalTokens": 0})), "Overloaded", 1234)
        .unwrap();
    let state = session.value(OP_STATE, &operation).unwrap().clone();
    assert_eq!(
        (
            &state["at"],
            &state["nextAttempt"],
            &state["notBefore"],
            &state["errorMessage"]
        ),
        (
            &json!("summary.retry_wait"),
            &json!(2),
            &json!(1234),
            &json!("Overloaded")
        )
    );
    assert_eq!(state["summaryContext"]["retryPolicy"]["maxAttempts"], 4);
    assert_eq!(session.state().usage_rows()[0]["id"], first_usage);
    op.retry(&mut session).unwrap();
    let state = session.value(OP_STATE, &operation).unwrap().clone();
    assert_eq!(
        (&state["at"], &state["attempt"], &state["usageIds"]),
        (&json!("summary.effect_pending"), &json!(2), &json!([]))
    );
    assert_ne!(state["request"]["usageId"], first_usage);
    assert_eq!(op.attempt(), 2);
    let text = std::fs::read_to_string(&session.metadata().path).unwrap();
    assert!(
        text.contains(r#""nextAttempt":2"#),
        "summary.ready for the second attempt"
    );
    op.record_usage(&mut session, &json!({"totalTokens": 5})).unwrap();
    assert_eq!(session.value(OP_STATE, &operation).unwrap()["attempt"], 2);
    let mut entry = Map::new();
    entry.insert("type".into(), "compaction".into());
    entry.insert("summary".into(), "After a retry.".into());
    let id = op.complete(&mut session, entry).unwrap();
    assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(id.as_str()));

    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut session, tip) = self::session(dir.path());
        let mut op = CompactionOp::begin(&mut session, &preparation(&tip), &[], None).unwrap();
        let operation = current_operation(&session).as_str().unwrap().to_string();
        op.request(&mut session, SummaryRetry::default()).unwrap();
        op.retry_wait(&mut session, None, "Overloaded", 1234).unwrap();
        if cancel {
            compaction_op::request_cancel(&mut session, 5).unwrap();
        }
        drop(op);
        let mut session = repo(dir.path()).open(session.metadata()).unwrap();
        let recovered = settle(&mut session).unwrap();
        assert_eq!(
            recovered.status.as_deref(),
            Some(if cancel { "aborted" } else { "failed" })
        );
        assert!(!recovered.resume);
        let result = session.value(OP_RESULT, &operation).unwrap();
        if !cancel {
            assert_eq!(result["error"]["code"], "interrupted");
        }
        assert_eq!(session.branch_tip("main").flatten().as_deref(), Some(tip.as_str()));
        assert_eq!(preparations(&session), 0);
        assert_eq!(current_operation(&session), Value::Null);
    }
}
