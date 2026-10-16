//! Replays fixtures/context_images.json (fixtures/generate-context-images.mjs): TypeScript's `contextImageBudget`
//! reads ULTRON_CONTEXT_IMAGES with `Number()`, so hex, octal and binary literals, exponents and a zero fraction are
//! budgets too; anything else keeps the default.

use serde_json::Value;
use session_core::view_images::context_image_budget;

#[test]
fn session_core_context_image_budget_reads_numbers_like_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/context_images.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 30);
    for case in cases {
        let value = case["value"].as_str();
        // A budget past `usize` drops nothing either way.
        let expected = case["budget"].as_f64().unwrap().min(usize::MAX as f64) as usize;
        assert_eq!(context_image_budget(value), expected, "ULTRON_CONTEXT_IMAGES={value:?}");
    }
}
