//! Replays `fixtures/model-resolver.json` (recorded from the TypeScript model resolver by
//! `fixtures/generate-model-resolver.mjs`): `parseModelPattern`, `resolveModelScopeFromModels`, `resolveCliModel`,
//! `findInitialModel` and minimatch, over the TypeScript tests' model lists and the built-in catalog.

use ai::Model;
use ai::glob::minimatch;
use ai::model_resolver::{
    InitialSettings, ScopedModel, find_initial_model, parse_model_pattern, resolve_cli_model, resolve_model_scope,
};
use serde_json::{Value, json};
use std::collections::HashMap;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/model-resolver.json")).unwrap()
}

fn reference(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn lists(fixture: &Value) -> HashMap<String, Vec<Model>> {
    let mut lists = HashMap::new();
    for (name, value) in fixture["lists"].as_object().unwrap() {
        let models: Vec<Model> = if name == "catalog" {
            catalog()
        } else {
            serde_json::from_value(value.clone()).unwrap()
        };
        lists.insert(name.clone(), models);
    }
    lists
}

/// Every built-in model, providers in catalog order, without the `claude-code` provider's (the fixture's catalog
/// leaves them out; tests/claude_code_provider.rs covers resolution with them).
fn catalog() -> Vec<Model> {
    ai::models::builtin_providers()
        .filter(|p| *p != ai::claude_code::PROVIDER)
        .flat_map(ai::models::builtin_models)
        .cloned()
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn opt(value: &Value) -> Option<&str> {
    value.as_str()
}

/// `{ref}` for a model of the list, `{ref, model}` for one built from it.
fn check_model(actual: Option<&Model>, expected: &Value, list: &[Model], context: &str) {
    match (actual, expected) {
        (None, Value::Null) => {}
        (Some(model), Value::Object(e)) => {
            assert_eq!(reference(model), e["ref"].as_str().unwrap(), "{context}");
            match e.get("model") {
                Some(full) => assert_eq!(
                    ai::json::normalize_numbers(serde_json::to_value(model).unwrap()),
                    ai::json::normalize_numbers(full.clone()),
                    "{context}"
                ),
                None => assert!(
                    list.iter().any(|m| m == model),
                    "{context}: {} is the list's own model",
                    reference(model)
                ),
            }
        }
        _ => panic!("{context}: got {:?}, expected {expected}", actual.map(reference)),
    }
}

#[test]
fn ai_model_resolver_catalog_matches_typescript() {
    let fixture = fixture();
    let refs: Vec<String> = catalog().iter().map(reference).collect();
    assert_eq!(refs, strings(&fixture["lists"]["catalog"]));
    assert!(
        ai::models::builtin_model("anthropic", "claude-sonnet-5-5").is_some(),
        "the catalog lists Sonnet 5.5"
    );
}

#[test]
fn ai_model_resolver_parse_pattern_matches_typescript() {
    let fixture = fixture();
    let lists = lists(&fixture);
    let cases = fixture["parse"].as_array().unwrap();
    assert_eq!(cases.len(), 46);
    for case in cases {
        let list = &lists[case["list"].as_str().unwrap()];
        let pattern = case["pattern"].as_str().unwrap();
        let allow = case["allowInvalidThinkingLevelFallback"].as_bool().unwrap();
        let parsed = parse_model_pattern(pattern, list, allow);
        let context = format!("parse {pattern:?} in {}", case["list"]);
        check_model(parsed.model.as_ref(), &case["model"], list, &context);
        assert_eq!(
            parsed.thinking_level.as_deref(),
            opt(&case["thinkingLevel"]),
            "{context}"
        );
        assert_eq!(parsed.warning.as_deref(), opt(&case["warning"]), "{context}");
    }
}

#[test]
fn ai_model_resolver_scope_matches_typescript() {
    let fixture = fixture();
    let lists = lists(&fixture);
    let cases = fixture["scope"].as_array().unwrap();
    assert_eq!(cases.len(), 29);
    for case in cases {
        let list = &lists[case["list"].as_str().unwrap()];
        let patterns = strings(&case["patterns"]);
        let (scoped, diagnostics) = resolve_model_scope(&patterns, list);
        let context = format!("scope {patterns:?} in {}", case["list"]);
        let actual: Vec<Value> = scoped
            .iter()
            .map(|s| json!({"ref": reference(&s.model), "thinkingLevel": s.thinking_level}))
            .collect();
        assert_eq!(Value::Array(actual), case["scoped"], "{context}");
        let actual: Vec<Value> = diagnostics
            .iter()
            .map(|d| json!({"type": "warning", "code": d.code, "message": d.message, "pattern": d.pattern}))
            .collect();
        assert_eq!(Value::Array(actual), case["diagnostics"], "{context}");
    }
}

#[test]
fn ai_model_resolver_cli_model_matches_typescript() {
    let fixture = fixture();
    let lists = lists(&fixture);
    let cases = fixture["cli"].as_array().unwrap();
    assert_eq!(cases.len(), 63);
    for case in cases {
        let list = &lists[case["list"].as_str().unwrap()];
        let auth = strings(&case["auth"]);
        let has_auth = |p: &str| auth.iter().any(|a| a == p);
        let result = resolve_cli_model(
            opt(&case["cliProvider"]),
            opt(&case["cliModel"]),
            opt(&case["cliThinking"]),
            list,
            &has_auth,
        );
        let context = format!(
            "cli {:?}/{:?} thinking {:?} auth {auth:?} in {}",
            case["cliProvider"], case["cliModel"], case["cliThinking"], case["list"]
        );
        check_model(result.model.as_ref(), &case["model"], list, &context);
        assert_eq!(
            result.thinking_level.as_deref(),
            opt(&case["thinkingLevel"]),
            "{context}"
        );
        assert_eq!(result.warning.as_deref(), opt(&case["warning"]), "{context}");
        assert_eq!(result.error.as_deref(), opt(&case["error"]), "{context}");
    }
}

#[test]
fn ai_model_resolver_initial_model_matches_typescript() {
    let fixture = fixture();
    let lists = lists(&fixture);
    let cases = fixture["initial"].as_array().unwrap();
    assert_eq!(cases.len(), 13);
    for case in cases {
        let list = &lists[case["list"].as_str().unwrap()];
        let auth = strings(&case["auth"]);
        let has_auth = |p: &str| auth.iter().any(|a| a == p);
        let (scoped, _): (Vec<ScopedModel>, _) = resolve_model_scope(&strings(&case["patterns"]), list);
        let s = &case["settings"];
        let settings = InitialSettings {
            default_provider: opt(&s["defaultProvider"]).map(str::to_string),
            default_model_id: opt(&s["defaultModelId"]).map(str::to_string),
            default_thinking_level: opt(&s["defaultThinkingLevel"]).map(str::to_string),
            model_thinking_levels: s["modelThinkingLevels"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let available: Vec<Model> = list.iter().filter(|m| has_auth(&m.provider)).cloned().collect();
        let (model, thinking) = find_initial_model(
            &scoped,
            case["isContinuing"].as_bool().unwrap(),
            &settings,
            list,
            &available,
            &has_auth,
        );
        let context = format!("initial {} {:?}", case["list"], case["patterns"]);
        check_model(model.as_ref(), &case["model"], list, &context);
        assert_eq!(thinking, case["thinkingLevel"].as_str().unwrap(), "{context}");
    }
}

#[test]
fn ai_model_resolver_minimatch_matches_typescript() {
    let fixture = fixture();
    let cases = fixture["globs"].as_array().unwrap();
    assert_eq!(cases.len(), 53);
    for case in cases {
        let (text, pattern) = (case["text"].as_str().unwrap(), case["pattern"].as_str().unwrap());
        assert_eq!(
            minimatch(text, pattern),
            case["match"].as_bool().unwrap(),
            "minimatch({text:?}, {pattern:?})"
        );
    }
}
