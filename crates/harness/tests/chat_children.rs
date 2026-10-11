//! Delegated children as durable tasks (`rlm.spawn` / `rlm.collect` / `rlm.results` / `rlm.cancel`): one journal,
//! the durable task table. A child is owned by the cell that spawned it and owns its own conversation; its timeout is
//! a durable deadline; cancelling the parent's run cascades to it. Cases run against Memory, SQLite and JSONL.

mod chat_support;
mod support;

use std::sync::{Arc, Mutex};

use ai::Message;
use chat_support::*;
use durable::{SubmissionStatus, TaskId, TaskStatus};
use harness::chat::faux::{FauxStep, rlm_call, text_message};
use harness::chat::host_ops::HostOps;
use harness::chat::{CellCall, CellResult, SubmissionDraft};
use harness::scheduler::Cancel;
use serde_json::{Map, Value, json};
use support::*;

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn op(ops: &HostOps, kind: &str, value: Value, call: &CellCall, signal: &Cancel) -> Result<Value, String> {
    ops.handle(kind, &payload(value), call, signal)
        .expect("a delegation operation")
}

/// What the child answers its brief with.
#[derive(Clone)]
enum Child {
    Answer(&'static str),
    /// Never answers (until its request is cancelled).
    Hang,
    /// Answers once `release` is cancelled.
    Gated(Cancel, &'static str),
}

/// Faux responses routed by conversation: the parent (first user message `go`) calls the REPL once and then says
/// `done`; a child (brief `brief`) behaves as `child`.
fn route(setup: &Setup, child: Child) {
    let steps = (0..40)
        .map(|_| {
            let child = child.clone();
            FauxStep::factory(move |context, _, signal| {
                let first_user = context
                    .messages
                    .iter()
                    .find(|message| matches!(message, Message::User(_)))
                    .and_then(text_of)
                    .unwrap_or_default();
                let answered = context
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::ToolResult(_)));
                if first_user == "go" {
                    return Ok(if answered {
                        text_message("done")
                    } else {
                        rlm_call("delegate", "c1")
                    });
                }
                match &child {
                    Child::Answer(text) => Ok(text_message(text)),
                    Child::Hang => Err(signal.wait()),
                    Child::Gated(release, text) => {
                        Cancel::any(&[release, signal]).signal().wait();
                        signal.check()?;
                        Ok(text_message(text))
                    }
                }
            })
        })
        .collect();
    setup.faux.set_responses(steps);
}

/// The `delegate` cell runs `body`.
fn delegate(
    setup: &Setup,
    body: impl Fn(&HostOps, &CellCall, &Cancel) -> Result<CellResult, String> + Send + Sync + 'static,
) {
    setup.cells.script(
        "delegate",
        Script {
            body: Some(Arc::new(body)),
            ..Script::default()
        },
    );
}

fn spawned(result: &Value) -> TaskId {
    TaskId(result["rlm_child_id"].as_str().unwrap().parse().unwrap())
}

fn children(chat: &harness::chat::ChatHarness) -> Vec<durable::TaskRecord> {
    chat.commit(|tx| {
        Ok(tx
            .scan_tasks(
                &durable::TaskQuery {
                    kind: Some("ultron.child".into()),
                    ..durable::TaskQuery::default()
                },
                100,
                None,
            )?
            .items)
    })
    .unwrap()
}

#[test]
fn harness_chat_children_spawn_a_child_owned_by_the_cell_and_collect_its_answer_in_the_host() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, Child::Answer("child says hi"));
        let calls: Arc<Mutex<Vec<CellCall>>> = Arc::default();
        let seen = calls.clone();
        delegate(&setup, move |ops, call, signal| {
            seen.lock().unwrap().push(call.clone());
            let handle = op(
                ops,
                "rlm.spawn",
                json!({ "prompt": "brief", "kwargs": { "name": "c" } }),
                call,
                signal,
            )?;
            let collected = op(
                ops,
                "rlm.collect",
                json!({ "selectors": [handle["rlm_child_id"]] }),
                call,
                signal,
            )?;
            Ok(CellResult::text(collected.to_string()))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let settled = root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);

        let child = children(&chat).remove(0);
        let cell = calls.lock().unwrap()[0].task_id;
        assert_eq!(child.owner, Some(cell));
        assert_eq!(child.conversation_id, root.id);
        assert_matches(
            &js(&child.state),
            json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "result": { "status": "succeeded", "value": "child says hi", "verification": "unverified" } } } }),
        );
        // The child's conversation is owned by the child task and holds its own transcript.
        let conversation =
            durable::ConversationId(js(&child.state)["outcome"]["result"]["conversation"].as_u64().unwrap());
        let record = chat.commit(|tx| Ok(tx.conversation(conversation)?)).unwrap().unwrap();
        assert_eq!(record.owner.map(|owner| owner.task_id), Some(child.id));
        let child_conversation = chat.conversation(conversation).unwrap().unwrap();
        assert_eq!(
            transcript(&child_conversation.entries().unwrap()),
            ["pi.user:brief", "pi.assistant:child says hi"]
        );
        // The cell's result is the collect result the REPL saw.
        let result = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "pi.tool-result")
            .unwrap();
        let text = result.model.unwrap()[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let collected: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            collected,
            json!({ "results": [{ "id": child.id.0.to_string(), "result": { "status": "succeeded", "value": "child says hi", "verification": "unverified" }, "conversation": conversation.0 }] })
        );
        assert_eq!(
            transcript(&root.entries().unwrap()),
            ["pi.user:go", "pi.assistant", "pi.tool-result", "pi.assistant:done"]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_children_hold_the_spawning_cell_completing_until_an_uncollected_child_ends() {
    each_backend(|backend| {
        let setup = Setup::new();
        let release = Cancel::new();
        route(&setup, Child::Gated(release.clone(), "late answer"));
        let spawned_child: Arc<Mutex<Option<TaskId>>> = Arc::default();
        let sink = spawned_child.clone();
        delegate(&setup, move |ops, call, signal| {
            let handle = op(
                ops,
                "rlm.spawn",
                json!({ "prompt": "brief", "kwargs": { "name": "c" } }),
                call,
                signal,
            )?;
            *sink.lock().unwrap() = Some(spawned(&handle));
            // Never waits: the child is still running.
            let results = op(ops, "rlm.results", json!({}), call, signal)?;
            Ok(CellResult::text(results.to_string()))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("go")).unwrap();
        eventually(|| live(&chat, root.id)["tools"][0]["status"] == "done");
        let cell = TaskId(live(&chat, root.id)["tools"][0]["taskId"].as_u64().unwrap());
        // The cell's result is written, but it holds its outcome while its child runs; the run waits for it.
        eventually(|| chat.get_task(cell).unwrap().unwrap().state.status() == TaskStatus::Completing);
        let child = spawned_child.lock().unwrap().unwrap();
        let entries = root.entries().unwrap();
        let result = entries.iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
        assert_eq!(
            result.model.as_ref().unwrap()[0]["content"][0]["text"],
            json!(json!({ "results": [], "running": [child.0.to_string()] }).to_string())
        );
        assert_eq!(submission.status().unwrap().status, SubmissionStatus::Placed);
        release.abort();
        assert_eq!(submission.wait(&bg()).unwrap().status, SubmissionStatus::Done);
        assert_eq!(
            chat.get_task(cell).unwrap().unwrap().state.status(),
            TaskStatus::Terminal
        );
        assert_eq!(
            chat.get_task(child).unwrap().unwrap().state.status(),
            TaskStatus::Terminal
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_children_time_out_on_a_durable_deadline_that_survives_reopen() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(1_000);
        route(&setup, Child::Hang);
        delegate(&setup, |ops, call, signal| {
            let handle = op(
                ops,
                "rlm.spawn",
                json!({ "prompt": "brief", "kwargs": { "name": "slow", "timeout_ms": 60_000 } }),
                call,
                signal,
            )?;
            Ok(CellResult::text(handle["rlm_child_id"].as_str().unwrap().to_owned()))
        });
        let (chat, root) = open_chat(storage.clone(), &setup);
        let submission = root.submit(SubmissionDraft::input("go")).unwrap();
        let mut child = None;
        eventually(|| {
            child = children(&chat).first().cloned();
            child
                .as_ref()
                .is_some_and(|child| js(&child.state)["checkpoint"]["phase"] == "wait")
        });
        let child = child.unwrap();
        assert_matches(
            &js(&child.state),
            json!({ "checkpoint": { "phase": "wait", "until": 61_000 } }),
        );
        chat.close().unwrap();

        // Reopened past the deadline: the child ends `failed` with a cancelled result, and its conversation's run is
        // aborted with it.
        setup.fix_clock(61_000);
        let (chat, _) = open_chat(storage.reopen(), &setup);
        let settled = chat.submission(submission.id).unwrap().unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let ended = chat.wait_for_task(child.id, &bg()).unwrap();
        assert_matches(
            &js(&ended.state),
            json!({
                "status": "terminal",
                "outcome": {
                    "status": "failed",
                    "error": { "message": "Subagent slow timed out after 60000 ms", "detail": { "reason": "timeout" } },
                    "result": { "result": { "status": "cancelled", "error": "Subagent slow timed out after 60000 ms" } },
                },
            }),
        );
        let conversation =
            durable::ConversationId(js(&ended.state)["outcome"]["result"]["conversation"].as_u64().unwrap());
        chat.harness()
            .conversation(conversation)
            .unwrap()
            .unwrap()
            .wait_for_idle(&bg())
            .unwrap();
        let brief = chat
            .commit(|tx| {
                Ok(tx
                    .committed_scan_submissions(
                        &durable::SubmissionQuery {
                            conversation_id: Some(conversation),
                            ..durable::SubmissionQuery::default()
                        },
                        10,
                        None,
                    )?
                    .items)
            })
            .unwrap();
        assert_matches(&js(&brief[0]), json!({ "status": "unanswered", "reason": "aborted" }));
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_children_cancel_a_child_and_report_it_cancelled() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, Child::Hang);
        delegate(&setup, |ops, call, signal| {
            let handle = op(
                ops,
                "rlm.spawn",
                json!({ "prompt": "brief", "kwargs": { "name": "c" } }),
                call,
                signal,
            )?;
            let id = handle["rlm_child_id"].clone();
            let cancelled = op(ops, "rlm.cancel", json!({ "selector": id }), call, signal)?;
            let collected = op(ops, "rlm.collect", json!({ "selectors": [id] }), call, signal)?;
            Ok(CellResult::text(
                json!({ "cancelled": cancelled, "collected": collected }).to_string(),
            ))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        assert_eq!(
            root.submit(SubmissionDraft::input("go"))
                .unwrap()
                .wait(&bg())
                .unwrap()
                .status,
            SubmissionStatus::Done
        );
        let result = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "pi.tool-result")
            .unwrap();
        let text: Value =
            serde_json::from_str(result.model.unwrap()[0]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["cancelled"], json!({ "cancelled": true }));
        assert_matches(
            &text["collected"]["results"][0],
            json!({ "result": { "status": "cancelled", "error": "Cancelled" } }),
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_children_are_aborted_with_the_run_that_spawned_them() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, Child::Hang);
        let spawned_child: Arc<Mutex<Option<TaskId>>> = Arc::default();
        let sink = spawned_child.clone();
        delegate(&setup, move |ops, call, signal| {
            let handle = op(
                ops,
                "rlm.spawn",
                json!({ "prompt": "brief", "kwargs": { "name": "c" } }),
                call,
                signal,
            )?;
            *sink.lock().unwrap() = Some(spawned(&handle));
            Ok(CellResult::text("spawned"))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("go")).unwrap();
        eventually(|| spawned_child.lock().unwrap().is_some());
        let child = spawned_child.lock().unwrap().unwrap();
        eventually(|| js(&chat.get_task(child).unwrap().unwrap().state)["checkpoint"]["phase"] == "wait");
        root.abort(false, &bg()).unwrap();
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        assert_matches(
            &js(&chat.wait_for_task(child, &bg()).unwrap().state),
            json!({ "status": "terminal", "outcome": { "status": "aborted" } }),
        );
        chat.wait_for_idle(&bg()).unwrap();
        assert!(chat.inspect().unwrap().tasks.is_empty());
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_children_reject_bad_spawns_and_unknown_children() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, Child::Answer("x"));
        let errors: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = errors.clone();
        delegate(&setup, move |ops, call, signal| {
            for (kind, payload) in [
                ("rlm.spawn", json!({ "prompt": " ", "kwargs": { "name": "c" } })),
                ("rlm.spawn", json!({ "prompt": "brief", "kwargs": {} })),
                (
                    "rlm.spawn",
                    json!({ "prompt": "brief", "kwargs": { "name": "c", "model": "faux" } }),
                ),
                ("rlm.collect", json!({ "selectors": ["999999"] })),
                ("rlm.cancel", json!({ "selector": "nope" })),
            ] {
                sink.lock()
                    .unwrap()
                    .push(op(ops, kind, payload, call, signal).unwrap_err());
            }
            Ok(CellResult::text("ok"))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        assert_eq!(
            *errors.lock().unwrap(),
            [
                "rlm.spawn prompt must be a non-empty string",
                "rlm.spawn name must be a non-empty string",
                "rlm.spawn model must be provider/model, got faux",
                "Unknown subagent 999999",
                "Unknown subagent nope",
            ]
        );
        assert!(children(&chat).is_empty());
        chat.close().unwrap();
    });
}
