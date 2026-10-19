//! `fixtures/legacy.json` lists every export of the TypeScript `legacy-api-aliases.ts` with its target API.

use ai::legacy::LEGACY_API_ALIASES;
use serde_json::{Value, json};

#[test]
fn ai_legacy_aliases_match_typescript_exports() {
    let expected: Vec<Value> = serde_json::from_str(include_str!("../fixtures/legacy.json")).unwrap();
    let mut actual: Vec<Value> = LEGACY_API_ALIASES
        .iter()
        .map(|a| json!({"name": a.name, "api": a.api, "simple": a.simple}))
        .collect();
    actual.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    assert_eq!(actual, expected);
    // Every legacy alias's API is ported, so each one is exported from `ai::legacy`.
    assert!(LEGACY_API_ALIASES.iter().all(|a| a.implemented));
    // Every implemented alias names an API with a model in the built-in catalog.
    for alias in LEGACY_API_ALIASES.iter().filter(|a| a.implemented) {
        assert!(
            ai::models::builtin_providers()
                .flat_map(ai::models::builtin_models)
                .any(|m| m.api == alias.api),
            "{}",
            alias.api
        );
    }
}
