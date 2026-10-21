//! Bare Claude ids in `--models` / `enabledModels` / `--model` patterns: TypeScript has no `claude-subscription`
//! provider, so there `claude-opus-5-5` matches the `anthropic` model alone (`claude-code` answers only to its full
//! reference, model-resolver.ts `isClaudeCodeModel`). The Claude subscription's models carry Anthropic's ids, so
//! they too answer only to `claude-subscription/<id>`, and a bare id still selects one model.

use ai::Model;
use model_scope::{find_exact_model_reference_match, parse_model_pattern, resolve_model_scope};

fn models() -> Vec<Model> {
    let anthropic = ai::models::builtin_model("anthropic", "claude-opus-4-6")
        .unwrap()
        .clone();
    let mut opus = anthropic.clone();
    opus.id = "claude-opus-5-5".into();
    let subscription = ai::claude_subscription::catalog::model("claude-opus-5-5").unwrap();
    let mut claude_code = subscription.clone();
    claude_code.api = "claude-code-cli".into();
    claude_code.provider = "claude-code".into();
    vec![subscription, opus, claude_code]
}

fn reference(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

#[test]
fn ai_model_scope_bare_claude_ids_match_only_the_anthropic_model() {
    let models = models();
    let exact = find_exact_model_reference_match("claude-opus-5-5", &models).map(reference);
    assert_eq!(exact.as_deref(), Some("anthropic/claude-opus-5-5"));
    let partial = parse_model_pattern("opus-5-5", &models, true).model.map(reference);
    assert_eq!(partial.as_deref(), Some("anthropic/claude-opus-5-5"));
    let patterns = ["claude-opus-5-5".to_string(), "claude-opus-5*".to_string()];
    let (scoped, diagnostics) = resolve_model_scope(&patterns, &models);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let picked: Vec<String> = scoped.iter().map(|s| reference(s.model)).collect();
    assert_eq!(picked, ["anthropic/claude-opus-5-5"]);
}

#[test]
fn ai_model_scope_full_references_still_select_subscription_and_claude_code_models() {
    let models = models();
    let patterns = [
        "claude-subscription/claude-opus-5-5".to_string(),
        "claude-code/claude-opus-5-5:high".to_string(),
        "claude-subscription/*".to_string(),
    ];
    let (scoped, diagnostics) = resolve_model_scope(&patterns, &models);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let picked: Vec<(String, Option<String>)> = scoped
        .iter()
        .map(|s| (reference(s.model), s.thinking_level.clone()))
        .collect();
    assert_eq!(
        picked,
        [
            ("claude-subscription/claude-opus-5-5".to_string(), None),
            ("claude-code/claude-opus-5-5".to_string(), Some("high".to_string())),
        ]
    );
    // Without the anthropic model a bare id selects nothing rather than the subscription's.
    let only: Vec<Model> = models.into_iter().filter(|m| m.provider != "anthropic").collect();
    assert_eq!(find_exact_model_reference_match("claude-opus-5-5", &only), None);
}
