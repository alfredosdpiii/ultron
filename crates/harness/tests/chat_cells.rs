//! The durable REPL cell (`pi.tool` for the model's one tool): port of upstream Pi Durable
//! `test/harness-tools-recovery.test.ts` and the resolution cases of `test/harness-tools.test.ts` (spec §8.4). A
//! cell's intent (its code, lane and replay policy) is committed before it runs; a cell interrupted after intent is
//! never run again unless both the recorded and the current policy are replay-safe, and the model gets an
//! `interrupted` result with the committed partial output instead. Not ported: environment/cwd reruns, `beforeTool`
//! hooks, and the `afterTools` hook rerun case (no tool hooks in the RLM runtime).

mod chat_support;
mod support;

use chat_support::*;
use durable::{EntryRecord, SubmissionStatus, TaskId};
use harness::chat::faux::{message, rlm_call, text_message, tool_call};
use harness::chat::{Replay, SubmissionDraft};
use serde_json::{Value, json};
use support::*;

const DONE: &str = "done";

fn results(entries: &[EntryRecord]) -> Vec<Value> {
    entries
        .iter()
        .filter(|entry| entry.kind == "pi.tool-result")
        .map(|entry| entry.model.as_ref().unwrap()[0].clone())
        .collect()
}

/// Text items of a result joined with `|` (upstream `text()`).
fn text(result: &Value) -> String {
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["text"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("|")
}

fn cell_task(chat: &harness::chat::ChatHarness, root: &harness::chat::Conversation) -> TaskId {
    let mut id = None;
    eventually(|| {
        id = live(chat, root.id)["tools"][0]["taskId"].as_u64();
        id.is_some()
    });
    TaskId(id.unwrap())
}

/// Wait until the blocking cell's first output is durable in its slot.
fn started(chat: &harness::chat::ChatHarness, root: &harness::chat::Conversation) {
    eventually(|| live(chat, root.id)["tools"][0]["details"] == json!({ "run": 1 }));
}

fn blocking(setup: &Setup, code: &str) {
    setup.cells.script(
        code,
        Script {
            blocking: 1,
            ..Script::default()
        },
    );
}

#[test]
fn harness_chat_cells_answer_an_unsafe_cell_interrupted_after_intent_with_its_durable_partial_output() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        blocking(&setup, "work");
        setup
            .faux
            .set_responses(vec![rlm_call("work", "c1").into(), text_message(DONE).into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let id = root.submit(SubmissionDraft::input("go")).unwrap().id;
        started(&chat, &root);
        let task = cell_task(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_matches(
            &js(&chat.get_task(task).unwrap().unwrap().state),
            json!({ "checkpoint": { "phase": "execute", "arguments": { "code": "work" }, "replay": "unsafe" } }),
        );
        assert_eq!(
            chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
            SubmissionStatus::Done
        );
        assert_eq!(setup.cells.runs(), ["work"]);
        let result = &results(&root.entries().unwrap())[0];
        assert_matches(result, json!({ "isError": true, "details": { "run": 1 } }));
        assert_eq!(
            text(result),
            "run 1\n|<harness>\n[error] Tool rlm was interrupted and may have partially run\n</harness>"
        );
        assert_eq!(live(&chat, root.id), json!({}));
        let outcome = js(&chat.get_task(task).unwrap().unwrap().state)["outcome"].clone();
        assert_matches(
            &outcome,
            json!({ "status": "failed", "error": { "message": "Tool rlm was interrupted and may have partially run" } }),
        );
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_cells_rerun_a_cell_only_when_both_the_stored_and_the_current_replay_policy_are_safe() {
    let cases = [
        (Replay::Safe, Replay::Safe, true),
        (Replay::Safe, Replay::Unsafe, false),
        (Replay::Unsafe, Replay::Safe, false),
    ];
    {
        let backend = Backend::Replay;
        for (stored, current, reruns) in cases {
            let storage = Controlled::new(backend);
            let setup = Setup::new();
            blocking(&setup, "work");
            setup.cells.set_replay("work", stored);
            setup
                .faux
                .set_responses(vec![rlm_call("work", "c1").into(), text_message(DONE).into()]);
            let (chat, root) = open_chat(storage.clone(), &setup);
            let id = root.submit(SubmissionDraft::input("go")).unwrap().id;
            started(&chat, &root);
            chat.close().unwrap();

            setup.cells.set_replay("work", current);
            let (chat, root) = open_chat(storage.reopen(), &setup);
            assert_eq!(
                chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
                SubmissionStatus::Done
            );
            let result = &results(&root.entries().unwrap())[0];
            assert_eq!(setup.cells.runs().len(), if reruns { 2 } else { 1 });
            assert_eq!(result["isError"], json!(!reruns));
            if reruns {
                assert_eq!(text(result), "run 2\n");
            }
            chat.close().unwrap();
        }
    }
}

#[test]
fn harness_chat_cells_answer_an_aborted_cell_with_its_partial_output_and_continue_the_run() {
    each_backend(|backend| {
        let setup = Setup::new();
        blocking(&setup, "work");
        setup
            .faux
            .set_responses(vec![rlm_call("work", "c1").into(), text_message(DONE).into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("go")).unwrap();
        started(&chat, &root);
        let task = cell_task(&chat, &root);
        assert_eq!(
            chat.abort_task(task, &bg()).unwrap(),
            harness::scheduler::AbortResult::Marked
        );
        let outcome = js(&chat.wait_for_task(task, &bg()).unwrap().state)["outcome"].clone();
        assert_eq!(outcome["status"], "aborted");
        assert!(outcome["result"]["entryId"].is_number());
        assert_eq!(submission.wait(&bg()).unwrap().status, SubmissionStatus::Done);
        let result = &results(&root.entries().unwrap())[0];
        assert_eq!(
            text(result),
            "run 1\n|<harness>\n[error] Tool rlm was aborted\n</harness>"
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_cells_let_context_derivation_answer_a_faulted_cell_and_continue_the_run() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup.cells.script(
            "bad",
            Script {
                panic: true,
                ..Script::default()
            },
        );
        let requests: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let sink = requests.clone();
        setup.faux.set_responses(vec![
            rlm_call("bad", "c1").into(),
            harness::chat::faux::FauxStep::factory(move |context, _, _| {
                let result = context
                    .messages
                    .iter()
                    .find(|message| matches!(message, ai::Message::ToolResult(_)))
                    .map(|message| text(&js(message)))
                    .unwrap_or_else(|| "none".into());
                sink.lock().unwrap().push(result);
                Ok(text_message(DONE))
            }),
        ]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let settled = root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert!(results(&root.entries().unwrap()).is_empty());
        assert_eq!(
            *requests.lock().unwrap(),
            ["Tool result unavailable: history ends before this call completed."]
        );
        let tasks = chat
            .commit(|tx| {
                Ok(tx
                    .scan_tasks(
                        &durable::TaskQuery {
                            conversation_id: Some(root.id),
                            ..durable::TaskQuery::default()
                        },
                        20,
                        None,
                    )?
                    .items)
            })
            .unwrap();
        let faulted = tasks.iter().find(|task| task.kind == "pi.tool").unwrap();
        assert_matches(
            &js(&faulted.state),
            json!({ "status": "terminal", "outcome": { "status": "faulted" } }),
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_cells_clear_the_interrupted_attempts_progress_before_a_safe_rerun() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.cells.script(
            "work",
            Script {
                blocking: 2,
                ..Script::default()
            },
        );
        setup.cells.set_replay("work", Replay::Safe);
        setup
            .faux
            .set_responses(vec![rlm_call("work", "c1").into(), text_message(DONE).into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        root.submit(SubmissionDraft::input("go")).unwrap();
        started(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        chat.resume().unwrap();
        eventually(|| live(&chat, root.id)["tools"][0]["details"] == json!({ "run": 2 }));
        let slot = live(&chat, root.id)["tools"][0].clone();
        assert_eq!(slot["output"], "run 2\n");
        assert_eq!(slot["status"], "running");
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_cells_answer_a_call_of_another_tool_as_unavailable_and_invalid_arguments_as_invalid() {
    each_backend(|backend| {
        let setup = Setup::new();
        let calls = message(
            vec![
                tool_call("bash", json!({ "command": "ls" }), "c1"),
                tool_call("rlm", json!({ "source": "print(1)" }), "c2"),
                tool_call("rlm", json!({ "code": "ok" }), "c3"),
            ],
            ai::StopReason::ToolUse,
        );
        setup.faux.set_responses(vec![calls.into(), text_message(DONE).into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let settled = root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let results = results(&root.entries().unwrap());
        let by_call = |id: &str| {
            results
                .iter()
                .find(|result| result["toolCallId"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(
            text(&by_call("c1")),
            "<harness>\n[error] Tool bash is not available\n</harness>"
        );
        assert_eq!(by_call("c1")["isError"], true);
        assert_eq!(
            text(&by_call("c2")),
            "<harness>\n[error] rlm needs a `code` string\n</harness>"
        );
        assert_eq!(text(&by_call("c3")), "ran ok");
        assert_eq!(by_call("c3")["isError"], false);
        // Only the valid call ran.
        assert_eq!(setup.cells.runs(), ["ok"]);
        chat.close().unwrap();
    });
}
