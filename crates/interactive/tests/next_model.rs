//! Replays `fixtures/next-model.json` (recorded from the TypeScript `nextModel` of client-tui-actions.ts by
//! `fixtures/generate-next-model.mjs`): Ctrl+P / Shift+Ctrl+P over the available models, scoped by `enabledModels`
//! through `resolveModelScopeFromModels` (globs, fuzzy patterns, `:<thinking>` suffixes, Claude Code CLI models only
//! by their full reference), in the available order.

use interactive::app::next_model;
use interactive::lane::ModelInfo;
use serde_json::Value;
use std::collections::HashMap;

#[test]
fn interactive_next_model_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/next-model.json")).unwrap();
    let lists: HashMap<String, Vec<ModelInfo>> = fixture["lists"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, rows)| {
            let models = rows
                .as_array()
                .unwrap()
                .iter()
                .map(|row| ModelInfo {
                    provider: row[0].as_str().unwrap().into(),
                    id: row[1].as_str().unwrap().into(),
                    name: row[2].as_str().unwrap().into(),
                    reasoning: row[3].as_bool().unwrap(),
                    context_window: 0,
                    thinking_levels: Vec::new(),
                })
                .collect();
            (name.clone(), models)
        })
        .collect();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 22);
    for case in cases {
        let list = &lists[case["list"].as_str().unwrap()];
        let (provider, id) = case["current"].as_str().unwrap().split_once('/').unwrap();
        let current = ModelInfo {
            provider: provider.into(),
            id: id.into(),
            name: String::new(),
            reasoning: false,
            context_window: 0,
            thinking_levels: Vec::new(),
        };
        let enabled: Vec<String> = case["enabled"]
            .as_array()
            .map(|a| a.iter().map(|p| p.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();
        let direction = case["direction"].as_i64().unwrap() as isize;
        let actual = match next_model(list, &current, direction, &enabled) {
            Ok(model) => (Some(format!("{}/{}", model.provider, model.id)), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let expected = (
            case["next"].as_str().map(str::to_string),
            case["error"].as_str().map(str::to_string),
        );
        assert_eq!(actual, expected, "{case}");
    }
}
