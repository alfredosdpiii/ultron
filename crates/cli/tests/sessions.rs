//! Session names and selection in the Rust build (user requirements beyond the TypeScript product): automatic names
//! from the first prompt and a model-made title, `-n`, `-r <name|id>`, and `ultron -r` listing this directory's
//! sessions. Every run uses private ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR dirs and a scripted provider.

mod common;

use common::{Naming, Sandbox, anthropic_text, mock_server, session_files};
use serde_json::{Value, json};
use session_journal::names;
use std::process::Output;
use std::time::{Duration, Instant};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    sandbox.ultron().args(args).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn disable_naming(sandbox: &Sandbox) {
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
}

/// The listing of every session file under the sandbox (by path order).
fn listings(sandbox: &Sandbox) -> Vec<names::SessionListing> {
    session_files(&sandbox.sessions_dir())
        .into_iter()
        .map(|p| names::listing(session_journal::repo::read_metadata(&p).unwrap()))
        .collect()
}

fn records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .skip(1)
        .flat_map(|l| match serde_json::from_str::<Value>(l).unwrap() {
            Value::Array(items) => items,
            one => vec![one],
        })
        .collect()
}

fn session_id(output: &Output) -> String {
    let printed: Value = serde_json::from_str(&stdout(output)).unwrap();
    printed["sessionId"].as_str().unwrap().to_string()
}

#[test]
fn cli_sessions_auto_name_from_first_prompt_without_naming_call() {
    let (port, seen) = mock_server(
        vec![anthropic_text("ok"), anthropic_text("again")],
        Naming::Title("unused".into()),
    );
    let sandbox = Sandbox::new(port);
    disable_naming(&sandbox);
    let prompt = "\n  /review @src/login.rs  ## Fix the   **flaky** login test\nsecond line";
    let first = run(&sandbox, &["-p", "--model", MODEL, prompt]);
    assert_eq!(stdout(&first), "ok\n", "{}", stderr(&first));
    let list = listings(&sandbox);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.as_deref(), Some("Fix the flaky login test"));
    assert!(list[0].automatic_name);
    // A later prompt never renames it.
    let second = run(&sandbox, &["-p", "-c", "--model", MODEL, "something else entirely"]);
    assert_eq!(stdout(&second), "again\n", "{}", stderr(&second));
    assert_eq!(listings(&sandbox)[0].name.as_deref(), Some("Fix the flaky login test"));
    assert!(
        seen.lock().unwrap().naming.is_empty(),
        "sessionNaming.enabled=false makes no request"
    );
}

#[test]
fn cli_sessions_model_title_replaces_the_automatic_name() {
    let (port, seen) = mock_server(
        vec![anthropic_text("ok")],
        Naming::Title("\"**Fix Login Tests.**\"".into()),
    );
    let sandbox = Sandbox::new(port);
    let long = format!("Fix the login tests {}", "x".repeat(3000));
    let output = run(&sandbox, &["--mode", "json", "--model", MODEL, &long]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let list = listings(&sandbox);
    assert_eq!(list[0].name.as_deref(), Some("Fix Login Tests"));
    assert!(list[0].automatic_name, "a model title is still an automatic name");
    let naming = seen.lock().unwrap().naming.clone();
    assert_eq!(naming.len(), 1);
    let body = &naming[0]["body"];
    assert_eq!(body["max_tokens"], 24);
    assert_eq!(
        body["system"][0]["text"],
        "Name this coding session in 3-6 words. Reply with the title only."
    );
    let sent = body["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        sent.chars().count(),
        2000,
        "the first prompt is cut to 2,000 characters"
    );
    assert!(
        body.get("thinking").is_none() || body["thinking"]["type"] == "disabled",
        "thinking off: {}",
        body["thinking"]
    );
    // The cheapest Anthropic catalog model that can run without thinking (none is non-reasoning), not the session model.
    assert_eq!(body["model"], "claude-haiku-4-5");
    // Its usage is recorded as a usage row of its own.
    let file = &session_files(&sandbox.sessions_dir())[0];
    let rows: Vec<Value> = records(file).into_iter().filter(|r| r["kind"] == "usage").collect();
    let naming_row = rows
        .iter()
        .find(|r| r["details"]["source"] == "session-naming")
        .unwrap();
    assert_eq!(naming_row["usage"]["totalTokens"], 15);
    assert_eq!(naming_row["adjustment"], false);
    assert_eq!(session_id(&output), list[0].metadata.id);
}

#[test]
fn cli_sessions_naming_setting_selects_the_model() {
    let (port, seen) = mock_server(vec![anthropic_text("ok")], Naming::Title("Chosen".into()));
    let sandbox = Sandbox::new(port);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"model":"anthropic/claude-sonnet-4-5"}}"#,
    )
    .unwrap();
    let output = run(&sandbox, &["-p", "--model", MODEL, "name me"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(seen.lock().unwrap().naming[0]["body"]["model"], "claude-sonnet-4-5");
    assert_eq!(listings(&sandbox)[0].name.as_deref(), Some("Chosen"));
}

#[test]
fn cli_sessions_naming_failure_keeps_the_fallback() {
    let (port, seen) = mock_server(vec![anthropic_text("ok")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "Refactor the parser"]);
    assert_eq!(
        (output.status.code(), stdout(&output)),
        (Some(0), "ok\n".to_string()),
        "{}",
        stderr(&output)
    );
    assert_eq!(seen.lock().unwrap().naming.len(), 1);
    assert_eq!(listings(&sandbox)[0].name.as_deref(), Some("Refactor the parser"));
}

#[test]
fn cli_sessions_slow_naming_never_holds_the_turn() {
    let (port, _seen) = mock_server(
        vec![anthropic_text("ok")],
        Naming::Slow(Duration::from_secs(6), "Too late".into()),
    );
    let sandbox = Sandbox::new(port);
    let started = Instant::now();
    let output = run(&sandbox, &["-p", "--model", MODEL, "Quick question"]);
    assert_eq!(stdout(&output), "ok\n", "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "print mode exits when the turn ends"
    );
    assert_eq!(listings(&sandbox)[0].name.as_deref(), Some("Quick question"));
}

#[test]
fn cli_sessions_user_name_wins_and_makes_no_naming_call() {
    let (port, seen) = mock_server(
        vec![anthropic_text("ok"), anthropic_text("two")],
        Naming::Title("Model title".into()),
    );
    let sandbox = Sandbox::new(port);
    let first = run(&sandbox, &["-p", "-n", "  Mine ", "--model", MODEL, "hello there"]);
    assert_eq!(stdout(&first), "ok\n", "{}", stderr(&first));
    let list = listings(&sandbox);
    assert_eq!((list[0].name.as_deref(), list[0].automatic_name), (Some("Mine"), false));
    assert!(seen.lock().unwrap().naming.is_empty());
    // -n on a resumed session renames it.
    let second = run(&sandbox, &["-p", "-c", "-n", "Renamed", "--model", MODEL, "more"]);
    assert_eq!(stdout(&second), "two\n", "{}", stderr(&second));
    assert_eq!(listings(&sandbox)[0].name.as_deref(), Some("Renamed"));
}

#[test]
fn cli_sessions_resume_by_name_id_and_prefix() {
    let replies: Vec<String> = (0..12).map(|i| anthropic_text(&format!("r{i}"))).collect();
    let (port, seen) = mock_server(replies, Naming::Title("unused".into()));
    let sandbox = Sandbox::new(port);
    disable_naming(&sandbox);
    let create = |args: &[&str]| {
        let output = run(&sandbox, args);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        session_id(&output)
    };
    let alpha = create(&["--mode", "json", "-n", "Alpha project", "--model", MODEL, "a"]);
    std::thread::sleep(Duration::from_millis(20));
    let beta_old = create(&["--mode", "json", "-n", "beta", "--model", MODEL, "b"]);
    std::thread::sleep(Duration::from_millis(20));
    let beta_new = create(&["--mode", "json", "-n", "Beta", "--model", MODEL, "c"]);
    std::thread::sleep(Duration::from_millis(20));
    let auto = create(&["--mode", "json", "--model", MODEL, "Investigate memory growth"]);
    let resumed = |selector: &str| {
        let output = run(&sandbox, &["--mode", "json", "-r", selector, "--model", MODEL, "next"]);
        assert_eq!(output.status.code(), Some(0), "-r {selector}: {}", stderr(&output));
        session_id(&output)
    };
    assert_eq!(resumed("ALPHA PROJECT"), alpha, "exact name, case-insensitive");
    assert_eq!(resumed("alp"), alpha, "unique name prefix");
    assert_eq!(
        resumed("beta"),
        beta_new,
        "several sessions share the name: the most recent wins"
    );
    assert_eq!(resumed("investigate memory growth"), auto, "an automatic name");
    assert_eq!(resumed(&beta_old), beta_old, "an id");
    assert_eq!(resumed(&alpha[..13]), alpha, "an id prefix");
    let history = seen.lock().unwrap().requests.last().unwrap()["body"]["messages"].clone();
    assert_eq!(
        history[1]["content"][0]["text"], "a",
        "the resumed session's history is sent"
    );

    let unknown = run(&sandbox, &["-p", "-r", "gamma", "--model", MODEL, "x"]);
    assert_eq!(unknown.status.code(), Some(1));
    let message = stderr(&unknown);
    assert!(
        message.starts_with("Error: No session found matching 'gamma' in this directory."),
        "{message}"
    );
    assert!(
        message.contains("(Alpha project)") && message.contains("(Beta)"),
        "{message}"
    );
    let ambiguous = run(&sandbox, &["-p", "-r", "0", "--model", MODEL, "x"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    assert!(
        stderr(&ambiguous).starts_with("Error: Session selector '0' is ambiguous; matches: "),
        "{}",
        stderr(&ambiguous)
    );
}

#[test]
fn cli_sessions_resume_without_selector_lists_this_directory() {
    let (port, _seen) = mock_server(
        vec![anthropic_text("ok"), anthropic_text("ok"), anthropic_text("ok")],
        Naming::Title("x".into()),
    );
    let sandbox = Sandbox::new(port);
    disable_naming(&sandbox);
    let elsewhere = sandbox.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let other = sandbox
        .ultron()
        .current_dir(&elsewhere)
        .args(["-p", "-n", "other dir", "--model", MODEL, "x"])
        .output()
        .unwrap();
    assert_eq!(other.status.code(), Some(0), "{}", stderr(&other));
    assert_eq!(
        stdout(&run(&sandbox, &["-r"])),
        format!("No sessions in {}\n", sandbox.cwd.display())
    );
    run(
        &sandbox,
        &["-p", "-n", "First one", "--model", MODEL, "first   prompt\ntext"],
    );
    std::thread::sleep(Duration::from_millis(20));
    run(&sandbox, &["-p", "--model", MODEL, "Second prompt"]);
    let listed = run(&sandbox, &["-r"]);
    assert_eq!(listed.status.code(), Some(0));
    let lines: Vec<Vec<String>> = stdout(&listed)
        .lines()
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    assert_eq!(lines.len(), 2, "only this directory's sessions: {lines:?}");
    assert_eq!(
        (lines[0][2].as_str(), lines[0][3].as_str()),
        ("Second prompt", "Second prompt")
    );
    assert_eq!(
        (lines[1][2].as_str(), lines[1][3].as_str()),
        ("First one", "first prompt text")
    );
    let library = names::list_sessions(&sandbox.sessions_dir(), &sandbox.cwd.display().to_string());
    assert_eq!(
        library.iter().map(|s| s.metadata.id.clone()).collect::<Vec<_>>(),
        lines.iter().map(|l| l[0].clone()).collect::<Vec<_>>()
    );
}

#[test]
fn cli_sessions_listing_reads_v3_names() {
    let sandbox = Sandbox::new(1);
    let dir = sandbox.sessions_dir().join("--legacy--");
    std::fs::create_dir_all(&dir).unwrap();
    let header = json!({"type": "session", "version": 3, "id": "legacy-1", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": sandbox.cwd.display().to_string()});
    let message = json!({"type": "message", "id": "m", "parentId": null, "timestamp": "2026-01-01T00:00:01.000Z", "message": {"role": "user", "content": "old prompt", "timestamp": 1}});
    let info = json!({"type": "session_info", "id": "i", "parentId": "m", "timestamp": "2026-01-01T00:00:02.000Z", "name": "  legacy name "});
    std::fs::write(dir.join("a.jsonl"), format!("{header}\n{message}\n{info}\n")).unwrap();
    let listed = stdout(&run(&sandbox, &["-r"]));
    assert!(listed.starts_with("legacy-1\t"), "{listed}");
    assert!(listed.ends_with("\tlegacy name\told prompt\n"), "{listed}");
}
