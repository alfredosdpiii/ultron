//! Gaps between native Ultron and `ultron claude` (wave 44 audit), through the product entries: `ultron mcp`
//! (`mcp_server::start` and the MCP loop over pipes) and `ultron claude --print-config` (`launcher::run`), with a
//! private temp root as their environment. Claude Code subagents are a FAKE `claude` (a script this test writes,
//! named by ULTRON_CLAUDE_BIN; never the real CLI) that records how it was started and reports scripted usage.
//!
//! - a Claude Code child's model: `--child-model` / `claudeCode.childModel` when named, else `rlm.childModel` only
//!   when it names a Claude Code model (another provider's failed every spawn);
//! - a Claude Code child's reported usage is charged to the turn that spawned it (the ledger, the root's token
//!   limit, the counters' child models) in the session's own values;
//! - job and tool-call ends are masked with the session's secrets (MCP-registered values included);
//! - the guide names the `mcp.json` servers and the Rust APIs the external root serves, and none it cannot;
//! - Loki's policy, SYSTEM.md and APPEND_SYSTEM.md reach the root's and the subagents' prompts;
//! - a named frame model beats `rlm.frameModel` and ULTRON_RLM_FRAME_MODEL;
//! - `schedule.every|at`, `rlm.spawn(keep=True)`, `rlm.ask` and the swarm are refused with a clear error;
//! - each turn records a working-tree checkpoint, as native Ultron's root turns do.

#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;

use claude_root::control::ControlClient;
use claude_root::mcp_protocol::serve;
use claude_root::mcp_server::{Running, start};
use serde_json::{Value, json};
use session_core::RunEnv;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// A fake `claude -p --output-format stream-json`: records its model, brief and system prompt next to itself
/// (`children.jsonl`), then streams a reply whose usage is `TOKENS=<n>` input tokens from the brief (default 3),
/// 2 output tokens, $0.25 and 2 turns.
const FAKE_CLAUDE: &str = r#"
import json, os, sys
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
brief = sys.stdin.read()
with open(args[args.index("--system-prompt-file") + 1], encoding="utf-8") as handle:
    prompt = handle.read()
model = args[args.index("--model") + 1]
tokens = 3
for word in brief.split():
    if word.startswith("TOKENS="):
        tokens = int(word[len("TOKENS="):])
with open(os.path.join(here, "children.jsonl"), "a", encoding="utf-8") as log:
    log.write(json.dumps({"model": model, "brief": brief, "prompt": prompt}) + "\n")
for line in [
    {"type": "system", "subtype": "init", "model": model},
    {"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "working"}]}},
    {"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "child did: " + brief.strip()}]}},
    {"type": "result", "subtype": "success", "is_error": False, "result": "child did: " + brief.strip(),
     "num_turns": 2, "usage": {"input_tokens": tokens, "output_tokens": 2}, "total_cost_usd": 0.25,
     "duration_ms": 40},
]:
    print(json.dumps(line), flush=True)
"#;

/// A fake stdio MCP server with one tool, `echo_token`: it waits for `<cwd>/go`, then answers with its FAKE_TOKEN.
const FAKE_MCP: &str = r#"
import json, os, sys, time
for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    if method == "initialize":
        result = {"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}},
                  "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo_token", "description": "Echo the token", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        while not os.path.exists("go"):
            time.sleep(0.02)
        result = {"content": [{"type": "text", "text": "token " + os.environ["FAKE_TOKEN"]}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)
"#;

struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new().prefix("ucp-").tempdir_in("/tmp").unwrap();
        for sub in ["h", "a", "s", "r", "w", "tmp", "bin"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Root { dir }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    fn write(&self, sub: &str, text: &str) {
        std::fs::write(self.path(sub), text).unwrap();
    }

    /// Install the fake `claude`; the environment then names it.
    fn claude(&self) -> String {
        let path = self.path("bin/claude");
        executable_support::install(&path, &format!("#!{} -I\n{FAKE_CLAUDE}", python()));
        path.display().to_string()
    }

    fn children(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("bin/children.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn env(&self, extra: &[(&str, &str)]) -> RunEnv {
        let p = |s: &str| self.path(s).display().to_string();
        let mut vars: Vec<(String, String)> = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), p("h")),
            ("ULTRON_CODING_AGENT_DIR".into(), p("a")),
            ("ULTRON_SERVER_DIR".into(), p("s")),
            ("XDG_RUNTIME_DIR".into(), p("r")),
            ("TMPDIR".into(), p("tmp")),
            ("ULTRON_SELF_COMMAND".into(), r#"["/opt/ultron/bin/ultron"]"#.into()),
            ("ULTRON_LOKI".into(), "off".into()),
            ("ULTRON_PYTHON".into(), python()),
            ("PI_OFFLINE".into(), "1".into()),
        ];
        for (key, value) in extra {
            vars.retain(|(k, _)| k != key);
            vars.push((key.to_string(), value.to_string()));
        }
        RunEnv {
            cwd: self.path("w"),
            home: self.path("h"),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            docs: repo().join("packages/coding-agent/docs"),
        }
    }

    /// `git` in the working directory, with a fixed identity and no system or user configuration.
    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(self.path("w"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("h"))
            .envs(GIT_IDENTITY)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_repo(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["config", "commit.gpgsign", "false"]);
        self.write("w/a.txt", "a\n");
        self.git(&["add", "a.txt"]);
        self.git(&["commit", "-qm", "init"]);
    }
}

const GIT_IDENTITY: [(&str, &str); 5] = [
    ("GIT_AUTHOR_NAME", "Test User"),
    ("GIT_AUTHOR_EMAIL", "test@example.com"),
    ("GIT_COMMITTER_NAME", "Test User"),
    ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// A server started through its product entry, its MCP connection on pipes.
struct Server {
    running: Arc<Running>,
    stdin: Option<std::io::PipeWriter>,
    stdout: BufReader<std::io::PipeReader>,
    serving: Option<std::thread::JoinHandle<()>>,
    next: u64,
}

impl Server {
    fn start(args: &[&str], env: RunEnv) -> Server {
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let (running, handlers) = start(&argv, &claude_root::Ctx { env }).unwrap();
        let (input, stdin) = std::io::pipe().unwrap();
        let (stdout, output) = std::io::pipe().unwrap();
        let serving = std::thread::spawn(move || {
            serve(handlers, &mut BufReader::new(input), Box::new(output));
        });
        let mut server = Server {
            running,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            serving: Some(serving),
            next: 0,
        };
        server.call("initialize", json!({"protocolVersion": "2025-06-18"}));
        server
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        loop {
            let mut text = String::new();
            assert!(self.stdout.read_line(&mut text).unwrap() > 0, "the server closed");
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// One cell's whole reply.
    fn reply(&mut self, code: &str) -> Value {
        self.call("tools/call", json!({"name": "rlm", "arguments": {"code": code}}))["result"].clone()
    }

    /// One cell's result text.
    fn cell(&mut self, code: &str) -> String {
        self.reply(code)["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn control(&self, op: &str, fields: Value) -> Result<Value, String> {
        let mut client = ControlClient::connect(Path::new(&self.running.socket()), HANG_GUARD)?;
        client.request(op, fields.as_object().cloned().unwrap_or_default(), HANG_GUARD)
    }

    fn hook(&self, event: &str) -> Value {
        self.control("hook", json!({"event": event})).unwrap()
    }

    /// Wait until `n` root events are waiting for the next result.
    fn wait_events(&self, n: u64) {
        wait_until("the root's events", || {
            self.control("status", json!({})).unwrap()["pendingEvents"]
                .as_u64()
                .unwrap_or(0)
                >= n
        });
    }

    /// A session value of the server's session.
    fn value(&self, namespace: &str, key: &str) -> Value {
        let opened = self.running.opened().unwrap();
        let session = session_core::durable_store::lock(&opened.session);
        session.value(namespace, key).cloned().unwrap_or(Value::Null)
    }

    fn close(mut self) {
        drop(self.stdin.take());
        self.serving.take().unwrap().join().unwrap();
        self.running.shutdown();
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn one Claude Code child from a cell and wait for its end to reach the root; returns its task id.
fn spawn_child(server: &mut Server, brief: &str, before: u64) -> String {
    let printed = server.cell(&format!(
        "h = await rlm.spawn({brief:?}, name='helper')\nprint(h.rlm_child_id)"
    ));
    server.wait_events(before + 1);
    // The id, without the budget line a cell result may end with.
    printed.lines().next().unwrap().to_string()
}

#[test]
fn claude_root_w25c_claude_children_ignore_another_providers_child_model_and_keep_the_named_one() {
    // rlm.childModel names another provider's model: every spawn used to fail ("subagents here are Claude Code
    // processes"). It is ignored; the child runs on the Claude default.
    let root = Root::new();
    let claude = root.claude();
    root.write("a/settings.json", r#"{"rlm": {"childModel": "openai/gpt-5.5"}}"#);
    let mut server = Server::start(&[], root.env(&[("ULTRON_CLAUDE_BIN", &claude)]));
    spawn_child(&mut server, "first brief", 0);
    let next = server.cell("print('next')");
    assert!(
        next.starts_with("<runtime_event kind=\"child_done\"") && next.contains("status=\"succeeded\""),
        "{next}"
    );
    server.close();
    // rlm.childModel names a Claude Code model: spawns that name none run on it.
    root.write("a/settings.json", r#"{"rlm": {"childModel": "claude-code/haiku"}}"#);
    let mut server = Server::start(&[], root.env(&[("ULTRON_CLAUDE_BIN", &claude)]));
    spawn_child(&mut server, "second brief", 0);
    server.close();
    // A named child model (`--child-model`, here as `ultron claude` passes it) is never overridden by it.
    let mut server = Server::start(
        &["--child-model", "sonnet"],
        root.env(&[("ULTRON_CLAUDE_BIN", &claude)]),
    );
    spawn_child(&mut server, "third brief", 0);
    // A spawn that names a model still runs on it.
    let spawned = server.cell("h = await rlm.spawn('fourth brief', name='named', model='claude-code/opus')\nprint(1)");
    assert!(spawned.ends_with("\n1"), "{spawned}");
    server.wait_events(1);
    server.close();
    let models: Vec<(String, String)> = root
        .children()
        .iter()
        .map(|c| {
            (
                c["brief"].as_str().unwrap().to_string(),
                c["model"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        models,
        [
            ("first brief".to_string(), "claude-opus-5-5".to_string()),
            ("second brief".into(), "haiku".into()),
            ("third brief".into(), "sonnet".into()),
            ("fourth brief".into(), "opus".into()),
        ]
    );
}

#[test]
fn claude_root_w25c_claude_child_usage_is_charged_to_the_turn_that_spawned_it() {
    let root = Root::new();
    let claude = root.claude();
    let mut server = Server::start(
        &[],
        root.env(&[("ULTRON_CLAUDE_BIN", &claude), ("ULTRON_MAX_TOTAL_TOKENS", "1000")]),
    );
    server.hook("user-prompt");
    let turn = server.control("status", json!({})).unwrap()["turn"]
        .as_str()
        .unwrap()
        .to_string();
    let task = spawn_child(&mut server, "TOKENS=4000 spend tokens", 0);
    // The child's 4,002 tokens count toward the turn's limit: its next cell is refused.
    let refused = server.reply("print('more')");
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("<runtime_event kind=\"child_done\""), "{text}");
    assert!(
        text.ends_with(&format!(
            "\n[Ultron] Usage token limit reached for root turn:{turn}: 4002 of 1000 tokens used (max_total_tokens, ULTRON_MAX_TOTAL_TOKENS). No further model work is admitted for this root; answer with what you have.; answer with what you have."
        )),
        "{text}"
    );
    // The session's ledger holds the child's model call, settled with what it reported, under the turn's root.
    let ledger = server.value("ultron.usage", "root");
    let turn_root = &ledger["roots"][format!("turn:{turn}")];
    let calls = turn_root["calls"].as_array().unwrap_or_else(|| panic!("{ledger}"));
    let call = calls
        .iter()
        .find(|c| c["kind"] == "model" && c["taskId"] == task.as_str())
        .unwrap_or_else(|| panic!("{ledger}"));
    assert_eq!(call["status"], "succeeded", "{call}");
    assert_eq!(
        (
            &call["usage"]["inputTokens"],
            &call["usage"]["outputTokens"],
            &call["usage"]["totalTokens"],
            &call["usage"]["cost"],
            &call["usage"]["wallMs"]
        ),
        (&json!(4000), &json!(2), &json!(4002), &json!(0.25), &json!(40)),
        "{call}"
    );
    // Its two turns: the tokens and cost on the first.
    assert_eq!(turn_root["turns"]["count"], 2, "{turn_root}");
    assert_eq!(turn_root["turns"]["tokens"], 4002, "{turn_root}");
    // The task is in the session's own journal, and its model in the counters (`ultron usage` reads both).
    let tasks = server.value("ultron.tasks", "root");
    assert!(
        tasks["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == task.as_str()),
        "{tasks}"
    );
    let opened = server.running.opened().unwrap();
    opened.lane.stack.stats.flush();
    let stats = server.value("ultron.module", "stats");
    assert_eq!(
        stats["childModels"][task.as_str()],
        "claude-code/claude-opus-5-5",
        "{stats}"
    );
    // A new turn has a budget of its own.
    server.hook("stop");
    server.hook("user-prompt");
    assert_eq!(
        server.cell("print('again')"),
        "again\n[budget left for this request: 1,000 tokens]"
    );
    server.close();
}

#[test]
fn claude_root_w25c_a_spent_root_admits_no_claude_child() {
    // The first child spends the turn's tokens; a second spawned after it by the same cell is refused at admission
    // (the cell started before the tree was spent) instead of running unmetered.
    let root = Root::new();
    let claude = root.claude();
    let mut server = Server::start(
        &[],
        root.env(&[("ULTRON_CLAUDE_BIN", &claude), ("ULTRON_MAX_TOTAL_TOKENS", "1000")]),
    );
    server.hook("user-prompt");
    let turn = server.control("status", json!({})).unwrap()["turn"]
        .as_str()
        .unwrap()
        .to_string();
    let code = r#"
import asyncio
async def ended(h):
    while True:
        try:
            return (await rlm.collect([h]))[0]["result"]
        except RuntimeError:
            await asyncio.sleep(0.05)
first = await ended(await rlm.spawn('TOKENS=4000 first', name='first'))
h2 = await rlm.spawn('second', name='second')
second = await ended(h2)
print(first["status"])
print(second["status"], second["error"])
print(h2.rlm_child_id)
"#;
    let printed = server.cell(code);
    // (The cell's result ends with the budget line.)
    assert_eq!(
        printed.lines().take(2).collect::<Vec<_>>().join("\n"),
        format!(
            "succeeded\nfailed Usage token limit reached for root turn:{turn}: 4002 of 1000 tokens used (max_total_tokens, ULTRON_MAX_TOTAL_TOKENS). No further model work is admitted for this root; answer with what you have."
        )
    );
    assert_eq!(root.children().len(), 1);
    // The refused child's model is still counted (it never reported one).
    let second = printed.lines().nth(2).unwrap();
    let opened = server.running.opened().unwrap();
    opened.lane.stack.stats.flush();
    let stats = server.value("ultron.module", "stats");
    assert_eq!(stats["childModels"][second], "claude-code/claude-opus-5-5", "{stats}");
    server.close();
}

#[test]
fn claude_root_w25c_event_summaries_are_masked_with_the_sessions_secrets() {
    let root = Root::new();
    // A fake secret built from parts, so the source holds no secret-shaped literal.
    let token = &["zq-mcp-regi", "stered-7731"].concat();
    let server_script = root.path("bin/fake_mcp.py");
    std::fs::write(&server_script, FAKE_MCP).unwrap();
    root.write(
        "a/mcp.json",
        &json!({"mcpServers": {"fake": {"command": python(), "args": ["-I", server_script], "env": {"FAKE_TOKEN": token}}}})
            .to_string(),
    );
    let mut server = Server::start(&[], root.env(&[]));
    // The server's env value is registered as a secret when it connects; a tool call and a job then end after the
    // cell, carrying it in their output.
    let started = server.cell(
        "c = await mcp.call('fake_echo_token', yield_after=0)\nj = await bash('while [ ! -f go ]; do sleep 0.02; done; echo token zq-mcp-registered-7731', yield_after=0)\nprint(c.id, j.id)",
    );
    assert!(!started.contains("Error"), "{started}");
    root.write("w/go", "");
    server.wait_events(2);
    let next = server.cell("print('next')");
    let events: Vec<&str> = next.lines().filter(|l| l.starts_with("<runtime_event")).collect();
    assert_eq!(events.len(), 2, "{next}");
    assert!(events.iter().any(|e| e.contains("kind=\"tool_done\"")), "{next}");
    assert!(events.iter().any(|e| e.contains("kind=\"job_done\"")), "{next}");
    for event in &events {
        assert!(!event.contains(token), "{event}");
        assert!(event.contains("token "), "{event}");
    }
    server.close();
}

#[test]
fn claude_root_w25c_named_frame_model_beats_rlm_frame_model() {
    let root = Root::new();
    root.write(
        "a/settings.json",
        r#"{"rlm": {"frameModel": "groq/llama-3.3-70b-versatile"}}"#,
    );
    let frame = "import json\nprint(json.dumps((await rlm._models())['frame']))";
    // Named (`--frame-model`): it wins over rlm.frameModel and ULTRON_RLM_FRAME_MODEL.
    let mut server = Server::start(
        &["--frame-model", "claude-code/sonnet"],
        root.env(&[("ULTRON_RLM_FRAME_MODEL", "groq/other-model")]),
    );
    let named: Value = serde_json::from_str(&server.cell(frame)).unwrap();
    assert_eq!(named["model"], "claude-code/sonnet", "{named}");
    server.close();
    // Not named: the settings decide as in native Ultron.
    let mut server = Server::start(&[], root.env(&[]));
    let unnamed: Value = serde_json::from_str(&server.cell(frame)).unwrap();
    assert_eq!(
        unnamed,
        json!({"model": "groq/llama-3.3-70b-versatile", "source": "setting"})
    );
    server.close();
}

#[test]
fn claude_root_w25c_each_turn_records_a_working_tree_checkpoint() {
    let root = Root::new();
    root.git_repo();
    let mut server = Server::start(&[], root.env(&[]));
    server.hook("user-prompt");
    let first = server.control("status", json!({})).unwrap()["turn"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(server.cell("open('b.txt', 'w').write('b\\n')\nprint('wrote')"), "wrote");
    server.hook("stop");
    server.hook("user-prompt");
    let second = server.control("status", json!({})).unwrap()["turn"]
        .as_str()
        .unwrap()
        .to_string();
    let opened = server.running.opened().unwrap();
    let listed = session_journal::checkpoints::list(&session_core::durable_store::lock(&opened.session));
    let keys: Vec<&str> = listed.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, [first.as_str(), second.as_str()], "{listed:?}");
    // The second turn's checkpoint holds the file the first wrote; the first's does not.
    let tree = |value: &Value| root.git(&["ls-tree", "--name-only", value["commit"].as_str().unwrap()]);
    assert_eq!(tree(&listed[0].1), "a.txt");
    assert_eq!(tree(&listed[1].1), "a.txt\nb.txt");
    assert_eq!(listed[1].1["kind"], "turn");
    server.close();
    // `checkpoints.enabled: false` records none.
    let root = Root::new();
    root.git_repo();
    root.write("a/settings.json", r#"{"checkpoints": {"enabled": false}}"#);
    let server = Server::start(&[], root.env(&[]));
    server.hook("user-prompt");
    let opened = server.running.opened().unwrap();
    assert!(session_journal::checkpoints::list(&session_core::durable_store::lock(&opened.session)).is_empty());
    server.close();
}

/// `ultron claude --print-config` as the launcher prints it (in process, the fake `claude` on the injected PATH).
fn print_config(root: &Root, extra: &[(&str, &str)]) -> String {
    let fake = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_claude.py")).unwrap();
    executable_support::install(&root.path("bin/claude"), &format!("#!{} -I\n{fake}", python()));
    let path = format!("{}:/usr/bin:/bin", root.path("bin").display());
    let mut vars: Vec<(&str, &str)> = vec![("PATH", &path)];
    vars.extend(extra);
    let ctx = claude_root::Ctx { env: root.env(&vars) };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = ["--print-config".to_string(), "-p".into(), "hi".into()];
    assert_eq!(claude_root::launcher::run(&args, &ctx, &mut out, &mut err), Ok(0));
    let printed: Value = serde_json::from_slice(&out).unwrap();
    let args: Vec<&str> = printed["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    args[args.iter().position(|a| *a == "--system-prompt").unwrap() + 1].to_string()
}

#[test]
fn claude_root_w25c_guide_names_the_mcp_servers_and_the_rust_apis_the_external_root_serves() {
    let root = Root::new();
    root.write(
        "a/mcp.json",
        r#"{"mcpServers": {"docs-search": {"command": "/bin/false"}}}"#,
    );
    let prompt = print_config(&root, &[]);
    for wanted in [
        "MCP servers (docs-search) are reached through the `mcp` namespace",
        "type=\"pitfall\"",
        "await rlm.images.generate(",
        "await diagram.mermaid(text)",
        "Contracts: `rlm.spawn(brief, name=..., contract=",
        // Served from the server's durable runtime and timer (tests/durable_apis.rs).
        "keep=True",
        "rlm.ask",
        "swarm.route",
        "schedule.every",
    ] {
        assert!(prompt.contains(wanted), "{wanted}: {prompt}");
    }
    // Claude Code owns its context: no `ctx`.
    assert!(!prompt.contains("`ctx` edits"), "{prompt}");
    // The TypeScript guide (ULTRON_GUIDE=ts) names none of them, as TypeScript's `ultron claude` did.
    let ts = print_config(&root, &[("ULTRON_GUIDE", "ts")]);
    for absent in [
        "MCP servers",
        "type=\"pitfall\"",
        "rlm.images",
        "diagram.mermaid",
        "Contracts:",
    ] {
        assert!(!ts.contains(absent), "{absent}: {ts}");
    }
}

#[test]
fn claude_root_w25c_system_md_and_append_system_md_shape_root_and_child_prompts() {
    let root = Root::new();
    root.write("a/SYSTEM.md", "CUSTOM BASE RULES: answer in haiku.\n");
    root.write("a/APPEND_SYSTEM.md", "APPENDED RULES: cite sources.\n");
    let prompt = print_config(&root, &[]);
    assert!(prompt.contains("CUSTOM BASE RULES: answer in haiku."), "{prompt}");
    assert!(prompt.contains("APPENDED RULES: cite sources."), "{prompt}");
    // SYSTEM.md takes the working rules' place; the tool and runtime guide stay.
    assert!(!prompt.contains("# Working with the user"), "{prompt}");
    assert!(
        prompt.contains("# Your tool") && prompt.contains("## Runtime"),
        "{prompt}"
    );
    assert!(
        prompt.find("CUSTOM BASE RULES").unwrap() < prompt.find("APPENDED RULES").unwrap()
            && prompt.find("APPENDED RULES").unwrap() < prompt.find("## Runtime").unwrap(),
        "{prompt}"
    );
    // A Claude Code subagent gets both after its own identity and finishing rules.
    let claude = root.claude();
    let mut server = Server::start(&[], root.env(&[("ULTRON_CLAUDE_BIN", &claude)]));
    spawn_child(&mut server, "child brief", 0);
    server.close();
    let child = root.children()[0]["prompt"].as_str().unwrap().to_string();
    assert!(child.starts_with("You are a subagent of Ultron"), "{child}");
    assert!(child.contains("CUSTOM BASE RULES: answer in haiku."), "{child}");
    assert!(child.contains("APPENDED RULES: cite sources."), "{child}");
    assert!(child.contains("# Finishing"), "{child}");
}

#[test]
fn claude_root_w25c_lokis_policy_reaches_the_root_prompt_and_claude_subagents() {
    let root = Root::new();
    root.git_repo();
    let loki = [("ULTRON_LOKI", "on")];
    let mut vars: Vec<(&str, &str)> = GIT_IDENTITY.to_vec();
    vars.extend(loki);
    // A first server sets Loki up (the minimal install, committed in the background).
    let mut server = Server::start(&[], root.env(&vars));
    assert_eq!(server.cell("print(1)"), "1");
    wait_until("the .loki commit", || {
        root.git(&["log", "-1", "--format=%s"]) == "Add Loki guardrails"
    });
    server.close();
    let note = claude_root::guide::loki_note_at(Some(&root.path("w"))).expect("Loki's policy note");
    // The launcher puts the policy in the root's prompt: a root without Ultron's hooks has it too.
    let prompt = print_config(&root, &vars);
    assert!(prompt.contains(&note), "{prompt}");
    // `ultron claude`'s server (`--no-instructions`) then leaves it out of the SessionStart context.
    let server = Server::start(&["--no-instructions"], root.env(&vars));
    assert_eq!(server.hook("session-start"), json!({}));
    server.close();
    // A server whose prompt is not Ultron's (instructions on) still sends it with the session context.
    let server = Server::start(&[], root.env(&vars));
    let context = server.hook("session-start");
    assert_eq!(
        context["hookSpecificOutput"]["additionalContext"].as_str(),
        Some(note.as_str()),
        "{context}"
    );
    server.close();
    // A Claude Code subagent's prompt carries it.
    let claude = root.claude();
    let mut child_vars = vars.clone();
    child_vars.push(("ULTRON_CLAUDE_BIN", &claude));
    let mut server = Server::start(&[], root.env(&child_vars));
    spawn_child(&mut server, "child brief", 0);
    server.close();
    let child = root.children()[0]["prompt"].as_str().unwrap().to_string();
    assert!(child.contains(&note), "{child}");
}
