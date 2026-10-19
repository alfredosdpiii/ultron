//! `ultron mcp` servers run in-process through their product entry (`mcp_server::start` + the MCP `serve` loop over
//! pipes, as `mcp_server::run` drives them), with a private temp root as their environment: no `claude` on the
//! injected PATH (children are Ultron lanes), Loki off, and an OpenAI-compatible fake provider on 127.0.0.1 where a
//! test runs a child. Covered (mcp-server.ts, external-root.ts, session-worker.ts with `externalRoot`):
//!
//! - a subagent server's `rlm.finish` is relayed to its parent as `child.finish` with ULTRON_PARENT_TOKEN, which its
//!   kernel never sees; its spawns count from `--level` and `--allowance`;
//! - `--children ultron` children run on `--ultron-child-model`;
//! - `ctx.*` works on the root lane, shell-job ends reach Claude Code at the top of the next result, and
//!   `ctx.state`, `rlm.pool` and `async.pending` are answered over the control socket;
//! - the nudges of ULTRON_TOOL_ROUNDS_NUDGE are appended to cell results;
//! - the shutdown a signal runs (tested as the function it calls, never with a signal) unregisters the server and
//!   closes its runtime.

use claude_root::control::{ControlClient, listen};
use claude_root::mcp_protocol::serve;
use claude_root::mcp_server::{Running, start};
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new().prefix("ucm-").tempdir_in("/tmp").unwrap();
        for sub in ["h", "a", "s", "r", "w", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Root { dir }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
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
}

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

    /// One cell's result text.
    fn cell(&mut self, code: &str) -> String {
        let reply = self.call("tools/call", json!({"name": "rlm", "arguments": {"code": code}}));
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn control(&self, op: &str, fields: Value) -> Result<Value, String> {
        let mut client = ControlClient::connect(Path::new(&self.running.socket()), HANG_GUARD)?;
        client.request(op, fields.as_object().cloned().unwrap_or_default(), HANG_GUARD)
    }

    fn inspect(&self, request: &str) -> Result<Value, String> {
        self.control("inspect", json!({"request": request, "payload": {}}))
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

/// The requests a fake parent server received.
type Requests = Arc<Mutex<Vec<Map<String, Value>>>>;

/// A parent server's control socket that records `child.finish` requests and accepts them.
fn parent(root: &Root) -> (claude_root::control::ControlServer, Requests) {
    let seen: Requests = Arc::default();
    let record = seen.clone();
    let server = listen(
        &root.path("r/parent.sock"),
        Arc::new(move |request: Map<String, Value>| {
            record.lock().unwrap().push(request);
            Ok(json!({"recorded": true, "status": "blocked"}))
        }),
    )
    .unwrap();
    (server, seen)
}

#[test]
fn claude_root_mcp_child_server_relays_its_verdict_with_its_token_hidden_from_the_kernel() {
    let root = Root::new();
    let (_parent, seen) = parent(&root);
    let parent_socket = root.path("r/parent.sock").display().to_string();
    let mut server = Server::start(
        &[
            "--child",
            "--parent-socket",
            &parent_socket,
            "--parent-lane",
            "lane-9",
            "--level",
            "1",
            "--allowance",
            "0",
        ],
        root.env(&[("ULTRON_PARENT_TOKEN", "tok-77")]),
    );
    assert_eq!(
        server.cell("import os\nprint(os.environ.get('ULTRON_PARENT_TOKEN'))"),
        "None"
    );
    let finished = server.cell("print(await rlm.finish('blocked', 'no network'))");
    assert_eq!(finished, "{'recorded': True, 'status': 'blocked'}");
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let mut request = Value::Object(requests[0].clone());
    request.as_object_mut().unwrap().remove("id");
    assert_eq!(
        request,
        json!({"token": "tok-77", "payload": {"status": "blocked", "summary": "no network", "evidence": [],
            "outputs": {}, "changed_files": []}, "op": "child.finish"})
    );
    // `--allowance 0`: this subagent does its brief itself.
    let refused = server.cell("await rlm.spawn('x', name='x')");
    assert!(
        refused.contains("rlm.spawn refused: this subagent was started with depth=0"),
        "{refused}"
    );
    server.close();
}

#[test]
fn claude_root_mcp_child_server_spawns_count_from_its_level_and_allowance() {
    let root = Root::new();
    let (_parent, _) = parent(&root);
    let parent_socket = root.path("r/parent.sock").display().to_string();
    let args = |level: &'static str, allowance: &'static str| -> Vec<String> {
        [
            "--child",
            "--parent-socket",
            &parent_socket,
            "--parent-lane",
            "L",
            "--level",
            level,
            "--allowance",
            allowance,
        ]
        .iter()
        .map(|a| a.to_string())
        .collect()
    };
    let start = |level, allowance, env: RunEnv| {
        let owned = args(level, allowance);
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        Server::start(&refs, env)
    };
    // Allowance 2: one more level below its children at most.
    let mut server = start("1", "2", root.env(&[]));
    let refused = server.cell("await rlm.spawn('x', name='x', depth=2)");
    assert!(
        refused.contains("rlm.spawn depth=2 is too deep here: at most 1"),
        "{refused}"
    );
    server.close();
    // Levels above count toward ULTRON_SPAWN_DEPTH: at level 1 of 2 a child may nest no further.
    let mut server = start("1", "5", root.env(&[("ULTRON_SPAWN_DEPTH", "2")]));
    let refused = server.cell("await rlm.spawn('x', name='x', depth=1)");
    assert!(
        refused.contains("rlm.spawn depth=1 is too deep here: at most 0"),
        "{refused}"
    );
    server.close();
    let mut server = start("2", "5", root.env(&[("ULTRON_SPAWN_DEPTH", "2")]));
    let refused = server.cell("await rlm.spawn('x', name='x')");
    assert!(refused.contains("subagents nest at most 2 level(s) deep"), "{refused}");
    server.close();
}

/// An OpenAI-compatible chat-completions endpoint on 127.0.0.1 that answers every request with `reply` and records
/// the request bodies.
fn fake_provider(reply: &'static str) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                let mut first = true;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        return;
                    }
                    if header.trim().is_empty() && !first {
                        break;
                    }
                    first = false;
                    if let Some((k, v)) = header.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let chunk = |delta: Value, finish: Value| {
                    format!(
                        "data: {}\n\n",
                        json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "m",
                            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}})
                    )
                };
                let sse = chunk(json!({"role": "assistant", "content": reply}), Value::Null)
                    + &chunk(json!({}), json!("stop"))
                    + "data: [DONE]\n\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    sse.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(sse.as_bytes());
            });
        }
    });
    (port, requests)
}

#[test]
fn claude_root_mcp_ultron_children_run_on_the_ultron_child_model() {
    let root = Root::new();
    let (port, requests) = fake_provider("child answer");
    std::fs::write(
        root.path("a/models.json"),
        json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
    )
    .unwrap();
    let mut server = Server::start(
        &[
            "--children",
            "ultron",
            "--ultron-child-model",
            "groq/llama-3.3-70b-versatile",
        ],
        root.env(&[]),
    );
    let spawned = server.cell("h = await rlm.spawn('say hi', name='helper')\nprint(h.model)");
    assert_eq!(spawned, "groq/llama-3.3-70b-versatile");
    // The child's end waits for the root and leads its next result.
    wait_until("the child's end", || {
        server.control("status", json!({})).unwrap()["pendingEvents"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    });
    let next = server.cell("print('next')");
    assert!(next.starts_with("<runtime_event kind=\"child_done\""), "{next}");
    assert!(next.contains("child answer"), "{next}");
    // The child's requests went to the child model, and only to it.
    let models: Vec<Value> = requests.lock().unwrap().iter().map(|r| r["model"].clone()).collect();
    assert!(!models.is_empty());
    assert!(models.iter().all(|m| m == "llama-3.3-70b-versatile"), "{models:?}");
    server.close();
}

#[test]
fn claude_root_mcp_root_kernel_has_ctx_and_answers_the_viewer() {
    let root = Root::new();
    let mut server = Server::start(&[], root.env(&[]));
    let noted = server.cell(
        "n = await ctx.note('remember 42')\nitems = (await ctx.history())['items']\nprint([i['kind'] for i in items], items[-1]['preview'])",
    );
    assert_eq!(noted, "['note'] remember 42");
    // The `/rlm` panel's view (and `ultron watch`'s context pane): one note on the root lane.
    assert_eq!(
        server.inspect("ctx.state").unwrap(),
        json!({"forgotten": [], "forgottenCount": 0, "collapsedCount": 0, "pinned": [], "pinnedCount": 0, "notes": 1})
    );
    // The worker answers `rlm.pool` itself: the root lane's live kernel.
    let pool = server.inspect("rlm.pool").unwrap();
    assert!(pool.to_string().contains("\"main\""), "{pool}");
    assert_eq!(
        server.inspect("async.pending").unwrap(),
        json!({"pending": false, "jobs": 0, "tools": 0, "tasks": 0, "events": 0, "running": 0})
    );
    server.close();
}

#[test]
fn claude_root_mcp_shell_job_ends_lead_the_next_result() {
    let root = Root::new();
    let mut server = Server::start(&[], root.env(&[]));
    let started = server.cell(
        "j = await bash('while [ ! -f go ]; do sleep 0.02; done; echo done', yield_after=0)\nprint(j.id, j.running)",
    );
    let id = started.split(' ').next().unwrap().to_string();
    assert!(started.ends_with(" True"), "{started}");
    let pending = server.inspect("async.pending").unwrap();
    assert_eq!(pending["jobs"], 1, "{pending}");
    assert_eq!(pending["pending"], true, "{pending}");
    // The job ends once the file it waits for exists.
    std::fs::write(root.path("w/go"), "").unwrap();
    wait_until("the job's end", || {
        server.control("status", json!({})).unwrap()["pendingEvents"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    });
    let next = server.cell("print('next')");
    let first = next.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("<runtime_event kind=\"job_done\" id=\"{id}\"")),
        "{next}"
    );
    assert!(next.ends_with("\nnext"), "{next}");
    server.close();
}

#[test]
fn claude_root_mcp_events_off_answers_async_pending_as_typescript() {
    let root = Root::new();
    let server = Server::start(&[], root.env(&[("ULTRON_ASYNC_EVENTS", "off")]));
    assert_eq!(
        server.inspect("async.pending").unwrap(),
        json!({"pending": false, "jobs": 0, "tasks": 0, "events": 0})
    );
    server.close();
}

#[test]
fn claude_root_mcp_tool_round_nudge_is_appended_to_cell_results() {
    let root = Root::new();
    let mut server = Server::start(&[], root.env(&[("ULTRON_TOOL_ROUNDS_NUDGE", "2")]));
    assert_eq!(server.cell("print(1)"), "1");
    assert_eq!(
        server.cell("print(2)"),
        "2\n[Ultron] You have used 2 rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown."
    );
    // The Stop hook ends the turn: counting starts over.
    server.control("hook", json!({"event": "stop"})).unwrap();
    assert_eq!(server.cell("print(3)"), "3");
    server.close();
}

#[test]
fn claude_root_mcp_signal_shutdown_unregisters_and_closes_the_runtime() {
    let root = Root::new();
    let mut server = Server::start(
        &["--socket", &root.path("r/s.sock").display().to_string()],
        root.env(&[]),
    );
    assert_eq!(server.cell("x = 41\nprint(x + 1)"), "42");
    let registry = root.path("r/ultron-claude");
    let registered = || std::fs::read_dir(&registry).map(|d| d.count()).unwrap_or(0);
    assert_eq!(registered(), 1);
    // What the SIGTERM/SIGINT/SIGHUP handler runs before it exits (no signal is sent here).
    server.running.shutdown();
    assert_eq!(registered(), 0);
    assert!(!root.path("r/s.sock").exists());
    assert!(std::os::unix::net::UnixStream::connect(root.path("r/s.sock")).is_err());
    // The runtime is closed: a cell fails instead of running.
    let after = server.call("tools/call", json!({"name": "rlm", "arguments": {"code": "print(x)"}}));
    assert_eq!(after["result"]["isError"], true, "{after}");
    // A second shutdown (the disconnect after a signal) does nothing.
    server.running.shutdown();
    server.close();
}

#[test]
fn claude_root_mcp_work_of_a_turn_is_charged_to_that_turns_root() {
    let root = Root::new();
    let mut server = Server::start(&[], root.env(&[]));
    let printed = server.cell("import json\nprint(json.dumps((await agents.status())['usage']))");
    let turn = server.control("status", json!({})).unwrap()["turn"]
        .as_str()
        .unwrap()
        .to_string();
    let usage: Value = serde_json::from_str(&printed).unwrap_or_else(|_| panic!("{printed}"));
    assert_eq!(usage["rootId"], format!("turn:{turn}"), "{usage}");
    server.close();
}

/// Loki's one-time setup note (session-worker.ts `setupLoki` -> external-root.ts `lokiNotice`): an `ultron mcp`
/// root in a Git repository without `.loki/` installs and commits it, and Claude Code is told once, with the session
/// context its SessionStart hook adds.
#[test]
fn claude_root_mcp_loki_setup_note_comes_with_the_session_context_once() {
    let root = Root::new();
    let identity = [
        ("GIT_AUTHOR_NAME", "Test User"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test User"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];
    let git = |args: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path("w"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path("h"))
            .envs(identity)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(root.path("w/a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "init"]);
    let mut vars: Vec<(&str, &str)> = identity.to_vec();
    vars.push(("ULTRON_LOKI", "on"));
    let mut server = Server::start(&[], root.env(&vars));
    // The runtime opens with the first cell; the commit follows in the background.
    assert_eq!(server.cell("print(1)"), "1");
    wait_until("the .loki commit", || {
        git(&["log", "-1", "--format=%s"]) == "Add Loki guardrails"
    });
    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let context = server.control("hook", json!({"event": "session-start"})).unwrap();
    let text = context["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        text.ends_with(&format!(
            "Loki set up and committed ({sha}): .loki/ holds the guardrail engine and policy."
        )),
        "{context}"
    );
    // Once: the next user turn does not repeat it.
    let turn = server.control("hook", json!({"event": "user-prompt"})).unwrap();
    assert!(!turn.to_string().contains("Loki set up"), "{turn}");
    server.close();
}

/// The persona's auto-nudge through the hook command's path (`ultron hook stop|user-prompt --socket ...` as the
/// launcher installs it): the server reads the transcript Claude Code names and blocks the stop with the nudge, at
/// most five times until the next prompt; persona off answers nothing.
#[test]
fn claude_root_mcp_persona_stop_hook_nudges_announced_steps_until_the_cap() {
    let nudge = json!({"decision": "block", "reason": resources::persona::NUDGE_TEXT});
    let root = Root::new();
    std::fs::write(root.path("a/settings.json"), json!({"persona": "ultron"}).to_string()).unwrap();
    let transcript = root.path("tmp/transcript.jsonl");
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": "fix it"}}),
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "mcp__ultron__rlm", "input": {}}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "1"}]}}),
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Patched.\n\nNext I'll run the tests."}]}}),
    ];
    std::fs::write(&transcript, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    let env = root.env(&[]);
    let var = env.var.clone();
    let mut server = Server::start(&[], env);
    assert_eq!(server.cell("print(1)"), "1");
    let socket = || Some(PathBuf::from(server.running.socket()));
    let input = json!({"session_id": "c1", "transcript_path": transcript, "stop_hook_active": true}).to_string();
    let hook = |event: &str| {
        let output = claude_root::hooks::run_hook(event, socket(), &input, &*var);
        if output.is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(&output).unwrap()
        }
    };
    for _ in 0..5 {
        assert_eq!(hook("stop"), nudge);
    }
    assert_eq!(hook("stop"), json!({}), "the cap");
    // The user's next prompt starts the count over.
    hook("user-prompt");
    assert_eq!(hook("stop"), nudge);
    // Persona off (read per Stop): nothing.
    std::fs::write(root.path("a/settings.json"), json!({}).to_string()).unwrap();
    hook("user-prompt");
    assert_eq!(hook("stop"), json!({}));
    server.close();
}
