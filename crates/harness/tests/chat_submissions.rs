//! Port of upstream Pi Durable `test/harness-submissions.test.ts` (spec §6): admission, request-ID deduplication,
//! `whenBusy: "reject"`, waits, withdrawal results, reopen, and settlement against the transaction's latest record.
//! Every case runs against Memory, SQLite and JSONL storage. Not ported: "rejects a wait whose submission read spans
//! the start of close" (the controlled storage holds commits, not reads) and the typed entry token case (Rust entries
//! are untyped records).

mod chat_support;
mod support;

use chat_support::*;
use durable::{
    ConversationOwnership, EntryDraft, SessionError, SubmissionCreate, SubmissionId, SubmissionSettlement,
    SubmissionStatus, SubmissionType, TaskOptions, TaskQuery, TaskStatus,
};
use harness::chat::faux::{gated, text_message, unanswered};
use harness::chat::{SubmissionAbort, SubmissionDraft, WhenBusy};
use harness::scheduler::{Cancel, Ctx, HarnessError, RegistryReader};
use serde_json::json;
use support::*;

#[test]
fn harness_chat_submissions_appends_an_idle_write_and_settles_it_done_without_a_turn() {
    each_backend(|backend| {
        let (chat, root) = open_chat(Controlled::new(backend), &Setup::new());
        let submission = root
            .submit(SubmissionDraft::write(
                json!({ "kind": "note", "data": { "text": "x" } }),
            ))
            .unwrap();
        let settled = submission.wait(&bg()).unwrap();
        let entry = settled.entry.expect("an entry").0;
        assert_eq!(
            record(&settled),
            json!({ "id": submission.id.0, "conversationId": root.id.0, "type": "write", "status": "done", "entry": entry })
        );
        let entries = root.entries().unwrap();
        assert_eq!(
            js(&entries),
            json!([{ "id": entry, "conversationId": root.id.0, "kind": "note", "data": { "text": "x" } }])
        );
        assert_eq!(live(&chat, root.id), json!({}));
        let tasks = chat
            .commit(|tx| {
                Ok(tx
                    .scan_tasks(
                        &TaskQuery {
                            conversation_id: Some(root.id),
                            ..TaskQuery::default()
                        },
                        10,
                        None,
                    )?
                    .items)
            })
            .unwrap();
        assert!(tasks.is_empty());
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_submissions_places_idle_input_and_rejects_busy_input_with_when_busy_reject_without_writing() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(42);
        let (busy, reached) = unanswered();
        setup.faux.set_responses(vec![busy]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        reached.wait();
        let placed = submission.status().unwrap();
        assert_eq!(placed.status, SubmissionStatus::Placed);
        let entry = root
            .commit(|tx| Ok(tx.entry_of("pi.user", placed.entry.unwrap())?))
            .unwrap()
            .unwrap();
        assert_eq!(
            json!(entry.model),
            json!([{ "role": "user", "content": "hi", "timestamp": 42 }])
        );
        let state = live(&chat, root.id);
        assert_eq!(state["run"]["inputs"], json!([submission.id.0]));
        let run = durable::TaskId(state["run"]["taskId"].as_u64().unwrap());
        assert_eq!(chat.get_task(run).unwrap().unwrap().kind, "pi.generation");

        let commits = storage.commit_count();
        let rejected = root.submit(SubmissionDraft::input("again").when_busy(WhenBusy::Reject));
        let Err(HarnessError::Session(SessionError::Busy(conversation))) = rejected else {
            panic!("expected ConversationBusy");
        };
        assert_eq!(conversation, root.id);
        assert_eq!(
            HarnessError::Session(SessionError::Busy(conversation)).name(),
            "ConversationBusy"
        );
        assert_eq!(storage.commit_count(), commits);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_submissions_deduplicates_request_ids_per_conversation_before_any_write() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        let (busy, reached) = unanswered();
        setup.faux.set_responses(vec![busy]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let first = root.submit(SubmissionDraft::input("hi").request_id("r1")).unwrap();
        reached.wait();
        let commits = storage.commit_count();
        // Deduplication runs before the busy check.
        let again = root
            .submit(SubmissionDraft::input("different").request_id("r1"))
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(storage.commit_count(), commits);
        let error = root
            .submit(SubmissionDraft::write(json!({ "kind": "note" })).request_id("r1"))
            .err()
            .unwrap();
        assert_eq!(
            error.message(),
            "Request r1 already identifies a submission of type input"
        );
        assert_matches(&status(&first), json!({ "requestId": "r1", "status": "placed" }));

        let other = chat
            .create_conversation(ConversationOwnership::Ownerless, None)
            .unwrap();
        let write = other
            .submit(SubmissionDraft::write(json!({ "kind": "note" })).request_id("r1"))
            .unwrap();
        assert_ne!(write.id, first.id);
        let repeated = other
            .submit(SubmissionDraft::write(json!({ "kind": "note" })).request_id("r1"))
            .unwrap();
        assert_eq!(repeated.id, write.id);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_submissions_reports_abort_results_and_looks_submissions_up_by_conversation() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (step, _reached, release) = gated(text_message("answer"));
        setup.faux.set_responses(vec![step]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        assert_eq!(submission.abort().unwrap(), SubmissionAbort::AlreadyPlaced);
        assert_eq!(
            chat.abort_submission(submission.id, Some(root.id)).unwrap(),
            SubmissionAbort::AlreadyPlaced
        );
        let other = chat
            .create_conversation(ConversationOwnership::Ownerless, None)
            .unwrap();
        assert_eq!(
            chat.abort_submission(submission.id, Some(other.id)).unwrap(),
            SubmissionAbort::NotFound
        );
        assert_eq!(
            chat.abort_submission(SubmissionId(999_999), None).unwrap(),
            SubmissionAbort::NotFound
        );
        assert!(chat.submission(SubmissionId(999_999)).unwrap().is_none());

        release.abort();
        submission.wait(&bg()).unwrap();
        assert_eq!(submission.abort().unwrap(), SubmissionAbort::Settled);
        assert_eq!(
            chat.abort_submission(submission.id, None).unwrap(),
            SubmissionAbort::Settled
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_submissions_cancels_only_a_wait_and_rejects_pending_waits_on_close() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (busy, reached) = unanswered();
        setup.faux.set_responses(vec![busy]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        reached.wait();
        let controller = Cancel::new();
        let cancelled = {
            let submission = submission.clone();
            let ctx = Ctx::with_signal(&controller);
            spawn(move || submission.wait(&ctx))
        };
        let pending = {
            let submission = submission.clone();
            spawn(move || submission.wait(&bg()))
        };
        // Settle only: a wait that starts after its signal was cancelled, or after the close, ends the same way.
        flush();
        controller.cancel(HarnessError::msg("stop waiting"));
        assert_eq!(cancelled.join().unwrap().err().unwrap().message(), "stop waiting");
        assert_eq!(submission.status().unwrap().status, SubmissionStatus::Placed);
        chat.close().unwrap();
        assert_eq!(pending.join().unwrap().err().unwrap().message(), "Harness is closed");
    });
}

#[test]
fn harness_chat_submissions_reacquires_a_submission_after_reopen_and_settles_it_durably() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        // The first process never answers; the reopened one does.
        let (busy, reached) = unanswered();
        setup
            .faux
            .set_responses(vec![busy, text_message("after reopen").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let id = root
            .submit(SubmissionDraft::input("hi").request_id("print"))
            .unwrap()
            .id;
        reached.wait();
        chat.close().unwrap();

        let (chat, _) = open_chat(storage.reopen(), &setup);
        let submission = chat.submission(id).unwrap().unwrap();
        assert_eq!(submission.status().unwrap().status, SubmissionStatus::Placed);
        chat.resume().unwrap();
        let settled = submission.wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert_eq!(settled.kind, SubmissionType::Input);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_eq!(chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap(), settled);
        let again = root.submit(SubmissionDraft::input("hi").request_id("print")).unwrap();
        assert_eq!(again.id, id);
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_submissions_enables_scheduling_when_a_caller_submits_or_waits() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup.faux.set_responses(vec![text_message("answer").into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        // No resume(): submitting asks for progress.
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        assert_eq!(submission.wait(&bg()).unwrap().status, SubmissionStatus::Done);
        chat.close().unwrap();

        let passive = Setup::new();
        let (chat, root) = open_chat(Controlled::new(backend), &passive);
        let generation = passive.registry.snapshot().task("pi.generation").unwrap().clone();
        let task = root
            .commit(|tx| Ok(tx.create_task(generation.definition(), json!({}), TaskOptions::default())?))
            .unwrap();
        // A committed task alone does not start scheduling; waiting for it does. (A settle: a slow machine can only
        // let the check pass wrongly.)
        flush();
        assert_eq!(
            chat.get_task(task).unwrap().unwrap().state.status(),
            TaskStatus::Pending
        );
        assert_eq!(
            chat.wait_for_task(task, &bg()).unwrap().state.status(),
            TaskStatus::Terminal
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_submissions_settles_submissions_by_their_current_record_in_the_transaction() {
    each_backend(|backend| {
        let (chat, root) = open_chat(Controlled::new(backend), &Setup::new());
        let entry = root.append(EntryDraft::new("note")).unwrap().id;
        let raw = |kind, status| SubmissionCreate {
            conversation_id: root.id,
            request_id: None,
            kind,
            status,
            entry: None,
            answer: None,
            reason: None,
            detail: None,
        };
        let create = |record: SubmissionCreate| chat.commit(|tx| Ok(tx.create_submission(record)?.id)).unwrap();
        let queued = create(raw(SubmissionType::Input, SubmissionStatus::Queued));
        let write = create(raw(SubmissionType::Write, SubmissionStatus::Queued));
        let answer = SubmissionSettlement::Done { answer: entry };
        for id in [queued, write] {
            let error = root
                .commit(|tx| {
                    tx.settle_submission(id, answer.clone());
                    Ok(())
                })
                .err()
                .unwrap();
            assert!(error.message().contains("is not a placed input"), "{error}");
        }
        let missing = root
            .commit(|tx| {
                tx.settle_submission(
                    SubmissionId(999_999),
                    SubmissionSettlement::Unanswered {
                        reason: "x".into(),
                        detail: None,
                    },
                );
                Ok(())
            })
            .err()
            .unwrap();
        assert!(missing.message().contains("does not exist"), "{missing}");

        // A submission created earlier in the same commit settles; a second settlement leaves the first.
        let placed = chat
            .commit(|tx| {
                let mut record = raw(SubmissionType::Input, SubmissionStatus::Placed);
                record.entry = Some(entry);
                let id = tx.create_submission(record)?.id;
                tx.settle_submission(id, answer.clone());
                tx.settle_submission(
                    id,
                    SubmissionSettlement::Unanswered {
                        reason: "late".into(),
                        detail: None,
                    },
                );
                Ok(id)
            })
            .unwrap();
        assert_matches(
            &status(&chat.submission(placed).unwrap().unwrap()),
            json!({ "status": "done", "entry": entry.0, "answer": entry.0 }),
        );
        chat.close().unwrap();
    });
}
