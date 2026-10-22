//! `/usage` and `ultron usage` price a provider on a subscription login as notional (session-worker.ts passes
//! `subscriptionProviders`: every provider `modelRuntime.isUsingSubscription`). Ported from
//! ultron-session-report.test.ts ("a provider the worker knows to be on a subscription login is priced as one"),
//! plus the list itself: Claude Code when configured, an OAuth login whose provider is a subscription (Anthropic
//! Pro/Max, OpenAI Codex, GitHub Copilot, xAI, Kimi, Meta), never an API key or a non-subscription OAuth
//! (OpenRouter), and always `claude-subscription`.

use cli::usage::log::read_session_log;
use cli::usage::report::{build_session_report, build_session_report_with};
use serde_json::{Value, json};
use session_core::RunEnv;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// rootOnlySession: one root response on provider `priced`, $0.10.
fn session(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("session.jsonl");
    let lines = [
        json!({"v": 4, "kind": "header", "id": "s1", "storageVersion": 1, "createdAt": 1000, "cwd": "/x"}),
        json!({"kind": "entry", "id": "e1", "parentId": null, "timestamp": 1001, "type": "message",
            "message": {"role": "assistant", "provider": "priced", "model": "m", "content": [],
                "usage": {"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
                    "cost": {"total": 0.1}}}}),
        json!({"kind": "value", "op": "set", "seq": 2, "namespace": "pi.branch.tip", "key": "main", "value": "e1"}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn cli_usage_a_provider_on_a_subscription_login_is_priced_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let log = read_session_log(&session(dir.path())).unwrap();
    let report = build_session_report_with(&log, &["priced".to_string()]);
    let cost = &report["usage"]["total"]["cost"];
    assert_eq!(cost["reportedUsd"], Value::Null);
    assert!((cost["subscriptionUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9, "{cost}");
    // Without it the cost is reported.
    let plain = build_session_report(&log);
    assert!((plain["usage"]["total"]["cost"]["reportedUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9);
    assert_eq!(plain["usage"]["total"]["cost"]["subscriptionUsd"], Value::Null);
}

fn env(dir: &Path, vars: &[(&str, String)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(dir.join("cwd")).unwrap();
    let oauth =
        json!({"type": "oauth", "access": "fake-access", "refresh": "fake-refresh", "expires": 4_102_444_800_000_u64});
    let auth = json!({
        "anthropic": oauth,
        "openai-codex": oauth,
        "openrouter": oauth,
        "openai": {"type": "api_key", "key": "fake-openai-key"},
    });
    std::fs::write(agent.join("auth.json"), auth.to_string()).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    map.insert("PATH".into(), dir.join("empty-path").display().to_string());
    RunEnv {
        cwd: dir.join("cwd"),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

fn sorted(mut providers: Vec<String>) -> Vec<String> {
    providers.sort();
    providers
}

#[test]
fn cli_usage_subscription_providers_are_the_runtimes_subscription_logins() {
    let dir = tempfile::tempdir().unwrap();
    let providers = session_core::lane::subscription_providers(&env(dir.path(), &[]));
    assert_eq!(sorted(providers), ["anthropic", "claude-subscription", "openai-codex"]);
    // Claude Code counts once its CLI is configured.
    let cli = dir.path().join("claude");
    std::fs::write(&cli, "").unwrap();
    let providers = session_core::lane::subscription_providers(&env(
        dir.path(),
        &[("ULTRON_CLAUDE_CODE_BIN", cli.display().to_string())],
    ));
    assert_eq!(
        sorted(providers),
        ["anthropic", "claude-code", "claude-subscription", "openai-codex"]
    );
}

/// `ultron usage --json <file>`: a response on the Anthropic Pro/Max login (an OAuth credential) is notional.
#[test]
fn cli_usage_command_prices_an_oauth_subscription_login_as_notional() {
    let dir = tempfile::tempdir().unwrap();
    let path = session(dir.path());
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"priced\"", "\"anthropic\"");
    std::fs::write(&path, text).unwrap();
    let run = |env: &RunEnv| -> Value {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = ["usage", "--json", &path.display().to_string()].map(str::to_string);
        assert_eq!(
            cli::usage::run(&args, env, &mut out, &mut err),
            Some(0),
            "{}",
            String::from_utf8_lossy(&err)
        );
        serde_json::from_slice(&out).unwrap()
    };
    let report = run(&env(dir.path(), &[]));
    let cost = &report["usage"]["total"]["cost"];
    assert_eq!(cost["reportedUsd"], Value::Null, "{cost}");
    assert!((cost["subscriptionUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9, "{cost}");
    // The same session with an API key for anthropic reports the cost.
    let keyed = dir.path().join("keyed");
    let env = env(&keyed, &[]);
    std::fs::write(
        keyed.join("agent/auth.json"),
        json!({"anthropic": {"type": "api_key", "key": "fake-anthropic-key"}}).to_string(),
    )
    .unwrap();
    let report = run(&env);
    assert!((report["usage"]["total"]["cost"]["reportedUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9);
}

/// The Claude subscription's models are listed when Claude Code is found by ULTRON_CLAUDE_CODE_BIN (what lists the
/// claude-code provider), not only by ULTRON_CLAUDE_COMMAND or PATH.
#[test]
fn cli_models_claude_subscription_is_listed_by_claude_code_bin() {
    let dir = tempfile::tempdir().unwrap();
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude");
    let listed = |env: &RunEnv| {
        session_core::lane::available_models(env)
            .into_iter()
            .filter(|m| m.provider == "claude-subscription")
            .count()
    };
    assert_eq!(listed(&env(dir.path(), &[])), 0, "no Claude Code: nothing listed");
    let env = env(dir.path(), &[("ULTRON_CLAUDE_CODE_BIN", wrapper.display().to_string())]);
    assert_eq!(listed(&env), ai::claude_subscription::catalog::pinned_routes().len());
}

/// `ultron mcp`'s `usage.report` prices as `/usage` does: the Anthropic Pro/Max login of the server's environment
/// is notional (before, the hook built the plain report and reported the cost), an API key is reported.
#[test]
fn cli_usage_mcp_report_prices_the_servers_subscription_logins_as_notional() {
    let dir = tempfile::tempdir().unwrap();
    let path = session(dir.path());
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"priced\"", "\"anthropic\"");
    std::fs::write(&path, text).unwrap();
    let report = cli::mcp_session_report(&path, &env(dir.path(), &[])).unwrap();
    let cost = &report["usage"]["total"]["cost"];
    assert_eq!(cost["reportedUsd"], Value::Null, "{cost}");
    assert!((cost["subscriptionUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9, "{cost}");
    let keyed = dir.path().join("keyed");
    let env = env(&keyed, &[]);
    std::fs::write(
        keyed.join("agent/auth.json"),
        json!({"anthropic": {"type": "api_key", "key": "fake-anthropic-key"}}).to_string(),
    )
    .unwrap();
    let report = cli::mcp_session_report(&path, &env).unwrap();
    let cost = &report["usage"]["total"]["cost"];
    assert!((cost["reportedUsd"].as_f64().unwrap() - 0.1).abs() < 1e-9, "{cost}");
    assert_eq!(cost["subscriptionUsd"], Value::Null, "{cost}");
    assert!(cli::mcp_session_report(&dir.path().join("missing.jsonl"), &env).is_err());
}
