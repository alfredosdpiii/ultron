//! `--claude` in the Rust build: TypeScript's model selection mapped to the claude-subscription provider (decision
//! 2026-10-13 claude: Claude runs through claude-subscription, Ultron's harness keeps the loop, the tool list is
//! exactly `[rlm]`).

use claude_root::worker_root::subscription_root;

fn no_env(_: &str) -> Option<String> {
    None
}

fn root(provider: Option<&str>, model: Option<&str>) -> Result<(Option<String>, String), String> {
    subscription_root(provider, model, &no_env).map(|r| (r.provider, r.model))
}

#[test]
fn claude_root_claude_flag_selects_the_claude_subscription_model() {
    let ok = |model: &str| Ok((None, model.to_string()));
    assert_eq!(root(None, None), ok("claude-subscription/claude-opus-5-5"));
    assert_eq!(root(None, Some("sonnet")), ok("claude-subscription/sonnet"));
    assert_eq!(root(None, Some("claude-code/haiku")), ok("claude-subscription/haiku"));
    assert_eq!(
        root(None, Some("claude-subscription/claude-fable-5-1")),
        ok("claude-subscription/claude-fable-5-1")
    );
    assert_eq!(root(Some("claude-code"), Some("opus")), ok("claude-subscription/opus"));
    assert_eq!(
        root(Some("claude-subscription"), Some("opus")),
        ok("claude-subscription/opus")
    );
    assert_eq!(
        root(Some("claude-subscription"), None),
        ok("claude-subscription/claude-opus-5-5")
    );
    let env = |k: &str| (k == "ULTRON_CLAUDE_MODEL").then(|| " sonnet ".to_string());
    assert_eq!(
        subscription_root(None, None, &env).unwrap().model,
        "claude-subscription/sonnet"
    );
    // Other providers and ids the route table refuses fail with TypeScript's wording.
    assert!(
        root(None, Some("openai/gpt-5"))
            .unwrap_err()
            .contains("does not serve openai/gpt-5")
    );
    assert!(
        root(Some("openai"), None)
            .unwrap_err()
            .contains("does not serve openai")
    );
    assert!(
        root(None, Some("gpt-5"))
            .unwrap_err()
            .contains("--model claude-code/<model>")
    );
    assert!(
        root(None, Some("claude-Opus"))
            .unwrap_err()
            .contains("does not serve claude-subscription/claude-Opus")
    );
}
