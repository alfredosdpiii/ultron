//! The swarm items (rewriteplan "Swarm design items") as durable host operations: persistent children
//! (`rlm.spawn(keep=True)` + `rlm.ask`), the agent registry and routing, durable peer messages, and the session's
//! task board and notes. Operations are called the way a cell's host requests reach them; durability is checked
//! across reopen on SQLite and JSONL.

mod chat_support;
mod support;

use std::sync::{Arc, Mutex};

use ai::Message;
use chat_support::*;
use durable::{ConversationId, ConversationOwnership, SubmissionStatus, TaskId, TaskStatus};
use harness::chat::faux::{FauxStep, rlm_call, text_message};
use harness::chat::host_ops::HostOps;
use harness::chat::{CellCall, CellResult, ChatHarness, SubmissionDraft};
use harness::scheduler::Cancel;
use serde_json::{Map, Value, json};
use support::*;

fn op(ops: &HostOps, kind: &str, value: Value, call: &CellCall) -> Result<Value, String> {
    let payload: Map<String, Value> = value.as_object().unwrap().clone();
    ops.handle(kind, &payload, call, &Cancel::new())
        .expect("a host operation")
}

/// A request as the cell of `conversation` makes it.
fn call_from(conversation: ConversationId) -> CellCall {
    CellCall {
        conversation_id: conversation,
        task_id: TaskId(0),
        call_id: "c".into(),
        name: "rlm".into(),
        arguments: json!({ "code": "" }),
    }
}

/// Faux responses routed by conversation: the root (first user `go`) runs `delegate` then says `done`; a child
/// answers its brief and each follow-up with `answer(question)`.
fn route(setup: &Setup, answer: fn(&str) -> String) {
    let steps = (0..60)
        .map(|_| {
            FauxStep::factory(move |context, _, _| {
                let users: Vec<String> = context
                    .messages
                    .iter()
                    .filter(|message| matches!(message, Message::User(_)))
                    .filter_map(text_of)
                    .collect();
                let answered = context
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::ToolResult(_)));
                if users.first().map(String::as_str) == Some("go") {
                    return Ok(if answered {
                        text_message("done")
                    } else {
                        rlm_call("delegate", "c1")
                    });
                }
                Ok(text_message(&answer(
                    users.last().map(String::as_str).unwrap_or_default(),
                )))
            })
        })
        .collect();
    setup.faux.set_responses(steps);
}

fn child_answer(question: &str) -> String {
    match question {
        "You own the parser." => "I know the parser.".into(),
        other => format!("re: {other}"),
    }
}

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

fn spawn_kept(setup: &Setup, handle: Arc<Mutex<Option<Value>>>) {
    delegate(setup, move |ops, call, signal| {
        let payload = |value: Value| value.as_object().unwrap().clone();
        let spawned = ops
            .handle(
                "rlm.spawn",
                &payload(json!({ "prompt": "You own the parser.", "kwargs": { "name": "parser" }, "keep": true })),
                call,
                signal,
            )
            .unwrap()?;
        let collected = ops
            .handle(
                "rlm.collect",
                &payload(json!({ "selectors": [spawned["rlm_child_id"]] })),
                call,
                signal,
            )
            .unwrap()?;
        *handle.lock().unwrap() = Some(spawned.clone());
        Ok(CellResult::text(collected.to_string()))
    });
}

fn child_task(handle: &Arc<Mutex<Option<Value>>>) -> TaskId {
    TaskId(
        handle.lock().unwrap().as_ref().unwrap()["rlm_child_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    )
}

#[test]
fn harness_chat_swarm_keeps_a_persistent_child_alive_and_answers_follow_ups_through_its_inbox_across_reopen() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        route(&setup, child_answer);
        let handle: Arc<Mutex<Option<Value>>> = Arc::default();
        spawn_kept(&setup, handle.clone());
        let (chat, root) = open_chat(storage.clone(), &setup);
        assert_eq!(
            root.submit(SubmissionDraft::input("go"))
                .unwrap()
                .wait(&bg())
                .unwrap()
                .status,
            SubmissionStatus::Done
        );
        let child = child_task(&handle);
        // The kept child answered its brief and stays alive as a background task of the root conversation.
        let record = chat.get_task(child).unwrap().unwrap();
        assert!(record.background);
        assert_eq!(record.owner, None);
        assert_matches(
            &js(&record.state),
            json!({ "status": "running", "checkpoint": { "phase": "idle", "result": { "status": "succeeded", "value": "I know the parser." } } }),
        );
        chat.wait_for_idle(&bg()).unwrap();
        let ops = HostOps::new(chat.clone());
        let asked = op(
            &ops,
            "rlm.ask",
            json!({ "child": child.0.to_string(), "question": "And the lexer?" }),
            &call_from(root.id),
        )
        .unwrap();
        assert_eq!(
            asked,
            json!({ "id": child.0.to_string(), "status": "succeeded", "value": "re: And the lexer?" })
        );
        chat.close().unwrap();

        // After a restart the child is still there, with its context: the follow-up is its third user message.
        let (chat, root) = open_chat(storage.reopen(), &setup);
        chat.resume().unwrap();
        let ops = HostOps::new(chat.clone());
        let asked = op(
            &ops,
            "rlm.ask",
            json!({ "child": child.0.to_string(), "question": "Still there?" }),
            &call_from(root.id),
        )
        .unwrap();
        assert_eq!(asked["value"], "re: Still there?");
        let conversation = ConversationId(
            js(&chat.get_task(child).unwrap().unwrap().state)["checkpoint"]["conversation"]
                .as_u64()
                .unwrap(),
        );
        let child_conversation = chat.conversation(conversation).unwrap().unwrap();
        assert_eq!(
            transcript(&child_conversation.entries().unwrap()),
            [
                "pi.user:You own the parser.",
                "pi.assistant:I know the parser.",
                "pi.user:And the lexer?",
                "pi.assistant:re: And the lexer?",
                "pi.user:Still there?",
                "pi.assistant:re: Still there?",
            ]
        );
        // The caller's context grew only by its own turn; the child's transcript stays in the child.
        assert!(
            !transcript(&root.entries().unwrap())
                .iter()
                .any(|entry| entry.contains("lexer"))
        );
        // Cancelling ends it.
        assert_eq!(
            op(
                &ops,
                "rlm.cancel",
                json!({ "selector": child.0.to_string() }),
                &call_from(root.id)
            )
            .unwrap(),
            json!({ "cancelled": true })
        );
        assert_eq!(
            chat.wait_for_task(child, &bg()).unwrap().state.status(),
            TaskStatus::Terminal
        );
        let ended = op(
            &ops,
            "rlm.ask",
            json!({ "child": child.0.to_string(), "question": "?" }),
            &call_from(root.id),
        );
        assert_eq!(ended.unwrap_err(), format!("Subagent {} has ended", child.0));
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_swarm_refuses_to_ask_an_ordinary_child() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, child_answer);
        let asked: Arc<Mutex<Option<String>>> = Arc::default();
        let sink = asked.clone();
        delegate(&setup, move |ops, call, signal| {
            let payload = |value: Value| value.as_object().unwrap().clone();
            let spawned = ops
                .handle(
                    "rlm.spawn",
                    &payload(json!({ "prompt": "brief", "kwargs": { "name": "once" } })),
                    call,
                    signal,
                )
                .unwrap()?;
            let error = ops
                .handle(
                    "rlm.ask",
                    &payload(json!({ "child": spawned["rlm_child_id"], "question": "more?" })),
                    call,
                    signal,
                )
                .unwrap()
                .unwrap_err();
            *sink.lock().unwrap() = Some(error);
            Ok(CellResult::text("ok"))
        });
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        let error = asked.lock().unwrap().clone().unwrap();
        assert!(
            error.ends_with("is not persistent; spawn it with keep=True to ask it follow-ups"),
            "{error}"
        );
        chat.close().unwrap();
    });
}

fn conversations(chat: &ChatHarness, count: usize) -> Vec<ConversationId> {
    (0..count)
        .map(|_| {
            chat.create_conversation(ConversationOwnership::Ownerless, None)
                .unwrap()
                .id
        })
        .collect()
}

#[test]
fn harness_chat_swarm_registers_what_agents_hold_and_routes_a_question_to_the_agent_with_the_context() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(5);
        let (chat, _) = open_chat(storage.clone(), &setup);
        let ops = HostOps::new(chat.clone());
        let [lexer, docs] = conversations(&chat, 2)[..] else {
            unreachable!()
        };
        op(&ops, "swarm.register", json!({ "name": "lexer-agent", "topics": ["tokenizer", "lexer"], "files": ["src/lexer.rs"], "work": "fixed the string escape bug" }), &call_from(lexer)).unwrap();
        op(
            &ops,
            "swarm.register",
            json!({ "name": "docs-agent", "topics": ["documentation"], "files": ["README.md"] }),
            &call_from(docs),
        )
        .unwrap();
        // A second registration merges.
        let merged = op(
            &ops,
            "swarm.register",
            json!({ "topics": ["lexer", "unicode"], "work": "audited unicode identifiers" }),
            &call_from(lexer),
        )
        .unwrap();
        assert_eq!(
            merged,
            json!({
                "name": "lexer-agent",
                "agent": {
                    "topics": ["tokenizer", "lexer", "unicode"],
                    "files": ["src/lexer.rs"],
                    "recent": ["fixed the string escape bug", "audited unicode identifiers"],
                    "conversation": lexer.0,
                    "updatedAt": 5,
                },
            })
        );
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        let ops = HostOps::new(chat.clone());
        let routed = op(
            &ops,
            "swarm.route",
            json!({ "query": "Why does src/lexer.rs reject unicode?" }),
            &call_from(root.id),
        )
        .unwrap();
        let names: Vec<&str> = routed
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["lexer-agent"]);
        // src/lexer.rs is a file (2), unicode a topic (2) and recent work (1).
        assert_eq!(routed[0]["score"], 5);
        let routed = op(
            &ops,
            "swarm.route",
            json!({ "query": "update the documentation in README.md" }),
            &call_from(root.id),
        )
        .unwrap();
        assert_eq!(routed[0]["name"], "docs-agent");
        assert_eq!(routed.as_array().unwrap().len(), 1);
        assert_eq!(
            op(&ops, "swarm.route", json!({ "query": "weather" }), &call_from(root.id)).unwrap(),
            json!([])
        );
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_swarm_registers_spawned_children_with_their_brief_as_recent_work() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, child_answer);
        let handle: Arc<Mutex<Option<Value>>> = Arc::default();
        spawn_kept(&setup, handle.clone());
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        let ops = HostOps::new(chat.clone());
        let agents = op(&ops, "swarm.agents", json!({}), &call_from(root.id)).unwrap();
        let child = child_task(&handle);
        assert_matches(
            &agents["parser"],
            json!({ "recent": ["You own the parser."], "child": child.0.to_string() }),
        );
        let routed = op(
            &ops,
            "swarm.route",
            json!({ "query": "who owns the parser" }),
            &call_from(root.id),
        )
        .unwrap();
        assert_eq!(routed[0]["name"], "parser");
        op(
            &ops,
            "rlm.cancel",
            json!({ "selector": child.0.to_string() }),
            &call_from(root.id),
        )
        .unwrap();
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_swarm_delivers_durable_peer_messages_and_marks_them_read() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(9);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let ops = HostOps::new(chat.clone());
        let [a, b] = conversations(&chat, 2)[..] else {
            unreachable!()
        };
        op(&ops, "swarm.register", json!({ "name": "alice" }), &call_from(a)).unwrap();
        op(&ops, "swarm.register", json!({ "name": "bob" }), &call_from(b)).unwrap();
        assert_eq!(
            op(
                &ops,
                "swarm.send",
                json!({ "to": "bob", "text": "lexer is yours" }),
                &call_from(a)
            )
            .unwrap(),
            json!({ "id": 1, "woke": false })
        );
        op(
            &ops,
            "swarm.send",
            json!({ "to": "bob", "text": "ping" }),
            &call_from(root.id),
        )
        .unwrap();
        chat.close().unwrap();

        let (chat, _) = open_chat(storage.reopen(), &setup);
        let ops = HostOps::new(chat.clone());
        assert_eq!(
            op(&ops, "swarm.inbox", json!({}), &call_from(b)).unwrap(),
            json!([
                { "id": 1, "from": "alice", "text": "lexer is yours", "at": 9, "read": false },
                { "id": 2, "from": "root", "text": "ping", "at": 9, "read": false },
            ])
        );
        // Read once: they are marked read in the same commit.
        assert_eq!(op(&ops, "swarm.inbox", json!({}), &call_from(b)).unwrap(), json!([]));
        assert_eq!(
            op(&ops, "swarm.inbox", json!({ "all": true }), &call_from(b))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(op(&ops, "swarm.inbox", json!({}), &call_from(a)).unwrap(), json!([]));
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_swarm_wakes_a_persistent_child_with_a_message_as_a_follow_up() {
    each_backend(|backend| {
        let setup = Setup::new();
        route(&setup, child_answer);
        let handle: Arc<Mutex<Option<Value>>> = Arc::default();
        spawn_kept(&setup, handle.clone());
        let (chat, root) = open_chat(Controlled::new(backend), &setup);
        root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
        let ops = HostOps::new(chat.clone());
        assert_eq!(
            op(
                &ops,
                "swarm.send",
                json!({ "to": "parser", "text": "rebase on main", "wake": true }),
                &call_from(root.id)
            )
            .unwrap(),
            json!({ "id": 1, "woke": true })
        );
        let child = child_task(&handle);
        let conversation = ConversationId(
            js(&chat.get_task(child).unwrap().unwrap().state)["checkpoint"]["conversation"]
                .as_u64()
                .unwrap(),
        );
        let child_conversation = chat.conversation(conversation).unwrap().unwrap();
        child_conversation.wait_for_idle(&bg()).unwrap();
        let entries = transcript(&child_conversation.entries().unwrap());
        assert_eq!(
            entries[entries.len() - 2..],
            [
                "pi.user:Message from root: rebase on main",
                "pi.assistant:re: Message from root: rebase on main"
            ]
        );
        op(
            &ops,
            "rlm.cancel",
            json!({ "selector": child.0.to_string() }),
            &call_from(root.id),
        )
        .unwrap();
        chat.close().unwrap();
    });
}

#[test]
fn harness_chat_swarm_keeps_a_durable_task_board_with_exclusive_claims() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        setup.fix_clock(3);
        let (chat, root) = open_chat(storage.clone(), &setup);
        let ops = HostOps::new(chat.clone());
        let [a, b] = conversations(&chat, 2)[..] else {
            unreachable!()
        };
        op(&ops, "swarm.register", json!({ "name": "alice" }), &call_from(a)).unwrap();
        op(&ops, "swarm.register", json!({ "name": "bob" }), &call_from(b)).unwrap();
        assert_eq!(
            op(
                &ops,
                "board.add",
                json!({ "title": "fix lexer", "detail": "escapes" }),
                &call_from(root.id)
            )
            .unwrap(),
            json!({ "id": 1, "title": "fix lexer", "status": "open", "by": "root", "notes": [], "updatedAt": 3, "detail": "escapes" })
        );
        op(&ops, "board.add", json!({ "title": "write docs" }), &call_from(root.id)).unwrap();
        assert_matches(
            &op(&ops, "board.claim", json!({ "id": 1 }), &call_from(a)).unwrap(),
            json!({ "status": "claimed", "owner": "alice" }),
        );
        assert_eq!(
            op(&ops, "board.claim", json!({ "id": 1 }), &call_from(b)).unwrap_err(),
            "Board task 1 is claimed by alice"
        );
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        let ops = HostOps::new(chat.clone());
        assert_matches(
            &op(
                &ops,
                "board.update",
                json!({ "id": 1, "status": "done", "note": "merged" }),
                &call_from(a),
            )
            .unwrap(),
            json!({ "status": "done", "owner": "alice", "notes": [{ "by": "alice", "text": "merged", "at": 3 }] }),
        );
        assert_eq!(
            op(&ops, "board.claim", json!({ "id": 1 }), &call_from(b)).unwrap_err(),
            "Board task 1 is done"
        );
        let open: Vec<Value> = op(&ops, "board.list", json!({ "status": "open" }), &call_from(root.id))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["title"].clone())
            .collect();
        assert_eq!(open, [json!("write docs")]);
        assert_eq!(
            op(&ops, "board.list", json!({}), &call_from(root.id))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            op(&ops, "board.update", json!({ "id": 7 }), &call_from(root.id)).unwrap_err(),
            "No board task 7"
        );
        chat.close().unwrap();
    }
}

#[test]
fn harness_chat_swarm_keeps_shared_notes_and_refuses_a_stale_writer() {
    {
        let backend = Backend::Replay;
        let storage = Controlled::new(backend);
        let setup = Setup::new();
        let (chat, root) = open_chat(storage.clone(), &setup);
        let ops = HostOps::new(chat.clone());
        let [a] = conversations(&chat, 1)[..] else {
            unreachable!()
        };
        op(&ops, "swarm.register", json!({ "name": "alice" }), &call_from(a)).unwrap();
        assert_eq!(
            op(&ops, "notes.read", json!({}), &call_from(root.id)).unwrap(),
            json!({ "text": "", "version": 0 })
        );
        op(
            &ops,
            "notes.append",
            json!({ "text": "plan: split the lexer" }),
            &call_from(root.id),
        )
        .unwrap();
        assert_eq!(
            op(
                &ops,
                "notes.append",
                json!({ "text": "lexer split done" }),
                &call_from(a)
            )
            .unwrap(),
            json!({ "version": 2 })
        );
        chat.close().unwrap();

        let (chat, root) = open_chat(storage.reopen(), &setup);
        let ops = HostOps::new(chat.clone());
        assert_eq!(
            op(&ops, "notes.read", json!({}), &call_from(root.id)).unwrap(),
            json!({ "text": "[root] plan: split the lexer\n[alice] lexer split done\n", "version": 2 })
        );
        assert_eq!(
            op(
                &ops,
                "notes.write",
                json!({ "text": "rewritten", "expect_version": 1 }),
                &call_from(root.id)
            )
            .unwrap_err(),
            "The notes changed (version 2, expected 1); read them again"
        );
        assert_eq!(
            op(
                &ops,
                "notes.write",
                json!({ "text": "rewritten", "expect_version": 2 }),
                &call_from(root.id)
            )
            .unwrap(),
            json!({ "version": 3 })
        );
        assert_eq!(
            op(&ops, "notes.read", json!({}), &call_from(root.id)).unwrap()["text"],
            "rewritten"
        );
        chat.close().unwrap();
    }
}
