//! Replays `fixtures/extension-providers.json` (recorded from the TypeScript model runtime by
//! `fixtures/generate-extension-providers.mjs`): providers extensions register (`registerProvider`) over the built-in
//! catalog and models.json: their models, the refused registrations, merging, availability, request auth, references.

use ai::Model;
use ai::model_registry::ModelRegistry;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/extension-providers.json")).unwrap()
}

fn normalize(value: Value) -> Value {
    ai::json::normalize_numbers(value)
}

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
    spec: Value,
    name: String,
    registry: ModelRegistry,
    registered: Vec<Option<String>>,
    env: HashMap<String, String>,
    providers: Vec<String>,
    _dir: tempfile::TempDir,
}

fn cases() -> Vec<Case> {
    let cases = fixture()["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 9);
    cases
        .into_iter()
        .map(|spec| {
            let dir = tempfile::Builder::new()
                .prefix("ai-extension-providers-")
                .tempdir()
                .unwrap();
            let path = dir.path().join("models.json");
            if !spec["models"].is_null() {
                fs::write(&path, serde_json::to_string(&spec["models"]).unwrap()).unwrap();
            }
            let mut registry = ModelRegistry::load(&path, dir.path());
            let registered = spec["register"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    registry
                        .register_extension_provider(r[0].as_str().unwrap(), r[1].as_object().unwrap())
                        .err()
                })
                .collect();
            let env = spec["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect();
            Case {
                name: spec["name"].as_str().unwrap().to_string(),
                providers: spec["providers"].as_object().unwrap().keys().cloned().collect(),
                registry,
                registered,
                env,
                spec,
                _dir: dir,
            }
        })
        .collect()
}

fn models_json(models: &[Model]) -> Value {
    normalize(serde_json::to_value(models).unwrap())
}

#[test]
fn ai_extension_providers_compose_like_typescript() {
    for case in cases() {
        for provider in &case.providers {
            assert_eq!(
                models_json(&case.registry.models(provider)),
                normalize(case.spec["providers"][provider].clone()),
                "{}: {provider}",
                case.name
            );
        }
        assert_eq!(json!(case.registry.error()), case.spec["error"], "{}", case.name);
    }
}

#[test]
fn ai_extension_providers_refuse_registrations_that_do_not_compose() {
    let mut refused = 0;
    for case in cases() {
        assert_eq!(json!(case.registered), case.spec["registered"], "{}", case.name);
        refused += case.registered.iter().flatten().count();
    }
    assert_eq!(refused, 3);
}

#[test]
fn ai_extension_providers_availability_matches_typescript() {
    for case in cases() {
        let lookup = |name: &str| case.env.get(name).cloned();
        let available: Vec<String> = case
            .registry
            .all_models()
            .into_iter()
            .filter(|m| case.providers.contains(&m.provider))
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
fn ai_extension_providers_request_auth_matches_typescript() {
    let mut checked = 0;
    for case in cases() {
        let lookup = |name: &str| case.env.get(name).cloned();
        for entry in case.spec["auth"].as_array().unwrap() {
            let (provider, id) = entry["model"].as_str().unwrap().split_once('/').unwrap();
            let model = case.registry.find(provider, id).unwrap();
            let actual = match case.registry.request_auth(
                &model,
                entry["apiKey"].as_str(),
                &lookup,
                inherited(provider, &case.env),
            ) {
                Ok(None) => Value::Null,
                Ok(Some(auth)) => {
                    let headers = auth
                        .headers
                        .map(|h| Value::Object(h.into_iter().map(|(k, v)| (k, json!(v))).collect::<Map<_, _>>()));
                    json!({"apiKey": auth.api_key, "headers": headers})
                }
                Err(error) => json!({"error": error}),
            };
            assert_eq!(actual, entry["result"], "{}: {}", case.name, entry["model"]);
            checked += 1;
        }
    }
    assert_eq!(checked, 9);
}

#[test]
fn ai_extension_providers_references_match_typescript() {
    let mut checked = 0;
    for case in cases() {
        for entry in case.spec["resolve"].as_array().unwrap() {
            match case
                .registry
                .resolve(entry["provider"].as_str(), entry["model"].as_str().unwrap())
            {
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
    assert_eq!(checked, 9);
}

/// Extension providers come after the built-in and models.json providers, in registration order.
#[test]
fn ai_extension_providers_are_listed_after_models_json_providers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("models.json");
    fs::write(
        &path,
        r#"{"providers":{"json-p":{"baseUrl":"http://h","api":"openai-completions","apiKey":"k","models":[{"id":"a"}]}}}"#,
    )
    .unwrap();
    let mut registry = ModelRegistry::load(&path, dir.path());
    let config = |id: &str| {
        json!({"baseUrl": "http://h", "api": "openai-completions", "apiKey": "k", "models": [{"id": id}]})
            .as_object()
            .unwrap()
            .clone()
    };
    registry.register_extension_provider("zeta", &config("z")).unwrap();
    registry.register_extension_provider("alpha", &config("a")).unwrap();
    let ids = registry.provider_ids();
    let tail: Vec<&str> = ids[ids.len() - 3..].iter().map(String::as_str).collect();
    assert_eq!(tail, ["json-p", "zeta", "alpha"]);
    // A definition that leaves the required members out gets modelFromJson's defaults.
    let model = registry.find("zeta", "z").unwrap();
    assert_eq!(
        (
            model.name.as_str(),
            model.context_window,
            model.max_tokens,
            model.input.clone()
        ),
        ("z", 128000, 16384, vec!["text".to_string()])
    );
}
