//! Port of upstream Pi Durable `test/harness-inbox.test.ts` ("inbox" and "usage", spec §6, §8.6): queued
//! submissions in `pi.inbox`, writes before user items at boundaries, steer/follow-up modes, resets, stale head
//! writes, tool-round controls, withdrawal, failed runs leaving queued items, reopen, positional Chord operations,
//! and the usage ledger. The model's tool is the REPL (`rlm`); upstream's `hold` tool is a scripted cell whose code is
//! `hold`. Cases run against Memory, SQLite and JSONL storage unless they need one backend.

mod chat_support;
mod support;

use std::sync::{Arc, Mutex};

use ai::{Message, StopReason};
use chat_support::*;
use durable::{
    CommitChange, DocDefinition, DocSemantics, DocumentContent, EntryDraft, Owner, StorageWrite, SubmissionStatus,
    TaskId,
};
use harness::chat::docs::{INBOX, USAGE};
use harness::chat::faux::{FauxOptions, FauxStep, error_message, gated, message, rlm_call, text_message, tool_call};
use harness::chat::usage::{Bucket, record_usage};
use harness::chat::{CellResult, Control, QueueMode, SubmissionAbort, SubmissionDraft, WhenBusy};
use serde_json::{Value, json};
use support::*;

fn answer(text: &str) -> FauxStep {
    text_message(text).into()
}

fn hold() -> FauxStep {
    rlm_call("hold", "c1").into()
}

/// Register a `hold` cell that waits for `gate` and then returns `result`.
fn hold_cell(setup: &Setup, gate: &harness::scheduler::Cancel, result: CellResult) {
    setup.cells.hold("hold", gate, result);
}

fn empty() -> CellResult {
    CellResult {
        output: Some(Vec::new()),
        ..CellResult::default()
    }
}

fn open_gate() -> harness::scheduler::Cancel {
    let gate = harness::scheduler::Cancel::new();
    gate.abort();
    gate
}

fn ids(values: &[&harness::chat::Submission]) -> Vec<u64> {
    values.iter().map(|submission| submission.id.0).collect()
}

fn queued(chat: &harness::chat::ChatHarness, root: &harness::chat::Conversation) -> Vec<(u64, String)> {
    inbox(chat, root)
}

#[test]
fn harness_chat_inbox_queues_busy_submissions_and_places_writes_before_user_items_at_the_final_boundary_one_follow_up_per_run()
 {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second"), answer("third")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f1 = root.submit(SubmissionDraft::input("f1")).unwrap();
        let write = root
            .submit(SubmissionDraft::write(json!({ "kind": "note", "data": "w" })))
            .unwrap();
        let f2 = root
            .submit(SubmissionDraft::input("f2").when_busy(WhenBusy::FollowUp))
            .unwrap();
        for submission in [&f1, &write, &f2] {
            assert_eq!(status(submission)["status"], "queued");
        }
        assert_eq!(
            queued(&chat, &root),
            vec![
                (f1.id.0, "followUp".to_owned()),
                (write.id.0, "write".to_owned()),
                (f2.id.0, "followUp".to_owned()),
            ]
        );
        assert_eq!(transcript(&root.entries().unwrap()), ["pi.user:a"]);

        release.abort();
        f2.wait(&bg()).unwrap();
        assert_eq!(
            transcript(&root.entries().unwrap()),
            [
                "pi.user:a",
                "pi.assistant:first",
                "note",
                "pi.user:f1",
                "pi.assistant:second",
                "pi.user:f2",
                "pi.assistant:third",
            ]
        );
        let answers = assistants(&root);
        assert_matches(&status(&input), json!({ "status": "done", "answer": answers[0].id.0 }));
        assert_matches(&status(&write), json!({ "status": "done" }));
        assert_matches(&status(&f1), json!({ "status": "done", "answer": answers[1].id.0 }));
        assert_matches(&status(&f2), json!({ "status": "done", "answer": answers[2].id.0 }));
        assert!(queued(&chat, &root).is_empty());
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_places_every_follow_up_in_one_successor_run_with_follow_up_mode_all() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("both")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        setup.settings(|settings| settings.follow_up_mode = QueueMode::All);
        root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f1 = root.submit(SubmissionDraft::input("f1")).unwrap();
        let f2 = root.submit(SubmissionDraft::input("f2")).unwrap();
        release.abort();
        let settled = f2.wait(&bg()).unwrap();
        let mut expected = record(&settled);
        expected["id"] = json!(f1.id.0);
        expected["entry"] = status(&f1)["entry"].clone();
        assert_eq!(status(&f1), expected);
        assert_eq!(
            transcript(&root.entries().unwrap()),
            [
                "pi.user:a",
                "pi.assistant:first",
                "pi.user:f1",
                "pi.user:f2",
                "pi.assistant:both"
            ]
        );
        assert_eq!(setup.faux.call_count(), 2);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_reads_queue_modes_when_the_final_boundarys_commit_runs_on_the_session_line() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("both")]);
        let yielded = Deferred::new();
        let signal = yielded.clone();
        setup.hooks.on_yield = Some(Arc::new(move |_| {
            signal.resolve();
            None
        }));
        let storage = Controlled::new(backend);
        let (chat, root) = open_chat(storage.clone(), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f1 = root.submit(SubmissionDraft::input("f1")).unwrap();
        let f2 = root.submit(SubmissionDraft::input("f2")).unwrap();
        // Occupy the line, let the answer queue its boundary commit behind it, then change the mode.
        let held = storage.hold_commits();
        let marker = DocDefinition::new("test.marker", 1, DocSemantics::Session, |_| json!({ "n": 0 }))
            .define()
            .unwrap();
        let occupying = {
            let root = root.clone();
            spawn(move || {
                root.commit(|tx| {
                    let doc = tx.doc(&marker, Owner::Session)?;
                    doc.set(&[chord_delta::Seg::from("n")], json!(1))
                        .map_err(|error| harness::scheduler::HarnessError::msg(error.to_string()))?;
                    Ok(())
                })
            })
        };
        held.wait_entered();
        release.abort();
        yielded.wait();
        // The boundary commit has queued behind the held one.
        line_reaches(chat.harness(), 2);
        setup.settings(|settings| settings.follow_up_mode = QueueMode::All);
        held.release();
        occupying.join().unwrap().unwrap();
        let settled = f2.wait(&bg()).unwrap();
        let mut expected = record(&settled);
        expected["id"] = json!(f1.id.0);
        expected["entry"] = status(&f1)["entry"].clone();
        assert_eq!(status(&f1), expected);
        assert_eq!(setup.faux.call_count(), 2);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_adds_steers_to_the_run_at_the_post_tools_boundary_and_holds_follow_ups_for_the_final_boundary() {
    each_backend(|backend| {
        let setup = Setup::new();
        let gate = harness::scheduler::Cancel::new();
        hold_cell(&setup, &gate, empty());
        setup
            .faux
            .set_responses(vec![hold(), answer("after tools"), answer("follow-up")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        tool_running(&chat, &root);
        let steer = root
            .submit(SubmissionDraft::input("s").when_busy(WhenBusy::Steer))
            .unwrap();
        let follow_up = root.submit(SubmissionDraft::input("f")).unwrap();
        gate.abort();
        follow_up.wait(&bg()).unwrap();
        assert_eq!(
            transcript(&root.entries().unwrap()),
            [
                "pi.user:a",
                "pi.assistant",
                "pi.tool-result",
                "pi.user:s",
                "pi.assistant:after tools",
                "pi.user:f",
                "pi.assistant:follow-up",
            ]
        );
        let answers = assistants(&root);
        assert_matches(&status(&input), json!({ "status": "done", "answer": answers[1].id.0 }));
        assert_matches(&status(&steer), json!({ "status": "done", "answer": answers[1].id.0 }));
        assert_matches(
            &status(&follow_up),
            json!({ "status": "done", "answer": answers[2].id.0 }),
        );
        chat.close().unwrap();
    });
}

fn describe(message: &Message) -> String {
    let role = match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    };
    format!("{role}:{}", text_of(message).unwrap_or_default())
}

#[test]
fn harness_chat_inbox_ends_the_run_at_a_queued_reset_after_tools_and_runs_earlier_follow_ups_in_the_new_context() {
    each_backend(|backend| {
        let setup = Setup::new();
        let gate = harness::scheduler::Cancel::new();
        hold_cell(&setup, &gate, empty());
        let requests: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let recorded = requests.clone();
        let record_step = FauxStep::factory(move |context, _, _| {
            recorded
                .lock()
                .unwrap()
                .push(context.messages.iter().map(describe).collect());
            Ok(text_message("fresh"))
        });
        setup.faux.set_responses(vec![hold(), record_step]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        tool_running(&chat, &root);
        let follow_up = root.submit(SubmissionDraft::input("f")).unwrap();
        root.reset(None).unwrap();
        gate.abort();
        follow_up.wait(&bg()).unwrap();
        assert_matches(&status(&input), json!({ "status": "unanswered", "reason": "reset" }));
        assert_eq!(
            transcript(&root.entries().unwrap()),
            [
                "pi.user:a",
                "pi.assistant",
                "pi.tool-result",
                "pi.reset",
                "pi.user:f",
                "pi.assistant:fresh"
            ]
        );
        let reset = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "pi.reset")
            .unwrap();
        assert_eq!(reset.head, Some(reset.id));
        // The follow-up's request starts at the reset: the complete system baseline after the cut leads the follow-up.
        assert_eq!(
            *requests.lock().unwrap(),
            vec![vec!["system:".to_owned(), "user:f".to_owned()]]
        );
        assert_eq!(setup.faux.call_count(), 2);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_places_a_queued_reset_after_the_answer_at_the_final_boundary() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        root.reset(Some("handoff")).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        chat.wait_for_idle(&bg()).unwrap();
        assert_matches(&status(&input), json!({ "status": "done" }));
        assert_eq!(
            transcript(&root.entries().unwrap()),
            ["pi.user:a", "pi.assistant:first", "pi.reset:handoff"]
        );
        let view = root.context(None).unwrap();
        assert_eq!(view.messages.len(), 1);
        assert_matches(&js(&view.messages[0]), json!({ "role": "user", "content": "handoff" }));
        assert!(js(&view.messages[0])["timestamp"].is_number());
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_resets_an_idle_conversation_at_once_with_or_without_handoff_text() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup.fix_clock(7);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.append(EntryDraft::new("note")).unwrap();
        root.reset(None).unwrap();
        let view = root.context(None).unwrap();
        let head = view.head.clone().unwrap();
        assert_eq!(head.kind, "pi.reset");
        assert!(head.model.is_none());
        assert!(view.messages.is_empty());
        root.reset(Some("carry on")).unwrap();
        let view = root.context(None).unwrap();
        let head = view.head.clone().unwrap();
        assert_eq!(head.head, Some(head.id));
        assert_eq!(
            js(&view.messages),
            json!([{ "role": "user", "content": "carry on", "timestamp": 7 }])
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_makes_a_queued_head_write_stale_when_it_targets_an_entry_before_the_active_range() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let old = root.append(EntryDraft::new("note")).unwrap();
        root.reset(None).unwrap();
        let reset = root.context(None).unwrap().head.unwrap();
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let stale = root
            .submit(SubmissionDraft::write(json!({ "kind": "summary", "head": old.id.0 })))
            .unwrap();
        let fresh = root
            .submit(SubmissionDraft::write(json!({ "kind": "summary", "head": reset.id.0 })))
            .unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        assert_matches(&status(&stale), json!({ "status": "unanswered", "reason": "stale" }));
        assert_matches(&status(&fresh), json!({ "status": "done" }));
        assert!(queued(&chat, &root).is_empty());

        // The fresh summary's marker starts the range at the reset: a target inside the range is not stale, even when
        // it is older than the marker itself. A reset placed earlier in the same boundary makes it stale.
        let inside = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "pi.user")
            .unwrap();
        let second = root.submit(SubmissionDraft::input("b")).unwrap();
        let kept = root
            .submit(SubmissionDraft::write(
                json!({ "kind": "summary", "head": inside.id.0 }),
            ))
            .unwrap();
        second.wait(&bg()).unwrap();
        assert_matches(&status(&kept), json!({ "status": "done" }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_makes_a_head_write_stale_behind_a_reset_placed_earlier_in_the_same_boundary() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let target = root.entries().unwrap()[0].clone();
        root.reset(None).unwrap();
        let summary = root
            .submit(SubmissionDraft::write(
                json!({ "kind": "summary", "head": target.id.0 }),
            ))
            .unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        assert_matches(&status(&summary), json!({ "status": "unanswered", "reason": "stale" }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_ends_the_run_with_a_pi_reset_entry_when_a_cell_requests_a_handoff() {
    each_backend(|backend| {
        let setup = Setup::new();
        hold_cell(
            &setup,
            &open_gate(),
            controlled(Control {
                handoff: Some("continue here".into()),
                terminate: None,
            }),
        );
        setup.faux.set_responses(vec![hold()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let settled = root.submit(SubmissionDraft::input("a")).unwrap().wait(&bg()).unwrap();
        let entries = root.entries().unwrap();
        let calling = entries.iter().find(|entry| entry.kind == "pi.assistant").unwrap();
        assert_matches(&record(&settled), json!({ "status": "done", "answer": calling.id.0 }));
        assert_eq!(
            transcript(&entries),
            ["pi.user:a", "pi.assistant", "pi.tool-result", "pi.reset:continue here"]
        );
        let last = entries.last().unwrap();
        assert_eq!(last.head, Some(last.id));
        assert_eq!(setup.faux.call_count(), 1);
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

fn yield_once(setup: &mut Setup, always: bool) {
    let yields = Arc::new(Mutex::new(0usize));
    setup.hooks.on_yield = Some(Arc::new(move |_| {
        let mut count = yields.lock().unwrap();
        *count += 1;
        (always || *count == 1).then(|| json!("more"))
    }));
}

#[test]
fn harness_chat_inbox_drops_an_on_yield_continuation_when_the_final_boundary_selects_a_follow_up() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second")]);
        yield_once(&mut setup, false);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let follow_up = root.submit(SubmissionDraft::input("f")).unwrap();
        release.abort();
        follow_up.wait(&bg()).unwrap();
        assert_matches(&status(&input), json!({ "status": "done" }));
        assert_eq!(
            transcript(&root.entries().unwrap()),
            ["pi.user:a", "pi.assistant:first", "pi.user:f", "pi.assistant:second"]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_withdraws_a_queued_submission_and_removes_its_item() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let kept = root.submit(SubmissionDraft::write(json!({ "kind": "note" }))).unwrap();
        let withdrawn = root.submit(SubmissionDraft::input("f")).unwrap();
        assert_eq!(withdrawn.abort().unwrap(), SubmissionAbort::Aborted);
        assert_matches(
            &record(&withdrawn.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        assert_eq!(queued(&chat, &root), vec![(kept.id.0, "write".to_owned())]);
        release.abort();
        input.wait(&bg()).unwrap();
        assert_matches(&status(&kept), json!({ "status": "done" }));
        assert_eq!(setup.faux.call_count(), 1);
        chat.close().unwrap();
    });
}

fn invalid_request() -> ai::AssistantMessage {
    error_message("invalid request")
}

#[test]
fn harness_chat_inbox_leaves_queued_items_after_a_failed_run_until_the_next_submission_places_them_in_order() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (failing, reached, release) = gated(invalid_request());
        setup
            .faux
            .set_responses(vec![failing, answer("for f"), answer("for g")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        release.abort();
        assert_matches(
            &record(&input.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "model_error" }),
        );
        chat.wait_for_idle(&bg()).unwrap();
        assert_eq!(status(&f)["status"], "queued");
        assert_eq!(queued(&chat, &root), vec![(f.id.0, "followUp".to_owned())]);

        // Idle with a queued item: the new input queues behind it, and a final boundary places the older one first.
        let g = root
            .submit(SubmissionDraft::input("g").when_busy(WhenBusy::Reject))
            .unwrap();
        g.wait(&bg()).unwrap();
        assert_matches(&status(&f), json!({ "status": "done" }));
        let entries = transcript(&root.entries().unwrap());
        assert_eq!(
            entries[entries.len() - 4..],
            ["pi.user:f", "pi.assistant:for f", "pi.user:g", "pi.assistant:for g"]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_keeps_queued_submissions_across_reopen_and_settles_them_afterwards() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        let (first, reached, _release) = gated(text_message("first"));
        setup
            .faux
            .set_responses(vec![first, answer("first again"), answer("f")]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f = root.submit(SubmissionDraft::input("f")).unwrap().id;
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        let settled = chat.submission(f).unwrap().unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let entries = transcript(&root.entries().unwrap());
        assert_eq!(entries[entries.len() - 2..], ["pi.user:f", "pi.assistant:f"]);
        chat.close().unwrap();
    }
}

/// The ops of every `pi.inbox` change from now on.
fn inbox_ops(chat: &harness::chat::ChatHarness) -> Arc<Mutex<Vec<Value>>> {
    let ops: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = ops.clone();
    chat.harness()
        .session()
        .subscribe_commits(move |publication| {
            for change in &publication.changes {
                if let CommitChange::Document { record, ops, .. } = change
                    && record.identity.kind == "pi.inbox"
                    && !ops.is_empty()
                {
                    sink.lock()
                        .unwrap()
                        .push(Value::Array(ops.iter().map(|op| op.to_json()).collect()));
                }
            }
        })
        .unwrap();
    ops
}

#[test]
fn harness_chat_inbox_commits_inbox_changes_as_positional_chord_operations_and_a_base_when_empty() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let ops = inbox_ops(&chat);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let note = || SubmissionDraft::write(json!({ "kind": "note" }));
        let w1 = root.submit(note()).unwrap();
        let f1 = root.submit(SubmissionDraft::input("f1")).unwrap();
        let w2 = root.submit(note()).unwrap();
        let f2 = root.submit(SubmissionDraft::input("f2")).unwrap();
        let w3 = root.submit(note()).unwrap();
        assert_eq!(
            *ops.lock().unwrap(),
            vec![
                json!([["p", ["items"], 0, 0, [{ "id": w1.id.0, "mode": "write", "entry": { "kind": "note" } }]]]),
                json!([["p", ["items"], 1, 0, [{ "id": f1.id.0, "mode": "followUp", "content": "f1" }]]]),
                json!([["p", ["items"], 2, 0, [{ "id": w2.id.0, "mode": "write", "entry": { "kind": "note" } }]]]),
                json!([["p", ["items"], 3, 0, [{ "id": f2.id.0, "mode": "followUp", "content": "f2" }]]]),
                json!([["p", ["items"], 4, 0, [{ "id": w3.id.0, "mode": "write", "entry": { "kind": "note" } }]]]),
            ]
        );
        release.abort();
        input.wait(&bg()).unwrap();
        eventually(|| ops.lock().unwrap().len() > 5);
        // Every write and the first follow-up leave; only f2 at index 3 remains. No retained value is carried.
        let removal = ops.lock().unwrap()[5].clone();
        let removal = removal.as_array().unwrap();
        assert!(
            removal
                .iter()
                .all(|op| op[0] == "p" && op[4].as_array().is_some_and(Vec::is_empty))
        );
        assert!(removal.contains(&json!(["p", ["items"], 4, 1, []])));
        assert!(!Value::Array(removal.clone()).to_string().contains("f2"));
        f2.wait(&bg()).unwrap();
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_settles_a_stale_write_at_once_while_idle_and_a_queued_one_behind_waiting_items() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (failing, reached, release) = gated(invalid_request());
        setup.faux.set_responses(vec![failing, answer("for f")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let old = root.append(EntryDraft::new("note")).unwrap();
        root.reset(None).unwrap();
        let before = root.entries().unwrap().len();
        let idle = root
            .submit(SubmissionDraft::write(json!({ "kind": "summary", "head": old.id.0 })))
            .unwrap();
        assert_matches(
            &record(&idle.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "stale" }),
        );
        assert_eq!(root.entries().unwrap().len(), before);

        // After a failed run, a follow-up waits; a stale write queues behind it and the boundary rejects it.
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        chat.wait_for_idle(&bg()).unwrap();
        let queued_write = root
            .submit(SubmissionDraft::write(json!({ "kind": "summary", "head": old.id.0 })))
            .unwrap();
        assert_matches(
            &record(&queued_write.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "stale" }),
        );
        assert_matches(&record(&f.wait(&bg()).unwrap()), json!({ "status": "done" }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_keeps_an_on_yield_continuation_across_a_queued_plain_write_with_the_runs_original_input() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second")]);
        yield_once(&mut setup, false);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let write = root.submit(SubmissionDraft::write(json!({ "kind": "note" }))).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        let entries = root.entries().unwrap();
        assert_eq!(
            transcript(&entries),
            [
                "pi.user:a",
                "pi.assistant:first",
                "note",
                "pi.user:more",
                "pi.assistant:second"
            ]
        );
        assert_matches(
            &status(&input),
            json!({ "status": "done", "answer": entries.last().unwrap().id.0 }),
        );
        assert_matches(&status(&write), json!({ "status": "done" }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_drops_an_on_yield_continuation_for_a_queued_reset() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        yield_once(&mut setup, true);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        root.reset(None).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        chat.wait_for_idle(&bg()).unwrap();
        assert_matches(&status(&input), json!({ "status": "done" }));
        assert_eq!(
            transcript(&root.entries().unwrap()),
            ["pi.user:a", "pi.assistant:first", "pi.reset"]
        );
        assert_eq!(setup.faux.call_count(), 1);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_adds_every_steer_to_the_run_at_the_post_tools_boundary_with_steering_mode_all() {
    each_backend(|backend| {
        let setup = Setup::new();
        let gate = harness::scheduler::Cancel::new();
        hold_cell(&setup, &gate, empty());
        setup
            .faux
            .set_responses(vec![hold(), answer("after tools"), answer("follow-up")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        setup.settings(|settings| settings.steering_mode = QueueMode::All);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        tool_running(&chat, &root);
        let s1 = root
            .submit(SubmissionDraft::input("s1").when_busy(WhenBusy::Steer))
            .unwrap();
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        let s2 = root
            .submit(SubmissionDraft::input("s2").when_busy(WhenBusy::Steer))
            .unwrap();
        gate.abort();
        f.wait(&bg()).unwrap();
        let entries = root.entries().unwrap();
        assert_eq!(
            transcript(&entries),
            [
                "pi.user:a",
                "pi.assistant",
                "pi.tool-result",
                "pi.user:s1",
                "pi.user:s2",
                "pi.assistant:after tools",
                "pi.user:f",
                "pi.assistant:follow-up",
            ]
        );
        let answers = assistants(&root);
        for submission in [&input, &s1, &s2] {
            assert_matches(
                &status(submission),
                json!({ "status": "done", "answer": answers[1].id.0 }),
            );
        }
        let _ = ids(&[&s1, &s2]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_queues_an_idle_steer_behind_waiting_items_and_places_it_with_the_first_follow_up_in_id_order() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (failing, reached, release) = gated(invalid_request());
        setup.faux.set_responses(vec![failing, answer("both")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        chat.wait_for_idle(&bg()).unwrap();
        let steer = root
            .submit(SubmissionDraft::input("s").when_busy(WhenBusy::Steer))
            .unwrap();
        let settled = steer.wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert_matches(
            &status(&f),
            json!({ "status": "done", "answer": settled.answer.unwrap().0 }),
        );
        let entries = transcript(&root.entries().unwrap());
        assert_eq!(
            entries[entries.len() - 3..],
            ["pi.user:f", "pi.user:s", "pi.assistant:both"]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_starts_a_queued_follow_up_after_a_terminating_round() {
    each_backend(|backend| {
        let setup = Setup::new();
        let gate = harness::scheduler::Cancel::new();
        hold_cell(
            &setup,
            &gate,
            controlled(Control {
                terminate: Some(true),
                handoff: None,
            }),
        );
        setup.faux.set_responses(vec![hold(), answer("follow-up")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        tool_running(&chat, &root);
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        gate.abort();
        f.wait(&bg()).unwrap();
        let calling = assistants(&root)[0].clone();
        assert_matches(&status(&input), json!({ "status": "done", "answer": calling.id.0 }));
        assert_eq!(
            transcript(&root.entries().unwrap()),
            [
                "pi.user:a",
                "pi.assistant",
                "pi.tool-result",
                "pi.user:f",
                "pi.assistant:follow-up"
            ]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_writes_the_last_handoff_in_call_order_and_then_runs_queued_follow_ups_in_the_new_context() {
    each_backend(|backend| {
        let setup = Setup::new();
        let first_gate = harness::scheduler::Cancel::new();
        let second_gate = harness::scheduler::Cancel::new();
        let handoff = |text: &str| {
            controlled(Control {
                handoff: Some(text.into()),
                terminate: None,
            })
        };
        setup.cells.hold("hold", &first_gate, handoff("one"));
        setup.cells.hold("later", &second_gate, handoff("two"));
        let round = message(
            vec![
                tool_call("rlm", json!({ "code": "hold" }), "c1"),
                tool_call("rlm", json!({ "code": "later" }), "c2"),
            ],
            StopReason::ToolUse,
        );
        setup.faux.set_responses(vec![round.into(), answer("follow-up")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        tool_running(&chat, &root);
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        second_gate.abort();
        eventually(|| live(&chat, root.id)["tools"][1]["status"] == "done");
        first_gate.abort();
        f.wait(&bg()).unwrap();
        assert_matches(&status(&input), json!({ "status": "done" }));
        let entries = transcript(&root.entries().unwrap());
        assert_eq!(
            entries[entries.len() - 4..],
            ["pi.tool-result", "pi.reset:two", "pi.user:f", "pi.assistant:follow-up"]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_leaves_the_inbox_alone_when_the_runs_task_is_aborted() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, _release) = gated(text_message("never"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let f = root.submit(SubmissionDraft::input("f")).unwrap();
        let task = TaskId(live(&chat, root.id)["run"]["taskId"].as_u64().unwrap());
        chat.abort_task(task, &bg()).unwrap();
        assert_matches(
            &record(&input.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        chat.wait_for_idle(&bg()).unwrap();
        assert_eq!(status(&f)["status"], "queued");
        assert_eq!(queued(&chat, &root), vec![(f.id.0, "followUp".to_owned())]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_returns_a_queued_submission_for_its_repeated_request_id_without_a_second_item() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, _release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let queued_input = root.submit(SubmissionDraft::input("f").request_id("r")).unwrap();
        let again = root.submit(SubmissionDraft::input("f").request_id("r")).unwrap();
        assert_eq!(again.id, queued_input.id);
        assert_eq!(queued(&chat, &root), vec![(queued_input.id.0, "followUp".to_owned())]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_withdraws_a_middle_item_with_one_positional_removal() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (first, reached, _release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        let items: Vec<_> = ["x", "y", "z"]
            .iter()
            .map(|text| root.submit(SubmissionDraft::input(*text)).unwrap())
            .collect();
        let ops = inbox_ops(&chat);
        items[1].abort().unwrap();
        assert_eq!(*ops.lock().unwrap(), vec![json!([["p", ["items"], 1, 1, []]])]);
        assert_eq!(
            queued(&chat, &root),
            vec![
                (items[0].id.0, "followUp".to_owned()),
                (items[2].id.0, "followUp".to_owned()),
            ]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_stores_the_inbox_as_a_base_exactly_when_it_becomes_empty() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        let (first, reached, release) = gated(text_message("first"));
        setup.faux.set_responses(vec![first, answer("second")]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let input = root.submit(SubmissionDraft::input("a")).unwrap();
        reached.wait();
        root.submit(SubmissionDraft::input("f1")).unwrap();
        let f2 = root.submit(SubmissionDraft::input("f2")).unwrap();
        release.abort();
        input.wait(&bg()).unwrap();
        f2.wait(&bg()).unwrap();
        let commits = storage.commits();
        let inbox_id = commits
            .iter()
            .flatten()
            .find_map(|write| match write {
                StorageWrite::DocumentCreate { record, .. } if record.kind == "pi.inbox" => Some(record.id),
                _ => None,
            })
            .unwrap();
        let written: Vec<(&str, bool)> = commits
            .iter()
            .flatten()
            .filter_map(|write| match write {
                StorageWrite::DocumentChange { id, content } if *id == inbox_id => Some(match content {
                    DocumentContent::Base { value, .. } => {
                        ("base", value["items"].as_array().is_some_and(Vec::is_empty))
                    }
                    DocumentContent::Delta { .. } => ("delta", false),
                }),
                _ => None,
            })
            .collect();
        // The f1 push, the f2 push, removing f1, and emptying the inbox; upstream's recording starts after the first.
        assert_eq!(
            written,
            vec![("delta", false), ("delta", false), ("delta", false), ("base", true)]
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_inbox_keeps_a_complete_inbox_base_exactly_while_it_is_empty_and_a_usage_base_on_every_change() {
    let info = durable::CheckpointInfo {
        deltas_since_base: 1000,
    };
    let check = |doc: &durable::Doc, value: Value| {
        let definition = doc.definition();
        (definition.checkpoint_when.as_ref().unwrap())(definition, &value, &[], info).unwrap()
    };
    assert!(check(&INBOX, json!({ "items": [] })));
    assert!(!check(
        &INBOX,
        json!({ "items": [{ "id": 1, "mode": "followUp", "content": "x" }] })
    ));
    assert!(check(&USAGE, json!({ "models": {}, "tools": {} })));
}

/// Entries in the commit and the `pi.usage` ops it wrote.
type UsageCommit = (usize, Vec<Value>);

// ─── usage ─────────────────────────────────────────────────────────────────────

fn spent(input: u64, output: u64, cache_read: u64, cache_write: u64, total: u64, cost: [f64; 5]) -> Value {
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": cache_write,
        "totalTokens": total,
        "cost": { "input": cost[0], "output": cost[1], "cacheRead": cost[2], "cacheWrite": cost[3], "total": cost[4] },
    })
}

fn assistant_messages(root: &harness::chat::Conversation) -> Vec<ai::AssistantMessage> {
    assistants(root)
        .iter()
        .filter_map(|entry| match first_message(entry) {
            Some(Message::Assistant(message)) => Some(message),
            _ => None,
        })
        .collect()
}

#[test]
fn harness_chat_usage_totals_assistant_usage_per_model_and_cell_usage_per_tool_and_sums_the_session() {
    each_backend(|backend| {
        let setup = Setup::new();
        let usage = spent(1, 2, 3, 4, 10, [0.1, 0.2, 0.3, 0.4, 1.0]);
        hold_cell(
            &setup,
            &open_gate(),
            CellResult {
                output: Some(Vec::new()),
                usage: Some(usage.clone()),
                ..CellResult::default()
            },
        );
        setup.faux.set_responses(vec![hold(), answer("done"), answer("other")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap().wait(&bg()).unwrap();
        let messages = assistant_messages(&root);
        let (input, output, total) = messages.iter().fold((0, 0, 0), |sum, message| {
            (
                sum.0 + message.usage.input,
                sum.1 + message.usage.output,
                sum.2 + message.usage.total_tokens,
            )
        });
        let ledger = chat.snapshot(&USAGE, Owner::Conversation(root.id)).unwrap().unwrap();
        assert_matches(
            &ledger["models"]["faux/faux-1"],
            json!({ "input": input, "output": output, "totalTokens": total }),
        );
        assert_eq!(ledger["tools"], json!({ "rlm": usage }));
        let result = root
            .entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == "pi.tool-result")
            .unwrap();
        assert_eq!(result.model.as_ref().unwrap()[0]["usage"], usage);

        // A fork starts at zero; the Session total adds every conversation once.
        let last = root.entries().unwrap().last().unwrap().id;
        let fork = root.fork(last, durable::ConversationOwnership::Ownerless).unwrap();
        assert_eq!(
            *chat.snapshot(&USAGE, Owner::Conversation(fork.id)).unwrap().unwrap(),
            json!({ "models": {}, "tools": {} })
        );
        fork.submit(SubmissionDraft::input("b")).unwrap().wait(&bg()).unwrap();
        let fork_usage =
            chat.snapshot(&USAGE, Owner::Conversation(fork.id)).unwrap().unwrap()["models"]["faux/faux-1"].clone();
        let session = chat.usage().unwrap();
        assert_eq!(
            session["models"]["faux/faux-1"]["output"],
            json!(output + fork_usage["output"].as_u64().unwrap())
        );
        assert_eq!(session["tools"], json!({ "rlm": usage }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_usage_counts_failed_attempts_converted_partials_and_cell_usage() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            tokens_per_second: Some(200.0),
            token_size: Some(1),
            ..FauxOptions::default()
        });
        let usage = spent(5, 0, 0, 0, 5, [0.0; 5]);
        hold_cell(
            &setup,
            &open_gate(),
            CellResult {
                output: Some(Vec::new()),
                usage: Some(usage.clone()),
                ..CellResult::default()
            },
        );
        setup.faux.set_responses(vec![
            error_message("503 Service Unavailable").into(),
            hold(),
            answer("done"),
            answer(&"x".repeat(400)),
        ]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        setup.settings(|settings| {
            settings.retry.enabled = true;
            settings.retry.max_retries = 1;
            settings.retry.base_delay_ms = 1;
        });
        root.submit(SubmissionDraft::input("a")).unwrap().wait(&bg()).unwrap();
        assert_eq!(
            chat.snapshot(&USAGE, Owner::Conversation(root.id)).unwrap().unwrap()["tools"],
            json!({ "rlm": usage })
        );

        // A partial committed while streaming becomes an aborted entry on abort; its usage counts too.
        let input = root.submit(SubmissionDraft::input("b")).unwrap();
        eventually(|| !live(&chat, root.id)["generation"]["message"].is_null());
        let task = TaskId(live(&chat, root.id)["run"]["taskId"].as_u64().unwrap());
        chat.abort_task(task, &bg()).unwrap();
        input.wait(&bg()).unwrap();
        let messages = assistant_messages(&root);
        assert_eq!(
            messages.iter().map(|message| message.stop_reason).collect::<Vec<_>>(),
            [
                StopReason::Error,
                StopReason::ToolUse,
                StopReason::Stop,
                StopReason::Aborted
            ]
        );
        let output: u64 = messages.iter().map(|message| message.usage.output).sum();
        assert_eq!(
            chat.snapshot(&USAGE, Owner::Conversation(root.id)).unwrap().unwrap()["models"]["faux/faux-1"]["output"],
            json!(output)
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_usage_keeps_tools_named_like_object_prototype_keys_in_the_ledger_and_the_session_total() {
    each_backend(|backend| {
        let (chat, root) = open_chat(Controlled::new(backend), &Setup::new());
        let usage = spent(1, 1, 0, 0, 2, [0.0; 5]);
        let names = ["constructor", "__proto__", "toString"];
        for name in names {
            for _ in 0..2 {
                root.commit(|tx| record_usage(tx, root.id, Bucket::Tools, name, &usage))
                    .unwrap();
            }
        }
        let total = chat.usage().unwrap();
        let keys: Vec<&str> = total["tools"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, names);
        for name in names {
            assert_eq!(total["tools"][name]["output"], json!(2));
        }
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_usage_keeps_usage_totals_exact_across_reopen_counting_a_partial_converted_after_reopen_once() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::with(FauxOptions {
            tokens_per_second: Some(200.0),
            token_size: Some(1),
            ..FauxOptions::default()
        });
        setup
            .faux
            .set_responses(vec![answer("first"), answer(&"x".repeat(400)), answer("again")]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        root.submit(SubmissionDraft::input("a")).unwrap().wait(&bg()).unwrap();
        root.submit(SubmissionDraft::input("b")).unwrap();
        eventually(|| !live(&chat, root.id)["generation"]["message"].is_null());
        // Closing mid-stream keeps the committed partial; the reopened request converts it into an aborted entry.
        chat.close().unwrap();
        let (chat, root) = open_chat(storage.reopen(), &setup);
        chat.resume().unwrap();
        chat.wait_for_idle(&bg()).unwrap();
        let messages = assistant_messages(&root);
        assert_eq!(
            messages.iter().map(|message| message.stop_reason).collect::<Vec<_>>(),
            [StopReason::Stop, StopReason::Aborted, StopReason::Stop]
        );
        let total = chat.usage().unwrap()["models"]["faux/faux-1"].clone();
        let sum = |field: fn(&ai::Usage) -> u64| messages.iter().map(|message| field(&message.usage)).sum::<u64>();
        assert_eq!(
            [
                total["input"].clone(),
                total["output"].clone(),
                total["totalTokens"].clone()
            ],
            [
                json!(sum(|usage| usage.input)),
                json!(sum(|usage| usage.output)),
                json!(sum(|usage| usage.total_tokens)),
            ]
        );
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_usage_records_usage_as_numeric_sets_on_the_ledger_in_the_entrys_commit() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup.faux.set_responses(vec![answer("one"), answer("two")]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let commits: Arc<Mutex<Vec<UsageCommit>>> = Arc::default();
        let sink = commits.clone();
        chat.harness()
            .session()
            .subscribe_commits(move |publication| {
                for change in &publication.changes {
                    let CommitChange::Document { record, ops, .. } = change else {
                        continue;
                    };
                    if record.identity.kind != "pi.usage" {
                        continue;
                    }
                    let entries = publication
                        .changes
                        .iter()
                        .filter(|other| matches!(other, CommitChange::Table(StorageWrite::Entry { .. })))
                        .count();
                    sink.lock()
                        .unwrap()
                        .push((entries, ops.iter().map(|op| op.to_json()).collect()));
                }
            })
            .unwrap();
        root.submit(SubmissionDraft::input("a")).unwrap().wait(&bg()).unwrap();
        root.submit(SubmissionDraft::input("b")).unwrap().wait(&bg()).unwrap();
        // A waiter can wake before this later-registered listener has seen the settling commit.
        eventually(|| commits.lock().unwrap().len() >= 2);
        let commits = commits.lock().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].1.len(), 1);
        assert_eq!(commits[0].1[0][0], "s");
        assert_eq!(commits[0].1[0][1], json!(["models", "faux/faux-1"]));
        assert!(commits[0].1[0][2].is_object());
        assert!(commits[1].1.iter().all(|op| op[0] == "s" && op[1][1] == "faux/faux-1"));
        assert!(commits.iter().all(|(entries, _)| *entries >= 1));
        chat.close().unwrap();
    });
}
