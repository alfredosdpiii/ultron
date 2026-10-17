//! `ultron mcp` end to end through the `ultron` binary (mcp-server.ts): a Claude Code subagent's verdict travels from
//! its own `ultron mcp --child` server to the parent's (`rlm.finish` -> `child.finish` with ULTRON_PARENT_TOKEN, which
//! the child's kernel never sees), the child's spawns count from the allowance its parent gave it, and the server
//! answers `usage.report` with the session report of its session file.
//!
//! The `claude` on PATH is a FAKE (a Python script, never the real CLI): as a subagent it starts the MCP server its
//! `--mcp-config` names, as Claude Code would, runs one `rlm` cell there and streams that cell's text as its reply.

mod executable_support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_ultron");
/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// A Claude Code subagent that drives its own `ultron mcp --child` server: one cell, its text as the reply.
const FAKE_CLAUDE: &str = r#"
import json, os, subprocess, sys
args = sys.argv[1:]
if "--output-format" not in args:
    sys.exit(7)
brief = sys.stdin.read()
server = json.loads(args[args.index("--mcp-config") + 1])["mcpServers"]["ultron"]
def reply(text):
    for line in [
        {"type": "system", "subtype": "init", "model": "claude-opus-5-5"},
        {"type": "result", "subtype": "success", "is_error": False, "result": text, "num_turns": 1,
         "usage": {"input_tokens": 3, "output_tokens": 2}, "total_cost_usd": 0},
    ]:
        print(json.dumps(line), flush=True)
if brief.startswith("grand"):
    # A grandchild, started by a child's own server: what it inherited and what its server was given.
    path = os.environ["FAKE_CHILD_LOG"] + ".grand"
    with open(path + ".tmp", "w", encoding="utf-8") as log:
        json.dump({"inherited": os.environ.get("ULTRON_PARENT_TOKEN"), "own": server["env"].get("ULTRON_PARENT_TOKEN"),
                   "args": server["args"]}, log)
    os.rename(path + ".tmp", path)
    reply("grand done")
    sys.exit(0)
env = dict(os.environ)
env.update(server["env"])
child = subprocess.Popen([server["command"]] + server["args"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         env=env, text=True)
def call(id, method, params):
    child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": id, "method": method, "params": params}) + "\n")
    child.stdin.flush()
    while True:
        message = json.loads(child.stdout.readline())
        if message.get("id") == id:
            return message
call(1, "initialize", {"protocolVersion": "2025-06-18"})
code = (
    "import asyncio, os\n"
    "g = await rlm.spawn('grand', name='g')\n"
    "path = os.environ['FAKE_CHILD_LOG'] + '.grand'\n"
    "for _ in range(2400):\n"
    "    if os.path.exists(path):\n"
    "        break\n"
    "    await asyncio.sleep(0.05)\n"
    "print('GRAND', open(path).read())\n"
) if brief.startswith("nest") else (
    "import os\n"
    "print('TOKEN', os.environ.get('ULTRON_PARENT_TOKEN'))\n"
    "try:\n"
    "    await rlm.spawn('x', name='x')\n"
    "except Exception as e:\n"
    "    print('SPAWN', e)\n"
    "print('FINISH', await rlm.finish('passed', 'relayed', evidence=['true: exit 0']))\n"
)
text = call(2, "tools/call", {"name": "rlm", "arguments": {"code": code}})["result"]["content"][0]["text"]
child.stdin.close()
child.wait()
with open(os.environ["FAKE_CHILD_LOG"], "a", encoding="utf-8") as log:
    log.write(json.dumps({"text": text, "args": server["args"], "token": server["env"].get("ULTRON_PARENT_TOKEN")}) + "\n")
reply("child done")
"#;

struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Mcp {
    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        writeln!(
            self.stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
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

fn control(socket: &Path, request: Value) -> Value {
    let stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writeln!(writer, "{request}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn start(root: &Path, socket: &Path, log: &Path) -> Mcp {
    let p = |s: &str| root.join(s).display().to_string();
    let mut child = Command::new(BIN)
        .args(["mcp", "--socket", &socket.display().to_string()])
        .current_dir(root.join("w"))
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", p("bin")))
        .env("HOME", p("h"))
        .env("ULTRON_CODING_AGENT_DIR", p("a"))
        .env("ULTRON_SERVER_DIR", p("s"))
        .env("XDG_RUNTIME_DIR", p("r"))
        .env("TMPDIR", p("tmp"))
        .env("ULTRON_SELF_COMMAND", json!([BIN]).to_string())
        .env("ULTRON_LOKI", "off")
        .env("ULTRON_PYTHON", python())
        .env("FAKE_CHILD_LOG", log)
        .env("CLAUDE_CODE_SESSION_ID", "sess-relay")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut mcp = Mcp {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        child,
    };
    mcp.call(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    mcp
}

fn temp_root() -> tempfile::TempDir {
    let dir = tempfile::Builder::new().prefix("ucr-").tempdir_in("/tmp").unwrap();
    for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    executable_support::install(
        &dir.path().join("bin/claude"),
        &format!("#!{} -I\n{FAKE_CLAUDE}", python()),
    );
    dir
}

#[test]
fn cli_claude_root_child_verdict_relays_from_its_own_server_to_the_parent() {
    let dir = temp_root();
    let root = dir.path();
    let (socket, log) = (root.join("r/s.sock"), root.join("children.jsonl"));
    let mut mcp = start(root, &socket, &log);
    let spawned = mcp.cell(
        2,
        "h = await rlm.spawn('please finish', name='helper')\nprint('spawned')",
    );
    assert!(spawned.ends_with("spawned"), "{spawned}");
    let deadline = Instant::now() + HANG_GUARD;
    while control(&socket, json!({"id": 1, "op": "status"}))["result"]["pendingEvents"]
        .as_u64()
        .unwrap_or(0)
        == 0
    {
        assert!(Instant::now() < deadline, "the child never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
    let record: Value = serde_json::from_str(std::fs::read_to_string(&log).unwrap().lines().next().unwrap()).unwrap();
    let text = record["text"].as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    // The token is in the child server's environment, never in its kernel's.
    assert_eq!(record["token"].as_str().unwrap().len(), 48);
    assert_eq!(lines[0], "TOKEN None", "{text}");
    // Spawned with depth=0 (the default): the child does its brief itself.
    assert!(
        lines[1].starts_with("SPAWN rlm.spawn refused: this subagent was started with depth=0"),
        "{text}"
    );
    assert_eq!(
        lines[2],
        "FINISH {'recorded': True, 'status': 'passed', 'next': 'End your turn now with a short reply; the host checks changed_files against the files that changed while you ran.'}",
        "{text}"
    );
    let args: Vec<&str> = record["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let after = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
    assert_eq!((after("--level"), after("--allowance")), ("1", "0"));
    // The parent's task carries the relayed verdict, checked: its end leads the root's next result.
    let next = mcp.cell(3, "print('next')");
    let first = next.lines().next().unwrap();
    assert!(first.starts_with("<runtime_event kind=\"child_done\""), "{next}");
    assert!(
        first.contains(r#"status="succeeded" summary="rlm-child@1: [passed, verified] child done""#),
        "{next}"
    );
    drop(mcp.stdin);
    assert!(mcp.child.wait().unwrap().success());
}

#[test]
fn cli_claude_root_mcp_answers_usage_report_with_its_session_report() {
    let dir = temp_root();
    let root = dir.path();
    let (socket, log) = (root.join("r/s.sock"), root.join("children.jsonl"));
    let mut mcp = start(root, &socket, &log);
    assert_eq!(mcp.cell(2, "print(6 * 7)"), "42");
    let status = control(&socket, json!({"id": 1, "op": "status"}))["result"].clone();
    let report = control(
        &socket,
        json!({"id": 2, "op": "inspect", "request": "usage.report", "payload": {}}),
    );
    assert_eq!(report["ok"], true, "{report}");
    let report = &report["result"];
    assert_eq!(report["schema"], "ultron.session-report/1");
    assert_eq!(report["session"]["id"], status["sessionId"]);
    assert_eq!(report["mode"], "ultron claude");
    drop(mcp.stdin);
    assert!(mcp.child.wait().unwrap().success());
}

#[test]
fn cli_claude_root_nested_children_carry_their_level_and_never_their_parents_token() {
    let dir = temp_root();
    let root = dir.path();
    let (socket, log) = (root.join("r/s.sock"), root.join("children.jsonl"));
    let mut mcp = start(root, &socket, &log);
    // depth=1: the child may spawn one level further, from its own server.
    let spawned = mcp.cell(
        2,
        "h = await rlm.spawn('nest once', name='nester', depth=1)\nprint('spawned')",
    );
    assert!(spawned.ends_with("spawned"), "{spawned}");
    let deadline = Instant::now() + HANG_GUARD;
    while !log.exists() {
        assert!(Instant::now() < deadline, "the child never ended");
        std::thread::sleep(Duration::from_millis(50));
    }
    let child: Value = serde_json::from_str(std::fs::read_to_string(&log).unwrap().lines().next().unwrap()).unwrap();
    let text = child["text"].as_str().unwrap();
    let grand: Value = serde_json::from_str(text.strip_prefix("GRAND ").unwrap_or_else(|| panic!("{text}"))).unwrap();
    let flag = |args: &Value, flag: &str| -> String {
        let args: Vec<&str> = args.as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        args[args.iter().position(|a| *a == flag).unwrap() + 1].to_string()
    };
    assert_eq!(
        (flag(&child["args"], "--level"), flag(&child["args"], "--allowance")),
        ("1".into(), "1".into())
    );
    // The grandchild is two levels down, and the child's server named itself as its parent.
    assert_eq!(
        (flag(&grand["args"], "--level"), flag(&grand["args"], "--allowance")),
        ("2".into(), "0".into())
    );
    assert_ne!(flag(&grand["args"], "--parent-socket"), socket.display().to_string());
    // Its own token, never the child's.
    assert_eq!(grand["inherited"], Value::Null, "{grand}");
    assert_eq!(grand["own"].as_str().unwrap().len(), 48);
    assert_ne!(grand["own"], child["token"]);
    drop(mcp.stdin);
    assert!(mcp.child.wait().unwrap().success());
}
