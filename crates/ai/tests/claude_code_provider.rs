//! Replays `fixtures/claude-code-provider.json` (recorded from the TypeScript model runtime by
//! `fixtures/generate-claude-code-provider.mjs`): the always registered `claude-code` provider — its five models, its
//! place after every catalog provider, availability by where its CLI would be found, models.json overrides,
//! `resolveCliModel`, `resolveModelScope` and `findInitialModel` with its models, and no remote catalog.
//!
//! The `claude` files are inert stand-ins (`exit 97`) the provider only checks for; nothing runs them.

use ai::Model;
use ai::model_registry::ModelRegistry;
use ai::model_resolver::{InitialSettings, find_initial_model, resolve_model_scope};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/claude-code-provider.json")).unwrap()
}

fn normalize(value: Value) -> Value {
    ai::json::normalize_numbers(value)
}

fn models_json(models: &[Model]) -> Value {
    normalize(serde_json::to_value(models).unwrap())
}

/// The generator's layout under a fresh root: `bin/claude` (executable), `plain/claude` (not executable),
/// `cmd/claude.cmd`, `home/.local/bin/claude`, and an empty `empty/`.
fn layout() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new().prefix("ai-claude-code-").tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for (dir, name, mode) in [
        ("bin", "claude", 0o755),
        ("plain", "claude", 0o644),
        ("cmd", "claude.cmd", 0o755),
        ("home/.local/bin", "claude", 0o755),
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        let file = root.join(dir).join(name);
        std::fs::write(&file, "#!/bin/sh\nexit 97\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    std::fs::create_dir_all(root.join("empty")).unwrap();
    (tmp, root)
}

struct Case {
    spec: Value,
    registry: ModelRegistry,
    env: HashMap<String, String>,
}

impl Case {
    fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
        |name: &str| self.env.get(name).cloned()
    }

    /// `hasConfiguredAuth` for every provider: its models.json key or the built-in provider's own check.
    fn configured(&self, provider: &str) -> bool {
        let lookup = self.lookup();
        let home = self.env.get("HOME").map(PathBuf::from);
        let source = ai::env::EnvSource {
            lookup: &lookup,
            exists: &|p: &Path| p.exists(),
            home,
        };
        let inherited = ai::env::env_api_key_in(provider, &source).is_some();
        self.registry.auth_configured(provider, &lookup, inherited)
    }

    fn available(&self) -> Vec<Model> {
        self.registry
            .provider_ids()
            .into_iter()
            .filter(|p| self.configured(p))
            .flat_map(|p| self.registry.models(&p))
            .collect()
    }
}

fn cases(root: &Path) -> Vec<Case> {
    let expand = |text: &str| text.replace("<root>", &root.display().to_string());
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let dir = root.join(format!("case-{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            if !spec["models"].is_null() {
                std::fs::write(dir.join("models.json"), spec["models"].to_string()).unwrap();
            }
            let mut env: HashMap<String, String> = [
                ("HOME".to_string(), expand("<root>/home")),
                ("PI_OFFLINE".to_string(), "1".to_string()),
            ]
            .into_iter()
            .collect();
            for (k, v) in spec["env"].as_object().unwrap() {
                env.insert(k.clone(), expand(v.as_str().unwrap()));
            }
            Case {
                spec: spec.clone(),
                registry: ModelRegistry::load(&dir.join("models.json"), &root.join("home")),
                env,
            }
        })
        .collect()
}

fn refs(models: &[Model]) -> Vec<String> {
    models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect()
}

#[test]
fn ai_claude_code_models_match_typescript() {
    let models: Vec<Model> = ai::models::builtin_models("claude-code").cloned().collect();
    assert_eq!(models_json(&models), normalize(fixture()["catalogModels"].clone()));
    let haiku = ai::models::builtin_model("claude-code", "claude-haiku-5-5").unwrap();
    assert!(ai::model_resolver::is_claude_code_model(haiku));
    assert_eq!(
        ai::models::supported_thinking_levels(haiku),
        ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
    );
    assert!(ai::models::builtin_model("claude-code", "claude-sonnet-5-5").is_none());
}

#[test]
fn ai_claude_code_provider_is_registered_last_like_typescript() {
    let ours: Vec<&str> = ai::models::builtin_providers().collect();
    let theirs: Vec<String> = serde_json::from_value(fixture()["providerOrder"].clone()).unwrap();
    assert_eq!(ours, theirs);
}

#[test]
fn ai_claude_code_availability_matches_typescript() {
    let (_tmp, root) = layout();
    for case in cases(&root) {
        let name = case.spec["name"].as_str().unwrap();
        assert_eq!(
            case.configured("claude-code"),
            case.spec["configured"].as_bool().unwrap(),
            "{name}: configured"
        );
        // The claude-code part of the available snapshot (the bundled catalogs of other providers are generated
        // separately for each implementation), and which providers are available.
        let ours = refs(&case.available());
        let theirs: Vec<String> = serde_json::from_value(case.spec["available"].clone()).unwrap();
        let claude_code = |refs: &[String]| -> Vec<String> {
            refs.iter().filter(|r| r.starts_with("claude-code/")).cloned().collect()
        };
        assert_eq!(claude_code(&ours), claude_code(&theirs), "{name}: available");
        let providers = |refs: &[String]| -> Vec<String> {
            let mut providers: Vec<String> = refs.iter().map(|r| r.split('/').next().unwrap().to_string()).collect();
            providers.dedup();
            providers
        };
        assert_eq!(providers(&ours), providers(&theirs), "{name}: available providers");
        assert_eq!(
            models_json(&case.registry.models("claude-code")),
            normalize(case.spec["providerModels"].clone()),
            "{name}: models"
        );
        assert_eq!(case.registry.error(), None, "{name}: error");
        // Requests always resolve (`getAuth` gives `{ auth: {} }`), CLI or not: the stream explains a missing one.
        let lookup = case.lookup();
        let source = ai::env::EnvSource {
            lookup: &lookup,
            exists: &|p: &Path| p.exists(),
            home: None,
        };
        assert!(!case.spec["auth"].is_null(), "{name}: TypeScript auth resolves");
        let inherited = ai::env::env_api_key_in("claude-code", &source);
        let key = case.registry.api_key("claude-code", None, &lookup, inherited).unwrap();
        assert!(key.is_some(), "{name}: request key");
    }
}

#[test]
fn ai_claude_code_find_cli_reports_the_path_typescript_checks() {
    let (_tmp, root) = layout();
    let found = |env: &[(&str, String)]| {
        let map: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        let lookup = |name: &str| map.get(name).cloned();
        ai::claude_code::find_cli(&ai::env::EnvSource {
            lookup: &lookup,
            exists: &|p: &Path| p.exists(),
            home: Some(root.join("home")),
        })
    };
    let r = root.display().to_string();
    assert_eq!(
        found(&[("PATH", format!("{r}/empty:{r}/bin/"))]),
        Some(format!("{r}/bin/claude"))
    );
    assert_eq!(
        found(&[("PATH", format!("{r}/empty;{r}/cmd"))]),
        Some(format!("{r}/cmd/claude.cmd"))
    );
    assert_eq!(
        found(&[("ULTRON_CLAUDE_CODE_BIN", format!("  {r}/bin/claude "))]),
        Some(format!("{r}/bin/claude"))
    );
    assert_eq!(
        found(&[("ULTRON_CLAUDE_CODE_BIN", "~/.local/bin/claude".into())]),
        Some("~/.local/bin/claude".into())
    );
    assert_eq!(
        found(&[
            ("ULTRON_CLAUDE_CODE_BIN", format!("{r}/missing")),
            ("PATH", format!("{r}/bin"))
        ]),
        None
    );
    assert_eq!(found(&[]), None);
}

#[test]
fn ai_claude_code_resolution_matches_typescript() {
    let (_tmp, root) = layout();
    for case in cases(&root) {
        let name = case.spec["name"].as_str().unwrap().to_string();
        let has_auth = |p: &str| case.configured(p);
        for expected in case.spec["resolve"].as_array().unwrap() {
            let provider = expected["provider"].as_str();
            let model = expected["model"].as_str().unwrap();
            let got = case.registry.resolve_cli(provider, model, None, &has_auth);
            let label = format!("{name}: {provider:?} {model}");
            assert_eq!(
                got.model.as_ref().map(|m| format!("{}/{}", m.provider, m.id)),
                expected["result"].as_str().map(str::to_string),
                "{label}"
            );
            assert_eq!(got.thinking_level.as_deref(), expected["thinking"].as_str(), "{label}");
            assert_eq!(got.warning.as_deref(), expected["warning"].as_str(), "{label}");
            assert_eq!(got.error.as_deref(), expected["error"].as_str(), "{label}");
        }
    }
}

#[test]
fn ai_claude_code_scopes_and_initial_model_match_typescript() {
    let (_tmp, root) = layout();
    for case in cases(&root) {
        let name = case.spec["name"].as_str().unwrap().to_string();
        let available = case.available();
        for expected in case.spec["scoped"].as_array().unwrap() {
            let patterns: Vec<String> = serde_json::from_value(expected["patterns"].clone()).unwrap();
            let (scoped, diagnostics) = resolve_model_scope(&patterns, &available);
            let got: Vec<Value> = scoped
                .iter()
                .map(|s| json!({"model": format!("{}/{}", s.model.provider, s.model.id), "thinking": s.thinking_level}))
                .collect();
            assert_eq!(Value::from(got), expected["models"], "{name}: {patterns:?}");
            let messages: Vec<&str> = diagnostics.iter().map(|d| d.message.as_str()).collect();
            assert_eq!(
                json!(messages),
                expected["diagnostics"],
                "{name}: {patterns:?} diagnostics"
            );
        }
        let all: Vec<Model> = case
            .registry
            .provider_ids()
            .iter()
            .flat_map(|p| case.registry.models(p))
            .collect();
        let has_auth = |p: &str| available.iter().any(|m| m.provider == p);
        for expected in case.spec["initial"].as_array().unwrap() {
            let settings = InitialSettings {
                default_provider: expected["settings"]["defaultProvider"].as_str().map(str::to_string),
                default_model_id: expected["settings"]["defaultModelId"].as_str().map(str::to_string),
                ..InitialSettings::default()
            };
            let (model, thinking) = find_initial_model(&[], false, &settings, &all, &available, &has_auth);
            assert_eq!(
                model.map(|m| format!("{}/{}", m.provider, m.id)),
                expected["model"].as_str().map(str::to_string),
                "{name}: initial {}",
                expected["settings"]
            );
            assert_eq!(thinking, expected["thinking"].as_str().unwrap(), "{name}");
        }
    }
}

/// A transport that fails the test if anything is sent.
struct NoNetwork;

impl ai::Transport for NoNetwork {
    fn send(&self, request: ai::HttpRequest) -> ai::http::BoxFuture<'_, Result<ai::HttpResponse, String>> {
        panic!("claude-code must not fetch a catalog: {}", request.url);
    }
}

#[tokio::test]
async fn ai_claude_code_has_no_remote_catalog() {
    // A stored pi.dev-style entry newer than the bundled catalog changes other providers, never claude-code.
    let entry = json!({
        "lastModified": 9_999_999_999_999_i64,
        "checkedAt": 0,
        "models": [{"id": "haiku", "name": "Overlaid", "api": "claude-code-cli", "provider": "claude-code",
            "baseUrl": "x", "reasoning": false, "input": ["text"],
            "cost": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 1, "maxTokens": 1}],
    });
    assert!(ai::model_refresh::remote_overlay("claude-code", Some(&entry), Some(0)).is_empty());
    let mut other = entry.clone();
    other["models"][0]["provider"] = json!("anthropic");
    assert!(!ai::model_refresh::remote_overlay("anthropic", Some(&other), Some(0)).is_empty());
    let now = || 0_i64;
    let refreshed = ai::model_refresh::refresh_remote_catalog(
        "claude-code",
        Some(&entry),
        "http://127.0.0.1:9",
        true,
        "ua",
        &now,
        &NoNetwork,
    )
    .await;
    assert_eq!((refreshed.persist, refreshed.error), (None, None));
    // Its models stay the CLI's with a stored entry beside models.json.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("models-store.json"),
        json!({"claude-code": entry}).to_string(),
    )
    .unwrap();
    let registry = ModelRegistry::load(&tmp.path().join("models.json"), tmp.path());
    let haiku = registry.find("claude-code", "haiku").unwrap();
    assert_eq!(haiku.name, "Claude Haiku (Claude Code CLI)");
}
