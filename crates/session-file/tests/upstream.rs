//! Ported expectations from packages/coding-agent/test/session-manager/*.test.ts and
//! test/session-context-edit.test.ts (format behavior), plus JavaScript-semantics literals taken from node 26.

use std::fs;

use serde_json::{Value, json};
use session_file::{
    Env, Leaf, NewSessionOptions, SessionManager, SystemEnv, Uuidv7, build_context_entries, build_session_context,
    find_most_recent_session, js, list_all_sessions, list_sessions, load_entries_from_file, migrate_session_entries,
};

/// Real clock and randomness; the agent dir is a temp dir, never the user's.
struct TestEnv(String);

impl Env for TestEnv {
    fn now_ms(&mut self) -> i64 {
        SystemEnv.now_ms()
    }
    fn random_uuid(&mut self) -> String {
        SystemEnv.random_uuid()
    }
    fn session_id(&mut self) -> String {
        SystemEnv.session_id()
    }
    fn agent_dir(&self) -> String {
        self.0.clone()
    }
}

fn env() -> Box<dyn Env> {
    Box::new(TestEnv("/nonexistent-agent-dir".into()))
}

fn mem() -> SessionManager {
    SessionManager::in_memory(env(), "/project", None, None).unwrap()
}

fn tmp() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_string_lossy().into_owned();
    (dir, path)
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text, "timestamp": 1 })
}

fn assistant(text: &str) -> Value {
    json!({
        "role": "assistant", "content": [{ "type": "text", "text": text }], "api": "anthropic-messages",
        "provider": "anthropic", "model": "claude-test",
        "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
        "stopReason": "stop", "timestamp": 1,
    })
}

fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    json!({ "type": "message", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "message": if role == "user" { user(text) } else { assistant(text) } })
}

fn compaction(id: &str, parent: &str, summary: &str, first_kept: &str) -> Value {
    json!({ "type": "compaction", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "summary": summary, "firstKeptEntryId": first_kept, "tokensBefore": 1000 })
}

fn branch_summary(id: &str, parent: &str, summary: &str, from: &str) -> Value {
    json!({ "type": "branch_summary", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "summary": summary, "fromId": from })
}

fn text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn roles(messages: &[Value]) -> Vec<&str> {
    messages.iter().map(|m| m["role"].as_str().unwrap()).collect()
}

fn id(v: &str) -> Value {
    Value::from(v)
}

// ---- build-context.test.ts ----

#[test]
fn session_file_context_of_empty_entries_is_empty_off_and_modelless() {
    let ctx = build_session_context(&[], Leaf::Last);
    assert_eq!(ctx.messages, Vec::<Value>::new());
    assert_eq!(ctx.thinking_level, Some(json!("off")));
    assert_eq!(ctx.model, None);
}

#[test]
fn session_file_context_tracks_thinking_and_assistant_model_over_model_change() {
    let entries = [
        msg("1", None, "user", "hello"),
        json!({ "type": "thinking_level_change", "id": "2", "parentId": "1", "timestamp": "t", "thinkingLevel": "high" }),
        json!({ "type": "model_change", "id": "3", "parentId": "2", "timestamp": "t", "provider": "openai", "modelId": "gpt-4" }),
        msg("4", Some("3"), "assistant", "hi"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.thinking_level, Some(json!("high")));
    assert_eq!(roles(&ctx.messages), ["user", "assistant"]);
    assert_eq!(
        ctx.model,
        Some(json!({ "provider": "anthropic", "modelId": "claude-test" }))
    );
    let ctx = build_session_context(&entries[..3], Leaf::Last);
    assert_eq!(ctx.model, Some(json!({ "provider": "openai", "modelId": "gpt-4" })));
}

#[test]
fn session_file_compaction_puts_summary_before_kept_messages() {
    let entries = [
        msg("1", None, "user", "first"),
        msg("2", Some("1"), "assistant", "response1"),
        msg("3", Some("2"), "user", "second"),
        msg("4", Some("3"), "assistant", "response2"),
        compaction("5", "4", "Summary of first two turns", "3"),
        msg("6", Some("5"), "user", "third"),
        msg("7", Some("6"), "assistant", "response3"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.messages.len(), 5);
    assert_eq!(ctx.messages[0]["summary"], "Summary of first two turns");
    assert_eq!(ctx.messages[0]["tokensBefore"], 1000);
    assert_eq!(ctx.messages[0]["timestamp"], 1_735_689_600_000_i64);
    assert_eq!(
        ctx.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["second", "response2", "third", "response3"]
    );
}

#[test]
fn session_file_multiple_compactions_use_the_latest() {
    let entries = [
        msg("1", None, "user", "a"),
        msg("2", Some("1"), "assistant", "b"),
        compaction("3", "2", "First summary", "1"),
        msg("4", Some("3"), "user", "c"),
        msg("5", Some("4"), "assistant", "d"),
        compaction("6", "5", "Second summary", "4"),
        msg("7", Some("6"), "user", "e"),
    ];
    let ctx = build_session_context(&entries, Leaf::Last);
    assert_eq!(ctx.messages.len(), 4);
    assert_eq!(ctx.messages[0]["summary"], "Second summary");
}

#[test]
fn session_file_context_entries_keep_custom_entries_but_messages_skip_them() {
    let custom = |id: &str, parent: &str| json!({ "type": "custom", "id": id, "parentId": parent, "timestamp": "t", "customType": "c", "data": {} });
    let entries = [
        msg("1", None, "user", "first"),
        custom("2", "1"),
        msg("3", Some("2"), "assistant", "response1"),
        custom("4", "3"),
        msg("5", Some("4"), "user", "second"),
        compaction("6", "5", "Summary", "4"),
        custom("7", "6"),
        msg("8", Some("7"), "assistant", "response2"),
    ];
    let ids: Vec<&Value> = build_context_entries(&entries, Leaf::Last)
        .iter()
        .map(|e| &e["id"])
        .collect();
    assert_eq!(ids, [&id("6"), &id("4"), &id("5"), &id("7"), &id("8")]);
    assert_eq!(
        roles(&build_session_context(&entries, Leaf::Last).messages),
        ["compactionSummary", "user", "assistant"]
    );
}

#[test]
fn session_file_context_follows_the_selected_branch_with_branch_summaries() {
    let entries = [
        msg("1", None, "user", "start"),
        msg("2", Some("1"), "assistant", "r1"),
        msg("3", Some("2"), "user", "q2"),
        msg("4", Some("3"), "assistant", "r2"),
        compaction("5", "4", "Compacted history", "3"),
        msg("6", Some("5"), "user", "q3"),
        msg("7", Some("6"), "assistant", "r3"),
        msg("8", Some("3"), "user", "wrong path"),
        msg("9", Some("8"), "assistant", "wrong response"),
        branch_summary("10", "3", "Tried wrong approach", "9"),
        msg("11", Some("10"), "user", "better approach"),
    ];
    let main = build_session_context(&entries, Leaf::Id(&id("7")));
    assert_eq!(main.messages[0]["summary"], "Compacted history");
    assert_eq!(
        main.messages[1..].iter().map(text).collect::<Vec<_>>(),
        ["q2", "r2", "q3", "r3"]
    );
    let branch = build_session_context(&entries, Leaf::Id(&id("11")));
    assert_eq!(
        roles(&branch.messages),
        ["user", "assistant", "user", "branchSummary", "user"]
    );
    assert_eq!(
        branch.messages[3],
        json!({ "role": "branchSummary", "summary": "Tried wrong approach", "fromId": "9", "timestamp": 1_735_689_600_000_i64 })
    );
    assert_eq!(text(&branch.messages[4]), "better approach");
}

#[test]
fn session_file_unknown_leaf_uses_last_entry_and_orphans_stand_alone() {
    let entries = [msg("1", None, "user", "hello"), msg("2", Some("1"), "assistant", "hi")];
    assert_eq!(
        build_session_context(&entries, Leaf::Id(&id("nonexistent")))
            .messages
            .len(),
        2
    );
    assert_eq!(build_session_context(&entries, Leaf::Root).messages.len(), 0);
    let orphan = [
        msg("1", None, "user", "hello"),
        msg("2", Some("missing"), "assistant", "orphan"),
    ];
    assert_eq!(build_session_context(&orphan, Leaf::Id(&id("2"))).messages.len(), 1);
}

// ---- migration.test.ts ----

#[test]
fn session_file_v1_entries_get_ids_parent_chain_and_version_3() {
    let mut entries = vec![
        json!({ "type": "session", "id": "sess-1", "timestamp": "2025-01-01T00:00:00Z", "cwd": "/tmp" }),
        json!({ "type": "message", "timestamp": "2025-01-01T00:00:01Z", "message": user("hi") }),
        json!({ "type": "message", "timestamp": "2025-01-01T00:00:02Z", "message": assistant("hello") }),
    ];
    let mut e = TestEnv(String::new());
    assert!(migrate_session_entries(&mut entries, &mut e));
    assert_eq!(entries[0]["version"], 3);
    assert_eq!(entries[1]["id"].as_str().unwrap().len(), 8);
    assert_eq!(entries[1]["parentId"], Value::Null);
    assert_eq!(entries[2]["id"].as_str().unwrap().len(), 8);
    assert_eq!(entries[2]["parentId"], entries[1]["id"]);
}

#[test]
fn session_file_migration_skips_current_entries() {
    let mut entries = vec![
        json!({ "type": "session", "id": "sess-1", "version": 2, "timestamp": "t", "cwd": "/tmp" }),
        json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "t", "message": user("hi") }),
        json!({ "type": "message", "id": "def67890", "parentId": "abc12345", "timestamp": "t", "message": assistant("x") }),
    ];
    migrate_session_entries(&mut entries, &mut TestEnv(String::new()));
    assert_eq!(entries[1]["id"], "abc12345");
    assert_eq!(entries[2]["id"], "def67890");
    assert_eq!(entries[2]["parentId"], "abc12345");
    assert!(!migrate_session_entries(&mut entries, &mut TestEnv(String::new())));
}

// ---- save-entry.test.ts, labels.test.ts, tree-traversal.test.ts ----

#[test]
fn session_file_custom_entries_join_the_tree_but_not_the_context() {
    let mut s = mem();
    let m1 = s.append_message(user("hello")).unwrap();
    let c = s.append_custom_entry("my_data", Some(json!({ "foo": "bar" }))).unwrap();
    let m2 = s.append_message(assistant("hi")).unwrap();
    let custom = s.entry(&c).unwrap();
    assert_eq!(custom["customType"], "my_data");
    assert_eq!(custom["data"], json!({ "foo": "bar" }));
    assert_eq!(custom["parentId"], Value::from(m1.as_str()));
    let path: Vec<&Value> = s.branch_entries(None).iter().map(|e| &e["id"]).collect();
    assert_eq!(path, [&id(&m1), &id(&c), &id(&m2)]);
    assert_eq!(s.build_session_context().messages.len(), 2);
}

#[test]
fn session_file_labels_set_clear_and_last_wins_with_timestamps() {
    let mut s = mem();
    let m = s.append_message(user("hello")).unwrap();
    assert_eq!(s.label(&m), None);
    s.append_label_change(&m, Some("checkpoint")).unwrap();
    assert_eq!(s.label(&m), Some(&json!("checkpoint")));
    s.append_label_change(&m, None).unwrap();
    assert_eq!(s.label(&m), None);
    s.append_label_change(&m, Some("first")).unwrap();
    let last = s.append_label_change(&m, Some("third")).unwrap();
    assert_eq!(s.label(&m), Some(&json!("third")));
    let tree = s.tree();
    let node = &tree.nodes[tree.roots[0]];
    assert_eq!(node.label_timestamp, s.entry(&last).map(|e| &e["timestamp"]));
    assert_eq!(s.build_session_context().messages.len(), 1);
    assert_eq!(
        s.append_label_change("non-existent", Some("x"))
            .unwrap_err()
            .to_string(),
        "Entry non-existent not found"
    );
}

#[test]
fn session_file_branched_session_keeps_path_labels_and_rewires_label_children() {
    let mut s = mem();
    let m1 = s.append_message(user("hello")).unwrap();
    s.append_label_change(&m1, Some("checkpoint")).unwrap();
    let model = s.append_model_change("anthropic", "claude-test").unwrap();
    let m2 = s.append_message(user("followup")).unwrap();
    let m3 = s.append_message(user("dropped")).unwrap();
    s.append_label_change(&m3, Some("third")).unwrap();
    s.create_branched_session(&m2).unwrap();
    assert_eq!(s.entry(&model).unwrap()["parentId"], Value::from(m1.as_str()));
    assert_eq!(s.label(&m1), Some(&json!("checkpoint")));
    assert_eq!(s.label(&m3), None);
    let labels: Vec<&Value> = s.entries().into_iter().filter(|e| e["type"] == "label").collect();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0]["parentId"], Value::from(m2.as_str()));
    assert_eq!(
        s.create_branched_session("nonexistent").unwrap_err().to_string(),
        "Entry nonexistent not found"
    );
}

#[test]
fn session_file_branch_moves_leaf_and_new_appends_fork_the_tree() {
    let mut s = mem();
    assert!(s.branch_entries(None).is_empty());
    assert!(s.tree().roots.is_empty());
    assert!(s.leaf_entry().is_none());
    let a = s.append_message(user("1")).unwrap();
    s.append_message(assistant("2")).unwrap();
    let c = s.append_message(user("3")).unwrap();
    s.branch(&a).unwrap();
    assert_eq!(s.leaf_id(), Some(&id(&a)));
    let d = s.append_message(user("4")).unwrap();
    assert_eq!(s.entry(&d).unwrap()["parentId"], Value::from(a.as_str()));
    let tree = s.tree();
    assert_eq!(tree.roots.len(), 1);
    assert_eq!(tree.nodes[tree.roots[0]].children.len(), 2);
    assert_eq!(
        s.branch("nonexistent").unwrap_err().to_string(),
        "Entry nonexistent not found"
    );
    let usage = json!({ "input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40, "totalTokens": 100,
        "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0.3, "cacheWrite": 0.4, "total": 1 } });
    s.branch(&c).unwrap();
    let summary = s
        .branch_with_summary(
            Some(&a),
            "Summary of abandoned work",
            None,
            Some(false),
            Some(usage.clone()),
        )
        .unwrap();
    let entry = s.entry(&summary).unwrap();
    assert_eq!(s.leaf_id(), Some(&id(&summary)));
    assert_eq!(entry["parentId"], Value::from(a.as_str()));
    assert_eq!(entry["fromId"], Value::from(c.as_str()));
    assert_eq!(entry["usage"], usage);
    assert_eq!(
        s.branch_with_summary(Some("nonexistent"), "summary", None, None, None)
            .unwrap_err()
            .to_string(),
        "Entry nonexistent not found"
    );
}

#[test]
fn session_file_forking_from_first_user_message_defers_then_writes_one_header() {
    let (_dir, path) = tmp();
    let mut s = SessionManager::create(env(), &path, Some(&path), None).unwrap();
    let id1 = s.append_message(user("first question")).unwrap();
    s.append_message(assistant("first answer")).unwrap();
    let file = s.create_branched_session(&id1).unwrap().unwrap();
    assert!(!fs::exists(&file).unwrap());
    s.append_custom_entry("preset-state", Some(json!({ "name": "plan" })))
        .unwrap();
    s.append_message(assistant("new answer")).unwrap();
    let content = fs::read_to_string(&file).unwrap();
    let records: Vec<Value> = content.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(records.iter().filter(|r| r["type"] == "session").count(), 1);
    let ids: std::collections::HashSet<&Value> = records[1..].iter().map(|r| &r["id"]).collect();
    assert_eq!(ids.len(), records.len() - 1);
    // Forking where the path has an assistant writes immediately.
    let assistant_id = s.entries()[2]["id"].as_str().unwrap().to_string();
    let file = s.create_branched_session(&assistant_id).unwrap().unwrap();
    assert!(fs::exists(&file).unwrap());
}

#[test]
fn session_file_usage_survives_a_file_backed_reload() {
    let (_dir, path) = tmp();
    let mut s = SessionManager::create(env(), &path, Some(&path), None).unwrap();
    let root = s.append_message(user("question")).unwrap();
    s.append_message(assistant("answer")).unwrap();
    let usage = json!({ "input": 10, "output": 20, "totalTokens": 100, "cost": { "input": 0.1, "total": 1 } });
    s.append_message(
        json!({ "role": "toolResult", "toolCallId": "call-1", "toolName": "nested-model",
        "content": [{ "type": "text", "text": "result" }], "isError": false, "usage": usage, "timestamp": 1 }),
    )
    .unwrap();
    s.append_compaction("summary", Some(&root), 100, None, Some(false), Some(usage.clone()))
        .unwrap();
    s.branch_with_summary(Some(&root), "branch summary", None, Some(false), Some(usage.clone()))
        .unwrap();
    let reopened = SessionManager::open(env(), s.session_file().unwrap(), Some(&path), None).unwrap();
    let entries = reopened.entries();
    assert!(entries.iter().any(|e| e["type"] == "compaction" && e["usage"] == usage));
    assert!(
        entries
            .iter()
            .any(|e| e["type"] == "branch_summary" && e["usage"] == usage)
    );
    assert!(
        entries
            .iter()
            .any(|e| e["message"]["role"] == "toolResult" && e["message"]["usage"] == usage)
    );
}

// ---- load-entries.test.ts ----

#[test]
fn session_file_preloaded_entries_are_adopted_with_leaf_labels_and_tree() {
    let mut source = mem();
    let first = source.append_message(user("hello")).unwrap();
    source.append_message(user("abandoned")).unwrap();
    source.branch(&first).unwrap();
    source.append_label_change(&first, Some("checkpoint")).unwrap();
    let last = source.append_message(user("kept")).unwrap();
    let stored: Vec<Value> = source.entries().into_iter().cloned().collect();
    let mut s = SessionManager::in_memory(env(), "/project", None, Some(stored.clone())).unwrap();
    assert_eq!(s.entries().into_iter().cloned().collect::<Vec<_>>(), stored);
    assert_eq!(s.label(&first), Some(&json!("checkpoint")));
    assert_eq!(s.tree().nodes[s.tree().roots[0]].children.len(), 2);
    let appended = s.append_message(user("continued")).unwrap();
    assert_eq!(s.entry(&appended).unwrap()["parentId"], Value::from(last.as_str()));
    assert!(!stored.iter().any(|e| e["id"] == appended.as_str()));
    assert_eq!(s.session_file(), None);
    assert!(!s.is_persisted());
}

#[test]
fn session_file_preloaded_headers_set_identity_and_migrate() {
    let opts = |id: &str| {
        Some(NewSessionOptions {
            id: Some(id.into()),
            parent_session: None,
        })
    };
    let body = vec![
        json!({ "type": "message", "id": "abc12345", "parentId": null, "timestamp": "t",
        "message": { "role": "hookMessage", "content": "from a hook", "timestamp": 1 } }),
    ];
    let headerless =
        SessionManager::in_memory(env(), "/project", opts("restored-session"), Some(body.clone())).unwrap();
    assert_eq!(headerless.session_id(), "restored-session");
    assert_eq!(headerless.header().unwrap()["cwd"], "/project");
    assert_eq!(headerless.entries()[0]["message"]["role"], "hookMessage");
    let mut with_header =
        vec![json!({ "type": "session", "version": 2, "id": "stored-session", "timestamp": "t", "cwd": "/stored" })];
    with_header.extend(body);
    let s = SessionManager::in_memory(env(), "/project", opts("ignored"), Some(with_header)).unwrap();
    assert_eq!(s.session_id(), "stored-session");
    assert_eq!(s.header().unwrap()["cwd"], "/stored");
    assert_eq!(s.header().unwrap()["version"], 3);
    assert_eq!(s.entries()[0]["message"]["role"], "custom");
    assert_eq!(s.entries()[0]["id"], "abc12345");
    let empty = SessionManager::in_memory(env(), "/project", opts("empty-session"), Some(vec![])).unwrap();
    assert_eq!(empty.session_id(), "empty-session");
    assert!(empty.entries().is_empty());
    assert_eq!(empty.leaf_id(), None);
}

// ---- custom-session-id.test.ts ----

fn is_uuid_v7(id: &str) -> bool {
    let b = id.as_bytes();
    id.len() == 36
        && [8, 13, 18, 23].iter().all(|i| b[*i] == b'-')
        && b[14] == b'7'
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
        && id
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[test]
fn session_file_session_ids_are_validated_or_uuid_v7() {
    let mut s = mem();
    assert!(is_uuid_v7(s.session_id()));
    assert_eq!(s.header().unwrap()["id"], Value::from(s.session_id()));
    for good in ["my-custom-id", "abc-123_def.456"] {
        s.new_session(Some(NewSessionOptions {
            id: Some(good.into()),
            parent_session: None,
        }))
        .unwrap();
        assert_eq!(s.session_id(), good);
    }
    for bad in [
        "", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def",
    ] {
        let err = s
            .new_session(Some(NewSessionOptions {
                id: Some(bad.into()),
                parent_session: None,
            }))
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Session id must be non-empty, contain only alphanumeric characters"),
            "{bad}"
        );
    }
    s.new_session(Some(NewSessionOptions {
        id: None,
        parent_session: Some("parent.jsonl".into()),
    }))
    .unwrap();
    assert!(is_uuid_v7(s.session_id()));
    assert_eq!(s.header().unwrap()["parentSession"], "parent.jsonl");
}

#[test]
fn session_file_created_and_forked_files_are_named_timestamp_underscore_id() {
    let (_dir, path) = tmp();
    let opts = Some(NewSessionOptions {
        id: Some("created-session-id".into()),
        parent_session: None,
    });
    let s = SessionManager::create(env(), &path, Some(&path), opts).unwrap();
    let file = s.session_file().unwrap().rsplit('/').next().unwrap().to_string();
    assert!(
        file.ends_with("Z_created-session-id.jsonl")
            && file.len() == "2025-01-01T00-00-00-000Z_created-session-id.jsonl".len()
    );
    assert!(!fs::exists(s.session_file().unwrap()).unwrap());
    let source = format!("{path}/source.jsonl");
    fs::write(
        &source,
        format!("{{\"type\":\"session\",\"version\":3,\"id\":\"src\",\"timestamp\":\"t\",\"cwd\":\"{path}\"}}\n"),
    )
    .unwrap();
    let forked = SessionManager::fork_from(env(), &source, &path, Some(&path), None).unwrap();
    assert!(is_uuid_v7(forked.header().unwrap()["id"].as_str().unwrap()));
    assert_eq!(forked.header().unwrap()["parentSession"], Value::from(source.as_str()));
}

// ---- file-operations.test.ts ----

#[test]
fn session_file_load_entries_repairs_only_session_files() {
    let (_dir, path) = tmp();
    let file = |name: &str, content: &str| {
        let p = format!("{path}/{name}");
        fs::write(&p, content).unwrap();
        p
    };
    assert!(
        load_entries_from_file(&format!("{path}/nonexistent.jsonl"))
            .unwrap()
            .is_empty()
    );
    assert!(load_entries_from_file(&file("empty.jsonl", "")).unwrap().is_empty());
    assert!(
        load_entries_from_file(&file("no-header.jsonl", "{\"type\":\"message\",\"id\":\"1\"}\n"))
            .unwrap()
            .is_empty()
    );
    assert!(
        load_entries_from_file(&file("malformed.jsonl", "not json\n"))
            .unwrap()
            .is_empty()
    );
    let header = "{\"type\":\"session\",\"id\":\"abc\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n";
    let message = "{\"type\":\"message\",\"id\":\"1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\",\"timestamp\":1}}";
    let mixed = load_entries_from_file(&file("mixed.jsonl", &format!("{header}not valid json\n{message}\n"))).unwrap();
    assert_eq!(mixed.len(), 2);
    let unterminated = file("unterminated.jsonl", &format!("{header}{message}"));
    assert_eq!(load_entries_from_file(&unterminated).unwrap().len(), 2);
    assert_eq!(
        fs::read_to_string(&unterminated).unwrap(),
        format!("{header}{message}\n")
    );
    let tail = file("malformed-tail.jsonl", &format!("{header}{{\"type\":\"message\""));
    assert_eq!(load_entries_from_file(&tail).unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(&tail).unwrap(),
        format!("{header}{{\"type\":\"message\"\n")
    );
    let invalid = file("invalid.jsonl", "{\"type\":\"message\",\"id\":\"1\"}");
    assert!(load_entries_from_file(&invalid).unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(&invalid).unwrap(),
        "{\"type\":\"message\",\"id\":\"1\"}"
    );
}

#[test]
fn session_file_open_reads_cwd_past_blank_and_malformed_prefixes_and_scan_limit() {
    let (_dir, path) = tmp();
    let stored = format!("{path}/stored-project");
    let write = |name: &str, prefix: &str, id: &str| {
        let p = format!("{path}/{name}.jsonl");
        fs::write(&p, format!("{prefix}{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"{stored}\"}}\n")).unwrap();
        p
    };
    let big = "a".repeat(1024 * 1024 + 1);
    let cases = [
        write("blank", "\n  \n", "leading-blank"),
        write("malformed", "not json\n{broken json\n", "leading-malformed"),
        write("multi", "", &"a".repeat(8192)),
        write("large-header", "", &big),
        write(
            "large-prefix",
            &format!("{}\n", "x".repeat(1024 * 1024 + 1)),
            "large-prefix",
        ),
    ];
    let ids = [
        "leading-blank",
        "leading-malformed",
        &"a".repeat(8192),
        &big,
        "large-prefix",
    ];
    for (file, id) in cases.iter().zip(ids) {
        for cwd_override in [None, Some(format!("{path}/override"))] {
            let s = SessionManager::open(env(), file, Some(&path), cwd_override.as_deref()).unwrap();
            assert_eq!(s.session_id(), id);
            assert_eq!(s.cwd(), cwd_override.unwrap_or_else(|| stored.clone()));
        }
    }
}

#[test]
fn session_file_open_initializes_empty_files_and_refuses_other_content() {
    let (_dir, path) = tmp();
    let empty = format!("{path}/empty.jsonl");
    fs::write(&empty, "").unwrap();
    let s = SessionManager::open(env(), &empty, Some(&path), None).unwrap();
    assert_eq!(s.session_file(), Some(empty.as_str()));
    let lines: Vec<Value> = fs::read_to_string(&empty)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["id"], Value::from(s.session_id()));
    let again = SessionManager::open(env(), &empty, Some(&path), None).unwrap();
    assert_eq!(again.session_id(), s.session_id());
    for (name, content) in [
        (
            "no-header.jsonl",
            "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n",
        ),
        ("not-a-session.log", "{\"type\":\"event\",\"data\":\"not a session\"}\n"),
    ] {
        let file = format!("{path}/{name}");
        fs::write(&file, content).unwrap();
        let err = SessionManager::open(env(), &file, Some(&path), None).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("Session file is not a valid ultron session: {file}")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), content);
    }
}

fn set_mtime(path: &str, ms: u64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[test]
fn session_file_most_recent_session_skips_invalid_files_and_filters_by_cwd() {
    let (_dir, path) = tmp();
    assert_eq!(find_most_recent_session(&path, None), None);
    assert_eq!(find_most_recent_session(&format!("{path}/nonexistent"), None), None);
    fs::write(format!("{path}/file.txt"), "hello").unwrap();
    fs::write(format!("{path}/invalid.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    fs::write(format!("{path}/oversized.jsonl"), "x".repeat(1024 * 1024 + 1)).unwrap();
    assert_eq!(find_most_recent_session(&path, None), None);
    let write = |name: &str, cwd: &str, mtime: u64| {
        let p = format!("{path}/{name}");
        fs::write(
            &p,
            format!("{{\"type\":\"session\",\"id\":\"{name}\",\"timestamp\":\"t\",\"cwd\":\"{cwd}\"}}\n"),
        )
        .unwrap();
        set_mtime(&p, mtime);
        p
    };
    let a = write("a.jsonl", &format!("{path}/project-a"), 1_000_000);
    let b = write("b.jsonl", &format!("{path}/project-b"), 2_000_000);
    set_mtime(&format!("{path}/invalid.jsonl"), 3_000_000);
    assert_eq!(find_most_recent_session(&path, None), Some(b.clone()));
    assert_eq!(
        find_most_recent_session(&path, Some(&format!("{path}/project-a"))),
        Some(a)
    );
    assert_eq!(
        find_most_recent_session(&path, Some(&format!("{path}/project-b"))),
        Some(b)
    );
}

#[test]
fn session_file_flat_dir_lists_by_cwd_and_continues_the_recent_one() {
    let (_dir, path) = tmp();
    let make = |cwd: &str, label: &str| {
        let mut s = SessionManager::create(env(), cwd, Some(&path), None).unwrap();
        s.append_message(user(label)).unwrap();
        s.append_message(assistant(&format!("reply to {label}"))).unwrap();
        s.session_file().unwrap().to_string()
    };
    let a = make(&format!("{path}/project-a"), "from A");
    let b = make(&format!("{path}/project-b"), "from B");
    set_mtime(&a, 1_000_000);
    set_mtime(&b, 2_000_000);
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let current: Vec<String> = list_sessions(&env_box, &format!("{path}/project-a"), Some(&path))
        .unwrap()
        .into_iter()
        .map(|s| s.path)
        .collect();
    assert_eq!(current, std::slice::from_ref(&a));
    let mut all: Vec<String> = list_all_sessions(&env_box, Some(&path))
        .into_iter()
        .map(|s| s.path)
        .collect();
    all.sort();
    let mut expected = vec![a.clone(), b];
    expected.sort();
    assert_eq!(all, expected);
    let continued = SessionManager::continue_recent(env(), &format!("{path}/project-a"), Some(&path)).unwrap();
    assert_eq!(continued.session_file(), Some(a.as_str()));
}

#[test]
fn session_file_session_info_modified_uses_last_message_timestamp() {
    let (_dir, path) = tmp();
    let file = format!("{path}/s.jsonl");
    fs::write(&file, "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n").unwrap();
    let mut s = SessionManager::open(env(), &file, None, None).unwrap();
    let mut message = assistant("later");
    message["timestamp"] = json!(1_700_000_123_456_i64);
    s.append_message(message).unwrap();
    let env_box = TestEnv("/nonexistent-agent-dir".into());
    let info = list_sessions(&env_box, "/tmp", Some(&path)).unwrap();
    assert_eq!(info[0].modified, Some(1_700_000_123_456));
    assert_eq!(info[0].created, Some(1_735_689_600_000));
}

// ---- session-context-edit.test.ts ----

fn tool_result(text: &str) -> Value {
    json!({ "role": "toolResult", "toolCallId": "call-1", "toolName": "read", "content": [{ "type": "text", "text": text }],
        "details": { "path": "large.txt" }, "isError": true, "timestamp": 1 })
}

#[test]
fn session_file_context_edits_omit_replace_and_latest_wins() {
    let mut s = mem();
    s.append_message(user("request")).unwrap();
    let a = s.append_message(assistant("partial")).unwrap();
    let r = s.append_message(tool_result("raw output")).unwrap();
    s.append_context_edit(&a, Value::Null).unwrap();
    s.append_context_edit(&r, Value::Null).unwrap();
    assert_eq!(
        s.branch_entries(None).iter().filter(|e| e["type"] == "message").count(),
        3
    );
    assert_eq!(roles(&s.build_session_projection().messages), ["user"]);
    assert_eq!(s.entry(&r).unwrap()["message"], tool_result("raw output"));

    let mut s = mem();
    let t = s.append_message(assistant("original")).unwrap();
    s.append_context_edit(&t, json!({ "content": [{ "type": "text", "text": "first" }] }))
        .unwrap();
    s.append_context_edit(&t, Value::Null).unwrap();
    s.append_context_edit(&t, json!({ "content": [{ "type": "text", "text": "restored" }] }))
        .unwrap();
    let projected = &s.build_session_projection().messages[0];
    assert_eq!(text(projected), "restored");
    assert_eq!(projected["usage"]["totalTokens"], 2);
    assert_eq!(text(&s.entry(&t).unwrap()["message"]), "original");
}

#[test]
fn session_file_string_edits_become_text_blocks_for_assistant_and_tool_results() {
    let mut s = mem();
    let a = s.append_message(assistant("original")).unwrap();
    let r = s.append_message(tool_result("original result")).unwrap();
    let ea = s
        .append_context_edit(&a, json!({ "content": "assistant replacement" }))
        .unwrap();
    let er = s
        .append_context_edit(&r, json!({ "content": "result replacement" }))
        .unwrap();
    assert_eq!(
        s.entry(&ea).unwrap()["replacement"],
        json!({ "content": [{ "type": "text", "text": "assistant replacement" }] })
    );
    assert_eq!(
        s.entry(&er).unwrap()["replacement"],
        json!({ "content": [{ "type": "text", "text": "result replacement" }] })
    );
    let projected = s.build_session_projection().messages;
    assert_eq!(
        projected[0]["content"],
        json!([{ "type": "text", "text": "assistant replacement" }])
    );
    assert_eq!(
        projected[1]["content"],
        json!([{ "type": "text", "text": "result replacement" }])
    );
}

#[test]
fn session_file_edits_are_branch_relative_and_apply_to_retained_entries() {
    let mut s = mem();
    let t = s.append_message(user("original")).unwrap();
    s.append_context_edit(&t, json!({ "content": "edited" })).unwrap();
    assert_eq!(text(&s.build_session_projection().messages[0]), "edited");
    s.branch(&t).unwrap();
    assert_eq!(text(&s.build_session_projection().messages[0]), "original");

    let mut s = mem();
    s.append_message(user("summarized")).unwrap();
    let kept = s.append_message(user("original retained")).unwrap();
    s.append_compaction("summary", Some(&kept), 100, None, None, None)
        .unwrap();
    s.append_context_edit(&kept, json!({ "content": "edited retained" }))
        .unwrap();
    let shown: Vec<String> = s
        .build_session_projection()
        .messages
        .iter()
        .map(|m| {
            m.get("summary")
                .and_then(Value::as_str)
                .map_or_else(|| text(m), str::to_string)
        })
        .collect();
    assert_eq!(shown, ["summary", "edited retained"]);
}

#[test]
fn session_file_retain_none_and_repeated_compactions_show_only_newest_summary() {
    let mut s = mem();
    s.append_message(user("discarded")).unwrap();
    let c = s
        .append_compaction("exact handoff", None, 100, None, None, None)
        .unwrap();
    s.append_message(user("after")).unwrap();
    assert_eq!(s.entry(&c).unwrap()["firstKeptEntryId"], Value::from(c.as_str()));
    let shown: Vec<String> = s
        .build_session_projection()
        .messages
        .iter()
        .map(|m| {
            m.get("summary")
                .and_then(Value::as_str)
                .map_or_else(|| text(m), str::to_string)
        })
        .collect();
    assert_eq!(shown, ["exact handoff", "after"]);

    let mut s = mem();
    s.append_message(user("summarized first")).unwrap();
    let kept = s.append_message(user("retained")).unwrap();
    s.append_compaction("first summary", Some(&kept), 100, None, None, None)
        .unwrap();
    s.append_message(assistant("after first compaction")).unwrap();
    s.append_compaction("second summary", Some(&kept), 80, None, None, None)
        .unwrap();
    let summaries: Vec<Value> = s
        .build_session_projection()
        .messages
        .iter()
        .filter_map(|m| m.get("summary").cloned())
        .collect();
    assert_eq!(summaries, [json!("second summary")]);
}

// ---- JavaScript semantics (literals printed by node 26) ----

#[test]
fn session_file_numbers_serialize_like_json_stringify() {
    let input = "[0,-0,1,-1,0.1,0.30000000000000004,1e21,1e-7,1.5e-7,0.000001,123456789012345678901,9007199254740993,1.7976931348623157e308,5e-324,-2.5e-10,100,1234.5678,1e20,12345678901234567890]";
    let parsed: Value = serde_json::from_str(input).unwrap();
    assert_eq!(
        js::stringify(&parsed),
        "[0,0,1,-1,0.1,0.30000000000000004,1e+21,1e-7,1.5e-7,0.000001,123456789012345680000,9007199254740992,1.7976931348623157e+308,5e-324,-2.5e-10,100,1234.5678,100000000000000000000,12345678901234567000]"
    );
}

#[test]
fn session_file_objects_and_strings_serialize_like_json_stringify() {
    let parsed: Value =
        serde_json::from_str(r#"{"b":1,"10":2,"2":3,"a":4,"01":5,"4294967295":6,"4294967294":7}"#).unwrap();
    assert_eq!(
        js::stringify(&parsed),
        r#"{"2":3,"10":2,"4294967294":7,"b":1,"a":4,"01":5,"4294967295":6}"#
    );
    let s = Value::from("\u{0}\u{8}\u{c}\n\r\t\u{1f}\u{7f}\"\\/\u{2028}é");
    assert_eq!(
        js::stringify(&s),
        "\"\\u0000\\b\\f\\n\\r\\t\\u001f\u{7f}\\\"\\\\/\u{2028}é\""
    );
}

#[test]
fn session_file_dates_parse_and_print_like_v8() {
    let cases: [(&str, Option<i64>); 18] = [
        ("2019-02-30", Some(1_551_484_800_000)),
        ("2019-02-31T00:00:00Z", Some(1_551_571_200_000)),
        ("2025-01-01T00:00:00.123456Z", Some(1_735_689_600_123)),
        ("2025-01-01T24:00:00Z", Some(1_735_776_000_000)),
        ("2025-01-01T00:00Z", Some(1_735_689_600_000)),
        ("2025-1-01", Some(1_735_689_600_000)),
        ("+002025-01-01T00:00:00Z", Some(1_735_689_600_000)),
        ("2025", Some(1_735_689_600_000)),
        ("2025-06", Some(1_748_736_000_000)),
        ("2025-01-01T00:00:00", Some(1_735_689_600_000)),
        ("2025-01-01T00:00:00.5+01:30", Some(1_735_684_200_500)),
        ("2025-01-01T00:00:00z", Some(1_735_689_600_000)),
        ("2025-01-01 00:00:00Z", Some(1_735_689_600_000)),
        ("2025-01-01T00:00:00+0100", Some(1_735_686_000_000)),
        (" 2025-01-01", Some(1_735_689_600_000)),
        ("2025-01-01T00:00:60Z", None),
        ("-000001-01-01T00:00:00Z", Some(-62_198_755_200_000)),
        ("2025-01-01T00:00:00.Z", None),
    ];
    for (input, expected) in cases {
        assert_eq!(js::parse_date(input), expected, "{input}");
    }
    let iso: Vec<String> = [0, 1_735_689_600_123, -1, 253_402_300_800_000, -62_198_755_200_000]
        .into_iter()
        .map(js::iso_string)
        .collect();
    assert_eq!(
        iso,
        [
            "1970-01-01T00:00:00.000Z",
            "2025-01-01T00:00:00.123Z",
            "1969-12-31T23:59:59.999Z",
            "+010000-01-01T00:00:00.000Z",
            "-000001-01-01T00:00:00.000Z"
        ]
    );
}

#[test]
fn session_file_trim_and_number_coercion_follow_javascript() {
    assert_eq!(js::trim(" \u{a0}\u{feff}\u{2028}x\u{85} "), "x\u{85}");
    let numbers: Vec<Option<f64>> = [
        "", " 12 ", "0x1F", "1e3", ".5", "5.", "+5", "-0b1", "abc", "Infinity", "1_000", "3",
    ]
    .into_iter()
    .map(|s| Some(js::to_number(&Value::from(s))).filter(|n| n.is_finite()))
    .collect();
    assert_eq!(
        numbers,
        [
            Some(0.0),
            Some(12.0),
            Some(31.0),
            Some(1000.0),
            Some(0.5),
            Some(5.0),
            Some(5.0),
            None,
            None,
            None,
            None,
            Some(3.0)
        ]
    );
}

#[test]
fn session_file_uuid_v7_is_monotonic_and_versioned() {
    let mut g = Uuidv7::default();
    let a = g.next(1_735_787_045_678, [0x11; 16]);
    let b = g.next(1_735_787_045_000, [0x22; 16]);
    assert!(is_uuid_v7(&a) && is_uuid_v7(&b));
    assert_eq!(&a[..13], "019424f8-632e");
    assert_eq!(
        &b[..13],
        "019424f8-632e",
        "clock going backwards keeps the last timestamp"
    );
    assert!(b > a);
}
