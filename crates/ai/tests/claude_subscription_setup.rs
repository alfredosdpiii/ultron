//! Ports of the Hermes plugin's `tests/test_directsdk_setup.py`, `test_directsdk_discovery.py` (missing-CLI and
//! resolution cases) and the discovery half of `test_directsdk_telemetry.py`: locating `claude`, its login state
//! and its own model picker through a relay that proves no Messages request was sent; plus the inert MCP server's
//! replies.

mod claude_subscription_support;

use ai::claude_subscription::catalog::pinned_routes;
use ai::claude_subscription::inert_mcp::{DENIAL, reply, serve};
use ai::claude_subscription::process::resolve_claude;
use ai::claude_subscription::setup::{DiscoveredModel, discover_models, setup_status};
use ai::claude_subscription::{Config, Env};
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

// An input no test here exercises; long so a loaded machine never trips it.
const TIMEOUT: Duration = Duration::from_secs(120);

fn install(path: &Path) -> String {
    std::fs::create_dir_all(path).unwrap();
    let binary = path.join("claude");
    std::fs::write(&binary, "").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    binary.display().to_string()
}

fn service_env(home: &Path) -> Env {
    [
        ("HOME", home.display().to_string()),
        ("PATH", home.join("empty").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn ai_claude_subscription_resolves_claude_outside_a_service_path() {
    seal();
    for prefix in [
        ".local/bin",
        ".claude/local",
        "bin",
        ".npm-global/bin",
        ".bun/bin",
        ".volta/bin",
    ] {
        let home = tempfile::tempdir().unwrap();
        let binary = install(&home.path().join(prefix));
        let resolved = resolve_claude(Some(&["claude".into(), "--verbose".into()]), &service_env(home.path())).unwrap();
        assert_eq!(resolved, [binary, "--verbose".to_string()], "{prefix}");
    }
}

#[test]
fn ai_claude_subscription_path_and_explicit_commands_take_precedence() {
    seal();
    let home = tempfile::tempdir().unwrap();
    let probed = install(&home.path().join(".local/bin"));
    let on_path = install(&home.path().join("path-bin"));
    let mut env = service_env(home.path());
    env.insert("PATH".into(), home.path().join("path-bin").display().to_string());
    assert_eq!(resolve_claude(None, &env).unwrap(), std::slice::from_ref(&on_path));
    let mut overridden = env.clone();
    overridden.insert("ULTRON_CLAUDE_COMMAND".into(), probed.clone());
    assert_eq!(resolve_claude(None, &overridden).unwrap(), [probed]);
    assert_eq!(
        resolve_claude(Some(&[home.path().join("missing/claude").display().to_string()]), &env),
        None
    );
    // Only the bare default name is probed outside PATH: an explicit wrapper is the user's choice.
    assert_eq!(resolve_claude(Some(&["custom-claude-wrapper".into()]), &env), None);
    // The env's PATH decides, never the test runner's.
    let mut nowhere = Env::new();
    nowhere.insert("PATH".into(), home.path().join("nowhere").display().to_string());
    assert_eq!(resolve_claude(None, &nowhere), None);
    let mut defaulted = Env::new();
    defaulted.insert("PATH".into(), "/bin:/usr/bin".into());
    assert_eq!(resolve_claude(None, &Env::new()), resolve_claude(None, &defaulted));
}

fn fake_config(fake: &Fake, upstream: &str, vars: &[(&str, &str)]) -> (Config, Env) {
    let config = fake.config(upstream, vars);
    let env = config.env.clone().unwrap();
    (config, env)
}

const PRO: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "subscriptionType": "pro"}"#;
const ACCOUNT: &str = r#"{"subscriptionType": "Claude Pro"}"#;

fn pinned_picker() -> Value {
    json!([
        {"value": "sonnet[1m]", "resolvedModel": "claude-sonnet-5[1m]", "displayName": "Sonnet 5 (1M context)", "description": "Sonnet 5 for long sessions"},
        {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus", "description": "Opus 5.5 · Best for everyday, complex tasks"},
        {"value": "opus[1m]", "resolvedModel": "claude-opus-5-5[1m]", "displayName": "Opus (1M context)", "description": "Opus 5.5 with 1M context · Draws from usage credits · $4/$20 per Mtok"},
        {"value": "haiku", "resolvedModel": "claude-haiku-4-5-20251001", "displayName": "Haiku", "description": "Haiku 4.5 · Fastest for quick answers"},
    ])
}

fn unpinned_picker() -> Value {
    json!([
        {"value": "opus", "resolvedModel": "claude-opus-9", "displayName": "Opus", "description": "Opus 9 · Best for everyday, complex tasks"},
        {"value": "opus[1m]", "resolvedModel": "claude-opus-9[1m]", "displayName": "Opus (1M context)", "description": "Opus 9 with 1M context · Draws from usage credits · $5/$25 per Mtok"},
        {"value": "claude-sonnet-5-9", "resolvedModel": "claude-sonnet-5-9", "displayName": "Sonnet", "description": "Sonnet 5.9 · Efficient for routine tasks"},
        {"value": "claude-fable-9[1m]", "resolvedModel": "claude-fable-9[1m]", "displayName": "Fable", "description": "Fable 9 · Most capable for your hardest tasks"},
        {"value": "default", "displayName": "Default (recommended)", "description": "Opus 9 · Best for everyday, complex tasks"},
    ])
}

fn discover(models: Value) -> Vec<DiscoveredModel> {
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| reply_status(stream, 500, "{}"));
    let models = models.to_string();
    let (config, env) = fake_config(
        &fake,
        &upstream.url(),
        &[("FAKE_AUTH", PRO), ("FAKE_ACCOUNT", ACCOUNT), ("FAKE_MODELS", &models)],
    );
    let found = discover_models(&config, &env, TIMEOUT).unwrap();
    assert!(upstream.requests().is_empty(), "discovery sends no Messages request");
    found
}

fn concat(a: Value, b: Value) -> Value {
    let mut all = a.as_array().unwrap().clone();
    all.extend(b.as_array().unwrap().iter().cloned());
    Value::Array(all)
}

#[test]
fn ai_claude_subscription_setup_status_reports_login_and_models_from_the_cli() {
    seal();
    let fake = Fake::new();
    let (config, env) = fake_config(&fake, "http://127.0.0.1:9", &[("FAKE_AUTH", PRO)]);
    let status = setup_status(&config, &env, TIMEOUT);
    assert!(status.available && status.logged_in);
    assert_eq!(status.plan, "Claude Pro");
    assert_eq!(
        status.login_command.unwrap(),
        [fake.command(), vec!["auth".into(), "login".into()]].concat()
    );
    let models = discover(pinned_picker());
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids[..3],
        [
            "claude-sonnet-5[1m]",
            "claude-opus-5-5[1m]",
            "claude-haiku-4-5-20251001"
        ]
    );
    let labels: Vec<&str> = models[..3].iter().map(|m| m.label.as_str()).collect();
    assert_eq!(labels, ["Sonnet 5 for long sessions", "Opus 5.5", "Haiku 4.5"]);
    assert_eq!(models[1].note, "usage credits");
    assert_eq!(models[2].note, "");
    assert!(models.iter().all(|m| m.upstream_requests == 0));
}

#[test]
fn ai_claude_subscription_every_model_the_cli_advertises_is_selectable() {
    seal();
    let pinned = discover(pinned_picker());
    let models = discover(concat(pinned_picker(), unpinned_picker()));
    let kept: Vec<DiscoveredModel> = models
        .iter()
        .filter(|m| !m.note.contains("unpinned"))
        .cloned()
        .collect();
    assert_eq!(kept, pinned);
    let mut fresh: Vec<(&str, &str, &str)> = models
        .iter()
        .filter(|m| m.note.contains("unpinned"))
        .map(|m| (m.id.as_str(), m.label.as_str(), m.note.as_str()))
        .collect();
    fresh.sort();
    assert_eq!(
        fresh,
        [
            ("claude-fable-9[1m]", "Fable 9", "unpinned"),
            ("claude-opus-9[1m]", "Opus 9", "usage credits · unpinned"),
            ("claude-sonnet-5-9", "Sonnet 5.9", "unpinned"),
        ]
    );
}

#[test]
fn ai_claude_subscription_pinned_models_the_picker_omits_stay_selectable() {
    seal();
    let models = discover(concat(pinned_picker(), unpinned_picker()));
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    let routes: Vec<String> = pinned_routes().into_iter().map(|(r, _, _)| r).collect();
    assert!(routes.iter().all(|r| ids.contains(&r.as_str())));
    let unique: std::collections::BTreeSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
    let appended: Vec<&str> = routes
        .iter()
        .map(String::as_str)
        .filter(|r| {
            ![
                "claude-sonnet-5[1m]",
                "claude-opus-5-5[1m]",
                "claude-haiku-4-5-20251001",
            ]
            .contains(r)
        })
        .collect();
    assert_eq!(ids[ids.len() - appended.len()..], appended[..]);
    let row = |id: &str| models.iter().find(|m| m.id == id).unwrap();
    assert_eq!(row("claude-opus-4-8[1m]").label, "Opus 4.8");
    assert_eq!(row("claude-opus-5[1m]").label, "Opus 5");
    // Pro bills Fable to usage credits from the first request; the appended row says so.
    assert_eq!(row("claude-fable-5-1[1m]").note, "usage credits");
}

#[test]
fn ai_claude_subscription_privacy_shrunk_picker_is_unioned_with_the_pinned_table() {
    seal();
    let picker = json!([
        {"value": "opus", "resolvedModel": "claude-opus-5-5", "displayName": "Opus", "description": "Opus 5.5 · Best for everyday, complex tasks"},
        {"value": "opus[1m]", "resolvedModel": "claude-opus-5-5[1m]", "displayName": "Opus (1M context)", "description": "Opus 5.5 with 1M context"},
        {"value": "sonnet", "resolvedModel": "claude-sonnet-5", "displayName": "Sonnet", "description": "Sonnet 5 · Efficient for routine tasks"},
        {"value": "fable", "resolvedModel": "claude-fable-5-1", "displayName": "Fable", "description": "Fable 5.1 · Most capable for your hardest tasks"},
        {"value": "haiku", "resolvedModel": "claude-haiku-4-5", "displayName": "Haiku", "description": "Haiku 4.5 · Fastest for quick answers"},
    ]);
    let models = discover(picker);
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    let advertised = [
        "claude-opus-5-5[1m]",
        "claude-sonnet-5[1m]",
        "claude-fable-5-1[1m]",
        "claude-haiku-4-5-20251001",
    ];
    assert_eq!(ids[..4], advertised);
    let routes: std::collections::BTreeSet<String> = pinned_routes().into_iter().map(|(r, _, _)| r).collect();
    let all: std::collections::BTreeSet<String> = ids.iter().map(|s| s.to_string()).collect();
    assert_eq!(all, routes);
    assert_eq!(all.len(), ids.len());
    assert!(!ids.iter().any(|i| i.starts_with("claude-opus-4-6")));
}

#[test]
fn ai_claude_subscription_logged_out_missing_or_leaking_cli_degrades_to_the_catalog() {
    seal();
    let fake = Fake::new();
    let (config, env) = fake_config(
        &fake,
        "http://127.0.0.1:9",
        &[
            ("FAKE_AUTH", r#"{"loggedIn": false, "authMethod": "none"}"#),
            ("FAKE_MODELS", "[]"),
        ],
    );
    let status = setup_status(&config, &env, TIMEOUT);
    assert!(status.available && !status.logged_in);
    assert_eq!(status.detail, ai::claude_subscription::LOGIN_HINT);
    assert_eq!(discover_models(&config, &env, TIMEOUT), None);
    let mut missing = config.clone();
    missing.command = Some(vec![fake.root().join("nope").display().to_string()]);
    let status = setup_status(&missing, &env, TIMEOUT);
    assert!(!status.available && !status.logged_in);
    assert!(status.detail.to_lowercase().contains("install"));
    assert_eq!(discover_models(&missing, &env, TIMEOUT), None);
    // A handshake that reaches the upstream is not trusted.
    let upstream = Upstream::start(|_, stream| reply_status(stream, 200, "{}"));
    let models = pinned_picker().to_string();
    let (config, env) = fake_config(
        &fake,
        &upstream.url(),
        &[
            ("FAKE_AUTH", PRO),
            ("FAKE_MODELS", &models),
            ("FAKE_HANDSHAKE_UPSTREAM", "1"),
        ],
    );
    assert_eq!(discover_models(&config, &env, TIMEOUT), None);
    assert_eq!(upstream.requests().len(), 1);
}

#[test]
fn ai_claude_subscription_discovery_probes_follow_the_telemetry_setting() {
    seal();
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| reply_status(stream, 500, "{}"));
    let models = pinned_picker().to_string();
    let base = [
        ("FAKE_AUTH", PRO),
        ("FAKE_MODELS", models.as_str()),
        ("DO_NOT_TRACK", "1"),
    ];
    let (config, env) = fake_config(&fake, &upstream.url(), &base);
    assert!(discover_models(&config, &env, TIMEOUT).is_some());
    let mut quiet = base.to_vec();
    quiet.push(("ULTRON_CLAUDE_TELEMETRY", "off"));
    let (config, env) = fake_config(&fake, &upstream.url(), &quiet);
    assert!(discover_models(&config, &env, TIMEOUT).is_some());
    let records = fake.records();
    assert_eq!(records.len(), 4, "auth status then the handshake, twice");
    for (i, record) in records.iter().enumerate() {
        let env = &record["env"];
        assert_eq!(env["DISABLE_AUTOUPDATER"], "1");
        assert_eq!(env["DISABLE_FEEDBACK_COMMAND"], "1");
        assert_eq!(env["DO_NOT_TRACK"], "1");
        assert_eq!(env.get("DISABLE_TELEMETRY").is_some(), i >= 2, "{i}");
    }
    assert_eq!(records[1]["kind"], "handshake");
    assert!(
        records[1]["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .unwrap()
            .contains("/admit/")
    );
}

#[test]
fn ai_claude_subscription_inert_mcp_serves_the_inventory_and_denies_every_call() {
    seal();
    let manifest = json!([{"name": "rlm", "description": "d", "inputSchema": {"type": "object"}}]);
    assert_eq!(
        reply(
            &manifest,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        )
        .unwrap(),
        json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "ultron-inert-inventory", "version": "1"}}})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#).unwrap()["result"],
        json!({"tools": manifest})
    );
    assert_eq!(
        reply(
            &manifest,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rlm","arguments":{"code":"import os"}}}"#
        )
        .unwrap()["result"],
        json!({"isError": true, "content": [{"type": "text", "text": DENIAL}]})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#).unwrap()["result"],
        json!({})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        None
    );
    assert_eq!(reply(&manifest, "not json"), None);
    let mut out = Vec::new();
    serve(
        &manifest,
        "{\"id\":1,\"method\":\"tools/list\"}\n\n{\"method\":\"x\"}\n".as_bytes(),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
    // The standalone binary refuses a missing manifest argument.
    let status = std::process::Command::new(inert_mcp())
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}

#[test]
fn ai_claude_subscription_setup_probes_never_see_api_credentials() {
    seal();
    // `claude auth status` with an API key in its environment would report key auth instead of the plan login.
    let fake = Fake::new();
    let (config, env) = fake_config(
        &fake,
        "http://127.0.0.1:9",
        &[
            ("FAKE_AUTH", PRO),
            ("ANTHROPIC_API_KEY", "sk-ant-api03-fixture"),
            ("ANTHROPIC_AUTH_TOKEN", "t"),
        ],
    );
    assert!(setup_status(&config, &env, TIMEOUT).logged_in);
    let probe = fake
        .records()
        .into_iter()
        .find(|r| r["kind"] == "auth")
        .expect("the status probe ran");
    assert!(probe["env"].get("ANTHROPIC_API_KEY").is_none());
    assert!(probe["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
}
