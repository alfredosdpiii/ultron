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

/// The stand-in registry of `scan --online` (as the generator's): 404 for a path naming `missing`, 503 for
/// `broken`, a redirect for `redirect`, else 200. Started once, on loopback.
fn registry() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || answer(stream));
            }
        });
        url
    })
}

fn answer(mut stream: std::net::TcpStream) {
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    let _ = reader.read_line(&mut request);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
    let reply = if path.contains("redirect") && !path.ends_with("/landed") {
        format!("HTTP/1.0 302 Found\r\nLocation: {path}/landed\r\nConnection: close\r\n\r\n")
    } else {
        let status = if path.to_lowercase().contains("missing") {
            "404 Not Found"
        } else if path.contains("broken") {
            "503 Service Unavailable"
        } else {
            "200 OK"
        };
        format!("HTTP/1.0 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")
    };
    let _ = stream.write_all(reply.as_bytes());
}

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
            if entry.file_name() == ".git" {
                continue;
            }
            if meta.file_type().is_symlink() {
                let relative = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let target = std::fs::read_link(&path).unwrap();
                found.insert(relative, format!("link {}", target.display()));
            } else if meta.is_dir() {
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
    /// Later runs of the same case (`runs`), as `{status, stdout, stderr}`.
    runs: Option<Value>,
    /// The `--record` events under HOME, oldest first, wall-clock fields zeroed.
    events: Option<Value>,
}

/// Every `--record` event under `home`, with `time` and `duration_ms` zeroed and paths scrubbed.
fn events(home: &Path, scrub: &dyn Fn(String) -> String) -> Value {
    let state = home.join(".local/state/loki");
    let mut directories: Vec<PathBuf> = match std::fs::read_dir(&state) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => return Value::Array(vec![]),
    };
    directories.sort();
    let mut found = Vec::new();
    for directory in directories {
        let text = std::fs::read_to_string(directory.join("events.jsonl")).unwrap();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let mut event: Value = serde_json::from_str(&scrub(line.to_string())).unwrap();
            for key in ["time", "duration_ms"] {
                if event.get(key).is_some() {
                    event[key] = Value::from(0);
                }
            }
            found.push(event);
        }
    }
    Value::Array(found)
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
    let uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&top).unwrap().uid().to_string()
    };
    let fill = |text: &str| {
        text.replace("<ROOT>", &root_text)
            .replace("<PYTHON>", &python())
            .replace("<UID>", &uid)
            .replace("<REGISTRY>", registry())
    };
    let stdin = match (&case["rawStdin"], &case["stdin"]) {
        (Value::String(raw), _) => raw.clone(),
        (_, Value::Null) => String::new(),
        (_, value) => fill(&expand(&serde_json::to_string(value).unwrap(), tokens)),
    };
    let (work_text, top_text) = (work.to_str().unwrap().to_string(), top.to_str().unwrap().to_string());
    // Elixir reports time their checks: zeroed on both sides.
    let durations = regex::Regex::new(r#""duration_ms": [0-9.]+"#).unwrap();
    let daemons = regex::Regex::new(r"\.cache/loki/d/[0-9a-f]{16}").unwrap();
    let scrub = |text: String| {
        let text = text.replace(&work_text, "<ROOT>").replace(&top_text, "<TOP>");
        let text = durations.replace_all(&text, r#""duration_ms": 0"#).into_owned();
        daemons.replace_all(&text, ".cache/loki/d/<ID>").into_owned()
    };
    let once = |extra: &Value| -> (i64, String, String) {
        let mut args: Vec<String> = Vec::new();
        if case["noRoot"] != true {
            args.extend(["--root".to_string(), root_text.clone()]);
        }
        args.extend(extra.as_array().unwrap().iter().map(|a| fill(a.as_str().unwrap())));
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
        (
            output.status.code().unwrap_or(-1) as i64,
            scrub(String::from_utf8(output.stdout).unwrap()),
            scrub(String::from_utf8(output.stderr).unwrap()),
        )
    };
    let (status, stdout, stderr) = once(&case["args"]);
    let runs = case["runs"].as_array().map(|runs| {
        Value::from(
            runs.iter()
                .map(|args| {
                    let (status, stdout, stderr) = once(args);
                    serde_json::json!({ "status": status, "stdout": stdout, "stderr": stderr })
                })
                .collect::<Vec<_>>(),
        )
    });
    Outcome {
        status,
        stdout,
        stderr,
        tree: (case["tree"] == true).then(|| tree(&work)),
        runs,
        events: (case["events"] == true).then(|| events(&top, &scrub)),
    }
}

/// Cases checked by their own tests below.
const OWN_TESTS: [&str; 6] = ["record-", "verify-", "apply-", "elixir-", "online-", "daemon-"];

#[test]
fn loki_hook_matches_loki_py_corpus() {
    check_cases(|name| !OWN_TESTS.iter().any(|p| name.starts_with(p)), 180);
}

/// `apply`: a manifest of writes, deletions and moves checked as one candidate, then written (or refused).
#[test]
fn loki_apply_transactions_match_loki_py() {
    check_cases(|name| name.starts_with("apply-"), 30);
}

/// `elixir`: the Mix check tiers, `--credo-new`, and the hook's format / Credo / Sobelow comparison of a
/// changed Elixir file against its committed base (a stand-in Mix and Elixir).
#[test]
fn loki_elixir_analysis_matches_loki_py() {
    check_cases(|name| name.starts_with("elixir-"), 25);
}

/// `scan --online`: every public dependency looked up in its registry (npm, PyPI, crates.io, the Go proxy, Hex);
/// a 404 is a missing package, any other failure an unavailable check (a stand-in registry on loopback).
#[test]
fn loki_scan_online_matches_loki_py() {
    check_cases(|name| name.starts_with("online-"), 6);
}

/// `daemon status|stop` without a daemon, and the daemon command's argument errors and help.
#[test]
fn loki_daemon_management_matches_loki_py() {
    check_cases(|name| name.starts_with("daemon-"), 5);
}

/// `verify-installation`: regular, non-symlinked files whose ancestors the agent neither owns nor can write.
#[test]
fn loki_verify_installation_matches_loki_py() {
    check_cases(|name| name.starts_with("verify-"), 8);
}

/// `--record`: each run's event (operation, outcome, targets, the policies read with their hashes, the rules
/// applied, the engine's hash) in `~/.local/state/loki/<sha256(root)>/events.jsonl`, as loki.py appends it.
#[test]
fn loki_record_events_match_loki_py() {
    check_cases(|name| name.starts_with("record-"), 25);
}

fn check_cases(select: impl Fn(&str) -> bool, minimum: usize) {
    let corpus = corpus();
    let tokens = corpus["tokens"].clone();
    let cases = corpus["cases"].as_array().unwrap();
    let filter = std::env::var("LOKI_CASE").unwrap_or_default();
    let selected: Vec<&Value> = cases
        .iter()
        .filter(|c| select(c["name"].as_str().unwrap()))
        .filter(|c| c["name"].as_str().unwrap().contains(&filter))
        .collect();
    assert!(
        !filter.is_empty() || selected.len() >= minimum,
        "corpus has {} cases",
        selected.len()
    );
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
                    let runs_match = actual.runs.as_ref() == expected.get("runs");
                    let events_match = actual.events.as_ref() == expected.get("events");
                    if actual.status != expected["status"].as_i64().unwrap()
                        || actual.stdout != expected["stdout"].as_str().unwrap()
                        || actual.stderr != expected["stderr"].as_str().unwrap()
                        || !tree_matches
                        || !runs_match
                        || !events_match
                    {
                        failures.lock().unwrap().push(format!(
                            "{}:\n  expected [{}] stdout={:?} stderr={:?} tree={} runs={:?} events={:?}\n  actual   [{}] stdout={:?} stderr={:?} tree={:?} runs={:?} events={:?}",
                            case["name"],
                            expected["status"],
                            expected["stdout"],
                            expected["stderr"],
                            expected.get("tree").map_or("-".into(), |t| t.to_string()),
                            expected.get("runs"),
                            expected.get("events"),
                            actual.status,
                            actual.stdout,
                            actual.stderr,
                            actual.tree,
                            actual.runs,
                            actual.events
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
