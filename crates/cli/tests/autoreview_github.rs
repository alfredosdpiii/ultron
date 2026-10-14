//! `ultron autoreview review <owner/repo#N>`, `status`, `once` with `autoreview.enabled: false`, and the
//! acknowledgement's logo, through `autoreview_cli::run_command_with` against the in-memory fake GitHub of
//! crates/review/tests/github_common (FakeHub behind the injected runner, a scripted engine): ported from
//! ultron-autoreview.test.ts with its literal expectations. No network, no real gh, no model.

#[path = "../../review/tests/github_common/mod.rs"]
mod github_common;

use cli::autoreview_cli::{Overrides, logo_text, run_command_with};
use github_common::{BOT, FakeEngine, FakeHub, FakePull, engine_result, head, major};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn env_for(dir: &Path) -> cli::Env {
    let agent = dir.join("agent");
    let cache = dir.join("cache");
    let home = dir.to_path_buf();
    cli::Env {
        cwd: dir.to_path_buf(),
        home: home.clone(),
        var: Arc::new(move |key| match key {
            "ULTRON_CODING_AGENT_DIR" => Some(agent.display().to_string()),
            "ULTRON_AUTOREVIEW_CACHE_DIR" => Some(cache.display().to_string()),
            "ULTRON_HINDSIGHT_URL" => Some("off".into()),
            "HOME" => Some(home.display().to_string()),
            _ => None,
        }),
        docs: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
    }
}

fn run(env: &cli::Env, overrides: &Overrides, args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_command_with(&args, env, overrides, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// "review <owner/repo#N> --dry-run --json reviews one pull request as the host's active account".
#[test]
fn cli_autoreview_review_pull_request_dry_run_then_status() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    let hub = FakeHub::new();
    hub.add_account(BOT, "github.com", true);
    hub.add_account("second", "github.com", false);
    hub.add_pull(FakePull::new("o", "r", 1));
    let engine = FakeEngine::new(vec![Ok(engine_result(json!({"findings": [major()]})))]);
    let overrides = Overrides {
        runner: Some(hub.clone()),
        engine: Some(engine.clone()),
    };
    let (code, out, err) = run(&env, &overrides, &["review", "o/r#1", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{err}");
    let printed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(printed["kind"], "dry-run");
    assert_eq!(printed["verdict"], "request_changes");
    assert_eq!(printed["event"], "REQUEST_CHANGES");
    assert_eq!(printed["sha"], head());
    assert_eq!(printed["inlineComments"], 1);
    assert_eq!(printed["findings"][0]["posted"], "inline");
    let files = printed["files"].as_array().unwrap();
    assert!(Path::new(files[0].as_str().unwrap()).exists());
    assert!(hub.api("^POST ").is_empty());
    assert_eq!(engine.specs().len(), 1);
    // status shows the account and the recorded review.
    let (code, out, _) = run(&env, &overrides, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("ultron-bot (github.com): last poll never"), "{out}");
    let line = regex::Regex::new(
        r"github\.com/o/r#1 aaaaaaa as ultron-bot: dry-run request_changes, 1 findings, pipeline 42 s",
    )
    .unwrap();
    assert!(line.is_match(&out), "{out}");
    assert!(
        out.starts_with("Posting: on; poll every 45 s; up to 3 reviews at once; model the default model\n"),
        "{out}"
    );
    let (code, out, _) = run(&env, &overrides, &["status", "--json"]);
    assert_eq!(code, 0);
    let status: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(status["accounts"][0]["login"], BOT);
    assert_eq!(status["recent"][0]["outcome"], "dry-run");
    assert_eq!(status["queue"], json!([]));
    assert_eq!(status["daemon"], Value::Null);
    // An unknown pull request target is a usage error.
    let (code, _, err) = run(&env, &overrides, &["review", "nonsense"]);
    assert_eq!(code, 2);
    assert!(err.contains("not a pull request: nonsense"), "{err}");
    // A host gh is not logged in to.
    let (code, _, err) = run(&env, &overrides, &["review", "https://ghe.example.com/o/r/pull/1"]);
    assert_eq!(code, 1);
    assert!(err.contains("gh is not logged in to ghe.example.com"), "{err}");
}

/// "with autoreview.enabled false (/autoreview off), once and run poll nothing and say why".
#[test]
fn cli_autoreview_disabled_polls_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/settings.json"),
        json!({"autoreview": {"enabled": false}}).to_string(),
    )
    .unwrap();
    let hub = FakeHub::new();
    hub.add_account(BOT, "github.com", true);
    let engine = FakeEngine::new(Vec::new());
    let overrides = Overrides {
        runner: Some(hub.clone()),
        engine: Some(engine.clone()),
    };
    for command in ["once", "run"] {
        let (code, _, err) = run(&env, &overrides, &[command, "--json"]);
        assert_eq!(code, 0);
        assert!(err.contains("autoreview is off (autoreview.enabled: false"), "{err}");
    }
    assert!(engine.specs().is_empty());
    assert!(hub.api("^GET notifications").is_empty());
}

/// The acknowledgement's art: the TUI's half-size logo, trimmed (`logoText`, recorded in review's settings.json).
#[test]
fn cli_autoreview_logo_text_matches_typescript() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../review/fixtures/settings.json");
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(logo_text(), settings["logo"].as_str().unwrap());
}

/// `once --json`: one poll cycle; a requested review is acknowledged and reviewed (here as a dry run), and every
/// outcome is listed with its pull request and account; the daemon lock is released afterwards.
#[test]
fn cli_autoreview_once_reviews_what_is_due() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    let hub = FakeHub::new();
    hub.add_account(BOT, "github.com", true);
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![github_common::Notification::new("review_requested", 1)];
    let engine = FakeEngine::new(vec![Ok(engine_result(json!({"findings": [major()]})))]);
    let overrides = Overrides {
        runner: Some(hub.clone()),
        engine: Some(engine.clone()),
    };
    let (code, out, err) = run(&env, &overrides, &["once", "--json", "--dry-run"]);
    assert_eq!(code, 0, "{err}");
    let printed: Value = serde_json::from_str(&out).unwrap();
    let reviews = printed["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1, "{printed}");
    assert_eq!(reviews[0]["pull"], "github.com/o/r#1");
    assert_eq!(reviews[0]["account"], BOT);
    assert_eq!(reviews[0]["kind"], "dry-run");
    assert_eq!(reviews[0]["verdict"], "request_changes");
    assert_eq!(engine.specs().len(), 1);
    assert_eq!(hub.api("^GET notifications").len(), 1);
    // The lock is free again: a second cycle runs. A dry run posted nothing, so with posting on the same commit is
    // reviewed again and posted (as the TS "dry run ... later real post").
    let (code, out, _) = run(&env, &overrides, &["once", "--json"]);
    assert_eq!(code, 0);
    let printed: Value = serde_json::from_str(&out).unwrap();
    let kinds: Vec<&str> = printed["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["posted"]);
    assert_eq!(engine.specs().len(), 2);
    assert_eq!(hub.api("^POST repos/o/r/pulls/1/reviews").len(), 1);
    // No account at all is an error.
    let empty = FakeHub::new();
    let overrides = Overrides {
        runner: Some(empty),
        engine: Some(engine),
    };
    let (code, _, err) = run(&env, &overrides, &["once"]);
    assert_eq!(code, 1);
    assert!(
        err.contains("gh is not logged in to any account (gh auth login)"),
        "{err}"
    );
}

/// The status line of a recent review: outcome, verdict, findings, pipeline, tag to ack (one decimal), ack to
/// review, pickup to post, cost and the stages (`describeStages`), newest first.
#[test]
fn cli_autoreview_status_renders_recent_reviews() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    std::fs::create_dir_all(dir.path().join("agent/autoreview")).unwrap();
    let state = json!({"version": 1, "accounts": {}, "pulls": {}, "queue": ["github.com/x a (reviewing)"], "recent": [
        {"account": "github.com/old-bot", "pull": "github.com/o/r#2", "sha": "b".repeat(40), "at": "2026-10-01T00:00:00.000Z",
         "outcome": "posted", "findings": 0, "totalMs": 1000, "pickupToPostMs": 2000},
        {"account": "github.com/ultron-bot", "pull": "github.com/o/r#1", "sha": "a".repeat(40), "at": "2026-10-02T00:00:00.000Z",
         "outcome": "posted", "verdict": "request_changes", "findings": 2, "totalMs": 42400, "tagToAckMs": 4000,
         "ackToPostMs": 50400, "pickupToPostMs": 54600, "costUsd": 0.314,
         "stages": {"scopeMs": 400, "findMs": 31000, "deepMs": 0, "verifyMs": 19500}}]});
    std::fs::write(dir.path().join("agent/autoreview/state.json"), state.to_string()).unwrap();
    let overrides = Overrides {
        runner: Some(FakeHub::new()),
        engine: None,
    };
    let (code, out, _) = run(&env, &overrides, &["status"]);
    assert_eq!(code, 0);
    let expected = "Recent reviews:\n  2026-10-02T00:00:00.000Z  github.com/o/r#1 aaaaaaa as ultron-bot: posted request_changes, 2 findings, pipeline 42 s, tag to ack 4.0 s, ack to review 50 s, pickup to post 55 s, $0.31; scope 0 s, find 31 s, verify 20 s\n  2026-10-01T00:00:00.000Z  github.com/o/r#2 bbbbbbb as old-bot: posted, 0 findings, pipeline 1 s, pickup to post 2 s\n";
    assert!(out.ends_with(expected), "{out}");
    assert!(
        out.contains("\nAccounts:\n  none (gh auth login)\n\nQueue: \n  github.com/x a (reviewing)\n"),
        "{out}"
    );
}
