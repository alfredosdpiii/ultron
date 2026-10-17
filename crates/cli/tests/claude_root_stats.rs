//! `ultron mcp` (Claude Code as the root agent) keeps the session's runtime counters as the TypeScript session worker
//! does for an external root (`new SessionStatsRecorder({ store: createSessionModuleStore(session, "stats"), root:
//! "external" })`, ExternalRootController's `beginRootTurn`/`endRootTurn`): the session value `ultron.module/stats`
//! says `root: "external"`, counts the cells Claude Code ran on the root lane and the Claude Code turns (one opened by
//! the UserPromptSubmit hook, one by the first cell of a turn, each closed by the next hook or by shutdown). A FAKE
//! `claude` is first on PATH and is never needed; the kernel is the test's Python.

mod executable_support;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_ultron");

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// Every `.jsonl` file under `dir`.
fn journals(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            journals(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// The last stats document anywhere in `value` (an object with `externalTurns` and `root`).
fn find_stats(value: &Value, found: &mut Option<Value>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("externalTurns") && map.contains_key("root") {
                *found = Some(value.clone());
            }
            map.values().for_each(|v| find_stats(v, found));
        }
        Value::Array(items) => items.iter().for_each(|v| find_stats(v, found)),
        _ => {}
    }
}

#[test]
fn cli_claude_root_mcp_counts_cells_and_turns_as_an_external_root() {
    let root = tempfile::Builder::new().prefix("ucs-").tempdir_in("/tmp").unwrap();
    let p = |s: &str| root.path().join(s);
    for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
        std::fs::create_dir(p(sub)).unwrap();
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fake = std::fs::read_to_string(repo.join("crates/claude-root/fixtures/fake_claude.py")).unwrap();
    executable_support::install(&p("bin/claude"), &format!("#!{} -I\n{fake}", python()));
    let env: Vec<(&str, String)> = vec![
        ("PATH", format!("{}:/usr/bin:/bin", p("bin").display())),
        ("HOME", p("h").display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", p("a").display().to_string()),
        ("ULTRON_SERVER_DIR", p("s").display().to_string()),
        ("XDG_RUNTIME_DIR", p("r").display().to_string()),
        ("TMPDIR", p("tmp").display().to_string()),
        ("ULTRON_SELF_COMMAND", r#"["/opt/ultron/bin/ultron"]"#.into()),
        ("ULTRON_LOKI", "off".into()),
        ("ULTRON_PYTHON", python()),
        ("ULTRON_OFFLINE", "1".into()),
        ("CLAUDE_CODE_SESSION_ID", "sess-stats".into()),
        ("LANG", "C.UTF-8".into()),
    ];
    let socket = p("r/s.sock").display().to_string();
    let mut child = Command::new(BIN)
        .args(["mcp", "--no-instructions", "--socket", &socket])
        .current_dir(p("w"))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, responses) = mpsc::channel::<Value>();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(message);
            }
        }
    });
    let stderr = child.stderr.take().unwrap();
    let stderr_reader = std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default());
    let mut stdin = child.stdin.take().unwrap();
    let mut answers: HashMap<i64, Value> = HashMap::new();
    let mut request = |id: i64, method: &str, params: Value| -> Value {
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        stdin.flush().unwrap();
        // Hang guard only: each answer arrives as an event on the channel.
        while !answers.contains_key(&id) {
            let message = responses
                .recv_timeout(Duration::from_secs(120))
                .expect("an answer from ultron mcp");
            if let Some(got) = message["id"].as_i64() {
                answers.insert(got, message);
            }
        }
        answers[&id].clone()
    };
    request(1, "initialize", json!({"protocolVersion": "2025-03-26"}));
    let cell = |n: i64, code: &str| json!({"name": "rlm", "arguments": {"code": code}, "_meta": {"n": n}});
    // A turn opened by the first cell.
    let first = request(2, "tools/call", cell(2, "x = 41"));
    assert!(
        first["result"]["content"].is_array() && first["result"]["isError"] != json!(true),
        "{first}"
    );
    request(3, "tools/call", cell(3, "x + 1"));
    // The UserPromptSubmit hook ends that turn and opens the next; the Stop hook closes it.
    let hook = |name: &str| {
        let output = Command::new(BIN)
            .args(["hook", name, "--socket", &socket])
            .current_dir(p("w"))
            .env_clear()
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut hook| {
                hook.stdin.take().unwrap().write_all(b"{}")?;
                hook.wait_with_output()
            })
            .unwrap();
        assert!(output.status.success(), "hook {name}: {output:?}");
    };
    hook("user-prompt");
    request(4, "tools/call", cell(4, "raise ValueError('boom')"));
    hook("stop");
    // A third turn, still open at shutdown, ends with the session.
    request(5, "tools/call", cell(5, "y = 1"));
    drop(request);
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();
    assert!(status.success(), "ultron mcp exited {status:?}: {stderr}");
    let mut files = Vec::new();
    journals(&p("a"), &mut files);
    let mut stats = None;
    for file in &files {
        for line in std::fs::read_to_string(file).unwrap().lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                find_stats(&value, &mut stats);
            }
        }
    }
    let stats = stats.unwrap_or_else(|| panic!("no stats document in {files:?}"));
    assert_eq!(stats["root"], json!("external"), "{stats}");
    assert_eq!(stats["cells"]["root"]["count"], json!(4), "{stats}");
    assert_eq!(stats["cells"]["root"]["failed"], json!(1), "{stats}");
    assert_eq!(stats["externalTurns"]["count"], json!(3), "{stats}");
    assert!(stats["externalTurns"]["wallMs"].as_f64().unwrap() >= 0.0);
}
