//! `ultron claude|mcp|hook|watch|guide` (crates/claude-root) replayed against the TypeScript recording
//! crates/claude-root/fixtures/claude.json (generate.mjs): the same commands, environment, files and stdin in a
//! private temp root with a FAKE `claude` first on PATH; stdout, stderr, exit codes, what the fake claude was
//! started with, the JSON-RPC frames of `ultron mcp` sessions, control-socket answers, hook output and
//! `ultron watch --list` must be equal after the generator's normalization.

mod executable_support;

use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ultron");

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn fixture() -> Value {
    let path = repo().join("crates/claude-root/fixtures/claude.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// A private temp root laid out like the generator's.
struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new()
            .prefix("ucg-")
            .rand_bytes(6)
            .tempdir_in("/tmp")
            .unwrap();
        for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let fake = std::fs::read_to_string(repo().join("crates/claude-root/fixtures/fake_claude.py")).unwrap();
        let claude = dir.path().join("bin/claude");
        executable_support::install(&claude, &format!("#!{} -I\n{fake}", python()));
        Root { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn sub(&self, text: &str) -> String {
        text.replace("<T>", &self.path().display().to_string())
    }

    fn place(&self, files: Option<&Value>) {
        for (path, content) in files.and_then(Value::as_object).into_iter().flatten() {
            let target = self.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, content.as_str().unwrap()).unwrap();
        }
    }

    fn env(&self, extra: Option<&Value>) -> Vec<(String, String)> {
        let p = |s: &str| self.path().join(s).display().to_string();
        let mut env: Vec<(String, String)> = vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", p("bin"))),
            ("HOME".into(), p("h")),
            ("ULTRON_CODING_AGENT_DIR".into(), p("a")),
            ("ULTRON_SERVER_DIR".into(), p("s")),
            ("XDG_RUNTIME_DIR".into(), p("r")),
            ("TMPDIR".into(), p("tmp")),
            ("ULTRON_SELF_COMMAND".into(), r#"["/opt/ultron/bin/ultron"]"#.into()),
            ("ULTRON_LOKI".into(), "off".into()),
            ("ULTRON_PYTHON".into(), python()),
            ("FAKE_CLAUDE_OUT".into(), p("launch.json")),
            ("LANG".into(), "C.UTF-8".into()),
        ];
        for (key, value) in extra.and_then(Value::as_object).into_iter().flatten() {
            let value = self.sub(value.as_str().unwrap());
            match env.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1 = value,
                None => env.push((key.clone(), value)),
            }
        }
        env
    }

    fn command(&self, args: &[String], env: &[(String, String)]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(self.path().join("w"))
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        command
    }

    fn normalize(&self, text: &str) -> String {
        static LAUNCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"L-[0-9a-z]+-[0-9a-f]{8}").unwrap());
        static RND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"ultron-claude-[A-Za-z0-9]{6}").unwrap());
        static DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"- Date: \d{4}-\d{2}-\d{2}").unwrap());
        let text = text
            .replace(&self.path().display().to_string(), "<T>")
            .replace(&repo().display().to_string(), "<REPO>");
        let text = LAUNCH.replace_all(&text, "L-<ID>");
        let text = RND.replace_all(&text, "ultron-claude-<RND>");
        DATE.replace_all(&text, "- Date: <DATE>").into_owned()
    }

    fn deep(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.normalize(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.deep(v)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), self.deep(v))).collect()),
            other => other.clone(),
        }
    }
}

/// `stablePrintConfig` of generate.mjs: the `--print-config` prompt length taken again from the normalized prompt
/// (UTF-16 units), so the comparison does not depend on the checkout's or temp root's path length.
fn stable_print_config(stdout: String) -> String {
    let Ok(mut printed) = serde_json::from_str::<Value>(&stdout) else {
        return stdout;
    };
    let prompt = printed["args"].as_array().and_then(|args| {
        let index = args.iter().position(|a| a == "--system-prompt")?;
        args.get(index + 1)?.as_str().map(|p| p.encode_utf16().count())
    });
    match (prompt, printed["systemPrompt"]["chars"].is_number()) {
        (Some(chars), true) => {
            printed["systemPrompt"]["chars"] = json!(chars);
            format!("{}\n", serde_json::to_string_pretty(&printed).unwrap())
        }
        _ => stdout,
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// Exact text (key order included) of a JSON value.
fn text(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

fn run_process(case: &Value) -> Vec<String> {
    let root = Root::new();
    root.place(case.get("files"));
    let mut env = root.env(case.get("env"));
    // `ultron guide` prints TypeScript's guide under ULTRON_GUIDE=ts (the Rust guide words engineering mode's
    // bundled-skills note as `ultron claude` launches with it).
    if case["name"].as_str().is_some_and(|name| name.starts_with("guide-")) {
        env.push(("ULTRON_GUIDE".into(), "ts".into()));
    }
    let args: Vec<String> = strings(&case["args"]).iter().map(|a| root.sub(a)).collect();
    let mut child = root
        .command(&args, &env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = case.get("stdin").and_then(Value::as_str).unwrap_or("").to_string();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    let name = case["name"].as_str().unwrap();
    let mut diffs = Vec::new();
    let mut check = |what: &str, got: String, want: &Value| {
        if want.as_str() != Some(got.as_str()) {
            diffs.push(format!(
                "{name} {what}:\n--- want\n{}\n--- got\n{got}",
                want.as_str().unwrap_or("?")
            ));
        }
    };
    check(
        "stdout",
        stable_print_config(root.normalize(&String::from_utf8_lossy(&output.stdout))),
        &case["stdout"],
    );
    check(
        "stderr",
        root.normalize(&String::from_utf8_lossy(&output.stderr)),
        &case["stderr"],
    );
    if output.status.code() != case["code"].as_i64().map(|c| c as i32) {
        diffs.push(format!(
            "{name} code: want {} got {:?}",
            case["code"],
            output.status.code()
        ));
    }
    if let Some(want) = case.get("launch") {
        let got: Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("launch.json")).unwrap()).unwrap();
        let got = root.deep(&got);
        if text(&got) != text(want) {
            diffs.push(format!("{name} launch:\n--- want\n{want:#}\n--- got\n{got:#}"));
        }
        // The session's temp directory is gone once Claude Code exited; its cleanup may still be running, so wait
        // for the recorded state (hang guard 120 s).
        let deadline = Instant::now() + Duration::from_secs(120);
        let left = loop {
            let left: Vec<String> = std::fs::read_dir(root.path().join("tmp"))
                .unwrap()
                .map(|e| format!("{}\n", e.unwrap().file_name().to_string_lossy()))
                .collect();
            if Some(left.concat().as_str()) == case["tempDirLeft"].as_str() || Instant::now() >= deadline {
                break left.concat();
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if Some(left.as_str()) != case["tempDirLeft"].as_str() {
            diffs.push(format!("{name} temp dir left: {left:?}"));
        }
    }
    diffs
}

fn replay_processes(filter: impl Fn(&str) -> bool) {
    let fixture = fixture();
    let cases: Vec<&Value> = fixture["processes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| filter(c["name"].as_str().unwrap()))
        .collect();
    assert!(!cases.is_empty());
    let diffs: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = cases.iter().map(|case| scope.spawn(|| run_process(case))).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n\n"));
}

#[test]
fn claude_root_guide_matches_typescript() {
    replay_processes(|name| name.starts_with("guide-"));
}

#[test]
fn claude_root_hook_commands_match_typescript() {
    replay_processes(|name| name.starts_with("hook-"));
}

#[test]
fn claude_root_watch_arguments_and_list_match_typescript() {
    replay_processes(|name| name.starts_with("watch-"));
}

#[test]
fn claude_root_launcher_print_config_and_errors_match_typescript() {
    replay_processes(|name| name.starts_with("claude-") && !name.starts_with("claude-launch"));
}

#[test]
fn claude_root_launcher_starts_fake_claude_like_typescript() {
    replay_processes(|name| name.starts_with("claude-launch"));
}

#[test]
fn claude_root_mcp_argument_errors_match_typescript() {
    replay_processes(|name| name.starts_with("mcp-"));
}

/// The `stable` of the generator: times and pids 0, the session id and turn prefix replaced.
fn stable(value: &Value) -> Value {
    static TURN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^cc-[0-9a-z]+-[0-9a-f]{8}-").unwrap());
    match value {
        Value::Array(items) => Value::Array(items.iter().map(stable).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = match (k.as_str(), v) {
                        ("startedAt" | "endedAt" | "pid", Value::Number(_)) => json!(0),
                        ("sessionId", Value::String(_)) => json!("<SID>"),
                        ("turn", Value::String(t)) => json!(TURN.replace(t, "cc-<ID>-")),
                        _ => stable(v),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn control_requests(socket: &Path, requests: &[Value]) -> Vec<Value> {
    let stream = UnixStream::connect(socket).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let mut request = request.as_object().unwrap().clone();
            request.insert("id".into(), json!(index + 1));
            writeln!(writer, "{}", Value::Object(request)).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap()
        })
        .collect()
}

fn run_mcp(case: &Value) -> Vec<String> {
    let name = case["name"].as_str().unwrap();
    let root = Root::new();
    let mut diffs = Vec::new();
    let _parent = (case.get("parent") == Some(&Value::Bool(true))).then(|| {
        let listener = UnixListener::bind(root.path().join("r/parent.sock")).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut sink = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut &stream, &mut sink);
                });
            }
        });
    });
    let env = root.env(case.get("env"));
    let mut args = vec!["mcp".to_string()];
    args.extend(strings(&case["args"]).iter().map(|a| root.sub(a)));
    let mut child = root
        .command(&args, &env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let responses: Arc<Mutex<HashMap<String, Value>>> = Arc::default();
    let (wake_tx, wake) = mpsc::channel::<()>();
    let stdout = child.stdout.take().unwrap();
    let seen = responses.clone();
    let reader = std::thread::spawn(move || {
        let mut nulls = 0;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            let message: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
            let key = if message["id"].is_null() {
                nulls += 1;
                format!("null#{}", nulls - 1)
            } else {
                message["id"].to_string()
            };
            seen.lock().unwrap().insert(key, message);
            let _ = wake_tx.send(());
        }
    });
    let stderr = child.stderr.take().unwrap();
    let stderr_reader = std::thread::spawn(move || std::io::read_to_string(stderr).unwrap_or_default());
    let mut stdin = child.stdin.take().unwrap();
    let mut received = 0;
    for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
        for line in strings(&step["send"]) {
            writeln!(stdin, "{}", root.sub(&line)).unwrap();
        }
        stdin.flush().unwrap();
        let want = step["receive"].as_array().unwrap();
        let keys: Vec<String> = want
            .iter()
            .map(|m| {
                if m["id"].is_null() {
                    "null#0".to_string()
                } else {
                    m["id"].to_string()
                }
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(120);
        while !keys.iter().all(|k| responses.lock().unwrap().contains_key(k)) && Instant::now() < deadline {
            let _ = wake.recv_timeout(Duration::from_millis(200));
        }
        for (key, want) in keys.iter().zip(want) {
            received += 1;
            let got = responses
                .lock()
                .unwrap()
                .get(key)
                .map(|m| root.deep(m))
                .unwrap_or(Value::Null);
            if text(&got) != text(want) {
                diffs.push(format!(
                    "{name} step {index} id {key}:\n--- want\n{want}\n--- got\n{got}"
                ));
            }
        }
    }
    let socket = strings(&case["args"])
        .iter()
        .position(|a| a == "--socket")
        .map(|i| PathBuf::from(root.sub(&strings(&case["args"])[i + 1])));
    if let Some(want) = case.get("control") {
        let sent = case["controlRequests"].as_array().unwrap().clone();
        let got = root.deep(&stable(&Value::Array(control_requests(
            socket.as_ref().unwrap(),
            &sent,
        ))));
        if text(&got) != text(want) {
            diffs.push(format!("{name} control:\n--- want\n{want:#}\n--- got\n{got:#}"));
        }
    }
    if let Some(want) = case.get("hooks") {
        for want in want.as_array().unwrap() {
            let args: Vec<String> = strings(&want["args"]).iter().map(|a| root.sub(a)).collect();
            let mut hook = root
                .command(&args, &env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            hook.stdin
                .take()
                .unwrap()
                .write_all(root.sub(want["stdin"].as_str().unwrap()).as_bytes())
                .unwrap();
            let output = hook.wait_with_output().unwrap();
            let got = json!({
                "args": want["args"],
                "stdin": want["stdin"],
                "stdout": root.normalize(&String::from_utf8_lossy(&output.stdout)),
                "stderr": root.normalize(&String::from_utf8_lossy(&output.stderr)),
                "code": output.status.code(),
            });
            if text(&got) != text(want) {
                diffs.push(format!("{name} hook:\n--- want\n{want:#}\n--- got\n{got:#}"));
            }
        }
    }
    if let Some(want) = case.get("status") {
        let got = root.deep(&stable(&Value::Array(control_requests(
            socket.as_ref().unwrap(),
            &[json!({"op": "status"})],
        ))));
        if text(&got) != text(want) {
            diffs.push(format!("{name} status:\n--- want\n{want:#}\n--- got\n{got:#}"));
        }
    }
    if let Some(want) = case.get("watchList") {
        static PID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\tpid \d+\t").unwrap());
        static CHILD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"child-\d+-[0-9a-f]{6}").unwrap());
        let output = root.command(&["watch".into(), "--list".into()], &env).output().unwrap();
        let got = root.normalize(&String::from_utf8_lossy(&output.stdout));
        let got = PID.replace_all(&got, "\tpid <PID>\t");
        let got = CHILD.replace_all(&got, "child-<PID>-<RND>").into_owned();
        if want.as_str() != Some(got.as_str()) {
            diffs.push(format!("{name} watch --list: want {want} got {got:?}"));
        }
    }
    drop(stdin);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();
    let extra = responses.lock().unwrap().len() as i64 - received;
    if Some(extra) != case["extra"].as_i64() {
        diffs.push(format!("{name}: {extra} unexpected responses"));
    }
    if status.code() != case["code"].as_i64().map(|c| c as i32) {
        diffs.push(format!("{name} exit: {status}"));
    }
    let left: String = std::fs::read_dir(root.path().join("r/ultron-claude"))
        .map(|entries| {
            entries
                .map(|e| format!("{}\n", e.unwrap().file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
    if Some(left.as_str()) != case["registryLeft"].as_str() {
        diffs.push(format!("{name} registry left: {left:?}"));
    }
    if Some(stderr.trim().is_empty()) != case["stderrEmpty"].as_bool() {
        diffs.push(format!("{name} stderr: {stderr}"));
    }
    diffs
}

fn replay_mcp(names: &[&str]) {
    let fixture = fixture();
    let cases: Vec<&Value> = fixture["mcp"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| names.contains(&c["name"].as_str().unwrap()))
        .collect();
    assert_eq!(cases.len(), names.len());
    let diffs: Vec<String> = cases.iter().flat_map(|c| run_mcp(c)).collect();
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n\n"));
}

#[test]
fn claude_root_mcp_protocol_frames_match_typescript() {
    replay_mcp(&["protocol"]);
}

#[test]
fn claude_root_mcp_cells_control_hooks_match_typescript() {
    replay_mcp(&["cells"]);
}

#[test]
fn claude_root_mcp_loki_session_context_matches_typescript() {
    replay_mcp(&["loki"]);
}

#[test]
fn claude_root_mcp_child_server_matches_typescript() {
    replay_mcp(&["child", "child-unlimited"]);
}

#[test]
fn claude_root_skip_permissions_setting_leaves_the_flag_out() {
    // Rust-only setting (DECISIONS): `claudeCode.skipPermissions: false` drops --dangerously-skip-permissions;
    // the default keeps TypeScript's behavior.
    let root = Root::new();
    let env = root.env(None);
    let run = |root: &Root| {
        let output = root
            .command(
                &["claude".into(), "--print-config".into(), "-nc".into(), "-ns".into()],
                &env,
            )
            .output()
            .unwrap();
        let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
        strings(&printed["args"]).contains(&"--dangerously-skip-permissions".to_string())
    };
    assert!(run(&root));
    root.place(Some(
        &json!({"a/settings.json": "{\"claudeCode\": {\"skipPermissions\": false}}\n"}),
    ));
    assert!(!run(&root));
}

/// Engineering mode's bundled-skills note: `ultron guide --for claude` prints the proactive wording `ultron claude`
/// launches Claude Code with (the Rust guide); `ULTRON_GUIDE=ts` keeps TypeScript's. New in Rust (wave 41).
#[test]
fn claude_root_w18s_guide_for_claude_words_skills_as_the_launch_does() {
    const PROACTIVE: &str = "Use them on your own: when a task fits one, read it and follow it without being asked";
    const TS: &str = "Read one only when the user asks for it or the task plainly calls for that workflow";
    let fixture = fixture();
    let case = fixture["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "guide-engineering")
        .unwrap();
    let run = |args: &[&str], extra: &[(&str, &str)]| {
        let root = Root::new();
        root.place(case.get("files"));
        let mut env = root.env(None);
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let args: Vec<String> = args.iter().map(|a| root.sub(a)).collect();
        let output = root.command(&args, &env).stdin(Stdio::null()).output().unwrap();
        let launched = std::fs::read_to_string(root.path().join("launch.json"))
            .ok()
            .map(|text| serde_json::from_str::<Value>(&text).unwrap());
        (String::from_utf8_lossy(&output.stdout).into_owned(), launched)
    };
    let guide = ["guide", "--for", "claude", "--cwd", "<T>/w", "-nc"];
    let (rust, _) = run(&guide, &[]);
    assert!(rust.contains(PROACTIVE) && !rust.contains(TS), "{rust}");
    let (ts, _) = run(&guide, &[("ULTRON_GUIDE", "ts")]);
    assert!(ts.contains(TS) && !ts.contains(PROACTIVE), "{ts}");
    // The launch's system prompt carries the same note, line for line.
    let (_, launched) = run(&["claude", "-p", "hi", "-nc"], &[]);
    let launched = launched.expect("the fake claude was launched");
    let prompt = launched["files"]["--system-prompt-file"]["content"].as_str().unwrap();
    let note = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("Bundled skills in "))
            .map(str::to_string)
    };
    assert!(note(prompt).is_some_and(|line| line.contains(PROACTIVE)), "{prompt}");
    assert_eq!(note(&rust), note(prompt));
}
