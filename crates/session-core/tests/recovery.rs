//! Recovery of a session whose worker died mid-run (crate::recovery). The crashed journals are written by the
//! same run journal the worker uses (session_journal::run), stopped where a SIGKILL would leave them; settling
//! must leave a journal that reopens idle, keeps every committed entry, settles in-flight work as the TypeScript
//! harness does (`drive/recovery.ts`: interrupted assistant from the committed frame prefix, no provider call, no
//! cell replay) and hands back queued messages.

use serde_json::{Value, json};
use session_core::recovery::{
    CONTINUE_PROMPT, INTERRUPTED_ASSISTANT, INTERRUPTED_CELL, MAX_RECOVERY_DEPTH, RECOVERY_DEPTH, Recovered,
    interrupted_assistant, reduce_frames, settle,
};
use session_journal::run::{self, OP_META, OP_RESULT, OP_STATE, OP_TOOL_ARGS, PENDING_ENTRY, PENDING_FRAMES, Run};
use session_journal::session::LANE_STATE;
use session_journal::{Session, SessionRepo, TestClock, Write};

fn repo(dir: &std::path::Path) -> SessionRepo {
    SessionRepo::new(dir, TestClock::shared(1_790_000_000_000, 1))
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": 1})
}

fn admit(session: &mut Session, prompt: Value) -> Run {
    let config = run::lane_config("anthropic", "claude-sonnet-4-5", "medium", &["rlm"]);
    Run::admit(session, config, run::default_settings(), prompt).unwrap()
}

fn partial(content: Value) -> Value {
    json!({"role": "assistant", "content": content, "api": "anthropic-messages", "provider": "anthropic",
        "model": "claude-sonnet-4-5", "usage": {"input": 3, "output": 1}, "stopReason": "pending", "timestamp": 2})
}

/// The messages of the session's `main` branch, in order.
fn branch(session: &Session) -> Vec<Value> {
    session
        .branch_entries("main")
        .unwrap()
        .into_iter()
        .map(|e| e["message"].clone())
        .collect()
}

fn reopened(dir: &std::path::Path, session: &Session) -> Session {
    repo(dir).open(session.metadata()).unwrap()
}

#[test]
fn session_core_recovery_settles_an_interrupted_stream_from_its_committed_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = repo(dir.path()).create("/w", None, None).unwrap();
    let mut run = admit(&mut session, user("hi"));
    run.begin_assistant(&mut session, 1000, 200_000).unwrap();
    let text = |t: &str| json!([{"type": "text", "text": t}]);
    for event in [
        json!({"type": "start", "partial": partial(json!([]))}),
        json!({"type": "text_start", "contentIndex": 0, "partial": partial(text(""))}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "Hel", "partial": partial(text("Hel"))}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "lo", "partial": partial(text("Hello"))}),
    ] {
        run.stream_event(&mut session, &event).unwrap();
    }
    let operation = run.operation_id().to_string();
    // The worker died here (SIGKILL: nothing else is written).
    drop(run);
    let mut session = reopened(dir.path(), &session);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(
        recovered,
        Recovered {
            operation_id: Some(operation.clone()),
            status: Some("failed".into()),
            resume: true,
            depth: 0,
            interrupted_calls: 0,
            queued: Vec::new(),
        }
    );
    let messages = branch(&session);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"][0]["text"], "hi");
    let assistant = &messages[1];
    assert_eq!(assistant["content"], json!([{"type": "text", "text": "Hello"}]));
    assert_eq!(assistant["stopReason"], "error");
    assert_eq!(assistant["errorMessage"], INTERRUPTED_ASSISTANT);
    assert_eq!(assistant["usage"]["totalTokens"], 0);
    assert_eq!(assistant["provider"], "anthropic");
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(result["error"]["code"], "interrupted");
    assert!(session.value(OP_META, &operation).is_none());
    assert!(session.value(OP_STATE, &operation).is_none());
    assert!(
        session.state().lists().iter().all(|(ns, _, _)| ns != PENDING_FRAMES),
        "the frames are dropped with the settled response"
    );
    assert_eq!(
        session.value(LANE_STATE, "main").unwrap(),
        &json!({"currentOperationId": null, "lastOperationId": operation, "inbox": []})
    );
    // The settled journal reopens idle and settling again finds nothing.
    let mut again = reopened(dir.path(), &session);
    assert_eq!(branch(&again).len(), 2);
    assert_eq!(settle(&mut again).unwrap(), Recovered::default());
}

#[test]
fn session_core_recovery_gives_running_cells_unknown_outcomes_and_keeps_finished_ones() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = repo(dir.path()).create("/w", None, None).unwrap();
    let mut run = admit(&mut session, user("work"));
    run.begin_assistant(&mut session, 1000, 200_000).unwrap();
    let calls = json!([
        {"type": "text", "text": "Two cells."},
        {"type": "toolCall", "id": "call_a", "name": "rlm", "arguments": {"code": "1"}},
        {"type": "toolCall", "id": "call_b", "name": "rlm", "arguments": {"code": "2"}},
    ]);
    let mut answer = partial(calls);
    answer["stopReason"] = json!("toolUse");
    assert!(run.commit_assistant(&mut session, answer).unwrap());
    run.tool_started(&mut session, "call_a", json!({"code": "1"})).unwrap();
    run.tool_started(&mut session, "call_b", json!({"code": "2"})).unwrap();
    let done = json!({"role": "toolResult", "toolCallId": "call_a", "toolName": "rlm",
        "content": [{"type": "text", "text": "1"}], "isError": false, "timestamp": 3});
    run.tool_finished(&mut session, "call_a", done.clone()).unwrap();
    let operation = run.operation_id().to_string();
    drop(run);
    let mut session = reopened(dir.path(), &session);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.interrupted_calls, 1);
    assert!(recovered.resume);
    let messages = branch(&session);
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "toolResult"]);
    assert_eq!(messages[2], done, "a cell that finished keeps its result");
    let lost = &messages[3];
    assert_eq!(lost["toolCallId"], "call_b");
    assert_eq!(lost["toolName"], "rlm");
    assert_eq!(lost["isError"], true);
    assert_eq!(lost["content"], json!([{"type": "text", "text": INTERRUPTED_CELL}]));
    assert!(session.state().scan_values(PENDING_ENTRY, "").is_empty());
    assert!(session.state().scan_values(OP_TOOL_ARGS, &operation).is_empty());
    assert_eq!(
        session.branch_tip("main").flatten(),
        session.state().entries().last().unwrap()["id"]
            .as_str()
            .map(str::to_string)
    );
}

#[test]
fn session_core_recovery_completes_an_answered_run_and_aborts_a_cancelled_one() {
    let dir = tempfile::tempdir().unwrap();
    // The final answer committed; only the terminal transaction was lost.
    let mut session = repo(dir.path()).create("/w", None, None).unwrap();
    let mut run = admit(&mut session, user("q"));
    run.begin_assistant(&mut session, 1000, 200_000).unwrap();
    let mut answer = partial(json!([{"type": "text", "text": "A."}]));
    answer["stopReason"] = json!("stop");
    assert!(!run.commit_assistant(&mut session, answer).unwrap());
    let operation = run.operation_id().to_string();
    drop(run);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("completed"));
    assert!(!recovered.resume);
    let result = session.value(OP_RESULT, &operation).unwrap();
    assert_eq!(result["status"], "completed");
    assert!(result.get("error").is_none());
    assert_eq!(branch(&session).len(), 2);

    // A cancel request was journaled before the worker died.
    let other = tempfile::tempdir().unwrap();
    let mut cancelled = repo(other.path()).create("/w", None, None).unwrap();
    let mut run = admit(&mut cancelled, user("stop me"));
    run.begin_assistant(&mut cancelled, 1000, 200_000).unwrap();
    run.request_cancel(&mut cancelled, 5).unwrap();
    drop(run);
    let recovered = settle(&mut cancelled).unwrap();
    assert_eq!(recovered.status.as_deref(), Some("aborted"));
    assert!(!recovered.resume);
    // The in-flight request still becomes an (empty) interrupted message.
    let messages = branch(&cancelled);
    assert_eq!(messages[1]["errorMessage"], INTERRUPTED_ASSISTANT);
    assert_eq!(messages[1]["content"], json!([]));
    assert_eq!(messages[1]["model"], "claude-sonnet-4-5");
}

#[test]
fn session_core_recovery_hands_back_queued_messages_and_stops_resuming_after_repeated_crashes() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = repo(dir.path()).create("/w", None, None).unwrap();
    let mut prompt = user(CONTINUE_PROMPT);
    prompt[RECOVERY_DEPTH] = json!(MAX_RECOVERY_DEPTH);
    let mut run = admit(&mut session, prompt);
    // A follow-up waits in the lane's inbox, as the session actor enqueues it.
    let follow = user("and then this");
    let mut lane = session.value(LANE_STATE, "main").unwrap().clone();
    lane["inbox"] = json!([{"entryId": "q-1", "kind": "followUp"}]);
    run.set_inbox(lane["inbox"].clone());
    session
        .commit(&[
            Write::set(PENDING_ENTRY, "q-1", json!({"type": "message", "payload": follow})),
            Write::set(LANE_STATE, "main", lane),
        ])
        .unwrap();
    drop(run);
    let recovered = settle(&mut session).unwrap();
    assert_eq!(recovered.depth, MAX_RECOVERY_DEPTH);
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    assert!(
        !recovered.resume,
        "a continuation interrupted again is not continued again"
    );
    assert_eq!(recovered.queued.len(), 1);
    assert_eq!(recovered.queued[0].entry_id, "q-1");
    assert_eq!(recovered.queued[0].kind, "followUp");
    assert_eq!(recovered.queued[0].message, follow);
    assert_eq!(
        session.value(LANE_STATE, "main").unwrap()["inbox"],
        json!([{"entryId": "q-1", "kind": "followUp"}]),
        "the inbox keeps the waiting message"
    );
    assert!(session.value(PENDING_ENTRY, "q-1").is_some());
}

#[test]
fn session_core_recovery_reduces_frames_to_text_and_thinking() {
    let frames = [
        json!({"type": "start", "partial": {"role": "assistant", "content": [], "provider": "p", "model": "m"}}),
        json!({"type": "thinking_start", "contentIndex": 0, "content": {"type": "thinking", "thinking": ""}}),
        json!({"type": "thinking_delta", "contentIndex": 0, "delta": "hm"}),
        json!({"type": "thinking_end", "contentIndex": 0, "content": "hmm"}),
        json!({"type": "text_start", "contentIndex": 1, "content": {"type": "text", "text": ""}}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "a"}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "b"}),
        json!({"type": "toolcall_start", "contentIndex": 2, "toolCall": {"type": "toolCall", "id": "c"}}),
    ];
    let reduced = reduce_frames(&frames).unwrap();
    assert_eq!(
        reduced["content"],
        json!([{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "ab"}])
    );
    assert_eq!(reduce_frames(&frames[1..]), None, "no start frame, no partial");
    let empty = interrupted_assistant(&json!({"provider": "p", "modelId": "m"}), None);
    assert_eq!(
        (&empty["provider"], &empty["model"], &empty["api"], &empty["content"]),
        (&json!("p"), &json!("m"), &json!("unknown"), &json!([]))
    );
}
