//! Replays `fixtures/model-registry.json` (recorded from the TypeScript model runtime by
//! `fixtures/generate-model-registry.mjs`): models.json providers and models composed over the built-in catalog,
//! composition errors, availability, request auth (key and headers) and exact model references.

use ai::model_registry::{
    ModelRegistry, env_names, request_options, resolve_config_value, resolve_config_value_or_err,
};
use ai::{Model, SimpleStreamOptions};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/model-registry.json")).unwrap()
}

/// A fresh directory, removed when the guard drops (the cases' models.json files never outlive the test).
fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("ai-model-registry-{tag}-"))
        .tempdir()
        .unwrap()
}

fn normalize(value: Value) -> Value {
    ai::json::normalize_numbers(value)
}

fn models_json(models: &[Model]) -> Value {
    normalize(serde_json::to_value(models).unwrap())
}

/// The key the built-in provider finds in `env` (its `checkAuth`/`resolve` without models.json).
fn inherited(provider: &str, env: &HashMap<String, String>) -> Option<String> {
    let lookup = |name: &str| env.get(name).cloned();
    let source = ai::env::EnvSource {
        lookup: &lookup,
        exists: &|_: &Path| false,
        home: None,
    };
    ai::env::env_api_key_in(provider, &source)
}

struct Case {
    name: String,
    registry: ModelRegistry,
    env: HashMap<String, String>,
    dir: PathBuf,
    spec: Value,
    /// Keeps the cases' root alive while any case is in use.
    _root: std::sync::Arc<tempfile::TempDir>,
}

fn cases(tag: &str) -> Vec<Case> {
    let guard = std::sync::Arc::new(temp_dir(tag));
    let root = guard.path().to_path_buf();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 13);
    cases
        .into_iter()
        .enumerate()
        .map(|(n, spec)| {
            let dir = root.join(format!("case-{n}"));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("models.json");
            fs::write(&path, serde_json::to_string_pretty(&spec["models"]).unwrap()).unwrap();
            let env = spec["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect();
            Case {
                name: spec["name"].as_str().unwrap().to_string(),
                registry: ModelRegistry::load(&path, &home),
                env,
                dir,
                spec,
                _root: guard.clone(),
            }
        })
        .collect()
}

fn case_providers(case: &Case) -> Vec<String> {
    case.spec["models"]["providers"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[test]
fn ai_model_registry_composes_models_like_typescript() {
    for case in cases("compose") {
        for provider in case_providers(&case) {
            assert_eq!(
                models_json(&case.registry.models(&provider)),
                normalize(case.spec["providers"][&provider].clone()),
                "{}: {provider}",
                case.name
            );
        }
    }
}

#[test]
fn ai_model_registry_errors_match_typescript() {
    for case in cases("errors") {
        let error = case
            .registry
            .error()
            .map(|e| e.replace(&case.dir.display().to_string(), "<dir>"));
        assert_eq!(json!(error), case.spec["error"], "{}", case.name);
    }
}

#[test]
fn ai_model_registry_availability_matches_typescript() {
    for case in cases("available") {
        let providers = case_providers(&case);
        let lookup = |name: &str| case.env.get(name).cloned();
        let available: Vec<String> = case
            .registry
            .all_models()
            .into_iter()
            .filter(|m| providers.contains(&m.provider))
            .filter(|m| {
                let inherited = inherited(&m.provider, &case.env).is_some();
                case.registry.auth_configured(&m.provider, &lookup, inherited)
            })
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(json!(available), case.spec["available"], "{}", case.name);
    }
}

#[test]
fn ai_model_registry_request_auth_matches_typescript() {
    let mut checked = 0;
    for case in cases("auth") {
        let lookup = |name: &str| case.env.get(name).cloned();
        for entry in case.spec["auth"].as_array().unwrap() {
            let (provider, id) = entry["model"].as_str().unwrap().split_once('/').unwrap();
            let model = case.registry.find(provider, id).unwrap();
            let auth = case.registry.request_auth(
                &model,
                entry["apiKey"].as_str(),
                &lookup,
                inherited(provider, &case.env),
            );
            let actual = match auth {
                Ok(None) => Value::Null,
                Ok(Some(auth)) => {
                    let headers = auth
                        .headers
                        .map(|h| Value::Object(h.into_iter().map(|(k, v)| (k, json!(v))).collect::<Map<_, _>>()));
                    json!({"apiKey": auth.api_key, "headers": headers})
                }
                Err(error) => json!({"error": error}),
            };
            let mut expected = entry["result"].clone();
            if let Some(map) = expected.as_object_mut() {
                map.remove("cause");
            }
            assert_eq!(actual, expected, "{}: {}", case.name, entry["model"]);
            checked += 1;
        }
    }
    assert_eq!(checked, 18);
}

#[test]
fn ai_model_registry_exact_references_match_typescript() {
    let mut checked = 0;
    for case in cases("resolve") {
        for entry in case.spec["resolve"].as_array().unwrap() {
            let resolved = case
                .registry
                .resolve(entry["provider"].as_str(), entry["model"].as_str().unwrap());
            match resolved {
                Ok(model) => assert_eq!(
                    normalize(serde_json::to_value(&model).unwrap()),
                    normalize(entry["result"].clone()),
                    "{}: {}",
                    case.name,
                    entry["model"]
                ),
                Err(error) => assert_eq!(json!(error), entry["error"], "{}: {}", case.name, entry["model"]),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 18);
}

/// A model resolved for requests carries its configured headers; they go out under the caller's own, never in the
/// model's JSON.
#[test]
fn ai_model_registry_request_headers_reach_options_not_json() {
    let case = cases("options").into_iter().next().unwrap();
    let model = case.registry.find("cliproxyapi", "gpt-6.1-sol").unwrap();
    let env = case.env.clone();
    let model = case
        .registry
        .for_requests(model, std::sync::Arc::new(move |name: &str| env.get(name).cloned()));
    assert!(!serde_json::to_string(&model).unwrap().contains("tok-1"));
    let mut options = SimpleStreamOptions::default();
    options.base.api_key = Some("proxy-key".into());
    options.base.headers = Some(
        [("x-model".to_string(), Some("caller".to_string()))]
            .into_iter()
            .collect(),
    );
    let prepared = request_options(&model, &options).unwrap().unwrap();
    let headers: Vec<(String, Option<String>)> = prepared.base.headers.unwrap().into_iter().collect();
    assert_eq!(
        headers,
        [
            ("x-proxy-token", "tok-1"),
            ("X-Static", "model-wins"),
            ("x-model", "caller")
        ]
        .map(|(k, v)| (k.to_string(), Some(v.to_string())))
    );
    // A model the registry did not prepare sends what the caller sends.
    let plain = case.registry.find("cliproxyapi", "plain").unwrap();
    assert!(request_options(&plain, &options).unwrap().is_none());
}

/// `authHeader` without any key fails the request with the TypeScript auth error.
#[test]
fn ai_model_registry_auth_header_needs_a_key() {
    let case = cases("bearer").into_iter().nth(1).unwrap();
    let model = case.registry.for_requests(
        case.registry.find("gw", "m").unwrap(),
        std::sync::Arc::new(|_: &str| None),
    );
    let Err(error) = request_options(&model, &SimpleStreamOptions::default()) else {
        panic!("a bearer header without a key must fail");
    };
    assert_eq!(
        error,
        "API key auth failed for provider gw: authHeader requires a resolved API key"
    );
}

/// `resolve-config-value.test.ts` literals: templates, escapes and commands.
#[test]
fn ai_model_registry_config_values() {
    let env: HashMap<&str, &str> = [("A", "x"), ("B", "y"), ("EMPTY", "")].into_iter().collect();
    let lookup = |name: &str| env.get(name).map(|v| v.to_string());
    let resolve = |value: &str| resolve_config_value(value, &lookup);
    assert_eq!(resolve("literal").as_deref(), Some("literal"));
    assert_eq!(resolve("$A").as_deref(), Some("x"));
    assert_eq!(resolve("pre-${A}-$B-post").as_deref(), Some("pre-x-y-post"));
    assert_eq!(resolve("$$A and $!").as_deref(), Some("$A and !"));
    assert_eq!(resolve("${not a name}").as_deref(), Some("${not a name}"));
    assert_eq!(resolve("$EMPTY"), None);
    assert_eq!(resolve("$MISSING"), None);
    assert_eq!(resolve("!printf '  out  '").as_deref(), Some("out"));
    assert_eq!(resolve("!true"), None);
    assert_eq!(resolve("!exit 1"), None);
    assert_eq!(env_names("$A ${B} $A $$C"), ["A", "B"]);
    assert!(env_names("!echo $A").is_empty());
    assert_eq!(resolve_config_value_or_err("lit$", "x", &lookup).as_deref(), Ok("lit$"));
    assert_eq!(
        resolve_config_value_or_err("$EMPTY", "the key", &lookup).unwrap_err(),
        "Failed to resolve the key from environment variable: EMPTY"
    );
}

/// claude-subscription takes any valid id (its catalog is a route table, not a list): a models.json entry for it
/// (a base URL, as tests and proxies set) applies to whatever id is named, and references resolve with or without it.
#[test]
fn ai_model_registry_dynamic_catalog_provider() {
    let tmp = temp_dir("dynamic");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("models.json");
    let none = ModelRegistry::load(&path, &dir);
    let plain = none.resolve(None, "claude-subscription/claude-opus-5-5").unwrap();
    assert_eq!(plain.provider, "claude-subscription");
    fs::write(
        &path,
        r#"{"providers":{"claude-subscription":{"baseUrl":"http://127.0.0.1:9"}}}"#,
    )
    .unwrap();
    let registry = ModelRegistry::load(&path, &dir);
    let model = registry.resolve(None, "claude-subscription/claude-opus-5-5").unwrap();
    assert_eq!(
        (model.id.as_str(), model.base_url.as_str()),
        ("claude-opus-5-5", "http://127.0.0.1:9")
    );
    assert_eq!(registry.error(), None);
    fs::remove_dir_all(&dir).unwrap();
}
