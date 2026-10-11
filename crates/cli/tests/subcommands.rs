//! Differential tests: the `ultron` binary against the TypeScript CLI's subcommands, recorded by
//! crates/cli/fixtures/subcommands.mjs. Each case is replayed in the same private layout (temp HOME, agent dir,
//! server dir, cwd `work`, TZ=UTC unless the case sets it) with the same files and modification times; stdout,
//! stderr, exit code and the collected files must match.

use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/subcommands.json")).expect("fixture parses")
}

fn repo() -> String {
    config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .display()
        .to_string()
}

fn set_mtime(path: &Path, ms: f64) {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Replays one recorded case; returns the differences.
fn replay(case: &Value) -> Vec<String> {
    // The generator's `mkdtemp` prefix: session files hold the temp path, so their size depends on its length.
    let tmp = tempfile::Builder::new()
        .prefix("ultron-subcmd-fixture-")
        .rand_bytes(6)
        .tempdir_in("/tmp")
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tmp_text = root.display().to_string();
    let fill = |s: &str| s.replace("<TMP>", &tmp_text);
    for d in ["work", "agent", "server"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let put = |rel: &str, text: Option<&str>, mtime: Option<f64>| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        match text {
            None => std::fs::create_dir_all(&path).unwrap(),
            Some(text) => std::fs::write(&path, fill(text)).unwrap(),
        }
        if let Some(ms) = mtime {
            set_mtime(&path, ms);
        }
    };
    for s in case["sessions"].as_array().into_iter().flatten() {
        put(s["path"].as_str().unwrap(), s["text"].as_str(), s["mtime"].as_f64());
    }
    for (rel, text) in case["files"].as_object().into_iter().flatten() {
        put(rel, text.as_str(), case["mtimes"].get(rel).and_then(Value::as_f64));
    }
    for (rel, mode) in case["modes"].as_object().into_iter().flatten() {
        let mode = mode.as_u64().unwrap() as u32;
        std::fs::set_permissions(root.join(rel), std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
    }
    for (rel, target) in case["symlinks"].as_object().into_iter().flatten() {
        let link = root.join(rel);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target.as_str().unwrap(), link).unwrap();
    }
    let args: Vec<String> = case["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| fill(a.as_str().unwrap()))
        .collect();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ultron"));
    cmd.args(&args)
        .current_dir(root.join(case["cwd"].as_str().unwrap_or("work")))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &root)
        .env("TZ", "UTC")
        .env("ULTRON_CODING_AGENT_DIR", root.join("agent"))
        .env("ULTRON_SERVER_DIR", root.join("server"));
    for (k, v) in case["env"].as_object().into_iter().flatten() {
        cmd.env(k, fill(v.as_str().unwrap()));
    }
    let output = cmd.output().unwrap();
    let repo = repo();
    let sub = |s: &str| normalize(&s.replace(&tmp_text, "<TMP>").replace(&repo, "<REPO>"));
    let mut diffs = Vec::new();
    compare(
        &mut diffs,
        &args,
        "stdout",
        &sub(&String::from_utf8_lossy(&output.stdout)),
        case["stdout"].as_str().unwrap(),
    );
    compare(
        &mut diffs,
        &args,
        "stderr",
        &sub(&String::from_utf8_lossy(&output.stderr)),
        case["stderr"].as_str().unwrap(),
    );
    let code = output.status.code().map(i64::from);
    if code != case["status"].as_i64() {
        diffs.push(format!("{args:?} exit code {code:?}, node {:?}", case["status"]));
    }
    for item in case["collected"].as_array().into_iter().flatten() {
        let rel = item["path"].as_str().unwrap();
        let bytes = std::fs::read(root.join(rel)).ok();
        if item["missing"] == Value::Bool(true) {
            if bytes.is_some() {
                diffs.push(format!("{args:?} wrote {rel}, node did not"));
            }
            continue;
        }
        let Some(bytes) = bytes else {
            diffs.push(format!("{args:?} did not write {rel}"));
            continue;
        };
        if let Some(text) = item["text"].as_str() {
            compare(&mut diffs, &args, rel, &sub(&String::from_utf8_lossy(&bytes)), text);
            continue;
        }
        let html = String::from_utf8_lossy(&bytes);
        let marker = r#"<script id="session-data" type="application/json">"#;
        let data = html
            .find(marker)
            .map(|at| &html[at + marker.len()..])
            .and_then(|rest| rest.find('<').map(|end| rest[..end].to_string()));
        if let (Some(data), Some(expected)) = (data, item["sessionData"].as_str()) {
            let decoded = cli::js::base64_decode(&data).map(|b| String::from_utf8_lossy(&b).into_owned());
            compare(
                &mut diffs,
                &args,
                &format!("{rel} session data"),
                &decoded.unwrap_or_default(),
                expected,
            );
        }
        if sha256(&bytes) != item["sha256"].as_str().unwrap() {
            diffs.push(format!(
                "{args:?} {rel}: sha256 differs ({} bytes, node {})",
                bytes.len(),
                item["bytes"]
            ));
        }
    }
    for tree_case in case["trees"].as_array().into_iter().flatten() {
        let rel = tree_case["path"].as_str().unwrap();
        let actual = tree(&root, rel, &sub);
        if actual != *tree_case {
            diffs.push(format!(
                "{args:?} tree {rel} differs:\n  rust: {actual}\n  node: {tree_case}"
            ));
        }
    }
    diffs
}

/// Timestamps a run takes from the clock (backup directory names, manifest times).
fn normalize(text: &str) -> String {
    let re = regex::Regex::new(r"\d{4}-\d\d-\d\dT\d\d[-:]\d\d[-:]\d\d[-.]\d{3}Z").unwrap();
    re.replace_all(text, "<TIME>").into_owned()
}

/// Every entry under `rel`, as the generator's `tree` records it.
fn tree(root: &Path, rel: &str, sub: &dyn Fn(&str) -> String) -> Value {
    use std::os::unix::fs::PermissionsExt;
    let base = root.join(rel);
    if !base.exists() {
        return serde_json::json!({"path": rel, "missing": true});
    }
    fn walk(base: &Path, dir: &str, out: &mut Vec<Value>, sub: &dyn Fn(&str) -> String) {
        let mut names: Vec<String> = std::fs::read_dir(base.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        for name in names {
            let p = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            let meta = std::fs::symlink_metadata(base.join(&p)).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(base.join(&p)).unwrap();
                out.push(
                    serde_json::json!({"path": normalize(&p), "kind": "symlink", "target": target.to_string_lossy()}),
                );
            } else if meta.is_dir() {
                out.push(serde_json::json!({"path": normalize(&p), "kind": "dir", "mode": mode}));
                walk(base, &p, out, sub);
            } else {
                let text = String::from_utf8_lossy(&std::fs::read(base.join(&p)).unwrap()).into_owned();
                out.push(serde_json::json!({"path": normalize(&p), "kind": "file", "mode": mode, "text": sub(&text)}));
            }
        }
    }
    let mut entries = Vec::new();
    walk(&base, "", &mut entries, sub);
    serde_json::json!({"path": rel, "entries": entries})
}

fn compare(diffs: &mut Vec<String>, args: &[String], what: &str, actual: &str, expected: &str) {
    if actual != expected {
        let line = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
        diffs.push(format!(
            "{args:?} {what} differs at line {line}:\n  rust: {:?}\n  node: {:?}",
            actual.lines().nth(line).unwrap_or("<end>"),
            expected.lines().nth(line).unwrap_or("<end>")
        ));
    }
}

fn replay_all(select: impl Fn(&[&str]) -> bool) -> usize {
    let fx = fixture();
    let mut count = 0;
    let mut diffs = Vec::new();
    for case in fx["cases"].as_array().unwrap() {
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        if !select(&args) {
            continue;
        }
        count += 1;
        diffs.extend(replay(case));
    }
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
    count
}

#[test]
fn cli_usage_matches_typescript() {
    assert!(replay_all(|args| args[0] == "usage") > 50);
}

#[test]
fn cli_export_matches_typescript() {
    assert!(replay_all(|args| args.contains(&"--export")) >= 12);
}

#[test]
fn cli_auth_matches_typescript() {
    assert!(replay_all(|args| args[0] == "auth") >= 30);
}

#[test]
fn cli_mcp_matches_typescript() {
    assert!(replay_all(|args| args[0] == "mcp") >= 40);
}

#[test]
fn cli_migrate_matches_typescript() {
    assert!(replay_all(|args| args[0] == "migrate") >= 20);
}

#[test]
fn cli_package_commands_match_typescript() {
    assert!(replay_all(|args| ["install", "remove", "uninstall", "update", "list", "config"].contains(&args[0])) >= 40);
}

#[test]
fn cli_setup_matches_typescript() {
    assert!(replay_all(|args| args[0] == "setup") >= 4);
}
