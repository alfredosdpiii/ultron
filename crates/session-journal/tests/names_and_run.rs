//! Session names (user requirements; literal expectations) and the run journal's own invariants: progress frames
//! replay to the committed message, a batch of several tool calls commits its results in source order, and a reopened
//! journal is idle with the run's result recorded.

use serde_json::{Value, json};
use session_journal::names::{self, auto_name, clean_title, normalize_session_name, resolve_resume};
use session_journal::run::{self, FrameEncoder, Run};
use session_journal::{SessionRepo, TestClock};

fn repo(dir: &std::path::Path) -> SessionRepo {
    SessionRepo::new(dir, TestClock::shared(1_790_000_000_000, 1))
}

#[test]
fn session_journal_auto_name_rules() {
    assert_eq!(auto_name("Fix the login bug").as_deref(), Some("Fix the login bug"));
    assert_eq!(
        auto_name("\n\n   \n  second line wins\nthird").as_deref(),
        Some("second line wins")
    );
    assert_eq!(
        auto_name("/review @src/a.rs @b.md tighten   the\tparser").as_deref(),
        Some("tighten the parser")
    );
    assert_eq!(
        auto_name("## **Plan** the `migration` > now").as_deref(),
        Some("Plan the migration > now")
    );
    assert_eq!(
        auto_name("> - 1. quoted list item").as_deref(),
        Some("quoted list item")
    );
    assert_eq!(
        auto_name("Investigate why the session journal grows quadratically when frames are written").as_deref(),
        Some("Investigate why the session journal grows…")
    );
    let unbroken = "x".repeat(80);
    assert_eq!(auto_name(&unbroken), Some(format!("{}…", "x".repeat(50))));
    assert_eq!(
        auto_name("exactly fifty characters long, no cut needed here!"),
        Some("exactly fifty characters long, no cut needed here!".into())
    );
    assert_eq!(auto_name("/skill:diagram-it"), None);
    assert_eq!(auto_name("   \n\t"), None);
    assert_eq!(auto_name("@file.txt"), None);
}

#[test]
fn session_journal_clean_title_rules() {
    assert_eq!(clean_title("\"Fix Login Tests.\"").as_deref(), Some("Fix Login Tests"));
    assert_eq!(
        clean_title("**Refactor the parser**\nextra").as_deref(),
        Some("Refactor the parser")
    );
    assert_eq!(
        clean_title("Title: Session journal port!").as_deref(),
        Some("Session journal port")
    );
    assert_eq!(clean_title("“Quoted   words”").as_deref(), Some("Quoted words"));
    let long = "word ".repeat(20);
    assert_eq!(clean_title(&long).map(|t| t.chars().count() <= 60), Some(true));
    assert_eq!(
        clean_title(&long).as_deref(),
        Some("word word word word word word word word word word word word")
    );
    assert_eq!(clean_title("  \n"), None);
    assert_eq!(clean_title("\"...\""), None);
    assert_eq!(normalize_session_name("  a  "), Some("a".into()));
    assert_eq!(normalize_session_name("\u{feff} \u{a0}"), None);
}

#[test]
fn session_journal_user_name_wins_over_automatic_and_late_titles() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let mut session = repo.create("/w", None, None).unwrap();
    assert_eq!(
        names::apply_auto_name(&mut session, "Fix the cache")
            .unwrap()
            .as_deref(),
        Some("Fix the cache")
    );
    assert!(names::is_automatic(&session));
    assert!(names::replace_auto_name(&mut session, "Cache Fix").unwrap());
    assert_eq!(session.name().as_deref(), Some("Cache Fix"));
    assert!(names::is_automatic(&session));
    names::set_user_name(&mut session, Some("  Mine ")).unwrap();
    assert!(!names::is_automatic(&session));
    assert!(
        !names::replace_auto_name(&mut session, "Late Title").unwrap(),
        "a late title never replaces a user name"
    );
    assert_eq!(session.name().as_deref(), Some("Mine"));
    assert_eq!(names::apply_auto_name(&mut session, "another prompt").unwrap(), None);
    // A name another writer sets (the TypeScript product's setName) is the user's too.
    let mut other = repo.create("/w", None, None).unwrap();
    names::apply_auto_name(&mut other, "first").unwrap();
    other.set_name(Some("from typescript")).unwrap();
    assert!(!names::replace_auto_name(&mut other, "title").unwrap());
    // A session that already has messages is not named by a later prompt.
    let mut talked = repo.create("/w", None, None).unwrap();
    talked
        .commit(&[session_journal::Write::set("pi.branch.tip", "main", Value::Null)])
        .unwrap();
    talked
        .append_message("main", json!({"role": "user", "content": "hi", "timestamp": 1}))
        .unwrap();
    assert_eq!(names::apply_auto_name(&mut talked, "later prompt").unwrap(), None);
    // The listing reads the flag back from the file.
    drop((session, other, talked));
    let listed = names::list_sessions(dir.path(), "/w");
    let by_name: Vec<(Option<String>, bool)> = listed.iter().map(|s| (s.name.clone(), s.automatic_name)).collect();
    assert!(by_name.contains(&(Some("Mine".into()), false)));
    assert!(by_name.contains(&(Some("from typescript".into()), false)));
    assert!(by_name.contains(&(None, false)));
}

#[test]
fn session_journal_resume_selector_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let make = |id: &str, name: Option<&str>| {
        let mut s = repo.create("/w", Some(id), None).unwrap();
        names::set_user_name(&mut s, name).unwrap();
    };
    make("aaaa-1", Some("Alpha"));
    make("bbbb-1", Some("beta"));
    make("bbbb-2", Some("Beta"));
    make("cccc-1", None);
    // Newest first, as the listing orders them.
    let mut sessions = names::list_sessions(dir.path(), "/w");
    sessions.sort_by_key(|s| std::cmp::Reverse(s.metadata.created_at));
    let id = |s: &str| resolve_resume(&sessions, s).map(|x| x.metadata.id.clone());
    assert_eq!(id("cccc-1"), Ok("cccc-1".into()));
    assert_eq!(id("alpha"), Ok("aaaa-1".into()));
    assert_eq!(id("BETA"), Ok("bbbb-2".into()), "the most recent of two equal names");
    assert_eq!(id("al"), Ok("aaaa-1".into()));
    assert_eq!(id("cc"), Ok("cccc-1".into()));
    assert_eq!(
        id("b"),
        Err("Session selector 'b' is ambiguous; matches: bbbb-2 (Beta), bbbb-1 (beta)".into())
    );
    assert_eq!(
        id("zeta"),
        Err("No session found matching 'zeta' in this directory. Named sessions: bbbb-2 (Beta), bbbb-1 (beta), aaaa-1 (Alpha)".into())
    );
}

/// `reduceAssistantMessageFrames` for the frames this encoder writes.
fn replay(frames: &[Value]) -> Value {
    let mut message = Value::Null;
    let mut json_args: Vec<String> = Vec::new();
    for frame in frames {
        let index = frame["contentIndex"].as_u64().unwrap_or(0) as usize;
        match frame["type"].as_str().unwrap() {
            "start" => message = frame["partial"].clone(),
            "text_start" | "thinking_start" => {
                message["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(frame["content"].clone());
            }
            "text_delta" => {
                let text = message["content"][index]["text"].as_str().unwrap().to_string();
                message["content"][index]["text"] = json!(text + frame["delta"].as_str().unwrap());
            }
            "text_end" => message["content"][index]["text"] = frame["content"].clone(),
            "toolcall_start" => {
                message["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(frame["toolCall"].clone());
                json_args.resize(index + 1, String::new());
            }
            "toolcall_delta" => json_args[index].push_str(frame["delta"].as_str().unwrap()),
            "toolcall_end" => {
                assert_eq!(
                    serde_json::from_str::<Value>(&json_args[index]).unwrap(),
                    frame["arguments"]
                );
                message["content"][index]["arguments"] = frame["arguments"].clone();
            }
            other => panic!("unexpected frame {other}"),
        }
    }
    message
}

#[test]
fn session_journal_frames_replay_to_the_message() {
    let partial = |content: Value| {
        json!({"role": "assistant", "content": content, "api": "anthropic-messages", "provider": "anthropic", "model": "m",
               "usage": {"input": 1}, "stopReason": "stop", "timestamp": 5})
    };
    let call = |args: Value| json!({"type": "toolCall", "id": "c1", "name": "rlm", "arguments": args});
    let events = [
        json!({"type": "start", "partial": partial(json!([]))}),
        json!({"type": "text_start", "contentIndex": 0, "partial": partial(json!([{"type": "text", "text": ""}]))}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "Hel", "partial": partial(json!([{"type": "text", "text": "Hel"}]))}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "lo", "partial": partial(json!([{"type": "text", "text": "Hello"}]))}),
        json!({"type": "text_end", "contentIndex": 0, "content": "Hello", "partial": partial(json!([{"type": "text", "text": "Hello"}]))}),
        json!({"type": "toolcall_start", "contentIndex": 1, "partial": partial(json!([{"type": "text", "text": "Hello"}, call(json!({}))]))}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "{\"code\":", "partial": partial(json!([]))}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "", "partial": partial(json!([]))}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "\"1+1\"}", "partial": partial(json!([]))}),
        json!({"type": "toolcall_end", "contentIndex": 1, "toolCall": call(json!({"code": "1+1"})), "partial": partial(json!([]))}),
        json!({"type": "done", "reason": "toolUse", "message": partial(json!([]))}),
    ];
    let mut encoder = FrameEncoder::default();
    let frames: Vec<Value> = events.iter().filter_map(|e| encoder.encode(e)).collect();
    assert_eq!(
        frames.len(),
        9,
        "the empty delta and the terminal event write no frame: {frames:#?}"
    );
    assert_eq!(frames[0]["partial"]["stopReason"], "pending");
    let message = replay(&frames);
    assert_eq!(
        message["content"],
        json!([{"type": "text", "text": "Hello"}, call(json!({"code": "1+1"}))])
    );
    // Nothing is encoded before the stream starts.
    assert_eq!(FrameEncoder::default().encode(&events[2]), None);
}

#[test]
fn session_journal_run_commits_a_tool_batch_in_source_order() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let mut session = repo.create("/w", Some("run"), None).unwrap();
    let config = run::ensure_lane(&mut session, &run::lane_config("anthropic", "m", "low", &["rlm"])).unwrap();
    let prompt = json!({"role": "user", "content": [{"type": "text", "text": "go"}], "timestamp": 7});
    let mut journal = Run::admit(&mut session, config, run::default_settings(), prompt).unwrap();
    journal.begin_assistant(&mut session, 100, 1000).unwrap();
    let call = |id: &str| json!({"type": "toolCall", "id": id, "name": "rlm", "arguments": {"code": id}});
    let assistant = json!({"role": "assistant", "content": [{"type": "text", "text": "two calls"}, call("c1"), call("c2")],
        "api": "x", "provider": "anthropic", "model": "m", "usage": {"totalTokens": 3, "cost": {"total": 0.5}}, "stopReason": "toolUse", "timestamp": 8});
    journal.record_turn(&mut session, &assistant).unwrap();
    assert!(journal.commit_assistant(&mut session, assistant).unwrap());
    let result = |id: &str| json!({"role": "toolResult", "toolCallId": id, "toolName": "rlm", "content": [{"type": "text", "text": id}], "details": {}, "isError": id == "c1", "timestamp": 9});
    journal.tool_started(&mut session, "c1", json!({"code": "c1"})).unwrap();
    journal.tool_started(&mut session, "c2", json!({"code": "c2"})).unwrap();
    // c2 finishes first; the entries still follow the calls' order.
    journal.tool_finished(&mut session, "c2", result("c2")).unwrap();
    assert_eq!(
        session.branch_entries("main").unwrap().len(),
        2,
        "results wait for the whole batch"
    );
    journal.tool_finished(&mut session, "c1", result("c1")).unwrap();
    journal.begin_assistant(&mut session, 100, 1000).unwrap();
    let last = json!({"role": "assistant", "content": [{"type": "text", "text": "done"}], "api": "x", "provider": "anthropic",
        "model": "m", "usage": {"totalTokens": 2}, "stopReason": "stop", "timestamp": 10});
    journal.record_turn(&mut session, &last).unwrap();
    assert!(!journal.commit_assistant(&mut session, last).unwrap());
    let operation = journal.operation_id().to_string();
    journal.finish(&mut session, None).unwrap();
    let metadata = session.metadata().clone();
    drop(session);

    let reopened = repo.open(&metadata).unwrap();
    let roles: Vec<String> = reopened
        .branch_entries("main")
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{}:{}",
                e["message"]["role"].as_str().unwrap(),
                e["message"]["toolCallId"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        roles,
        ["user:", "assistant:", "toolResult:c1", "toolResult:c2", "assistant:"]
    );
    let c1 = &reopened.branch_entries("main").unwrap()[2]["message"];
    assert!(c1.get("details").is_none(), "a failed tool's result has no details");
    assert_eq!(
        reopened.value("pi.lane.state", "main").unwrap(),
        &json!({"currentOperationId": null, "lastOperationId": operation, "inbox": []})
    );
    assert_eq!(reopened.value("pi.result", &operation).unwrap()["status"], "completed");
    assert!(reopened.state().scan_values("pi.op.", "").is_empty());
    for namespace in ["pi.op.meta", "pi.op.state", "pi.op.tool_args", "pi.pending.entry"] {
        assert!(
            reopened.state().scan_values(namespace, "").is_empty(),
            "{namespace} is cleared"
        );
    }
    assert!(
        reopened.state().lists().is_empty(),
        "frames are deleted with the response"
    );
    let turns = &reopened.value("ultron.usage", "root").unwrap()["roots"][format!("turn:{operation}")]["turns"];
    assert_eq!(
        (
            turns["count"].clone(),
            turns["tokens"].clone(),
            turns["cost"].clone(),
            turns["unpriced"].clone()
        ),
        (json!(2), json!(5), json!(0.5), json!(1))
    );
    let rows = reopened.state().usage_rows();
    assert_eq!(rows.len(), 2, "one usage row per response");
}

#[test]
fn session_journal_ensure_lane_keeps_a_resumed_lane() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repo(dir.path());
    let mut session = repo.create("/w", None, None).unwrap();
    let first = run::lane_config("anthropic", "a", "high", &["rlm"]);
    assert_eq!(run::ensure_lane(&mut session, &first).unwrap(), first);
    let path = session.metadata().path.clone();
    let lines = || std::fs::read_to_string(&path).unwrap().lines().count();
    let before = lines();
    // Another model and thinking level on resume: the stored ones win and nothing is written.
    let other = run::lane_config("groq", "b", "off", &["rlm"]);
    assert_eq!(run::ensure_lane(&mut session, &other).unwrap(), first);
    assert_eq!(lines(), before);
    // Different tools: only the tool names follow, in one write.
    let tools = run::lane_config("groq", "b", "off", &["rlm", "extra"]);
    let config = run::ensure_lane(&mut session, &tools).unwrap();
    assert_eq!(config, run::lane_config("anthropic", "a", "high", &["rlm", "extra"]));
    assert_eq!(lines(), before + 1);
}
