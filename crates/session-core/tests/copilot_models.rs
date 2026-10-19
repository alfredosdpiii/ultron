//! GitHub Copilot's `filterModels` in the available snapshot (`ModelRuntime.getAvailable`, packages/ai/src/models.ts
//! `provider.filterModels?.(models, credential)`, github-copilot.ts): an OAuth credential listing
//! `availableModelIds` limits the provider's models to those; without the list every catalog model is available.
//! Private temp profile, injected environment, nothing reaches the network.

use serde_json::json;
use session_core::RunEnv;
use session_core::lane;
use std::path::Path;
use std::sync::Arc;

fn env(dir: &Path, credential: serde_json::Value) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("auth.json"),
        json!({ "github-copilot": credential }).to_string(),
    )
    .unwrap();
    let agent_dir = agent.display().to_string();
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent_dir.clone())),
        docs: dir.join("docs"),
    }
}

fn copilot_ids(env: &RunEnv) -> Vec<String> {
    lane::available_models(env)
        .into_iter()
        .filter(|m| m.provider == "github-copilot")
        .map(|m| m.id)
        .collect()
}

#[test]
fn session_core_copilot_available_model_ids_limit_the_available_models() {
    let tmp = tempfile::tempdir().unwrap();
    let oauth = |extra: serde_json::Value| {
        let mut credential = json!({"type": "oauth", "refresh": "r", "access": "a", "expires": 4_102_444_800_000u64});
        credential
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        credential
    };
    let all = copilot_ids(&env(&tmp.path().join("all"), oauth(json!({}))));
    assert!(all.len() > 3, "{all:?}");
    let listed = [all[2].clone(), all[0].clone(), "not-in-the-catalog".to_string()];
    let limited = copilot_ids(&env(
        &tmp.path().join("limited"),
        oauth(json!({ "availableModelIds": listed })),
    ));
    // Catalog order, only the listed ones that exist.
    assert_eq!(limited, vec![all[0].clone(), all[2].clone()]);
    // A list with a non-string entry is ignored (`every(id => typeof id === "string")`).
    let mixed = copilot_ids(&env(
        &tmp.path().join("mixed"),
        oauth(json!({ "availableModelIds": [all[0], 3] })),
    ));
    assert_eq!(mixed, all);
}
