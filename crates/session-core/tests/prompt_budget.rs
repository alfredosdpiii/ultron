//! The prompt budget check before each root request (compaction::prompt_budget wired in the session actor's
//! generation), through a real session against a scripted provider on loopback: a huge AGENTS.md makes the system
//! prompt leave the conversation less than `compaction.reserveTokens` below the threshold, so the run fails before
//! any request with an error naming the sizes; turning auto-compaction off (only the window limits then) lets the
//! next prompt reach the provider, as the check reads the live settings before each request.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text};
use serde_json::json;
use session_core::core::Op;

#[test]
fn session_core_prompt_budget_fails_the_run_before_any_request() {
    let window = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .context_window;
    // The conversation must keep `reserve` below a threshold of `window - reserve`: at most `window - 2 * reserve`
    // fixed tokens (0.4 of the window).
    let reserve = window * 3 / 10;
    // 0.45 of the window in estimated tokens of project instructions.
    let line = "Follow the house rules carefully.\n";
    let agents = line.repeat((window as usize * 45 / 100 * 4).div_ceil(line.len()));
    let a = actor(
        in_order(vec![text("ANSWERED")]),
        Setup {
            settings: Some(json!({"compaction": {"reserveTokens": reserve}})),
            files: vec![("AGENTS.md", agents.into_bytes())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("hello");
    let error = ended["error"].to_string();
    assert!(
        error.contains("Prompt budget exceeded for anthropic/claude-sonnet-4-5: the system prompt ("),
        "{ended}"
    );
    assert!(
        error.contains(&format!(
            "below the compaction threshold of {} tokens (context window {window} minus compaction.reserveTokens {reserve}); the conversation needs at least {reserve}.",
            window - reserve
        )),
        "{ended}"
    );
    assert!(error.contains("Shorten the system prompt (AGENTS.md"), "{ended}");
    assert!(a.requests().is_empty(), "no request reaches the provider");
    // Auto-compaction off: the window alone limits, and the instructions fit it.
    a.call(Op::SetAutoCompaction(false)).unwrap();
    let ended = a.prompt("hello again");
    assert!(ended["error"].is_null(), "{ended}");
    assert_eq!(a.requests().len(), 1);
    assert_eq!(a.last_text(), "ANSWERED");
}

#[test]
fn session_core_prompt_budget_lets_an_ordinary_prompt_through() {
    let a = actor(
        in_order(vec![text("ANSWERED")]),
        Setup {
            files: vec![("AGENTS.md", b"Be brief.\n".to_vec())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("hello");
    assert!(ended["error"].is_null(), "{ended}");
    assert_eq!(a.requests().len(), 1);
}
