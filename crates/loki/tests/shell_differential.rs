//! Differential tests: the Rust `loki` binary's `shell`, `shell-writes` and `explain`
//! against loki.py output recorded in `fixtures/shell-corpus.json` by `fixtures/generate-shell.mjs`.

use serde_json::Value;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn corpus() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/shell-corpus.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=loki",
            "-c",
            "user.email=loki@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", cwd)
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn place(root: &Path, files: &Value) {
    for (path, content) in files.as_object().into_iter().flatten() {
        let target = root.join(path);
        match content {
            Value::Null => std::fs::remove_file(&target).unwrap(),
            Value::String(text) => {
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(&target, text).unwrap();
            }
            link => {
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(link["symlink"].as_str().unwrap(), &target).unwrap();
            }
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `(status, stdout, stderr)` of the Rust binary over one recorded case.
fn replay(case: &Value) -> (i64, String, String) {
    let dir = tempfile::Builder::new().prefix("loki-shell-").tempdir().unwrap();
    let top = std::fs::canonicalize(dir.path()).unwrap();
    place(&top, &case["files"]);
    if case["git"] == true {
        git(&top, &["init", "-q", "-b", "main"]);
        git(&top, &["add", "-A"]);
        git(&top, &["commit", "-q", "-m", "base"]);
    }
    place(&top, &case["after"]);
    for (path, mode) in case["chmod"].as_object().into_iter().flatten() {
        let mode = mode.as_u64().unwrap() as u32;
        std::fs::set_permissions(top.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let root = match case["rootSub"].as_str() {
        Some(sub) => top.join(sub),
        None => top.clone(),
    };
    let root_text = root.to_str().unwrap();
    if let Some(events) = case["events"].as_array() {
        let dir = top.join(".local/state/loki").join(sha256_hex(root_text.as_bytes()));
        std::fs::create_dir_all(&dir).unwrap();
        let lines: String = events.iter().map(|e| format!("{}\n", e.as_str().unwrap())).collect();
        std::fs::write(dir.join("events.jsonl"), lines).unwrap();
    }
    let stdin = if case["stdin"].is_null() {
        String::new()
    } else {
        serde_json::to_string(&case["stdin"])
            .unwrap()
            .replace("<ROOT>", root_text)
    };
    let args = case["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_loki"))
        .arg("--root")
        .arg(&root)
        .args(args)
        .current_dir(&root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &top)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The binary may exit before reading stdin (argument errors).
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let output = child.wait_with_output().unwrap();
    let top = top.to_str().unwrap();
    let scrub = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap().replace(top, "<ROOT>");
    (
        output.status.code().unwrap() as i64,
        scrub(output.stdout),
        scrub(output.stderr),
    )
}

#[test]
fn loki_shell_matches_loki_py_corpus() {
    let corpus = corpus();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 120, "corpus has {} cases", cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let expected = &case["expected"];
        let (status, stdout, stderr) = replay(case);
        if status != expected["status"].as_i64().unwrap()
            || stdout != expected["stdout"].as_str().unwrap()
            || stderr != expected["stderr"].as_str().unwrap()
        {
            failures.push(format!(
                "{}:\n  expected [{}] stdout={:?} stderr={:?}\n  actual   [{status}] stdout={stdout:?} stderr={stderr:?}",
                case["name"], expected["status"], expected["stdout"], expected["stderr"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn loki_shell_record_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_loki"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "shell",
            "--harness",
            "claude",
            "--record",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "loki: guard error: NotPorted: --record is not ported to the Rust engine\n"
    );
}
