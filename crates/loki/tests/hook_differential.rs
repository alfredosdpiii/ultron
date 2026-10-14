//! Differential tests: the Rust `loki` binary's `hook`, `protect` rule packs, `scan`,
//! `slop` and `init` against loki.py output recorded in `fixtures/hook-corpus.json`
//! by `fixtures/generate-hook.mjs`. Each case is rebuilt exactly as the generator
//! built it: a temp dir with a work tree, a bin dir of git/sh/python3 (node when
//! asked) plus the case's fake tools, HOME there, and the same git history.

#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn corpus() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/hook-corpus.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/usr/bin/python3".into())
}

fn which(name: &str) -> PathBuf {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} not on PATH"))
}

fn expand(text: &str, tokens: &Value) -> String {
    let mut out = text.to_string();
    for (name, parts) in tokens.as_object().unwrap() {
        let joined: String = parts.as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
        out = out.replace(&format!("@@{name}@@"), &joined);
    }
    out
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
            "-c",
            "init.defaultBranch=main",
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

fn place(root: &Path, files: &Value, tokens: &Value) {
    for (path, content) in files.as_object().into_iter().flatten() {
        let target = root.join(path);
        if content.is_null() {
            std::fs::remove_file(&target).unwrap();
            continue;
        }
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if let Some(text) = content.as_str() {
            std::fs::write(&target, expand(text, tokens)).unwrap();
        } else if let Some(bytes) = content["bytes"].as_array() {
            std::fs::write(
                &target,
                bytes.iter().map(|b| b.as_u64().unwrap() as u8).collect::<Vec<_>>(),
            )
            .unwrap();
        } else {
            executable_support::install(&target, content["exec"].as_str().unwrap());
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                walk(root, &path, found);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let bytes = std::fs::read(&path).unwrap_or_default();
                found.insert(
                    relative,
                    format!("{:o} {}", meta.permissions().mode() & 0o777, sha256_hex(&bytes)),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(root, root, &mut found);
    found
}

struct Outcome {
    status: i64,
    stdout: String,
    stderr: String,
    tree: Option<BTreeMap<String, String>>,
}

fn replay(case: &Value, tokens: &Value) -> Outcome {
    let dir = tempfile::Builder::new().prefix("loki-hook-").tempdir().unwrap();
    let top = std::fs::canonicalize(dir.path()).unwrap();
    let bin = top.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for tool in ["git", "sh", "env"] {
        std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), bin.join(tool)).unwrap();
    }
    std::os::unix::fs::symlink(python(), bin.join("python3")).unwrap();
    if case["node"] == true {
        std::os::unix::fs::symlink(which("node"), bin.join("node")).unwrap();
    }
    for (name, script) in case["bin"].as_object().into_iter().flatten() {
        // Executed by path by the hooks: written without a write descriptor in this process (ETXTBSY).
        executable_support::install(&bin.join(name), script.as_str().unwrap());
    }
    let work = top.join("w");
    std::fs::create_dir(&work).unwrap();
    place(&work, &case["files"], tokens);
    if case["typescript"] == true {
        let typescript =
            std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/../../node_modules/typescript")).unwrap();
        std::fs::create_dir_all(work.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(typescript, work.join("node_modules/typescript")).unwrap();
    }
    if case["git"] == true || case["gitInit"] == true {
        git(&work, &["init", "-q"]);
        if case["git"] == true {
            git(&work, &["add", "-A"]);
            git(&work, &["commit", "-q", "--allow-empty", "-m", "base"]);
        }
    }
    place(&work, &case["after"], tokens);
    for (path, target) in case["links"].as_object().into_iter().flatten() {
        std::os::unix::fs::symlink(target.as_str().unwrap(), work.join(path)).unwrap();
    }
    for path in case["fifo"].as_array().into_iter().flatten() {
        let status = Command::new("mkfifo")
            .arg(work.join(path.as_str().unwrap()))
            .status()
            .unwrap();
        assert!(status.success());
    }
    for (path, mode) in case["chmod"].as_object().into_iter().flatten() {
        let mode = mode.as_u64().unwrap() as u32;
        std::fs::set_permissions(work.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    if case["conflict"] == true {
        git(&work, &["checkout", "-q", "-b", "other"]);
        std::fs::write(work.join("a.txt"), "other\n").unwrap();
        git(&work, &["commit", "-q", "-am", "other"]);
        git(&work, &["checkout", "-q", "main"]);
        std::fs::write(work.join("a.txt"), "main\n").unwrap();
        git(&work, &["commit", "-q", "-am", "main"]);
        let _ = Command::new("git")
            .args(["merge", "-q", "other"])
            .current_dir(&work)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &work)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let root = match case["rootSub"].as_str() {
        Some(sub) => work.join(sub),
        None => work.clone(),
    };
    let root_text = root.to_str().unwrap().to_string();
    let fill = |text: &str| text.replace("<ROOT>", &root_text).replace("<PYTHON>", &python());
    let stdin = match (&case["rawStdin"], &case["stdin"]) {
        (Value::String(raw), _) => raw.clone(),
        (_, Value::Null) => String::new(),
        (_, value) => fill(&expand(&serde_json::to_string(value).unwrap(), tokens)),
    };
    let mut args: Vec<String> = Vec::new();
    if case["noRoot"] != true {
        args.extend(["--root".to_string(), root_text.clone()]);
    }
    args.extend(
        case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| fill(a.as_str().unwrap())),
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_loki"));
    command
        .args(&args)
        .current_dir(&root)
        .env_clear()
        .env("PATH", &bin)
        .env("HOME", &top)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in case["env"].as_object().into_iter().flatten() {
        command.env(key, fill(value.as_str().unwrap()));
    }
    let mut child = command.spawn().unwrap();
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let output = child.wait_with_output().unwrap();
    let (work_text, top_text) = (work.to_str().unwrap().to_string(), top.to_str().unwrap().to_string());
    let scrub = |bytes: Vec<u8>| {
        String::from_utf8(bytes)
            .unwrap()
            .replace(&work_text, "<ROOT>")
            .replace(&top_text, "<TOP>")
    };
    Outcome {
        status: output.status.code().unwrap_or(-1) as i64,
        stdout: scrub(output.stdout),
        stderr: scrub(output.stderr),
        tree: (case["tree"] == true).then(|| tree(&work)),
    }
}

#[test]
fn loki_hook_matches_loki_py_corpus() {
    let corpus = corpus();
    let tokens = corpus["tokens"].clone();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 180, "corpus has {} cases", cases.len());
    let filter = std::env::var("LOKI_CASE").unwrap_or_default();
    let selected: Vec<&Value> = cases
        .iter()
        .filter(|c| c["name"].as_str().unwrap().contains(&filter))
        .collect();
    let failures = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for chunk in selected.chunks(selected.len().div_ceil(8).max(1)) {
            let (tokens, failures) = (&tokens, &failures);
            scope.spawn(move || {
                for case in chunk {
                    let expected = &case["expected"];
                    let actual = replay(case, tokens);
                    let tree_matches = match (&actual.tree, expected.get("tree")) {
                        (Some(tree), Some(want)) => serde_json::to_value(tree).unwrap() == *want,
                        (None, None) => true,
                        _ => false,
                    };
                    if actual.status != expected["status"].as_i64().unwrap()
                        || actual.stdout != expected["stdout"].as_str().unwrap()
                        || actual.stderr != expected["stderr"].as_str().unwrap()
                        || !tree_matches
                    {
                        failures.lock().unwrap().push(format!(
                            "{}:\n  expected [{}] stdout={:?} stderr={:?} tree={}\n  actual   [{}] stdout={:?} stderr={:?} tree={:?}",
                            case["name"],
                            expected["status"],
                            expected["stdout"],
                            expected["stderr"],
                            expected.get("tree").map_or("-".into(), |t| t.to_string()),
                            actual.status,
                            actual.stdout,
                            actual.stderr,
                            actual.tree
                        ));
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        selected.len(),
        failures.join("\n")
    );
}
