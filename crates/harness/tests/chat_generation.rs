//! Port of upstream Pi Durable `test/harness-generation.test.ts` and `test/harness-generation-recovery.test.ts`
//! (spec §8.3): answers, partials, `no_model`, durable retry backoff, deferred polling, aborts, pinned request options
//! and provider session IDs, settings read at each decision, faults and orphans, and recovery after reopen (rerun
//! preparation, resend the pinned request, convert a committed partial, resume a backoff or a poll).
//!
//! RLM-first divergence: the model's tool list is exactly the REPL tool, so every first preparation appends a
//! `pi.system` entry declaring it; upstream's tool-less setups expect none. Expected entry kinds include it.
//! Not ported: registry and section-wrapper cases (no extension registry for sections) and "rejects a registry
//! without the built-in tasks" (the ChatHarness installs them).

mod chat_support;
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai::{AssistantMessage, AssistantMessageEvent, DeferredHandle, Message, Model, StopReason, TranscriptContext};
use chat_support::*;
use durable::{
    CommitChange, ConversationOwnership, DocumentContent, EntryDraft, Owner, StorageWrite, SubmissionCreate,
    SubmissionStatus, SubmissionType, TaskDefinition, TaskId, TaskOptions, TaskQuery,
};
use harness::chat::docs::{LIVE, PROVIDER};
use harness::chat::faux::{Faux, FauxOptions, FauxStep, error_message, message, text_message, unanswered};
use harness::chat::{
    AgentChange, ChatHarness, Conversation, ModelRef, Models, Settings, StreamRequest, SubmissionDraft,
};
use harness::scheduler::{AbortResult, Cancel, HarnessError, Result};
use indexmap::IndexMap;
use serde_json::{Value, json};
use support::*;

fn kinds(conversation: &Conversation) -> Vec<String> {
    conversation
        .entries()
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect()
}

fn run_task(chat: &ChatHarness, conversation: &Conversation) -> TaskId {
    let mut id = None;
    eventually(|| {
        id = live(chat, conversation.id)["run"]["taskId"].as_u64();
        id.is_some()
    });
    TaskId(id.unwrap())
}

fn checkpoint(chat: &ChatHarness, id: TaskId) -> Value {
    js(&chat.get_task(id).unwrap().unwrap().state)["checkpoint"].clone()
}

/// Every committed `pi.live` value from now on.
fn live_publications(chat: &ChatHarness) -> Arc<Mutex<Vec<Value>>> {
    let values: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = values.clone();
    chat.harness()
        .session()
        .subscribe_commits(move |publication| {
            for change in &publication.changes {
                if let CommitChange::Document { record, value, .. } = change
                    && record.identity.kind == "pi.live"
                    && let Some(value) = value.as_ref().and_then(|value| value.as_object().cloned())
                {
                    sink.lock().unwrap().push(Value::Object(value));
                }
            }
        })
        .unwrap();
    values
}

fn text_of_value(message: &Value) -> Option<String> {
    serde_json::from_value::<Message>(message.clone())
        .ok()
        .and_then(|message| text_of(&message))
}

fn sections(
    setup: &mut Setup,
    render: impl Fn() -> std::result::Result<Vec<(String, String)>, HarnessError> + Send + Sync + 'static,
) {
    setup.sections = Some(Arc::new(move |_, _| {
        Ok(render()?.into_iter().collect::<IndexMap<_, _>>())
    }));
}

fn preamble(setup: &mut Setup, text: &'static str) {
    sections(setup, move || Ok(vec![("preamble".to_owned(), text.to_owned())]));
}

fn error_503() -> FauxStep {
    error_message("503 Service Unavailable").into()
}

fn deferred() -> Setup {
    Setup::with(FauxOptions {
        pending_fetches: 0,
        poll_after_ms: Some(60_000),
        ..FauxOptions::default()
    })
}

fn stream_deferred(setup: &Setup) {
    setup.settings(|settings| {
        settings.stream.insert("deferred".into(), json!(true));
    });
}

/// A stream that replaces the faux provider's.
type StreamOverride =
    Arc<dyn Fn(&Cancel, &mut (dyn FnMut(&AssistantMessageEvent) + Send)) -> Result<AssistantMessage> + Send + Sync>;

/// The faux provider with a replaced stream, a failing deferred cancellation, or no models at all.
#[derive(Clone)]
struct Wrapped {
    faux: Faux,
    stream: Option<StreamOverride>,
    cancel_fails: bool,
    no_models: bool,
}

impl Wrapped {
    fn new(faux: &Faux) -> Wrapped {
        Wrapped {
            faux: faux.clone(),
            stream: None,
            cancel_fails: false,
            no_models: false,
        }
    }
}

impl Models for Wrapped {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<Model> {
        if self.no_models {
            return None;
        }
        self.faux.get_model(provider, model_id)
    }

    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        request: &StreamRequest,
        signal: &Cancel,
        on_event: &mut (dyn FnMut(&AssistantMessageEvent) + Send),
    ) -> Result<AssistantMessage> {
        match &self.stream {
            Some(stream) => stream(signal, on_event),
            None => self.faux.stream(model, context, request, signal, on_event),
        }
    }

    fn fetch_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<AssistantMessage> {
        self.faux.fetch_deferred(model, handle, signal)
    }

    fn cancel_deferred(&self, model: &Model, handle: &DeferredHandle, signal: &Cancel) -> Result<()> {
        if self.cancel_fails {
            return Err(HarnessError::msg("cancel failed"));
        }
        self.faux.cancel_deferred(model, handle, signal)
    }
}

fn open_wrapped(storage: Controlled, setup: &Setup, models: Wrapped) -> (ChatHarness, Conversation) {
    let mut setup = setup.clone();
    setup.models = Some(Arc::new(models));
    open_chat(storage, &setup)
}

/// A partial `text` streamed after an empty start, then `end` once `release` resolves (or the signal aborts).
fn short_stream(
    text: &'static str,
    release: Deferred,
    end: impl Fn() -> Result<AssistantMessage> + Send + Sync + 'static,
) -> StreamOverride {
    Arc::new(move |signal, on_event| {
        let mut partial = text_message(text);
        partial.stop_reason = StopReason::Pending;
        if text.is_empty() {
            partial.content.clear();
        }
        on_event(&AssistantMessageEvent::Start { partial });
        while !release.is_resolved() && signal.wait_timeout(Duration::from_millis(5)).is_none() {}
        end()
    })
}

/// Whether a live publication carried the partial `text`.
fn published_partial(values: &Mutex<Vec<Value>>, text: &str) -> bool {
    values
        .lock()
        .unwrap()
        .iter()
        .any(|value| text_of_value(&value["generation"]["message"]).as_deref() == Some(text))
}

#[test]
fn harness_chat_generation_answers_an_input_and_settles_its_submission() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        preamble(&mut setup, "You are helpful.");
        setup.faux.set_responses(vec![text_message("Hello there").into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let answer = root
            .commit(|tx| Ok(tx.entry_of("pi.assistant", settled.answer.unwrap())?))
            .unwrap()
            .unwrap();
        assert_eq!(
            text_of(&first_message(&answer).unwrap()).as_deref(),
            Some("Hello there")
        );
        let entries = root.entries().unwrap();
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant"]);
        assert_eq!(Some(entries[0].id), settled.entry);
        let system = &entries[1].model.as_ref().unwrap()[0];
        assert_eq!(system["role"], "system");
        assert_eq!(system["content"], "");
        assert_eq!(system["sections"], json!({ "preamble": "You are helpful." }));
        assert_eq!(system["toolsAdded"], json!([js(&rlm_tool())]));
        assert!(system["timestamp"].is_number());
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
        let task = &tasks[0];
        assert_eq!(task.kind, "pi.generation");
        // Entries written by the generation are attributed to it; the admitted user entry is not task work.
        assert_eq!(
            entries.iter().map(|entry| entry.by_task_id).collect::<Vec<_>>(),
            [None, Some(task.id), Some(task.id)]
        );
        assert_eq!(
            js(&task.state),
            json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "entryId": settled.answer.unwrap().0 } } })
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_stores_partials_as_deltas_and_a_complete_base_once_nothing_is_in_flight() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            tokens_per_second: Some(200.0),
            token_size: Some(1),
            ..FauxOptions::default()
        });
        setup.faux.set_responses(vec![text_message(&"w".repeat(200)).into()]);
        let storage = Controlled::new(backend);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        let commits = storage.commits();
        let live_id = commits
            .iter()
            .flatten()
            .find_map(|write| match write {
                StorageWrite::DocumentCreate { record, .. } if record.kind == "pi.live" => Some(record.id),
                _ => None,
            })
            .unwrap();
        let contents: Vec<&str> = commits
            .iter()
            .flatten()
            .filter_map(|write| match write {
                StorageWrite::DocumentChange { id, content } if *id == live_id => Some(match content {
                    DocumentContent::Base { .. } => "base",
                    DocumentContent::Delta { .. } => "delta",
                }),
                _ => None,
            })
            .collect();
        // Streaming writes deltas; the commit that settles the answer clears generation and writes a base.
        assert!(contents.contains(&"delta"), "{contents:?}");
        assert_eq!(contents.last(), Some(&"base"));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_still_ends_a_run_whose_input_something_else_already_settled() {
    each_backend(|backend| {
        let setup = Setup::new();
        let (busy, reached) = unanswered();
        setup.faux.set_responses(vec![busy]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        reached.wait();
        root.commit(|tx| {
            tx.settle_submission(
                submission.id,
                durable::SubmissionSettlement::Unanswered {
                    reason: "withdrawn".into(),
                    detail: None,
                },
            );
            Ok(())
        })
        .unwrap();
        let task = run_task(&chat, &root);
        chat.abort_task(task, &bg()).unwrap();
        assert_eq!(
            js(&chat.wait_for_task(task, &bg()).unwrap().state)["outcome"],
            json!({ "status": "aborted" })
        );
        // The earlier settlement stays; the run's own settlement leaves it unchanged.
        assert_matches(
            &status(&submission),
            json!({ "status": "unanswered", "reason": "withdrawn" }),
        );
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_fails_with_no_model_when_no_model_is_configured_or_the_model_is_unknown() {
    each_backend(|backend| {
        let (chat, root) = open_chat(Controlled::new(backend), &Setup::new());
        let plain = chat
            .create_conversation(ConversationOwnership::Ownerless, None)
            .unwrap();
        chat.resume().unwrap();
        let unset = plain.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_matches(&record(&unset), json!({ "status": "unanswered", "reason": "no_model" }));

        root.configure(&AgentChange::model(ModelRef::new("faux", "missing")))
            .unwrap();
        let unknown = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_matches(
            &record(&unknown),
            json!({ "status": "unanswered", "reason": "no_model" }),
        );
        assert!(unknown.entry.is_some());
        assert_eq!(kinds(&root), ["pi.user"]);
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
        assert_eq!(
            js(&tasks[0].state),
            json!({
                "status": "terminal",
                "outcome": {
                    "status": "failed",
                    "error": { "message": "Model faux/missing is not available", "detail": { "reason": "no_model" } },
                },
            })
        );
        assert_eq!(live(&chat, root.id), json!({}));
        assert_eq!(live(&chat, plain.id), json!({}));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_retries_a_retryable_error_after_a_durable_backoff_and_then_answers() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        preamble(&mut setup, "p");
        setup
            .faux
            .set_responses(vec![error_503(), text_message("recovered").into()]);
        setup.settings(|settings| {
            settings.retry.max_retries = 3;
            settings.retry.base_delay_ms = 1;
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let values = live_publications(&chat);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let entries = root.entries().unwrap();
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant", "pi.assistant"]);
        assert_eq!(entries[2].model.as_ref().unwrap()[0]["stopReason"], "error");
        let values = values.lock().unwrap();
        assert!(
            values
                .iter()
                .any(|value| value["generation"]["retry"]["error"] == "503 Service Unavailable")
        );
        assert!(values.iter().any(|value| value["generation"]["attempt"] == 2));
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_fails_with_model_error_once_retries_are_exhausted() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup
            .faux
            .set_responses(vec![error_503(), error_503(), text_message("never").into()]);
        setup.settings(|settings| {
            settings.retry.max_retries = 1;
            settings.retry.base_delay_ms = 1;
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_matches(
            &record(&settled),
            json!({ "status": "unanswered", "reason": "model_error", "detail": "503 Service Unavailable" }),
        );
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant", "pi.assistant"]);
        assert_eq!(setup.faux.pending(), 1);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_fails_a_retryable_error_without_retrying_when_the_retry_policy_is_disabled() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup
            .faux
            .set_responses(vec![error_503(), text_message("never").into()]);
        setup.settings(|settings| settings.retry.enabled = false);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_matches(
            &record(&settled),
            json!({ "status": "unanswered", "reason": "model_error" }),
        );
        assert_eq!(setup.faux.call_count(), 1);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_reports_section_failures_while_preparing() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        sections(&mut setup, || Err(HarnessError::msg("section failed")));
        setup.faux.set_responses(vec![text_message("ok").into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert!(setup.reports().contains(&HarnessError::msg("section failed")));
        // No section was rendered; the baseline still declares the REPL tool.
        let system = root.entries().unwrap()[1].model.clone().unwrap()[0].clone();
        assert!(system.get("sections").is_none());
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_fails_a_non_retryable_error_without_retrying() {
    each_backend(|backend| {
        let setup = Setup::new();
        setup.faux.set_responses(vec![
            error_message("Invalid request").into(),
            text_message("never").into(),
        ]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_matches(
            &record(&settled),
            json!({ "status": "unanswered", "reason": "model_error", "detail": "Invalid request" }),
        );
        let task = run_task_of(&chat, &root);
        assert_matches(
            &js(&task.state),
            json!({ "outcome": { "status": "failed", "error": { "message": "Invalid request", "detail": { "reason": "model_error" } } } }),
        );
        chat.close().unwrap();
    });
}

fn run_task_of(chat: &ChatHarness, conversation: &Conversation) -> durable::TaskRecord {
    chat.commit(|tx| {
        Ok(tx
            .scan_tasks(
                &TaskQuery {
                    conversation_id: Some(conversation.id),
                    ..TaskQuery::default()
                },
                10,
                None,
            )?
            .items
            .remove(0))
    })
    .unwrap()
}

#[test]
fn harness_chat_generation_polls_a_deferred_response_until_it_is_ready() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            pending_fetches: 1,
            poll_after_ms: Some(1),
            ..FauxOptions::default()
        });
        setup.faux.set_responses(vec![text_message("deferred answer").into()]);
        stream_deferred(&setup);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let values = live_publications(&chat);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert_eq!(setup.faux.deferred_fetch_count(), 2);
        let polls: Vec<u64> = values
            .lock()
            .unwrap()
            .iter()
            .filter_map(|value| value["generation"]["deferred"]["pollAt"].as_u64())
            .collect();
        assert_eq!(polls.len(), 2);
        assert!(polls[1] > polls[0]);
        let answer = root
            .commit(|tx| Ok(tx.entry_of("pi.assistant", settled.answer.unwrap())?))
            .unwrap()
            .unwrap();
        assert_eq!(
            text_of(&first_message(&answer).unwrap()).as_deref(),
            Some("deferred answer")
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_converts_the_committed_partial_when_aborted_during_streaming() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            tokens_per_second: Some(20.0),
            token_size: Some(1),
            ..FauxOptions::default()
        });
        setup.faux.set_responses(vec![text_message(&"x".repeat(400)).into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        let task = run_task(&chat, &root);
        eventually(|| text_of_value(&live(&chat, root.id)["generation"]["message"]).is_some());
        let partial = text_of_value(&live(&chat, root.id)["generation"]["message"]).unwrap();
        assert_eq!(chat.abort_task(task, &bg()).unwrap(), AbortResult::Marked);
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        let entries = root.entries().unwrap();
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant"]);
        let converted = entries[2].model.clone().unwrap()[0].clone();
        assert_eq!(converted["stopReason"], "aborted");
        assert!(text_of_value(&converted).unwrap().starts_with(&partial));
        assert_eq!(live(&chat, root.id), json!({}));
        assert_eq!(
            js(&chat.wait_for_task(task, &bg()).unwrap().state)["outcome"],
            json!({ "status": "aborted" })
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_cancels_a_deferred_response_when_aborted_during_polling() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            pending_fetches: 100,
            poll_after_ms: Some(60_000),
            ..FauxOptions::default()
        });
        setup.faux.set_responses(vec![text_message("never").into()]);
        stream_deferred(&setup);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        let task = run_task(&chat, &root);
        eventually(|| !live(&chat, root.id)["generation"]["deferred"].is_null());
        chat.abort_task(task, &bg()).unwrap();
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        assert_eq!(setup.faux.cancelled_deferred().len(), 1);
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_reports_a_failed_deferred_cancellation_and_still_ends_the_run_aborted() {
    each_backend(|backend| {
        let setup = Setup::with(FauxOptions {
            pending_fetches: 100,
            poll_after_ms: Some(60_000),
            ..FauxOptions::default()
        });
        setup.faux.set_responses(vec![text_message("never").into()]);
        stream_deferred(&setup);
        let mut models = Wrapped::new(&setup.faux);
        models.cancel_fails = true;
        let (chat, root) = open_wrapped(Controlled::new(backend), &setup, models);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        let task = run_task(&chat, &root);
        eventually(|| !live(&chat, root.id)["generation"]["deferred"].is_null());
        chat.abort_task(task, &bg()).unwrap();
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        assert!(setup.reports().contains(&HarnessError::msg("cancel failed")));
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

fn options_seen(seen: &Arc<Mutex<Vec<StreamRequest>>>, reply: &'static str) -> FauxStep {
    let seen = seen.clone();
    FauxStep::factory(move |_, request, _| {
        seen.lock().unwrap().push(request.clone());
        Ok(text_message(reply))
    })
}

#[test]
fn harness_chat_generation_forwards_stream_options_and_the_thinking_level() {
    each_backend(|backend| {
        let setup = Setup::new();
        let seen: Arc<Mutex<Vec<StreamRequest>>> = Arc::default();
        setup
            .faux
            .set_responses(vec![options_seen(&seen, "a"), options_seen(&seen, "b")]);
        setup.settings(|settings| {
            settings.stream = json!({ "timeoutMs": 1234, "headers": { "x-test": "1" } })
                .as_object()
                .unwrap()
                .clone();
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.configure(&AgentChange {
            model: None,
            thinking_level: Some(Some("high".into())),
        })
        .unwrap();
        chat.resume().unwrap();
        root.submit(SubmissionDraft::input("one")).unwrap().wait(&bg()).unwrap();
        // Both are read at the next preparation: the thinking level from pi.agent, the stream options live from settings.
        root.configure(&AgentChange {
            model: None,
            thinking_level: Some(None),
        })
        .unwrap();
        setup.settings(|settings| settings.stream = json!({ "timeoutMs": 99 }).as_object().unwrap().clone());
        root.submit(SubmissionDraft::input("two")).unwrap().wait(&bg()).unwrap();
        let session_id = chat.snapshot(&PROVIDER, Owner::Conversation(root.id)).unwrap().unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let seen = seen.lock().unwrap();
        assert_eq!(
            Value::Object(seen[0].options.clone()),
            json!({ "timeoutMs": 1234, "headers": { "x-test": "1" } })
        );
        assert_eq!(seen[0].reasoning.as_deref(), Some("high"));
        assert_eq!(seen[0].session_id.as_deref(), Some(session_id.as_str()));
        let simple = seen[0].simple_options();
        assert_eq!(simple.base.timeout_ms, Some(1234));
        assert_eq!(simple.reasoning.as_deref(), Some("high"));
        assert!(seen[1].reasoning.is_none());
        assert_eq!(Value::Object(seen[1].options.clone()), json!({ "timeoutMs": 99 }));
        assert_eq!(seen[1].session_id.as_deref(), Some(session_id.as_str()));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_keeps_provider_session_ids_request_local_across_concurrent_conversations() {
    each_backend(|backend| {
        let setup = Setup::new();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let capture = || {
            let seen = seen.clone();
            FauxStep::factory(move |context, request, _| {
                let text = context
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        Message::User(_) => text_of(message),
                        _ => None,
                    })
                    .unwrap_or_default();
                seen.lock()
                    .unwrap()
                    .push((text.clone(), request.session_id.clone().unwrap_or_default()));
                Ok(text_message(&format!("answer:{text}")))
            })
        };
        setup
            .faux
            .set_responses(vec![capture(), capture(), capture(), capture()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let child = chat
            .create_conversation(
                ConversationOwnership::Ownerless,
                Some(&AgentChange::model(Faux::model_ref())),
            )
            .unwrap();
        chat.resume().unwrap();
        for round in ["first", "second"] {
            let waits: Vec<_> = [&root, &child]
                .iter()
                .enumerate()
                .map(|(index, conversation)| {
                    conversation
                        .submit(SubmissionDraft::input(format!("{round}-{index}")))
                        .unwrap()
                })
                .collect();
            for submission in waits {
                submission.wait(&bg()).unwrap();
            }
        }
        let id = |conversation: &Conversation| {
            chat.snapshot(&PROVIDER, Owner::Conversation(conversation.id))
                .unwrap()
                .unwrap()["sessionId"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let (root_id, child_id) = (id(&root), id(&child));
        assert_ne!(root_id, child_id);
        let seen = seen.lock().unwrap();
        let of = |text: &str| -> Vec<String> {
            seen.iter()
                .filter(|(each, _)| each == text)
                .map(|(_, id)| id.clone())
                .collect()
        };
        assert_eq!(of("first-0"), std::slice::from_ref(&root_id));
        assert_eq!(of("second-0"), [root_id]);
        assert_eq!(of("first-1"), std::slice::from_ref(&child_id));
        assert_eq!(of("second-1"), [child_id]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_creates_and_persists_provider_state_before_a_legacy_conversations_request() {
    each_backend(|backend| {
        let setup = Setup::new();
        let sent: Arc<Mutex<Option<String>>> = Arc::default();
        let sink = sent.clone();
        setup.faux.set_responses(vec![FauxStep::factory(move |_, request, _| {
            *sink.lock().unwrap() = request.session_id.clone();
            Ok(text_message("ok"))
        })]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.commit(|tx| Ok(tx.retire_doc(&PROVIDER, Owner::Conversation(root.id))?))
            .unwrap();
        assert!(
            chat.snapshot(&PROVIDER, Owner::Conversation(root.id))
                .unwrap()
                .is_none()
        );
        chat.resume().unwrap();
        root.submit(SubmissionDraft::input("legacy"))
            .unwrap()
            .wait(&bg())
            .unwrap();
        let stored = chat.snapshot(&PROVIDER, Owner::Conversation(root.id)).unwrap().unwrap();
        let id = stored["sessionId"].as_str().unwrap();
        let pattern =
            regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap();
        assert!(pattern.is_match(id), "{id}");
        assert_eq!(sent.lock().unwrap().as_deref(), Some(id));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_reads_settings_at_every_decision() {
    each_backend(|backend| {
        let setup = Setup::new();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (first, second) = (seen.clone(), seen.clone());
        let live_settings = setup.settings.clone();
        setup.settings(|settings| {
            settings.retry.base_delay_ms = 1;
            settings.stream = json!({ "timeoutMs": 111 }).as_object().unwrap().clone();
        });
        setup.faux.set_responses(vec![
            FauxStep::factory(move |_, request, _| {
                first.lock().unwrap().push(request.options["timeoutMs"].clone());
                // The user changes the setting while the first attempt runs.
                live_settings.lock().unwrap().stream = json!({ "timeoutMs": 222 }).as_object().unwrap().clone();
                Ok(error_message("503 Service Unavailable"))
            }),
            FauxStep::factory(move |_, request, _| {
                second.lock().unwrap().push(request.options["timeoutMs"].clone());
                Ok(text_message("ok"))
            }),
        ]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        chat.resume().unwrap();
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        // The retry prepares again, so it resolves the settings again and sends the new timeout.
        assert_eq!(*seen.lock().unwrap(), [json!(111), json!(222)]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_resolves_settings_over_the_built_in_defaults() {
    let settings = Settings::default();
    assert!(settings.stream.is_empty());
    assert_eq!(
        (
            settings.retry.enabled,
            settings.retry.max_retries,
            settings.retry.base_delay_ms,
            settings.retry.max_delay_ms
        ),
        (true, 3, 2000, 60_000)
    );
    let compaction = settings.compaction;
    assert_eq!(
        (
            compaction.enabled,
            compaction.reserve_tokens,
            compaction.keep_recent_tokens,
            compaction.background_tokens
        ),
        (true, 16_384, 20_000, 32_768)
    );
    assert_eq!((settings.partial_interval_ms, settings.output_interval_ms), (100, 100));
    assert_eq!(settings.tool_execution, harness::chat::ToolExecution::Parallel);
    assert_eq!(settings.steering_mode, harness::chat::QueueMode::OneAtATime);
    assert_eq!(settings.follow_up_mode, harness::chat::QueueMode::OneAtATime);
    // retryDelayMs: baseDelayMs * 2^(attempt-1), capped at maxAgentDelayMs.
    assert_eq!(
        [1, 2, 3, 10].map(|attempt| settings.retry.delay_ms(attempt)),
        [2000, 4000, 8000, 60_000]
    );
}

#[test]
fn harness_chat_generation_commits_partials_no_more_often_than_the_partial_interval() {
    // `expected`: wait for the partial's publication before the stream ends; otherwise end it after a settle (an
    // interval of an hour cannot pass during the settle, however slow the machine).
    let published = |interval: u64, expected: bool| -> bool {
        let setup = Setup::new();
        setup.settings(|settings| settings.partial_interval_ms = interval);
        let mut models = Wrapped::new(&setup.faux);
        let release = Deferred::new();
        models.stream = Some(short_stream("partial", release.clone(), || Ok(text_message("final"))));
        let (chat, root) = open_wrapped(Controlled::new(Backend::Memory), &setup, models);
        let values = live_publications(&chat);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        if expected {
            eventually(|| published_partial(&values, "partial"));
        } else {
            std::thread::sleep(Duration::from_millis(300));
        }
        release.resolve();
        let settled = submission.wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        chat.close().unwrap();
        published_partial(&values, "partial")
    };
    assert!(published(100, true));
    assert!(!published(3_600_000, false));
}

#[test]
fn harness_chat_generation_renders_sections_per_conversation() {
    each_backend(|backend| {
        let mut setup = Setup::new();
        let sub: Arc<Mutex<Option<durable::ConversationId>>> = Arc::default();
        let which = sub.clone();
        setup.sections = Some(Arc::new(move |conversation, _| {
            let mut sections = IndexMap::new();
            let is_sub = *which.lock().unwrap() == Some(conversation);
            sections.insert(
                "cwd".into(),
                format!("<cwd>\n{}\n</cwd>", if is_sub { "/sub" } else { "/repo" }),
            );
            if !is_sub {
                sections.insert("agents".into(), "<agents>\nRead AGENTS.md\n</agents>".into());
            }
            Ok(sections)
        }));
        setup
            .faux
            .set_responses(vec![text_message("a").into(), text_message("b").into()]);
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let child = chat
            .create_conversation(
                ConversationOwnership::Ownerless,
                Some(&AgentChange::model(Faux::model_ref())),
            )
            .unwrap();
        *sub.lock().unwrap() = Some(child.id);
        chat.resume().unwrap();
        root.submit(SubmissionDraft::input("one")).unwrap().wait(&bg()).unwrap();
        child
            .submit(SubmissionDraft::input("two"))
            .unwrap()
            .wait(&bg())
            .unwrap();
        let system = |conversation: &Conversation| {
            conversation
                .entries()
                .unwrap()
                .into_iter()
                .find(|entry| entry.kind == "pi.system")
                .unwrap()
                .model
                .unwrap()[0]
                .clone()
        };
        assert_eq!(
            system(&root)["sections"],
            json!({ "cwd": "<cwd>\n/repo\n</cwd>", "agents": "<agents>\nRead AGENTS.md\n</agents>" })
        );
        assert_eq!(system(&child)["sections"], json!({ "cwd": "<cwd>\n/sub\n</cwd>" }));
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_commits_no_partial_for_a_response_that_turns_deferred_after_an_empty_start_event() {
    each_backend(|backend| {
        let setup = Setup::new();
        let mut models = Wrapped::new(&setup.faux);
        // Ended after a settle that gives a wrong empty partial a chance to be published.
        let release = Deferred::new();
        let settle = release.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            settle.resolve();
        });
        models.stream = Some(short_stream("", release, || {
            let mut deferred = message(Vec::new(), StopReason::Deferred);
            deferred.deferred = Some(DeferredHandle {
                provider: "faux".into(),
                model_id: "faux-1".into(),
                api: "faux".into(),
                id: "handle-1".into(),
                expires_at: None,
                poll_after_ms: Some(60_000),
                data: None,
            });
            Ok(deferred)
        }));
        let (chat, root) = open_wrapped(Controlled::new(backend), &setup, models);
        let values = live_publications(&chat);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        eventually(|| !live(&chat, root.id)["generation"]["deferred"].is_null());
        assert!(
            !values
                .lock()
                .unwrap()
                .iter()
                .any(|value| !value["generation"]["message"].is_null())
        );
        chat.abort_task(run_task(&chat, &root), &bg()).unwrap();
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        assert_eq!(kinds(&root), ["pi.user", "pi.system"]);
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_faults_a_run_task_settling_its_inputs_and_converting_the_committed_partial() {
    each_backend(|backend| {
        let setup = Setup::new();
        let mut models = Wrapped::new(&setup.faux);
        let release = Deferred::new();
        models.stream = Some(short_stream("partial", release.clone(), || {
            Err(HarnessError::msg("transport returned a malformed response"))
        }));
        let (chat, root) = open_wrapped(Controlled::new(backend), &setup, models);
        let values = live_publications(&chat);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        // The partial is committed before the stream fails.
        eventually(|| published_partial(&values, "partial"));
        release.resolve();
        let settled = submission.wait(&bg()).unwrap();
        assert_matches(
            &record(&settled),
            json!({ "status": "unanswered", "reason": "faulted" }),
        );
        assert!(
            settled
                .detail
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|detail| detail.contains("malformed response"))
        );
        assert!(
            values
                .lock()
                .unwrap()
                .iter()
                .any(|value| text_of_value(&value["generation"]["message"]).as_deref() == Some("partial"))
        );
        let entries = root.entries().unwrap();
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant"]);
        let converted = entries[2].model.clone().unwrap()[0].clone();
        assert_matches(&converted, json!({ "role": "assistant", "stopReason": "aborted" }));
        assert_eq!(text_of_value(&converted).as_deref(), Some("partial"));
        assert_eq!(live(&chat, root.id), json!({}));
        assert_matches(
            &js(&run_task_of(&chat, &root).state),
            json!({ "status": "terminal", "outcome": { "status": "faulted" } }),
        );
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_generation_orphans_a_blocked_run_task_with_full_run_cleanup() {
    each_backend(|backend| {
        let (chat, root) = open_chat(Controlled::new(backend), &Setup::new());
        // A run whose task was stored by a newer generation definition this process cannot run.
        let newer = TaskDefinition::new("pi.generation", 2, |_| json!({ "phase": "prepare", "attempt": 1 }));
        let (task, submission) = chat
            .commit(|tx| {
                let entry = tx.append_entry(
                    root.id,
                    EntryDraft {
                        model: Some(vec![json!({ "role": "user", "content": "hi", "timestamp": 1 })]),
                        ..EntryDraft::new("pi.user")
                    },
                )?;
                let submission = tx.create_submission(SubmissionCreate {
                    conversation_id: root.id,
                    request_id: None,
                    kind: SubmissionType::Input,
                    status: SubmissionStatus::Placed,
                    entry: Some(entry.id),
                    answer: None,
                    reason: None,
                    detail: None,
                })?;
                let task = tx.create_task(
                    &newer,
                    json!({}),
                    TaskOptions {
                        conversation_id: Some(root.id),
                        ..TaskOptions::default()
                    },
                )?;
                let live = tx.doc(&LIVE, Owner::Conversation(root.id))?;
                live.set(
                    &[chord_delta::Seg::from("run")],
                    json!({ "taskId": task.0, "inputs": [submission.id.0] }),
                )
                .map_err(|error| HarnessError::msg(error.to_string()))?;
                Ok((task, submission.id))
            })
            .unwrap();
        chat.resume().unwrap();
        let busy = root
            .submit(SubmissionDraft::input("busy").when_busy(harness::chat::WhenBusy::Reject))
            .err()
            .unwrap();
        assert!(busy.message().contains("is busy"));
        assert_eq!(chat.abort_task(task, &bg()).unwrap(), AbortResult::Marked);
        assert_eq!(
            js(&chat.wait_for_task(task, &bg()).unwrap().state)["outcome"],
            json!({ "status": "orphaned", "reason": "task_too_old" })
        );
        assert_matches(
            &status(&chat.submission(submission).unwrap().unwrap()),
            json!({ "status": "unanswered", "reason": "task_too_old" }),
        );
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    });
}

// ─── recovery ──────────────────────────────────────────────────────────────────

#[test]
fn harness_chat_generation_recovery_reruns_preparation_interrupted_before_its_commit() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let mut setup = Setup::new();
        let reached = Deferred::new();
        let block = Arc::new(Mutex::new(true));
        let (signal_reached, blocking) = (reached.clone(), block.clone());
        setup.sections = Some(Arc::new(move |_, signal| {
            let first = std::mem::replace(&mut *blocking.lock().unwrap(), false);
            if first {
                signal_reached.resolve();
                return Err(signal.wait());
            }
            Ok(IndexMap::from([("preamble".to_owned(), "p".to_owned())]))
        }));
        setup.faux.set_responses(vec![text_message("answer").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        let id = root.submit(SubmissionDraft::input("hi")).unwrap().id;
        reached.wait();
        let task = run_task(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_eq!(checkpoint(&chat, task), json!({ "phase": "prepare", "attempt": 1 }));
        chat.resume().unwrap();
        assert_eq!(
            chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
            SubmissionStatus::Done
        );
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant"]);
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_resends_a_request_interrupted_before_any_partial_without_repeating_preparation() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let mut setup = Setup::new();
        preamble(&mut setup, "p");
        let reached = Deferred::new();
        let signal_reached = reached.clone();
        let sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let timeouts: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (sent_sink, timeout_sink) = (sent.clone(), timeouts.clone());
        setup.faux.set_responses(vec![
            FauxStep::factory(move |_, _, signal| {
                signal_reached.resolve();
                Err(signal.wait())
            }),
            FauxStep::factory(move |context, request, _| {
                sent_sink.lock().unwrap().push(
                    context
                        .messages
                        .iter()
                        .map(|message| js(message)["role"].as_str().unwrap().to_owned())
                        .collect(),
                );
                timeout_sink.lock().unwrap().push(request.options["timeoutMs"].clone());
                Ok(text_message("answer"))
            }),
        ]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        setup.settings(|settings| settings.stream = json!({ "timeoutMs": 1234 }).as_object().unwrap().clone());
        chat.resume().unwrap();
        let id = root.submit(SubmissionDraft::input("hi")).unwrap().id;
        reached.wait();
        let task = run_task(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_matches(
            &checkpoint(&chat, task),
            json!({ "phase": "request", "attempt": 1, "thinkingLevel": "off", "streamOptions": { "timeoutMs": 1234 } }),
        );
        // The resend uses the pinned request, not options changed after preparation.
        setup.settings(|settings| settings.stream = json!({ "timeoutMs": 999 }).as_object().unwrap().clone());
        assert_matches(&live(&chat, root.id), json!({ "generation": { "attempt": 1 } }));
        chat.resume().unwrap();
        assert_eq!(
            chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
            SubmissionStatus::Done
        );
        assert_eq!(
            *sent.lock().unwrap(),
            vec![vec!["system".to_owned(), "user".to_owned()]]
        );
        assert_eq!(*timeouts.lock().unwrap(), vec![json!(1234)]);
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant"]);
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_converts_a_committed_partial_into_an_aborted_entry_and_resends_the_same_messages() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let slow = Setup::with(FauxOptions {
            tokens_per_second: Some(20.0),
            token_size: Some(1),
            ..FauxOptions::default()
        });
        slow.faux.set_responses(vec![text_message(&"z".repeat(400)).into()]);
        let (chat, root) = open_chat(storage.clone(), &slow);
        chat.resume().unwrap();
        let id = root.submit(SubmissionDraft::input("hi")).unwrap().id;
        let mut watched = None;
        eventually(|| {
            watched = text_of_value(&live(&chat, root.id)["generation"]["message"]);
            watched.is_some()
        });
        chat.close().unwrap();

        let setup = Setup::new();
        let sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let sink = sent.clone();
        setup.faux.set_responses(vec![FauxStep::factory(move |context, _, _| {
            sink.lock().unwrap().push(
                context
                    .messages
                    .iter()
                    .map(|message| js(message)["role"].as_str().unwrap().to_owned())
                    .collect(),
            );
            Ok(text_message("answer"))
        })]);
        let (chat, root) = open_chat(storage.reopen(), &setup);
        let partial = text_of_value(&live(&chat, root.id)["generation"]["message"]).unwrap();
        // Everything observers saw before the crash is durable.
        assert!(partial.starts_with(watched.as_deref().unwrap()));
        chat.resume().unwrap();
        assert_eq!(
            chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
            SubmissionStatus::Done
        );
        assert_eq!(
            *sent.lock().unwrap(),
            vec![vec!["system".to_owned(), "user".to_owned()]]
        );
        let entries = root.entries().unwrap();
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant", "pi.assistant"]);
        let converted = entries[2].model.clone().unwrap()[0].clone();
        assert_eq!(converted["stopReason"], "aborted");
        assert_eq!(text_of_value(&converted).as_deref(), Some(partial.as_str()));
        assert_eq!(live(&chat, root.id), json!({}));
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_resumes_a_retry_backoff_after_reopen() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(1_000);
        setup
            .faux
            .set_responses(vec![error_503(), text_message("recovered").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        setup.settings(|settings| {
            settings.retry.max_retries = 2;
            settings.retry.base_delay_ms = 60_000;
        });
        let id = root.submit(SubmissionDraft::input("hi")).unwrap().id;
        eventually(|| !live(&chat, root.id)["generation"]["retry"].is_null());
        let task = run_task(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_eq!(
            checkpoint(&chat, task),
            json!({ "phase": "retry", "attempt": 1, "until": 61_000 })
        );
        assert_eq!(
            live(&chat, root.id),
            json!({
                "run": { "taskId": task.0, "inputs": [id.0] },
                "generation": { "attempt": 1, "retry": { "at": 61_000, "error": "503 Service Unavailable" } },
            })
        );
        setup.fix_clock(61_000);
        chat.resume().unwrap();
        assert_eq!(
            chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap().status,
            SubmissionStatus::Done
        );
        assert_eq!(kinds(&root), ["pi.user", "pi.system", "pi.assistant", "pi.assistant"]);
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_resumes_polling_a_deferred_response_after_reopen() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = deferred();
        setup.fix_clock(1_000);
        setup.faux.set_responses(vec![text_message("deferred answer").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        stream_deferred(&setup);
        let id = root.submit(SubmissionDraft::input("hi")).unwrap().id;
        eventually(|| !live(&chat, root.id)["generation"]["deferred"].is_null());
        let task = run_task(&chat, &root);
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_matches(
            &checkpoint(&chat, task),
            json!({ "phase": "poll", "attempt": 1, "pollAt": 61_000 }),
        );
        setup.fix_clock(61_000);
        chat.resume().unwrap();
        let settled = chat.submission(id).unwrap().unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let answer = root
            .commit(|tx| Ok(tx.entry_of("pi.assistant", settled.answer.unwrap())?))
            .unwrap()
            .unwrap();
        assert_eq!(
            text_of(&first_message(&answer).unwrap()).as_deref(),
            Some("deferred answer")
        );
        assert_eq!(setup.faux.deferred_fetch_count(), 1);
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_fails_no_model_when_the_pinned_model_is_gone_after_reopen_in_request_and_in_poll() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = deferred();
        let (busy, reached) = unanswered();
        setup.faux.set_responses(vec![busy, text_message("deferred").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        let requesting = root.submit(SubmissionDraft::input("one")).unwrap().id;
        reached.wait();
        chat.close().unwrap();

        // Reopened without the faux provider: the request's pinned model is unknown.
        let mut empty = Wrapped::new(&setup.faux);
        empty.no_models = true;
        let (chat, _) = open_wrapped(storage.reopen(), &setup, empty.clone());
        chat.resume().unwrap();
        assert_matches(
            &record(&chat.submission(requesting).unwrap().unwrap().wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "no_model" }),
        );
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        stream_deferred(&setup);
        chat.resume().unwrap();
        let polling = root.submit(SubmissionDraft::input("two")).unwrap().id;
        eventually(|| !live(&chat, root.id)["generation"]["deferred"].is_null());
        chat.close().unwrap();

        let (chat, _) = open_wrapped(storage.reopen(), &setup, empty);
        chat.resume().unwrap();
        assert_matches(
            &record(&chat.submission(polling).unwrap().unwrap().wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "no_model" }),
        );
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_generation_recovery_runs_a_print_style_turn_and_reads_the_durable_answer_after_reopen() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let mut setup = Setup::new();
        preamble(&mut setup, "You are terse.");
        setup.faux.set_responses(vec![text_message("42").into()]);
        let (chat, root) = open_chat(storage.clone(), &setup);
        chat.resume().unwrap();
        let submission = root.submit(SubmissionDraft::input("answer?")).unwrap();
        let settled = submission.wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        let answer = root
            .commit(|tx| Ok(tx.entry_of("pi.assistant", settled.answer.unwrap())?))
            .unwrap()
            .unwrap();
        assert_eq!(text_of(&first_message(&answer).unwrap()).as_deref(), Some("42"));
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        assert_eq!(
            chat.submission(submission.id).unwrap().unwrap().status().unwrap(),
            settled
        );
        assert_eq!(
            root.commit(|tx| Ok(tx.entry_of("pi.assistant", settled.answer.unwrap())?))
                .unwrap(),
            Some(answer)
        );
        chat.close().unwrap();
    }
}

/// A provider `StreamFn` (as `crates/cli` composes it): one partial, then the answer, or an aborted message once its
/// request's `AbortSignal` fires while `hold` is set.
struct ScriptedStream {
    hold: bool,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl agent::StreamFn for ScriptedStream {
    fn stream<'a>(
        &'a self,
        _model: &'a ai::Model,
        context: &'a ai::TranscriptContext,
        options: &'a ai::SimpleStreamOptions,
        sink: &'a mut ai::EventSink<'_>,
    ) -> ai::http::BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(options.base.session_id.clone());
            let mut partial = text_message("par");
            partial.stop_reason = StopReason::Pending;
            sink(&AssistantMessageEvent::Start { partial });
            if self.hold {
                let signal = options
                    .base
                    .signal
                    .clone()
                    .expect("the request carries the invocation's signal");
                signal.cancelled().await;
                let mut aborted = text_message("par");
                aborted.stop_reason = StopReason::Aborted;
                return aborted;
            }
            let last = context.messages.iter().rev().find_map(text_of).unwrap_or_default();
            text_message(&format!("answer to {last}"))
        })
    }
}

#[test]
fn harness_chat_generation_runs_on_an_agent_stream_fn_through_the_models_adapter() {
    each_backend(|backend| {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let catalog = vec![Faux::new(FauxOptions::default()).get_model("faux", "faux-1").unwrap()];
        let mut setup = Setup::new();
        setup.models = Some(Arc::new(harness::chat::models::StreamFnModels::new(
            catalog.clone(),
            Arc::new(ScriptedStream {
                hold: false,
                seen: seen.clone(),
            }),
        )));
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let settled = root.submit(SubmissionDraft::input("hi")).unwrap().wait(&bg()).unwrap();
        assert_eq!(settled.status, SubmissionStatus::Done);
        assert_eq!(
            transcript(&root.entries().unwrap()),
            ["pi.user:hi", "pi.assistant:answer to hi"]
        );
        let session = chat.snapshot(&PROVIDER, Owner::Conversation(root.id)).unwrap().unwrap()["sessionId"].clone();
        assert_eq!(json!(seen.lock().unwrap()[0]), session);
        chat.close().unwrap();

        // Aborting the run aborts the provider request through its AbortSignal.
        setup.models = Some(Arc::new(harness::chat::models::StreamFnModels::new(
            catalog,
            Arc::new(ScriptedStream { hold: true, seen }),
        )));
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        let submission = root.submit(SubmissionDraft::input("hi")).unwrap();
        let task = run_task(&chat, &root);
        eventually(|| text_of_value(&live(&chat, root.id)["generation"]["message"]).is_some());
        chat.abort_task(task, &bg()).unwrap();
        assert_matches(
            &record(&submission.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        chat.close().unwrap();
    });
}
