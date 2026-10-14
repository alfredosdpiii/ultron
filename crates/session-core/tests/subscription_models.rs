//! The Claude subscription's pinned models are in the available snapshot (the /model selector, Ctrl+P scopes,
//! `--list-models`) when Claude Code is installed, and absent when it is not. Private temp profile and an injected
//! environment: the `claude` command is a stub that is never run, and nothing reaches the network.

use session_core::RunEnv;
use session_core::lane;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

fn env(dir: &Path, claude: &Path) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let map: HashMap<String, String> = [
        ("ULTRON_CODING_AGENT_DIR", agent.display().to_string()),
        ("ULTRON_CLAUDE_COMMAND", claude.display().to_string()),
        ("PATH", dir.join("empty-bin").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

fn subscription_ids(env: &RunEnv) -> Vec<String> {
    lane::available_models(env)
        .into_iter()
        .filter(|m| m.provider == "claude-subscription")
        .map(|m| m.id)
        .collect()
}

#[test]
fn session_core_subscription_models_are_listed_when_claude_code_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude");
    std::fs::write(&claude, "#!/bin/sh\nexit 97\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = env(dir.path(), &claude);
    let ids = subscription_ids(&env);
    let pinned: Vec<String> = ai::claude_subscription::catalog::pinned_routes()
        .into_iter()
        .map(|(_, id, _)| id.to_string())
        .collect();
    assert_eq!(ids, pinned);
    let opus = lane::available_models(&env)
        .into_iter()
        .find(|m| m.provider == "claude-subscription" && m.id == "claude-opus-5-5")
        .unwrap();
    assert_eq!(opus.context_window, 1_000_000);
    assert_eq!(opus.name, "Opus 5.5 (Claude subscription)");
}

#[test]
fn session_core_subscription_models_are_absent_without_claude_code() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path(), &dir.path().join("missing").join("claude"));
    assert!(subscription_ids(&env).is_empty());
}
