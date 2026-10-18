//! Installed hook commands (loki.py test_branch_without_loki_passes_and_deleted_engine_still_fails): every owned
//! hook `loki init` writes, the shell guard included, passes with a note on a branch without Loki (neither the
//! working tree nor HEAD has `.loki/loki.py`, another ref does) and still fails closed when the engine HEAD tracks is
//! deleted from the working tree, or was never committed. Git runs with a private HOME and no global config.

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn git(root: &Path, home: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Loki",
            "-c",
            "user.email=loki@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn run(command: &str, root: &Path, home: &Path) -> Output {
    Command::new("sh")
        .args(["-c", command])
        .current_dir(root)
        .env("HOME", home)
        .env("FACTORY_PROJECT_DIR", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Every handler command in the installed hook files.
fn handlers(root: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    for file in [".claude/settings.json", ".codex/hooks.json", ".factory/hooks.json"] {
        let document: Value = serde_json::from_str(&std::fs::read_to_string(root.join(file)).unwrap()).unwrap();
        let groups = document.get("hooks").unwrap_or(&document).as_object().unwrap().clone();
        for entries in groups.values() {
            for entry in entries.as_array().unwrap() {
                for handler in entry["hooks"].as_array().unwrap() {
                    commands.push(handler["command"].as_str().unwrap().to_string());
                }
            }
        }
    }
    commands
}

#[test]
fn loki_installed_hooks_pass_on_a_branch_without_loki_and_fail_closed_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, home) = (base.join("repo"), base.join("home"));
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("nested/.keep"), "").unwrap();
    git(&root, &home, &["init", "-q", "-b", "main"]);
    git(&root, &home, &["add", "."]);
    git(&root, &home, &["commit", "-q", "-m", "base"]);
    loki::init::init(root.to_str().unwrap(), false, true, None).unwrap();
    let commands = handlers(&root);
    assert!(
        commands.iter().any(|command| command.contains(" shell --harness ")),
        "{commands:?}"
    );
    assert!(commands.len() >= 8, "{commands:?}");
    // Never committed anywhere: still fails closed.
    std::fs::rename(root.join(".loki/loki.py"), base.join("engine.bak")).unwrap();
    for command in &commands {
        assert_eq!(run(command, &root, &home).status.code(), Some(2), "{command}");
    }
    std::fs::rename(base.join("engine.bak"), root.join(".loki/loki.py")).unwrap();
    git(&root, &home, &["add", "."]);
    git(&root, &home, &["commit", "-q", "-m", "Add Loki"]);
    // Deleted from the working tree while HEAD tracks it: still fails closed.
    std::fs::rename(root.join(".loki/loki.py"), base.join("engine.bak")).unwrap();
    for command in &commands {
        assert_eq!(run(command, &root, &home).status.code(), Some(2), "{command}");
    }
    std::fs::rename(base.join("engine.bak"), root.join(".loki/loki.py")).unwrap();
    // A branch without Loki: passes, and says Loki did not check.
    git(&root, &home, &["checkout", "-q", "-b", "plain", "HEAD~1"]);
    assert!(!root.join(".loki/loki.py").exists());
    for command in &commands {
        let output = run(command, &root, &home);
        assert_eq!(output.status.code(), Some(0), "{command}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "loki: .loki/loki.py is not on this branch; Loki did not check this\n",
            "{command}"
        );
    }
}

/// The hook templates `loki init` installs are loki.py's `templates/` (embedded from init-assets/templates): every
/// command starts with the missing-engine guard.
#[test]
fn loki_hook_templates_start_with_the_missing_engine_guard() {
    for name in [
        "claude-settings.fragment.json",
        "codex-hooks.json",
        "factory-hooks.json",
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("init-assets/templates")
            .join(name);
        let document: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let groups = document.get("hooks").unwrap_or(&document).as_object().unwrap().clone();
        let root = if name == "factory-hooks.json" {
            "\"$FACTORY_PROJECT_DIR\""
        } else {
            "\"$(git rev-parse --show-toplevel)\""
        };
        let prefix = loki::init::missing_engine_guard(root);
        for entries in groups.values() {
            for entry in entries.as_array().unwrap() {
                for handler in entry["hooks"].as_array().unwrap() {
                    let command = handler["command"].as_str().unwrap();
                    assert!(command.starts_with(&prefix), "{name}: {command}");
                }
            }
        }
    }
}
