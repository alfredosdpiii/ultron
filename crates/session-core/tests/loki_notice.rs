//! Loki's one-time setup note through the session (session-worker.ts `installLokiNotice` over `setupLoki`): opening a
//! session in a Git repository without `.loki/` installs and commits it; the note of that opens the first root
//! turn as a displayed `loki` custom message (in the transcript and the model's request), once; a later session in
//! the same repository has nothing to say.

mod p_session_support;
mod reopen_support;

use p_session_support::{HANG, text, user_texts};
use reopen_support::Home;
use serde_json::Value;
use session_core::core::Op;
use std::path::Path;
use std::time::{Duration, Instant};

const IDENTITY: [(&str, &str); 5] = [
    ("GIT_AUTHOR_NAME", "Test User"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test User"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

fn git(home: &Home, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(&home.cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &home.root)
        .envs(IDENTITY)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository_home() -> Home {
    let home = Home::new(None);
    git(&home, &["init", "-q", "-b", "main"]);
    git(&home, &["config", "commit.gpgsign", "false"]);
    std::fs::write(home.cwd.join("a.txt"), "a\n").unwrap();
    git(&home, &["add", "a.txt"]);
    git(&home, &["commit", "-qm", "init"]);
    home
}

/// Waits for the background commit of `.loki/` (a hang guard, never a pass condition).
fn wait_for_commit(home: &Home) -> String {
    let end = Instant::now() + HANG;
    loop {
        if git(home, &["log", "-1", "--format=%s"]) == "Add Loki guardrails" {
            return git(home, &["rev-parse", "--short", "HEAD"]);
        }
        assert!(Instant::now() < end, "the commit never landed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn prompt(w: &reopen_support::Worker, message: &str) {
    w.call(Op::PromptAndWait {
        message: message.into(),
        images: Vec::new(),
    })
    .unwrap();
}

fn loki_messages(home: &Home) -> Vec<Value> {
    home.writes()
        .into_iter()
        .filter(|w| w["type"] == "message" && w["message"]["customType"] == "loki")
        .map(|w| w["message"].clone())
        .collect()
}

#[test]
fn session_core_loki_notice_opens_the_first_root_turn_once_like_typescript() {
    let home = repository_home();
    let w = home.open(Box::new(|_, _| text("done")), &IDENTITY, None);
    let sha = wait_for_commit(&home);
    let note = format!("Loki set up and committed ({sha}): .loki/ holds the guardrail engine and policy.");
    assert!(Path::new(&home.cwd).join(".loki/loki.py").is_file());
    prompt(&w, "first");
    prompt(&w, "second");
    // The model saw the note once, before the first prompt.
    let requests = w.requests();
    assert_eq!(
        user_texts(&requests[0]),
        vec![Value::from(note.clone()), Value::from("first")]
    );
    assert_eq!(
        user_texts(&requests[1]),
        vec![Value::from(note.clone()), Value::from("first"), Value::from("second")]
    );
    // It is a displayed custom message of the transcript.
    let shown: Vec<Value> = w
        .events("message_end")
        .into_iter()
        .filter(|e| e["message"]["customType"] == "loki")
        .collect();
    assert_eq!(shown.len(), 1);
    w.close();
    let stored = loki_messages(&home);
    assert_eq!(stored.len(), 1);
    let message = &stored[0];
    assert_eq!(
        (
            &message["role"],
            &message["content"],
            &message["display"],
            &message["details"]
        ),
        (
            &Value::from("custom"),
            &Value::from(note),
            &Value::from(true),
            &serde_json::json!({})
        )
    );
    assert!(message["timestamp"].as_u64().is_some());
    // A later session in the repository uses the committed .loki/: nothing to say, nothing committed.
    let again = home.open(Box::new(|_, _| text("done")), &IDENTITY, None);
    prompt(&again, "third");
    again.close();
    assert_eq!(loki_messages(&home).len(), 1);
    assert_eq!(git(&home, &["rev-parse", "--short", "HEAD"]), sha);
}

#[test]
fn session_core_loki_notice_says_why_it_did_not_commit_and_nothing_when_loki_is_off() {
    let home = repository_home();
    let mut vars = IDENTITY.to_vec();
    vars.push(("ULTRON_LOKI_AUTOCOMMIT", "off"));
    let w = home.open(Box::new(|_, _| text("done")), &vars, None);
    prompt(&w, "first");
    w.close();
    assert_eq!(
        loki_messages(&home)
            .iter()
            .map(|m| m["content"].clone())
            .collect::<Vec<_>>(),
        vec![Value::from(
            "Loki set up in .loki/ (not committed: auto-commit is off (ULTRON_LOKI_AUTOCOMMIT)). Commit it to enable \
             post-write checks: git add .loki && git commit -m \"Add Loki guardrails\" -- .loki"
        )]
    );
    assert_eq!(git(&home, &["log", "-1", "--format=%s"]), "init");

    let off = repository_home();
    let mut vars = IDENTITY.to_vec();
    vars.push(("ULTRON_LOKI", "off"));
    let w = off.open(Box::new(|_, _| text("done")), &vars, None);
    prompt(&w, "first");
    w.close();
    assert!(loki_messages(&off).is_empty());
    assert!(!off.cwd.join(".loki").exists());
}
