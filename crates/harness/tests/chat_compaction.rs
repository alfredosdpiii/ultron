//! Port of the overflow and blocking parts of upstream Pi Durable `test/harness-compaction.test.ts` (spec §8.3,
//! §8.7): a context overflow compacts once with Ultron's summarization request (`crates/compaction`) and retries the
//! same attempt; a second overflow, a disabled policy, or a failed compaction fail with the overflow text; a blocking
//! threshold compaction runs before the request, also across reopen; a manual compaction places its summary through
//! a write submission. Range selection cases run on synthetic views.

mod chat_support;
mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ai::{AssistantMessage, Message, StopReason};
use chat_support::*;
use durable::{EntryRecord, Owner, SubmissionStatus, TaskId};
use harness::chat::compaction::select_cut;
use harness::chat::context::order_tool_results;
use harness::chat::docs::USAGE;
use harness::chat::faux::{FauxOptions, FauxStep, error_message, message, text_message};
use harness::chat::{ChatHarness, CompactionPolicy, ContextView, Conversation, StreamRequest, SubmissionDraft};
use harness::scheduler::Cancel;
use indexmap::IndexMap;
use serde_json::json;
use support::*;

const OVERFLOW: &str = "prompt is too long: 250000 tokens > 200000 maximum";

/// Small thresholds: no automatic compaction unless a test enables it.
const MANUAL: CompactionPolicy = CompactionPolicy {
    enabled: false,
    reserve_tokens: 1000,
    keep_recent_tokens: 150,
    background_tokens: 0,
};
const ENABLED: CompactionPolicy = CompactionPolicy {
    enabled: true,
    ..MANUAL
};
/// Blocking threshold at 700 tokens of a 1000-token window, no background compaction.
const BLOCKING: CompactionPolicy = CompactionPolicy {
    enabled: true,
    reserve_tokens: 300,
    keep_recent_tokens: 150,
    background_tokens: 0,
};

/// Text of about `tokens` estimated tokens, starting with `label`.
fn text(label: &str, tokens: usize) -> String {
    format!("{label} {}", "x".repeat((tokens * 4).saturating_sub(label.len() + 1)))
}

#[derive(Clone)]
enum Step {
    Message(Box<AssistantMessage>),
    /// Wait for `gate` (or cancellation), resolving `reached` when the request arrives.
    Gated(Cancel, Box<AssistantMessage>, Deferred),
    /// Answer with what the function returns when the request arrives.
    Call(Arc<dyn Fn() -> AssistantMessage + Send + Sync>),
}

fn step(message: AssistantMessage) -> Step {
    Step::Message(Box::new(message))
}

#[derive(Clone)]
struct Request {
    messages: Vec<Message>,
    options: StreamRequest,
}

/// Scripted faux model answering agent and summarization requests from separate queues.
#[derive(Clone, Default)]
struct Script {
    agent: Arc<Mutex<VecDeque<Step>>>,
    summaries: Arc<Mutex<VecDeque<Step>>>,
    agent_requests: Arc<Mutex<Vec<Request>>>,
    summary_requests: Arc<Mutex<Vec<Request>>>,
}

impl Script {
    fn agent(&self, step: Step) {
        self.agent.lock().unwrap().push_back(step);
    }
    fn summary(&self, step: Step) {
        self.summaries.lock().unwrap().push_back(step);
    }
    fn agent_requests(&self) -> Vec<Request> {
        self.agent_requests.lock().unwrap().clone()
    }
    fn summary_requests(&self) -> Vec<Request> {
        self.summary_requests.lock().unwrap().clone()
    }
}

fn is_summary_request(messages: &[Message]) -> bool {
    matches!(messages.first(), Some(Message::System(system))
        if ai::transcript::system_message_text(system).starts_with("You are a context summarization assistant"))
}

fn script(setup: &Setup) -> Script {
    let script = Script::default();
    let steps = (0..500)
        .map(|_| {
            let script = script.clone();
            FauxStep::factory(move |context, options, signal| {
                let request = Request {
                    messages: context.messages.clone(),
                    options: options.clone(),
                };
                let summary = is_summary_request(&request.messages);
                let step = if summary {
                    script.summary_requests.lock().unwrap().push(request);
                    script.summaries.lock().unwrap().pop_front()
                } else {
                    script.agent_requests.lock().unwrap().push(request);
                    script.agent.lock().unwrap().pop_front()
                };
                match step {
                    None => Err(harness::scheduler::HarnessError::msg("No scripted response")),
                    Some(Step::Message(message)) => Ok(*message),
                    Some(Step::Call(call)) => Ok(call()),
                    Some(Step::Gated(gate, message, reached)) => {
                        reached.resolve();
                        Cancel::any(&[&gate, signal]).signal().wait();
                        signal.check()?;
                        Ok(*message)
                    }
                }
            })
        })
        .collect();
    setup.faux.set_responses(steps);
    script
}

struct Chat {
    chat: ChatHarness,
    root: Conversation,
    setup: Setup,
    faux: Script,
}

fn setup_with(context_window: u64) -> Setup {
    let mut setup = Setup::with(FauxOptions {
        context_window: Some(context_window),
        ..FauxOptions::default()
    });
    setup.sections = Some(Arc::new(|_, _| {
        Ok(IndexMap::from([("preamble".to_owned(), "You are helpful.".to_owned())]))
    }));
    setup.settings(|settings| {
        settings.compaction = MANUAL;
        settings.retry.max_retries = 2;
        settings.retry.base_delay_ms = 1;
    });
    setup
}

fn open(storage: Controlled, setup: Setup, faux: Script) -> Chat {
    let (chat, root) = open_chat(storage, &setup);
    chat.resume().unwrap();
    Chat {
        chat,
        root,
        setup,
        faux,
    }
}

fn open_new(context_window: u64) -> Chat {
    let setup = setup_with(context_window);
    let faux = script(&setup);
    open(Controlled::new(Backend::Memory), setup, faux)
}

/// Run one turn: `user` answered by `reply`.
fn turn(chat: &Chat, user: &str, reply: &str) {
    chat.faux.agent(step(text_message(reply)));
    let submission = chat.root.submit(SubmissionDraft::input(user)).unwrap();
    assert_eq!(submission.wait(&bg()).unwrap().status, SubmissionStatus::Done);
}

/// Three turns of about 100-token messages.
fn history(chat: &Chat) {
    turn(chat, &text("u1", 100), &text("a1", 100));
    turn(chat, &text("u2", 100), &text("a2", 100));
    turn(chat, &text("u3", 100), &text("a3", 100));
}

fn kinds(conversation: &Conversation) -> Vec<String> {
    conversation
        .entries()
        .unwrap()
        .into_iter()
        .map(|entry| entry.kind)
        .collect()
}

fn user_text(message: Option<&Message>) -> String {
    message.and_then(text_of).unwrap_or_default()
}

fn first_user_text(messages: &[Message]) -> String {
    user_text(messages.iter().find(|message| matches!(message, Message::User(_))))
}

fn failure(text: &str) -> AssistantMessage {
    error_message(text)
}

fn compactions(chat: &Chat) -> Vec<durable::TaskRecord> {
    chat.chat
        .inspect()
        .unwrap()
        .tasks
        .into_iter()
        .map(|task| task.record)
        .filter(|record| record.kind == "pi.compaction")
        .collect()
}

// ─── range selection ───────────────────────────────────────────────────────────

fn entry(id: u64, kind: &str, model: Vec<serde_json::Value>, head: Option<u64>) -> EntryRecord {
    EntryRecord {
        id: durable::EntryId(id),
        conversation_id: durable::ConversationId(1),
        kind: kind.into(),
        model: Some(model),
        data: None,
        head: head.map(durable::EntryId),
        edits: None,
        by_task_id: None,
    }
}

fn user(content: &str) -> serde_json::Value {
    json!({ "role": "user", "content": content, "timestamp": 0 })
}

fn assistant(content: &str, calls: &[&str], stop: &str) -> serde_json::Value {
    let mut blocks = vec![json!({ "type": "text", "text": content })];
    blocks.extend(
        calls
            .iter()
            .map(|id| json!({ "type": "toolCall", "id": id, "name": "read", "arguments": {} })),
    );
    let mut message = js(&message(Vec::new(), StopReason::Stop));
    message["content"] = json!(blocks);
    message["stopReason"] = json!(stop);
    message
}

fn tool_result(call: &str, content: &str) -> serde_json::Value {
    json!({ "role": "toolResult", "toolCallId": call, "toolName": "read", "content": [{ "type": "text", "text": content }], "isError": false, "timestamp": 0 })
}

/// A view over `entries` whose contributions are their models, with excluded assistants removed.
fn view(entries: Vec<EntryRecord>, head: Option<EntryRecord>) -> ContextView {
    let all: Vec<EntryRecord> = head.iter().cloned().chain(entries).collect();
    let contributions: Vec<Vec<Message>> = all
        .iter()
        .map(|record| {
            record
                .model
                .iter()
                .flatten()
                .map(|message| serde_json::from_value::<Message>(message.clone()).unwrap())
                .filter(|message| {
                    !matches!(message, Message::Assistant(assistant)
                        if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted | StopReason::Deferred))
                })
                .collect()
        })
        .collect();
    let messages = order_tool_results(&contributions.concat());
    ContextView {
        head,
        entries: all,
        contributions,
        messages,
    }
}

#[test]
fn harness_chat_compaction_cuts_at_a_user_entry() {
    let cut = select_cut(
        &view(
            vec![
                entry(1, "pi.user", vec![user(&text("u1", 100))], None),
                entry(2, "pi.assistant", vec![assistant(&text("a1", 100), &[], "stop")], None),
                entry(3, "pi.user", vec![user(&text("u2", 100))], None),
                entry(4, "pi.assistant", vec![assistant(&text("a2", 100), &[], "stop")], None),
            ],
            None,
        ),
        150,
    );
    assert_eq!(cut, Some(2));
}

#[test]
fn harness_chat_compaction_never_cuts_at_a_tool_result_and_keeps_a_huge_last_result_with_its_assistant() {
    let entries = vec![
        entry(1, "pi.user", vec![user(&text("u1", 100))], None),
        entry(2, "pi.assistant", vec![assistant("calls", &["c1"], "toolUse")], None),
        entry(3, "pi.tool-result", vec![tool_result("c1", &text("r1", 500))], None),
    ];
    assert_eq!(select_cut(&view(entries, None), 150), Some(1));
}

#[test]
fn harness_chat_compaction_never_cuts_at_a_system_entry_or_an_excluded_error_answer() {
    let entries = vec![
        entry(1, "pi.user", vec![user(&text("u1", 100))], None),
        entry(
            2,
            "pi.assistant",
            vec![assistant(&text("bad", 100), &[], "error")],
            None,
        ),
        entry(
            3,
            "pi.system",
            vec![json!({ "role": "system", "content": "", "timestamp": 0 })],
            None,
        ),
        entry(4, "pi.user", vec![user(&text("u2", 200))], None),
    ];
    assert_eq!(select_cut(&view(entries, None), 150), Some(3));
}

#[test]
fn harness_chat_compaction_does_not_cut_at_a_user_entry_that_a_result_of_the_preceding_call_still_follows() {
    let entries = vec![
        entry(1, "pi.user", vec![user(&text("u1", 100))], None),
        entry(2, "pi.assistant", vec![assistant("calls", &["c1"], "toolUse")], None),
        entry(3, "pi.user", vec![user(&text("steer", 100))], None),
        entry(4, "pi.tool-result", vec![tool_result("c1", &text("r1", 100))], None),
        entry(5, "pi.assistant", vec![assistant(&text("a2", 100), &[], "stop")], None),
    ];
    assert_eq!(select_cut(&view(entries, None), 150), Some(4));
}

#[test]
fn harness_chat_compaction_finds_nothing_when_the_budget_is_never_reached_or_only_the_marker_precedes_the_cut() {
    let short = vec![entry(1, "pi.user", vec![user("short")], None)];
    assert_eq!(select_cut(&view(short, None), 150), None);
    let marker = entry(1, "pi.compaction", vec![user("summary")], Some(2));
    let kept = vec![entry(2, "pi.user", vec![user(&text("u", 300))], None)];
    assert_eq!(select_cut(&view(kept, Some(marker)), 150), None);
}

// ─── manual and overflow compaction ────────────────────────────────────────────

#[test]
fn harness_chat_compaction_places_the_summary_at_once_when_idle_and_keeps_raw_history() {
    let chat = open_new(100_000);
    history(&chat);
    let before = chat.root.entries().unwrap();
    let usage_before = chat
        .chat
        .snapshot(&USAGE, Owner::Conversation(chat.root.id))
        .unwrap()
        .unwrap()["models"]["faux/faux-1"]
        .clone();
    chat.faux.summary(step(text_message("SUMMARY")));

    let id = chat.root.compact(Some("focus on files")).unwrap();
    let outcome = js(&chat.chat.wait_for_task(id, &bg()).unwrap().state)["outcome"].clone();
    assert_eq!(outcome["status"], "completed");
    let submission = durable::SubmissionId(outcome["result"]["submissionId"].as_u64().unwrap());
    let placed = chat.chat.submission(submission).unwrap().unwrap().wait(&bg()).unwrap();
    assert_eq!(placed.status, SubmissionStatus::Done);

    // Raw history is unchanged; one summary entry heads the first kept entry, u3.
    let after = chat.root.entries().unwrap();
    assert_eq!(after[..before.len()], before[..]);
    let marker = after.last().unwrap();
    let u3 = before
        .iter()
        .find(|record| user_text(first_message(record).as_ref()).starts_with("u3"))
        .unwrap();
    assert_matches(
        &js(marker),
        json!({ "kind": "pi.compaction", "head": u3.id.0, "data": { "reason": "manual" } }),
    );
    assert_eq!(placed.entry, Some(marker.id));
    assert_eq!(
        user_text(first_message(marker).as_ref()),
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nSUMMARY\n</summary>"
    );

    // The model context is the summary followed by the kept entries.
    let view = chat.root.context(None).unwrap();
    assert_eq!(
        view.messages
            .iter()
            .map(|message| user_text(Some(message)))
            .collect::<Vec<_>>(),
        [
            user_text(first_message(marker).as_ref()),
            text("u3", 100),
            text("a3", 100)
        ]
    );

    // The summarizer saw the serialized prefix, the prompt, and the instructions, without caching.
    let request = &chat.faux.summary_requests()[0];
    assert_eq!(request.messages.len(), 2);
    let prompt = user_text(request.messages.get(1));
    assert!(prompt.starts_with("<conversation>\n[User]: u1 "), "{prompt}");
    assert!(prompt.contains("[Assistant]: a2 "));
    assert!(!prompt.contains("u3 "));
    assert!(prompt.contains("## Goal"));
    assert!(prompt.ends_with("\n\nAdditional focus: focus on files"));
    assert_eq!(request.options.options["cacheRetention"], "none");
    assert_eq!(request.options.options["maxTokens"], 800);
    assert!(request.options.options.get("deferred").is_none());
    let session = chat
        .chat
        .snapshot(&harness::chat::docs::PROVIDER, Owner::Conversation(chat.root.id))
        .unwrap()
        .unwrap()["sessionId"]
        .clone();
    assert_eq!(json!(request.options.session_id), session);

    // The summarizer's spend is in the ledger, and nothing counts it again later.
    let usage = chat
        .chat
        .snapshot(&USAGE, Owner::Conversation(chat.root.id))
        .unwrap()
        .unwrap()["models"]["faux/faux-1"]
        .clone();
    assert!(usage["input"].as_u64() > usage_before["input"].as_u64());
    turn(&chat, "next", "done");
    let agent = chat.faux.agent_requests().last().unwrap().messages.clone();
    // The next request: the summary, the kept turn, the new input, then one complete system baseline.
    assert_eq!(
        agent
            .iter()
            .map(|message| js(message)["role"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ["user", "user", "assistant", "user", "system"]
    );
    assert_eq!(js(&agent[4])["sections"], json!({ "preamble": "You are helpful." }));
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_compacts_and_retries_with_the_same_attempt_leaving_the_error_out_of_the_retry() {
    let chat = open_new(100_000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = ENABLED);
    chat.faux.summary(step(text_message("SUMMARY")));
    chat.faux.agent(step(failure(OVERFLOW)));
    let attempt: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
    {
        let (attempt, harness, root) = (attempt.clone(), chat.chat.clone(), chat.root.id);
        chat.faux.agent(Step::Call(Arc::new(move || {
            *attempt.lock().unwrap() = Some(harness.live(root).unwrap()["generation"]["attempt"].clone());
            text_message("fits")
        })));
    }
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
    assert_eq!(input.wait(&bg()).unwrap().status, SubmissionStatus::Done);
    let entries = kinds(&chat.root);
    assert_eq!(
        entries[entries.len() - 5..],
        ["pi.user", "pi.assistant", "pi.compaction", "pi.system", "pi.assistant"]
    );
    let compaction = chat
        .root
        .entries()
        .unwrap()
        .into_iter()
        .find(|record| record.kind == "pi.compaction")
        .unwrap();
    assert_eq!(compaction.data, Some(json!({ "reason": "overflow" })));
    let retry = chat.faux.agent_requests().last().unwrap().messages.clone();
    assert!(user_text(retry.first()).contains("SUMMARY"));
    assert!(
        !retry.iter().any(
            |message| matches!(message, Message::Assistant(assistant) if assistant.stop_reason == StopReason::Error)
        )
    );
    assert_eq!(*attempt.lock().unwrap(), Some(json!(1)));
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_fails_a_second_overflow_with_its_error_entry() {
    let chat = open_new(100_000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = ENABLED);
    chat.faux.summary(step(text_message("SUMMARY")));
    chat.faux.agent(step(failure(OVERFLOW)));
    chat.faux.agent(step(failure(OVERFLOW)));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
    assert_matches(
        &record(&input.wait(&bg()).unwrap()),
        json!({ "status": "unanswered", "reason": "model_error", "detail": OVERFLOW }),
    );
    let entries = kinds(&chat.root);
    assert_eq!(entries.iter().filter(|kind| *kind == "pi.compaction").count(), 1);
    assert_eq!(entries.last().unwrap(), "pi.assistant");
    // The second overflow starts no second compaction: one summarization request in all.
    assert_eq!(chat.faux.summary_requests().len(), 1);
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_never_compacts_twice_in_one_generation_even_when_another_cut_exists() {
    let chat = open_new(100_000);
    history(&chat);
    chat.setup.settings(|settings| {
        settings.compaction = CompactionPolicy {
            keep_recent_tokens: 250,
            ..ENABLED
        }
    });
    // While summarizing, the policy changes so that the compacted context still has a cut.
    let settings = chat.setup.settings.clone();
    chat.faux.summary(Step::Call(Arc::new(move || {
        settings.lock().unwrap().compaction.keep_recent_tokens = 50;
        text_message("SUMMARY")
    })));
    chat.faux.agent(step(failure(OVERFLOW)));
    chat.faux.agent(step(failure(OVERFLOW)));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
    assert_matches(
        &record(&input.wait(&bg()).unwrap()),
        json!({ "status": "unanswered", "reason": "model_error", "detail": OVERFLOW }),
    );
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert_eq!(
        kinds(&chat.root).iter().filter(|kind| *kind == "pi.compaction").count(),
        1
    );
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_fails_without_compacting_when_compaction_is_disabled_even_for_a_retryable_looking_overflow()
 {
    let chat = open_new(100_000);
    history(&chat);
    chat.faux.agent(step(failure(&format!("overloaded: {OVERFLOW}"))));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
    assert_matches(
        &record(&input.wait(&bg()).unwrap()),
        json!({ "status": "unanswered", "reason": "model_error" }),
    );
    assert_eq!(chat.faux.agent_requests().len(), 4);
    assert_eq!(chat.faux.summary_requests().len(), 0);
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_fails_after_a_blocking_threshold_compaction_in_the_same_generation() {
    let chat = open_new(1000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = BLOCKING);
    chat.faux.summary(step(text_message("SUMMARY")));
    chat.faux.agent(step(failure(OVERFLOW)));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 200))).unwrap();
    assert_matches(
        &record(&input.wait(&bg()).unwrap()),
        json!({ "status": "unanswered", "reason": "model_error" }),
    );
    assert_eq!(chat.faux.summary_requests().len(), 1);
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_overflow_fails_with_the_overflow_text_when_compaction_fails_or_cannot_cut() {
    for (name, requests) in [("fails", 1), ("cannot cut", 0)] {
        let chat = open_new(100_000);
        turn(&chat, &text("u1", 100), &text("a1", 100));
        chat.setup.settings(|settings| settings.compaction = ENABLED);
        match name {
            "fails" => chat.faux.summary(step(failure("bad request"))),
            _ => chat
                .setup
                .settings(|settings| settings.compaction.keep_recent_tokens = 100_000),
        }
        chat.faux.agent(step(failure(OVERFLOW)));
        let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
        assert_matches(
            &record(&input.wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "model_error", "detail": OVERFLOW }),
        );
        assert_eq!(chat.faux.summary_requests().len(), requests, "{name}");
        chat.chat.close().unwrap();
    }
}

#[test]
fn harness_chat_compaction_keeps_the_full_retry_budget_after_an_overflow_compaction() {
    let chat = open_new(100_000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = ENABLED);
    chat.faux.summary(step(text_message("SUMMARY")));
    for message in [
        failure("prompt is too long"),
        failure("overloaded"),
        failure("overloaded"),
        text_message("finally"),
    ] {
        chat.faux.agent(step(message));
    }
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
    assert_eq!(input.wait(&bg()).unwrap().status, SubmissionStatus::Done);
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_treats_a_length_stop_as_an_ordinary_answer_not_an_overflow() {
    let chat = open_new(100_000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = ENABLED);
    let mut cut_short = text_message("cut short");
    cut_short.stop_reason = StopReason::Length;
    cut_short.error_message = Some(OVERFLOW.into());
    chat.faux.agent(step(cut_short));
    let input = chat.root.submit(SubmissionDraft::input("go")).unwrap();
    assert_eq!(input.wait(&bg()).unwrap().status, SubmissionStatus::Done);
    assert_eq!(chat.faux.summary_requests().len(), 0);
    chat.chat.close().unwrap();
}

// ─── blocking threshold compaction ─────────────────────────────────────────────

#[test]
fn harness_chat_compaction_blocking_waits_for_its_compaction_which_appends_the_summary_before_the_request() {
    let chat = open_new(1000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = BLOCKING);
    let (gate, reached) = (Cancel::new(), Deferred::new());
    chat.faux.summary(Step::Gated(
        gate.clone(),
        Box::new(text_message("SUMMARY")),
        reached.clone(),
    ));
    chat.faux.agent(step(text_message("a4")));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 200))).unwrap();
    reached.wait();
    let child = compactions(&chat).remove(0);
    let generation = TaskId(live(&chat.chat, chat.root.id)["run"]["taskId"].as_u64().unwrap());
    assert_matches(
        &js(&child),
        json!({ "owner": generation.0, "background": false, "input": { "reason": "threshold" } }),
    );
    assert_matches(
        &js(&chat.chat.get_task(generation).unwrap().unwrap().state),
        json!({ "status": "waiting", "on": [child.id.0], "checkpoint": { "phase": "prepare", "attempt": 1, "compacted": child.id.0 } }),
    );
    assert_eq!(
        live(&chat.chat, chat.root.id)["compactions"],
        json!([{ "taskId": child.id.0, "reason": "threshold", "blocking": true, "attempt": 1 }])
    );
    // Nothing was appended before the wait.
    assert_eq!(kinds(&chat.root).last().unwrap(), "pi.user");
    gate.abort();
    assert_eq!(input.wait(&bg()).unwrap().status, SubmissionStatus::Done);
    assert_eq!(
        js(&chat.chat.wait_for_task(child.id, &bg()).unwrap().state)["outcome"]["status"],
        "completed"
    );
    let entries = kinds(&chat.root);
    assert_eq!(
        entries[entries.len() - 3..],
        ["pi.compaction", "pi.system", "pi.assistant"]
    );
    let request = chat.faux.agent_requests().last().unwrap().messages.clone();
    assert!(first_user_text(&request).contains("SUMMARY"));
    assert_eq!(
        request
            .iter()
            .filter(|message| matches!(message, Message::System(_)))
            .count(),
        1
    );
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_blocking_sends_the_request_once_without_a_second_compaction_when_the_kept_part_is_still_above_the_threshold()
 {
    let chat = open_new(1000);
    history(&chat);
    chat.setup.settings(|settings| {
        settings.compaction = CompactionPolicy {
            keep_recent_tokens: 700,
            ..BLOCKING
        }
    });
    chat.faux.summary(step(text_message("SUMMARY")));
    turn(&chat, &text("u4", 400), "a4");
    assert_eq!(chat.faux.summary_requests().len(), 1);
    assert_eq!(
        kinds(&chat.root).iter().filter(|kind| *kind == "pi.compaction").count(),
        1
    );
    chat.chat.close().unwrap();
}

#[test]
fn harness_chat_compaction_blocking_sends_the_request_anyway_when_its_compaction_fails_or_is_aborted() {
    let chat = open_new(1000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = BLOCKING);
    chat.faux.summary(step(failure("bad request")));
    turn(&chat, &text("u4", 200), "a4");
    assert!(!kinds(&chat.root).contains(&"pi.compaction".to_owned()));
    assert_eq!(
        first_user_text(&chat.faux.agent_requests().last().unwrap().messages),
        text("u1", 100)
    );
    chat.chat.close().unwrap();

    let chat = open_new(1000);
    history(&chat);
    chat.setup.settings(|settings| settings.compaction = BLOCKING);
    let reached = Deferred::new();
    chat.faux.summary(Step::Gated(
        Cancel::new(),
        Box::new(text_message("SUMMARY")),
        reached.clone(),
    ));
    chat.faux.agent(step(text_message("a4")));
    let input = chat.root.submit(SubmissionDraft::input(text("u4", 200))).unwrap();
    reached.wait();
    let child = compactions(&chat).remove(0);
    chat.chat.abort_task(child.id, &bg()).unwrap();
    assert_eq!(input.wait(&bg()).unwrap().status, SubmissionStatus::Done);
    assert!(!kinds(&chat.root).contains(&"pi.compaction".to_owned()));
    chat.chat.close().unwrap();
}

// ─── recovery ──────────────────────────────────────────────────────────────────

fn first(backend: Backend) -> (Chat, Controlled) {
    let storage = Controlled::new(backend);
    let setup = setup_with(128_000);
    let faux = script(&setup);
    let chat = open(storage.clone(), setup, faux);
    history(&chat);
    (chat, storage)
}

#[test]
fn harness_chat_compaction_recovery_keeps_a_generation_waiting_on_its_blocking_compaction_across_reopen() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let (chat, storage) = first(backend);
        let reached = Deferred::new();
        chat.faux.summary(Step::Gated(
            Cancel::new(),
            Box::new(text_message("SUMMARY")),
            reached.clone(),
        ));
        // The faux model window is 128k; this blocking threshold needs a small context window.
        chat.setup.settings(|settings| {
            settings.compaction = CompactionPolicy {
                reserve_tokens: 128_000 - 700,
                ..BLOCKING
            }
        });
        let input = chat.root.submit(SubmissionDraft::input(text("u4", 200))).unwrap();
        reached.wait();
        chat.chat.close().unwrap();
        let chat = open(storage.reopen(), chat.setup.clone(), chat.faux.clone());
        chat.faux.summary(step(text_message("SUMMARY")));
        chat.faux.agent(step(text_message("a4")));
        assert_eq!(
            chat.chat
                .submission(input.id)
                .unwrap()
                .unwrap()
                .wait(&bg())
                .unwrap()
                .status,
            SubmissionStatus::Done
        );
        let entries = kinds(&chat.root);
        assert_eq!(
            entries[entries.len() - 3..],
            ["pi.compaction", "pi.system", "pi.assistant"]
        );
        chat.chat.close().unwrap();
    }
}

#[test]
fn harness_chat_compaction_recovery_fails_an_overflow_run_with_its_text_when_its_compaction_fails_after_reopen() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let (chat, storage) = first(backend);
        chat.setup.settings(|settings| settings.compaction = ENABLED);
        let reached = Deferred::new();
        chat.faux.summary(Step::Gated(
            Cancel::new(),
            Box::new(text_message("SUMMARY")),
            reached.clone(),
        ));
        chat.faux.agent(step(failure(OVERFLOW)));
        let input = chat.root.submit(SubmissionDraft::input(text("u4", 100))).unwrap();
        reached.wait();
        let generation = TaskId(live(&chat.chat, chat.root.id)["run"]["taskId"].as_u64().unwrap());
        assert_matches(
            &js(&chat.chat.get_task(generation).unwrap().unwrap().state),
            json!({ "status": "waiting", "checkpoint": { "phase": "prepare", "overflow": OVERFLOW } }),
        );
        chat.chat.close().unwrap();
        let chat = open(storage.reopen(), chat.setup.clone(), chat.faux.clone());
        chat.faux.summary(step(failure("bad request")));
        assert_matches(
            &record(&chat.chat.submission(input.id).unwrap().unwrap().wait(&bg()).unwrap()),
            json!({ "status": "unanswered", "reason": "model_error", "detail": OVERFLOW }),
        );
        chat.chat.close().unwrap();
    }
}

#[test]
fn harness_chat_compaction_recovery_repeats_nothing_after_reopen_once_the_summary_is_placed() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let (chat, storage) = first(backend);
        chat.faux.summary(step(text_message("SUMMARY")));
        let id = chat.root.compact(None).unwrap();
        chat.chat.wait_for_task(id, &bg()).unwrap();
        chat.chat.wait_for_idle(&bg()).unwrap();
        let entries = chat.root.entries().unwrap();
        let usage = chat.chat.snapshot(&USAGE, Owner::Conversation(chat.root.id)).unwrap();
        chat.chat.close().unwrap();
        let chat = open(storage.reopen(), chat.setup.clone(), chat.faux.clone());
        chat.chat.wait_for_idle(&bg()).unwrap();
        assert_eq!(chat.faux.summary_requests().len(), 1);
        assert_eq!(chat.root.entries().unwrap(), entries);
        assert_eq!(
            chat.chat.snapshot(&USAGE, Owner::Conversation(chat.root.id)).unwrap(),
            usage
        );
        let inspection = chat.chat.inspect().unwrap();
        assert!(inspection.tasks.is_empty());
        assert!(inspection.submissions.is_empty());
        chat.chat.close().unwrap();
    }
}

/// TypeScript keeps a compaction's retained tail as the model saw it (`prepareCompaction` applies context edits
/// before choosing it): an edit of a kept entry still applies after the summary, and the summarizer never sees what
/// an edit removed. Here an edit is an entry placed after its targets, as every edit is (ctx's, a trim's, a
/// system baseline's), so whatever keeps its target keeps the edit too: the range after a summary starts at the
/// first kept entry, at or before every kept target.
#[test]
fn harness_chat_compaction_keeps_the_edits_of_kept_entries() {
    let chat = open_new(100_000);
    history(&chat);
    let entries = chat.root.entries().unwrap();
    let id_of = |label: &str| {
        entries
            .iter()
            .find(|record| user_text(first_message(record).as_ref()).starts_with(label))
            .unwrap()
            .id
            .0
    };
    let (u2, u3) = (id_of("u2"), id_of("u3"));
    let edit = chat
        .root
        .append(durable::EntryDraft {
            edits: Some(vec![
                json!({ "target": u3, "action": "replace", "messages": [user("u3 EDITED")] }),
                json!({ "target": u2, "action": "omit" }),
            ]),
            ..durable::EntryDraft::new("ultron.custom")
        })
        .unwrap();
    chat.faux.summary(step(text_message("SUMMARY")));
    let id = chat.root.compact(None).unwrap();
    let outcome = js(&chat.chat.wait_for_task(id, &bg()).unwrap().state)["outcome"].clone();
    let submission = durable::SubmissionId(outcome["result"]["submissionId"].as_u64().unwrap());
    chat.chat.submission(submission).unwrap().unwrap().wait(&bg()).unwrap();
    let marker = chat.root.entries().unwrap().last().unwrap().clone();
    assert_eq!(marker.kind, "pi.compaction");
    let head = marker.head.unwrap().0;
    assert!(head <= u3 && head < edit.id.0, "kept from {head}");
    let view = chat.root.context(None).unwrap();
    let texts: Vec<String> = view.messages.iter().map(|m| user_text(Some(m))).collect();
    assert!(texts.contains(&"u3 EDITED".to_string()), "{texts:?}");
    assert!(
        !texts.iter().any(|t| t.starts_with("u3 x") || t.starts_with("u2 ")),
        "{texts:?}"
    );
    let prompt = user_text(chat.faux.summary_requests()[0].messages.get(1));
    assert!(!prompt.contains("u2 "), "{prompt}");
    chat.chat.close().unwrap();
}
