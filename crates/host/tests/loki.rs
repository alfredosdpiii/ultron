//! Replays crates/host/fixtures/loki.json, recorded by fixtures/loki.mjs from the TypeScript host with Loki enabled
//! (session-worker.ts routing, file-hooks.ts, loki.ts's LokiGuard over the Python loki.py engine) serving the real
//! runtime.py: the same cells run through crates/kernel with this crate's FileHooks and LokiGuard (the Rust engine,
//! in-process) and must give identical results.

use host::file_hooks::{FileHooks, may_write_files};
use host::loki_guard::{LokiGuard, LokiMode, git_root, loki_mode};
use host::{Guard, Host};
use kernel::{Kernel, KernelOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/loki.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn mode(name: &str) -> LokiMode {
    match name {
        "on" => LokiMode::On,
        "advise" => LokiMode::Advise,
        _ => LokiMode::Off,
    }
}

/// The session's repository in a fresh temp dir, its guard as the CLI builds it, and each cell's normalized result.
fn replay(base: &Value, session: &Value) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().canonicalize().unwrap();
    let repo = tmp.join("repo");
    for files in [base, &session["files"]] {
        for (path, content) in files.as_object().into_iter().flatten() {
            let target = repo.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, content.as_str().unwrap()).unwrap();
        }
    }
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success());
    let cwd = match session["cwd"].as_str() {
        Some(sub) => repo.join(sub),
        None => repo.clone(),
    };
    let mut hooks = FileHooks::new(&cwd);
    let mode = mode(session["mode"].as_str().unwrap());
    if mode != LokiMode::Off {
        let root = git_root(&cwd).unwrap_or_else(|| cwd.clone());
        hooks.add(Box::new(LokiGuard::new(root, mode)));
    }
    let host = Arc::new(Host::new(&cwd, tmp.join(".jobs"), &tmp, Box::new(hooks)));
    let mut options = KernelOptions::new(&cwd, runtime());
    let tmp_text = tmp.to_string_lossy().into_owned();
    for (key, value) in [
        ("ULTRON_FILE_HOOKS", "1"),
        ("ULTRON_RLM_SPILL_DIR", ""),
        ("TMPDIR", &tmp_text),
        ("HOME", &tmp_text),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ] {
        options.env.push((key.into(), value.into()));
    }
    let kernel = Kernel::new(options, host).unwrap();
    let results = session["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|expected| {
            let code = expected["code"].as_str().unwrap();
            match kernel.execute(code, None, None) {
                Ok(result) => {
                    let recorded = json!({
                        "status": result.status,
                        "stdout": result.stdout,
                        "result": result.result,
                        "error": result.error.map(|e| format!("{}: {}", e.ename, e.evalue)),
                    });
                    let text = recorded.to_string().replace(&tmp_text, "<TMP>");
                    json!({ "code": code, "ok": serde_json::from_str::<Value>(&text).unwrap() })
                }
                Err(error) => json!({ "code": code, "error": error.message().replace(&tmp_text, "<TMP>") }),
            }
        })
        .collect();
    kernel.shutdown();
    results
}

#[test]
fn host_loki_cells_match_the_typescript_host() {
    let fixture = fixture();
    let sessions = fixture["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 6);
    for session in sessions {
        let expected = session["results"].as_array().unwrap();
        let actual = replay(&fixture["base"], session);
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual, expected, "session {}", session["name"]);
        }
        assert_eq!(actual.len(), expected.len());
    }
}

#[test]
fn host_may_write_files_matches_typescript() {
    for case in fixture()["mayWrite"].as_array().unwrap() {
        let command = case["command"].as_str().unwrap();
        assert_eq!(
            may_write_files(command),
            case["result"].as_bool().unwrap(),
            "{command:?}"
        );
    }
}

#[test]
fn host_loki_mode_matches_typescript() {
    for case in fixture()["modes"].as_array().unwrap() {
        let settings = case["settings"].as_object().unwrap();
        let expected = mode(case["mode"].as_str().unwrap());
        assert_eq!(loki_mode(case["env"].as_str(), settings), expected, "{case}");
    }
}

/// The python rule pack is ported, so a clean Python write is checked and passes without a note; a guard that errors
/// still fails open with file-hooks.ts's "did not check" note, and the write happens.
#[test]
fn host_loki_unported_check_writes_with_a_note() {
    struct Broken;
    impl host::file_hooks::FileWriteGuard for Broken {
        fn name(&self) -> &str {
            "Loki"
        }
        fn before_write(&self, _: &Path, _: &str, _: &Path) -> Result<host::file_hooks::Verdict, String> {
            Err("the elixir rule pack is not ported to the Rust engine".into())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut checked = FileHooks::new(&cwd);
    checked.add(Box::new(LokiGuard::new(&cwd, LokiMode::On)));
    let python = cwd.join("a.py").to_string_lossy().into_owned();
    assert_eq!(
        checked.before_write(&python, "x = 1\n"),
        json!({"blocked": false, "notes": []})
    );

    let mut broken = FileHooks::new(&cwd);
    broken.add(Box::new(Broken));
    let elixir = cwd.join("a.ex").to_string_lossy().into_owned();
    assert_eq!(
        broken.before_write(&elixir, "x = 1\n"),
        json!({"blocked": false, "notes": [
            "[Loki] Loki did not check this write to a.ex (the elixir rule pack is not ported to the Rust engine); it was written unchecked."
        ]})
    );
}
