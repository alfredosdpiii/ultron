//! `ultron mcp` with `--children claude` (the default when `claude` is installed), end to end (child.ts
//! `runClaudeChild`): `rlm.spawn` starts a FAKE `claude -p --output-format stream-json`
//! (crates/claude-root/fixtures/fake_claude.py; PATH is only its directory plus /usr/bin:/bin, never the real CLI)
//! attached to its own `ultron mcp --child` server config; the child relays its verdict over the parent's control
//! socket with its one-time token (`child.finish`), its final reply is the task's result, and the end reaches the
//! root as a `child_done` event at the top of its next rlm result.

mod executable_support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on an event.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

struct Server {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut text = String::new();
            assert!(self.stdout.read_line(&mut text).unwrap() > 0, "the server exited");
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    fn cell(&mut self, id: u64, code: &str) -> String {
        let reply = self.call(id, "tools/call", json!({"name": "rlm", "arguments": {"code": code}}));
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }
}

fn control(socket: &Path, op: &str) -> Value {
    request(socket, json!({"id": 1, "op": op}))
}

fn request(socket: &Path, request: Value) -> Value {
    let stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writeln!(writer, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn claude_root_mcp_spawns_claude_code_children_and_relays_their_verdict() {
    let dir = tempfile::Builder::new().prefix("ucc-").tempdir_in("/tmp").unwrap();
    let root = dir.path();
    for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
        std::fs::create_dir(root.join(sub)).unwrap();
    }
    let fake =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../claude-root/fixtures/fake_claude.py"))
            .unwrap();
    let claude = root.join("bin/claude");
    executable_support::install(&claude, &format!("#!{} -I\n{fake}", python()));
    let log = root.join("children.jsonl");
    let socket: PathBuf = root.join("r/s.sock");
    let p = |s: &str| root.join(s).display().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["mcp", "--socket", &socket.display().to_string()])
        .current_dir(root.join("w"))
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", p("bin")))
        .env("HOME", p("h"))
        .env("ULTRON_CODING_AGENT_DIR", p("a"))
        .env("ULTRON_SERVER_DIR", p("s"))
        .env("XDG_RUNTIME_DIR", p("r"))
        .env("TMPDIR", p("tmp"))
        .env("ULTRON_SELF_COMMAND", r#"["/opt/ultron/bin/ultron"]"#)
        .env("ULTRON_LOKI", "off")
        .env("ULTRON_PYTHON", python())
        .env("FAKE_CHILD_LOG", &log)
        // The parent's Claude Code session id must not reach its children.
        .env("CLAUDE_CODE_SESSION_ID", "sess-parent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = Server {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        child,
    };
    server.call(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let spawned = server.cell(
        2,
        "h = await rlm.spawn('please give a verdict', name='helper')\nprint('spawned')",
    );
    assert!(spawned.ends_with("spawned"), "{spawned}");
    // The child's end is waiting for the root once the server reports it.
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let status = control(&socket, "status");
        if status["result"]["pendingEvents"].as_u64().unwrap_or(0) >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the child never ended: {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let next = server.cell(3, "print('next')");
    let lines: Vec<&str> = next.lines().collect();
    assert!(lines[0].starts_with("<runtime_event kind=\"child_done\""), "{next}");
    assert!(lines[0].contains("child did: please give a verdict"), "{next}");
    assert_eq!(*lines.last().unwrap(), "next");
    // The child's reported usage (3 input and 2 output tokens) is the session's: `usage.report` (what `ultron usage`
    // prints) counts it as a sub-agent's, on the model it ran.
    let report = request(
        &socket,
        json!({"id": 2, "op": "inspect", "request": "usage.report", "payload": {}}),
    );
    let usage = &report["result"]["usage"];
    let subagents = &usage["lanes"]["subagents"];
    assert_eq!(
        (&subagents["input"], &subagents["output"], &subagents["totalTokens"]),
        (&json!(3), &json!(2), &json!(5)),
        "{report}"
    );
    let model = usage["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == "claude-code/claude-opus-5-5")
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(model["totalTokens"], 5, "{report}");

    let records: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let args: Vec<&str> = record["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let value = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
    assert_eq!(args[0], "-p");
    assert_eq!(value("--output-format"), "stream-json");
    assert_eq!(value("--model"), "claude-opus-5-5");
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--allowedTools"), "mcp__ultron__rlm");
    assert_eq!(value("--setting-sources"), "");
    assert!(args.contains(&"--dangerously-skip-permissions"));
    assert!(args.contains(&"--strict-mcp-config"));
    let config: Value = serde_json::from_str(value("--mcp-config")).unwrap();
    let ultron = &config["mcpServers"]["ultron"];
    assert_eq!(ultron["command"], "/opt/ultron/bin/ultron");
    let server_args: Vec<&str> = ultron["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(server_args[..3], ["mcp", "--child", "--no-instructions"]);
    let after = |flag: &str| server_args[server_args.iter().position(|a| *a == flag).unwrap() + 1];
    assert_eq!(after("--parent-socket"), socket.display().to_string());
    assert_eq!(after("--child-model"), "claude-opus-5-5");
    assert!(
        after("--parent-lane").starts_with("ultron.rlm-child."),
        "{server_args:?}"
    );
    assert_eq!(ultron["env"]["ULTRON_PARENT_TOKEN"].as_str().unwrap().len(), 48);
    assert_eq!(record["brief"], "please give a verdict");
    assert_eq!(record["env_has_session"], false);
    // The verdict was accepted from the token holder.
    assert_eq!(record["finish"]["ok"], true, "{record}");

    drop(server.stdin);
    assert!(server.child.wait().unwrap().success());
}
