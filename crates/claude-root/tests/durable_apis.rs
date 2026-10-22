//! `ultron claude` serves the durable many-agent APIs and schedules of a native session (crate::durable_apis,
//! crate::schedules): persistent children with `rlm.ask`, the swarm, the task board, shared notes, and scheduled
//! prompts that wake the idle root. TypeScript has none of them (swarm_api.py: "Hosts without these requests (the
//! TypeScript host) answer them with an error"; schedule_api.py likewise), so the expected values are the Rust
//! session's own (session_core::swarm_ops over harness::chat::host_ops), written out literally.
//!
//! Through the server's product entry (`mcp_server::start` and the MCP loop over pipes), a private temp root, and an
//! OpenAI-compatible fake provider on 127.0.0.1 scripted by what each request says. No real Claude Code.

use claude_root::control::ControlClient;
use claude_root::mcp_protocol::serve;
use claude_root::mcp_server::{Running, start};
use serde_json::{Value, json};
use session_core::RunEnv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);
const CHILD_MODEL: &str = "groq/llama-3.3-70b-versatile";

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
        let dir = tempfile::Builder::new().prefix("ucd-").tempdir_in("/tmp").unwrap();
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
            // A persistent child's requests carry its provider's own key (the fake provider ignores it).
            ("GROQ_API_KEY".into(), "fake-key-for-tests".into()),
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

struct Server {
    running: Arc<Running>,
    stdin: Option<std::io::PipeWriter>,
    /// The server's stdout lines, read on a thread of their own so every read has a hang guard.
    stdout: std::sync::mpsc::Receiver<String>,
    serving: Option<std::thread::JoinHandle<()>>,
    next: u64,
}

impl Server {
    /// A root server with in-memory Ultron subagents, the fake provider at `port` as `groq`, `args` after.
    fn start(root: &Root, port: u16, extra: &[(&str, &str)], args: &[&str]) -> Server {
        std::fs::write(
            root.path("a/models.json"),
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
        )
        .unwrap();
        let argv: Vec<&str> = ["--children", "ultron"].iter().chain(args).copied().collect();
        Server::launch(root.env(extra), &argv)
    }

    /// A server on `env` with exactly `argv`.
    fn launch(env: RunEnv, argv: &[&str]) -> Server {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        let (running, handlers) = start(&argv, &claude_root::Ctx { env }).unwrap();
        let (input, stdin) = std::io::pipe().unwrap();
        let (stdout, output) = std::io::pipe().unwrap();
        let serving = std::thread::spawn(move || {
            serve(handlers, &mut BufReader::new(input), Box::new(output));
        });
        let (lines, stdout_lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if lines.send(line).is_err() {
                    return;
                }
            }
        });
        let mut server = Server {
            running,
            stdin: Some(stdin),
            stdout: stdout_lines,
            serving: Some(serving),
            next: 0,
        };
        server.call("initialize", json!({"protocolVersion": "2025-06-18"}));
        server
    }

    /// [`Server::start`] with `--ultron-child-model` naming the fake provider's model.
    fn with_child_model(root: &Root, port: u16, extra: &[(&str, &str)], args: &[&str]) -> Server {
        let mut all = vec!["--ultron-child-model", CHILD_MODEL];
        all.extend(args);
        Server::start(root, port, extra, &all)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        loop {
            let text = self
                .stdout
                .recv_timeout(HANG_GUARD)
                .unwrap_or_else(|e| panic!("no answer to {method} within the hang guard: {e}"));
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    fn cell(&mut self, code: &str) -> String {
        let reply = self.call("tools/call", json!({"name": "rlm", "arguments": {"code": code}}));
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// `code` in a cell, printing the exception's text if it raises.
    fn attempt(&mut self, code: &str) -> String {
        self.cell(&format!(
            "try:\n    {code}\n    print('accepted')\nexcept Exception as e:\n    print(e)"
        ))
    }

    fn control(&self, op: &str, fields: Value) -> Result<Value, String> {
        let mut client = ControlClient::connect(Path::new(&self.running.socket()), HANG_GUARD)?;
        client.request(op, fields.as_object().cloned().unwrap_or_default(), HANG_GUARD)
    }

    fn hook(&self, event: &str) {
        self.control("hook", json!({"event": event})).unwrap();
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

/// What the fake provider answers: text, or one `rlm` call with this code.
enum Reply {
    Text(String),
    Cell(String),
}

type Requests = Arc<Mutex<Vec<Value>>>;
type Script = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// An OpenAI-compatible chat-completions endpoint on 127.0.0.1 answering by `script`; records the request bodies.
fn fake_provider(script: Script) -> (u16, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Requests = Arc::default();
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (seen, script) = (seen.clone(), script.clone());
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
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                seen.lock().unwrap().push(body.clone());
                let chunk = |delta: Value, finish: Value| {
                    format!(
                        "data: {}\n\n",
                        json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "m",
                            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}})
                    )
                };
                let sse = match script(&body) {
                    Reply::Text(text) => {
                        chunk(json!({"role": "assistant", "content": text}), Value::Null)
                            + &chunk(json!({}), json!("stop"))
                    }
                    Reply::Cell(code) => {
                        let call = json!({"index": 0, "id": "call_1", "type": "function",
                            "function": {"name": "rlm", "arguments": json!({"code": code}).to_string()}});
                        chunk(json!({"role": "assistant", "tool_calls": [call]}), Value::Null)
                            + &chunk(json!({}), json!("tool_calls"))
                    }
                } + "data: [DONE]\n\n";
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

/// The text of the request's last message (what the model is answering).
fn last_message(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map(Value::to_string)
        .unwrap_or_default()
}

fn has_tool_result(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m["role"] == "tool")
}

/// [`wait_events`] on a thread of its own, joined with a hang guard.
struct Waiter(std::sync::mpsc::Receiver<(i32, String)>);

impl Waiter {
    fn start(socket: &str) -> Waiter {
        let (done, result) = std::sync::mpsc::channel();
        let socket = socket.to_string();
        std::thread::spawn(move || {
            let _ = done.send(wait_events(&socket));
        });
        Waiter(result)
    }

    fn join(self) -> (i32, String) {
        self.0
            .recv_timeout(HANG_GUARD)
            .expect("the wait-events waiter returned within the hang guard")
    }
}

/// `ultron hook wait-events` against `socket`: its exit code and stderr.
fn wait_events(socket: &str) -> (i32, String) {
    let args: Vec<String> = ["wait-events", "--socket", socket]
        .iter()
        .map(|a| a.to_string())
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = claude_root::hooks::run(&args, &mut std::io::empty(), true, &|_| None, &mut out, &mut err);
    (code, String::from_utf8(err).unwrap())
}

/// A persistent child answers its brief, keeps its conversation for `rlm.ask`, and its model turns are charged to
/// the session's ledger.
fn kept_child_script() -> Script {
    Arc::new(|body: &Value| {
        let last = last_message(body);
        if last.contains("QUESTION-2") {
            // The follow-up is asked in the same conversation: the first answer is in it.
            let saw = body["messages"].to_string().contains("parser lives in src/parse.rs");
            return Reply::Text(format!("second answer (saw the first: {saw})"));
        }
        Reply::Text("parser lives in src/parse.rs".into())
    })
}

#[test]
fn claude_root_keep_children_answer_follow_ups_with_rlm_ask_and_are_charged_to_the_ledger() {
    let root = Root::new();
    let (port, requests) = fake_provider(kept_child_script());
    let mut server = Server::with_child_model(&root, port, &[], &[]);
    let id = server
        .cell("h = await rlm.spawn('KEEP-BRIEF: find the parser', name='parser', keep=True)\nprint(h.rlm_child_id)");
    let first = server.cell(
        "r = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['status'], r[0]['result']['value'])",
    );
    assert_eq!(first, "succeeded parser lives in src/parse.rs");
    let asked = server
        .cell("a = await rlm.ask(h, 'QUESTION-2: which file?', timeout=120)\nprint(a['id'], a['status'], a['value'])");
    assert_eq!(asked, format!("{id} succeeded second answer (saw the first: true)"));
    // The child runs on the server's Ultron child model.
    let bodies = requests.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    assert!(
        bodies.iter().all(|b| b["model"] == "llama-3.3-70b-versatile"),
        "{bodies:?}"
    );
    // A subagent's system prompt, not the root's.
    let system = bodies[0]["messages"][0].to_string();
    assert!(system.contains("If you are a subagent"), "{system}");
    // Both responses were charged to the session's ledger (its default root), with their tokens.
    let opened = server.running.opened().unwrap();
    let status = opened.lane.stack.usage.status(None).unwrap();
    assert_eq!(
        (status["turns"]["turns"].clone(), status["turns"]["tokens"].clone()),
        (json!(2), json!(14))
    );
    // The child is still alive: another question queues behind nothing and is answered.
    let again = server.cell("a = await rlm.ask(h, 'QUESTION-2 again', timeout=120)\nprint(a['status'])");
    assert_eq!(again, "succeeded");
    // An ordinary handle cannot be asked.
    let ordinary = server.attempt("await rlm.ask('999', 'more?', timeout=120)");
    assert_eq!(ordinary, "Unknown subagent 999");
    server.close();
}

#[test]
fn claude_root_board_notes_and_swarm_are_shared_by_the_root_and_its_kept_children() {
    let root = Root::new();
    let child_cell = "print(await board.add('fix lexer', detail='from the child'))\nprint(await notes.append('child note'))\nprint(await swarm.send('root', 'lexer claimed', wake=True))";
    let (port, _requests) = fake_provider(Arc::new(move |body: &Value| {
        if has_tool_result(body) {
            Reply::Text("child done".into())
        } else {
            Reply::Cell(child_cell.into())
        }
    }));
    let mut server = Server::with_child_model(&root, port, &[], &["--wake"]);
    let socket = server.running.socket();
    server.hook("user-prompt");
    let added = server.cell("t = await board.add('write docs')\nprint(t['id'], t['status'], t['by'])");
    assert_eq!(added, "1 open root");
    let done = server.cell("h = await rlm.spawn('KEEP-BRIEF: claim work', name='lexer', keep=True)\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['value'])");
    assert!(done.ends_with("child done"), "{done}");
    // The child's wake reached Claude Code's inbox (instead of a turn of the runtime's root conversation): with the
    // cell's result, or once the events' coalescing window closes.
    let opened = server.running.opened().unwrap();
    let woke = if done.contains("<swarm_message") {
        done.clone()
    } else {
        wait_until("the wake reaches the root's inbox", || opened.root.pending_events() > 0);
        opened.root.take_events().unwrap()
    };
    assert!(
        woke.contains("<swarm_message from=\"lexer\" to=\"root\">\nlexer claimed\n</swarm_message>"),
        "{woke}"
    );
    // The child's task and notes are the session's: the root reads them.
    let board = server.cell("print([(t['id'], t['title'], t['by']) for t in await board.list()])");
    assert_eq!(board, "[(1, 'write docs', 'root'), (2, 'fix lexer', 'lexer')]");
    let claimed = server.cell("t = await board.claim(2)\nprint(t['status'], t['owner'])");
    assert_eq!(claimed, "claimed root");
    let notes = server.cell("n = await notes.read()\nprint(repr(n['text']), n['version'])");
    assert_eq!(notes, "'[lexer] child note\\n' 1");
    let agents = server.cell("print(sorted(await swarm.agents()))");
    assert_eq!(agents, "['lexer']");
    // The child's message is in the root's swarm inbox too.
    let inbox = server.cell("print([(m['from'], m['text']) for m in await swarm.inbox()])");
    assert!(inbox.ends_with("[('lexer', 'lexer claimed')]"), "{inbox}");
    // A wake while Claude Code is idle wakes it through the wait-events hook.
    server.hook("stop");
    let sent = server.cell("print(await swarm.send('root', 'note to self', wake=True))");
    assert!(sent.ends_with("{'id': 2, 'woke': True}"), "{sent}");
    server.hook("stop");
    let (code, woken) = Waiter::start(&socket).join();
    assert_eq!(code, 2, "{woken}");
    assert!(
        woken.contains("<swarm_message from=\"root\" to=\"root\">\nnote to self\n</swarm_message>"),
        "{woken}"
    );
    server.close();
}

#[test]
fn claude_root_kept_children_and_the_board_survive_a_restart_of_the_claude_session() {
    let root = Root::new();
    let (port, _requests) = fake_provider(kept_child_script());
    let session = [("CLAUDE_CODE_SESSION_ID", "claude-session-1")];
    let mut server = Server::with_child_model(&root, port, &session, &[]);
    let id = server.cell("h = await rlm.spawn('KEEP-BRIEF: find the parser', name='parser', keep=True)\nr = await rlm.collect([h], timeout_ms=120000)\nprint(h.rlm_child_id)");
    assert_eq!(server.cell("print((await board.add('carry over'))['id'])"), "1");
    server.close();
    // `claude --resume`: the same Claude Code session reopens the same Ultron session and its durable runtime.
    let mut server = Server::with_child_model(&root, port, &session, &[]);
    let board = server.cell("print([t['title'] for t in await board.list()])");
    // (The kernel restarted from its checkpoint: the note after the output says what it kept.)
    assert!(board.starts_with("['carry over']\n"), "{board}");
    let asked = server.cell(&format!(
        "a = await rlm.ask('{id}', 'QUESTION-2: which file?', timeout=120)\nprint(a['status'], a['value'])"
    ));
    assert_eq!(asked, "succeeded second answer (saw the first: true)");
    server.close();
}

#[test]
fn claude_root_durable_apis_refusals_name_what_works_instead() {
    let root = Root::new();
    let (port, requests) = fake_provider(Arc::new(|body: &Value| {
        if has_tool_result(body) {
            Reply::Text("child done".into())
        } else {
            Reply::Cell("for attempt in [rlm.spawn('b', name='k', keep=True), rlm.ask('1', 'q', timeout=5), schedule.list()]:\n    try:\n        await attempt\n    except Exception as e:\n        print('refused:', e)".into())
        }
    }));
    // No Ultron model to run a persistent child on (no --ultron-child-model, no default model, no rlm.childModel).
    let mut server = Server::start(&root, port, &[], &[]);
    assert_eq!(
        server.attempt("await rlm.spawn('brief', name='kept', keep=True)"),
        "rlm.spawn(keep=True) needs an Ultron model in ultron claude (a Claude Code subagent cannot take follow-ups): name one with model=\"provider/model\", or set one with --ultron-child-model, claudeCode.ultronChildModel or rlm.childModel"
    );
    assert_eq!(
        server.attempt("await rlm.spawn('brief', name='kept', keep=True, model='claude-code/sonnet')"),
        "rlm.spawn(keep=True) cannot run on claude-code/sonnet: a persistent child in ultron claude runs on an Ultron model (a Claude Code model has no rlm tool and cannot take follow-ups); name one with model=\"provider/model\""
    );
    // Without a default, a spawn naming its model runs.
    let named = server.cell(&format!(
        "h = await rlm.spawn('KEEP-BRIEF', name='named', keep=True, model='{CHILD_MODEL}')\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['status'])"
    ));
    assert_eq!(named, "succeeded");
    // An in-memory subagent has no conversation in the durable runtime: persistent children and schedules are
    // refused to it (its swarm, board and notes: claude_root_in_memory_subagents_share_the_board_notes_and_swarm).
    let child = server.cell("h = await rlm.spawn('CHILD-BRIEF: add a task', name='plain', model='groq/llama-3.3-70b-versatile')\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['value'])");
    assert_eq!(child, "child done");
    let result = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|b| has_tool_result(b) && b["messages"].to_string().contains("CHILD-BRIEF"))
        .map(|b| b["messages"].to_string())
        .next()
        .unwrap();
    for refused in [
        "refused: rlm.spawn(keep=True) is not available to a subagent in ultron claude: persistent children belong to the root and its persistent children; use swarm.send, board and notes to reach them, and put what the root needs in your answer",
        "refused: rlm.ask is not available to a subagent in ultron claude: persistent children belong to the root and its persistent children; use swarm.send, board and notes to reach them, and put what the root needs in your answer",
        "refused: schedule.list is available to the root agent only",
    ] {
        assert!(result.contains(refused), "{refused}: {result}");
    }
    server.close();
}

#[test]
fn claude_root_a_claude_subagents_server_refuses_persistent_children_and_schedules() {
    use claude_root::unavailable::child_refusal;
    let payload = |value: Value| value.as_object().cloned().unwrap();
    for (kind, body, api) in [
        (
            "rlm.spawn",
            json!({"prompt": "p", "kwargs": {"name": "n", "keep": true}}),
            "rlm.spawn(keep=True)",
        ),
        (
            "rlm.spawn",
            json!({"prompt": "p", "keep": true}),
            "rlm.spawn(keep=True)",
        ),
        ("rlm.ask", json!({"child": "1", "question": "q"}), "rlm.ask"),
    ] {
        assert_eq!(
            child_refusal(kind, &payload(body)),
            Some(format!(
                "{api} is not available to a subagent in ultron claude: persistent children belong to the root and its persistent children; use swarm.send, board and notes to reach them, and put what the root needs in your answer"
            )),
            "{kind}"
        );
    }
    for kind in ["schedule.every", "schedule.at"] {
        assert_eq!(
            child_refusal(kind, &payload(json!({}))),
            Some(format!("{kind} is available to the root agent only"))
        );
    }
    for (kind, body) in [
        ("rlm.spawn", json!({"prompt": "p", "kwargs": {"name": "n"}})),
        ("schedule.list", json!({})),
        ("rlm.collect", json!({"selectors": ["1"]})),
        // Forwarded to the root (crate::shared_world).
        ("board.add", json!({"title": "t"})),
        ("notes.append", json!({"text": "t"})),
        ("swarm.route", json!({"query": "q"})),
    ] {
        assert_eq!(child_refusal(kind, &payload(body)), None, "{kind}");
    }
}

#[test]
fn claude_root_a_due_schedule_wakes_the_idle_claude_root_with_its_prompt() {
    let root = Root::new();
    let clock = root.path("clock");
    std::fs::write(&clock, "1000").unwrap();
    let clock_env = [(kernel_apis::prompt_schedule::CLOCK_FILE_ENV, clock.to_str().unwrap())];
    let (port, _requests) = fake_provider(Arc::new(|_: &Value| Reply::Text("unused".into())));
    let mut server = Server::start(&root, port, &clock_env, &["--wake"]);
    let socket = server.running.socket();
    server.hook("user-prompt");
    let id = server.cell("print(await schedule.at(30, 'check the build', name='build'))");
    assert!(id.starts_with("sch-"), "{id}");
    let opened = server.running.opened().unwrap();
    // A waiter stays while a schedule is pending (it does not end after the idle grace).
    assert!(opened.schedules_pending());
    server.hook("stop");
    let waiter = Waiter::start(&socket);
    wait_until("the waiter waits", || server.running.event_waiters() == 1);
    // The schedule's time comes: it fires into the root's inbox and the waiter wakes the model with its prompt.
    // (30 s, shorter than the retry delay: a timer that tried early would miss it.)
    std::fs::write(&clock, "31000").unwrap();
    let (code, woken) = waiter.join();
    assert_eq!(code, 2, "{woken}");
    assert!(
        woken.contains(&format!(
            "<scheduled_prompt schedule=\"build\" id=\"{id}\" run=\"1\">\nThis session's schedule fired: do this now as a new task.\ncheck the build\n</scheduled_prompt>"
        )),
        "{woken}"
    );
    // The run is recorded: a one-time schedule is done, and nothing is pending any more.
    let listed = server.cell("s = (await schedule.list())[0]\nprint(s['runs'], s['state'])");
    assert_eq!(listed, "1 done");
    assert!(!opened.schedules_pending());
    server.close();
}

#[test]
fn claude_root_an_every_schedule_fires_each_slot_once_and_cancel_stops_it() {
    let root = Root::new();
    let clock = root.path("clock");
    std::fs::write(&clock, "0").unwrap();
    let clock_env = [(kernel_apis::prompt_schedule::CLOCK_FILE_ENV, clock.to_str().unwrap())];
    let (port, _requests) = fake_provider(Arc::new(|_: &Value| Reply::Text("unused".into())));
    let mut server = Server::start(&root, port, &clock_env, &[]);
    let id = server.cell("print(await schedule.every('1m', 'poll ci', name='ci'))");
    let opened = server.running.opened().unwrap();
    let runs = |server: &mut Server| server.cell("print((await schedule.list())[0]['runs'])");
    // Three slots missed at once collapse into one run.
    std::fs::write(&clock, "200000").unwrap();
    wait_until("the first fire", || opened.root.pending_events() > 0);
    let first = opened.root.take_events().unwrap();
    assert_eq!(
        first,
        format!(
            "<scheduled_prompt schedule=\"ci\" id=\"{id}\" run=\"1\">\nThis session's schedule fired: do this now as a new task.\npoll ci\n</scheduled_prompt>"
        )
    );
    assert_eq!(runs(&mut server), "1");
    std::fs::write(&clock, "240000").unwrap();
    wait_until("the second fire", || opened.root.pending_events() > 0);
    assert!(opened.root.take_events().unwrap().contains("run=\"2\""));
    assert_eq!(
        server.cell("print((await schedule.cancel('ci'))['state'])"),
        "cancelled"
    );
    assert!(!opened.schedules_pending());
    server.close();
}

#[test]
fn claude_root_guide_names_persistent_agents_and_schedules_for_the_root_only() {
    let apis = claude_root::guide::external_root_apis(resources::runtime_guide::RuntimeGuideOptions {
        durable_apis: true,
        ..Default::default()
    });
    let root = claude_root::guide::claude_runtime_guide_for(Some(&apis), false, true);
    for wanted in [
        "`h = await rlm.spawn(brief, name=..., keep=True)`",
        "`await rlm.ask(h, question)`",
        "`await swarm.route(question)`",
        "`board` (tasks with exclusive claims) and `notes`",
        "`await schedule.every(\"1h\", prompt)`",
    ] {
        assert!(root.contains(wanted), "{wanted}: {root}");
    }
    // Claude Code owns its context: no `ctx`.
    assert!(!root.contains("`ctx` edits"), "{root}");
    // A Claude Code subagent's server serves neither (crate::unavailable::child_refusal).
    let child = claude_root::guide::claude_system_prompt(
        claude_root::guide::Audience::ClaudeChild,
        &claude_root::guide::GuideOptions {
            apis: Some(&apis),
            ..Default::default()
        },
        false,
    );
    for unwanted in ["keep=True", "rlm.ask", "swarm.route", "schedule.every"] {
        assert!(!child.contains(unwanted), "{unwanted}: {child}");
    }
    assert!(child.contains("Contracts: `rlm.spawn(brief"), "{child}");
}

/// A typed `rlm` definition (`summ@1`); `model`, when given, is the definition's.
fn summ_definition(id: &str, model: Option<&str>) -> String {
    let mut definition = json!({
        "id": id, "version": "1", "strategy": "rlm", "instructions": "TYPED-SUMMARY: summarize the text.",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"],
                        "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"],
                         "additionalProperties": false},
        "maxRepairs": 0, "inputDescription": "{text:string}", "outputDescription": "{answer:string}",
    });
    if let Some(model) = model {
        definition["model"] = json!(model);
    }
    definition.to_string()
}

/// The typed agent answers with the model id its request went to.
fn typed_script() -> Script {
    Arc::new(|body: &Value| {
        Reply::Text(json!({"answer": format!("on {}", body["model"].as_str().unwrap_or_default())}).to_string())
    })
}

#[test]
fn claude_root_typed_rlm_agents_run_on_the_durable_runtime_on_the_request_then_definition_then_child_model() {
    let root = Root::new();
    let (port, requests) = fake_provider(typed_script());
    let mut server = Server::with_child_model(&root, port, &[], &[]);
    let registered = server.cell(&format!(
        "import json\nawait agents.register(json.loads({:?}))\nawait agents.register(json.loads({:?}))\nprint('ok')",
        summ_definition("summ", None),
        summ_definition("named", Some("groq/openai/gpt-oss-20b"))
    ));
    assert_eq!(registered, "ok");
    let invoke = |server: &mut Server, definition: &str, model: Option<&str>| {
        let model = model.map_or("None".to_string(), |m| format!("'{m}'"));
        server.cell(&format!(
            "r = await agents.invoke('{definition}', {{'text': 'x'}}, model={model})\nprint(r)"
        ))
    };
    // Neither names a model: the root's Ultron child model.
    let plain = invoke(&mut server, "summ@1", None);
    let succeeded = |model: &str| {
        format!("{{'status': 'succeeded', 'value': {{'answer': 'on {model}'}}, 'verification': 'unverified'}}")
    };
    // (Results end with the cell's output; an earlier run's end may come first.)
    assert!(plain.ends_with(&succeeded("llama-3.3-70b-versatile")), "{plain}");
    // The request's model wins over the definition's, the definition's over the child model.
    let requested = invoke(&mut server, "named@1", Some("groq/llama-3.1-8b-instant"));
    assert!(requested.ends_with(&succeeded("llama-3.1-8b-instant")), "{requested}");
    let defined = invoke(&mut server, "named@1", None);
    assert!(defined.ends_with(&succeeded("openai/gpt-oss-20b")), "{defined}");
    let bodies = requests.lock().unwrap().clone();
    let models: Vec<&str> = bodies.iter().map(|b| b["model"].as_str().unwrap()).collect();
    assert_eq!(
        models,
        ["llama-3.3-70b-versatile", "llama-3.1-8b-instant", "openai/gpt-oss-20b"]
    );
    // The lane is a subagent: its system prompt is a subagent's, its prompt the definition's.
    assert!(bodies[0]["messages"][0].to_string().contains("If you are a subagent"));
    assert!(last_message(&bodies[0]).contains("TYPED-SUMMARY"));
    server.close();
}

/// `text` (a cell result) ends with `wanted`: an earlier run's end may come before the cell's output.
fn assert_ends(text: &str, wanted: &str) {
    assert!(text.ends_with(wanted), "{wanted:?} not at the end of {text:?}");
}

/// Register `definition`, run it once on `model=` (an Ultron model), retain the task and invoke the instance again:
/// the follow-up's outcome.
fn retained_follow_up(server: &mut Server, definition: &str, model: &str) -> String {
    let id = serde_json::from_str::<Value>(definition).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    server.cell(&format!(
        "import json\nawait agents.register(json.loads({definition:?}))\nt = await agents.spawn('{id}@1', {{'text': 'x'}}, model='{model}')\nprint((await t.result())['status'])\ni = await instances.retain(t.id)\nn = await instances.invoke(i['id'], {{'text': 'again'}})\nr = await agents.result(n['task_id'])\nprint(r['status'], r.get('error'))"
    ))
}

#[test]
fn claude_root_a_retained_typed_agent_cannot_continue_on_a_claude_code_model() {
    let root = Root::new();
    let (port, requests) = fake_provider(typed_script());
    // The definition names a Claude Code model: its first run on a named Ultron model works, its follow-up cannot.
    let mut server = Server::with_child_model(&root, port, &[], &[]);
    // On an Ultron model the instance continues its lane's conversation.
    let kept = retained_follow_up(&mut server, &summ_definition("kept", None), CHILD_MODEL);
    assert!(kept.ends_with("succeeded\nsucceeded None"), "{kept}");
    let follow_up = requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        follow_up["messages"].to_string().contains("on llama-3.3-70b-versatile"),
        "{follow_up}"
    );
    let outcome = retained_follow_up(
        &mut server,
        &summ_definition("claudy", Some("claude-code/sonnet")),
        CHILD_MODEL,
    );
    assert_ends(
        &outcome,
        "succeeded\nfailed claudy@1 cannot continue on claude-code/sonnet in ultron claude: a retained instance takes follow-ups in its lane's conversation, which a Claude Code model cannot (it has no rlm tool and cannot take follow-ups); name an Ultron model in the definition (\"model\": \"provider/model\")",
    );
    server.close();
    // Under `--children claude` the root's child model is its Claude Code children's: a definition naming none
    // continues on it, which is refused the same way (the first run, on a named model, ran on the runtime).
    let root = Root::new();
    let claude = root.path("h/claude");
    std::fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
    let bin = [("ULTRON_CLAUDE_BIN", claude.to_str().unwrap())];
    let mut server = Server::start(&root, port, &bin, &["--children", "claude", "--child-model", "sonnet"]);
    let outcome = retained_follow_up(&mut server, &summ_definition("plain", None), CHILD_MODEL);
    assert_ends(
        &outcome,
        "succeeded\nfailed plain@1 cannot continue on claude-code/sonnet in ultron claude: a retained instance takes follow-ups in its lane's conversation, which a Claude Code model cannot (it has no rlm tool and cannot take follow-ups); name an Ultron model in the definition (\"model\": \"provider/model\")",
    );
    // Both first runs reached the fake provider on the named model; neither refused follow-up did.
    let bodies = requests.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4, "{bodies:?}");
    server.close();
}

#[test]
fn claude_root_in_memory_subagents_share_the_board_notes_and_swarm_with_the_root_and_kept_children() {
    let root = Root::new();
    let scout_cell = "print((await board.add('fix lexer', detail='from scout'))['by'])\nprint(await notes.append('scout note'))\nprint(await swarm.send('root', 'scout says hi', wake=True))\nprint(await swarm.send('lexer', 'look at the lexer', wake=True))\nprint(await swarm.register(topics=['lexing']))\nprint([(m['from'], m['text']) for m in await swarm.inbox()])";
    let (port, requests) = fake_provider(Arc::new(move |body: &Value| {
        let last = last_message(body);
        if last.contains("Message from scout: look at the lexer") {
            Reply::Text("lexer heard scout".into())
        } else if body["messages"].to_string().contains("SCOUT-BRIEF") {
            if has_tool_result(body) {
                Reply::Text("scout done".into())
            } else {
                Reply::Cell(scout_cell.into())
            }
        } else {
            Reply::Text("lexer ready".into())
        }
    }));
    let mut server = Server::with_child_model(&root, port, &[], &[]);
    let kept = server.cell("k = await rlm.spawn('KEEP-BRIEF: own the lexer', name='lexer', keep=True)\nr = await rlm.collect([k], timeout_ms=120000)\nprint(r[0]['result']['value'])\nawait swarm.send('scout', 'welcome scout')");
    assert!(kept.contains("lexer ready"), "{kept}");
    let scouted = server.cell("h = await rlm.spawn('SCOUT-BRIEF: look around', name='scout')\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['value'])");
    assert!(scouted.contains("scout done"), "{scouted}");
    // What the scout's cell printed: everything is by the scout, its own inbox is the scout's.
    let result = requests
        .lock()
        .unwrap()
        .iter()
        .filter(|b| has_tool_result(b) && b["messages"].to_string().contains("SCOUT-BRIEF"))
        .map(|b| b["messages"].to_string())
        .next()
        .unwrap();
    for printed in [
        "scout\\n",
        "\\n{'version': 1}\\n",
        "{'id': 2, 'woke': True}",
        "{'id': 3, 'woke': True}",
        "'name': 'scout'",
        "[('root', 'welcome scout')]",
    ] {
        assert!(result.contains(printed), "{printed}: {result}");
    }
    // The root reads the scout's task and note, and its wake reached Claude Code's inbox.
    let board = server.cell("print([(t['title'], t['by']) for t in await board.list()])");
    assert!(board.ends_with("[('fix lexer', 'scout')]"), "{board}");
    let notes = server.cell("print(repr((await notes.read())['text']))");
    assert!(notes.ends_with("'[scout] scout note\\n'"), "{notes}");
    // The wake arrives with a cell result after it (before the result's output), or waits in the inbox.
    let woke = root_inbox(&server, [scouted.as_str(), board.as_str(), notes.as_str()]);
    assert!(
        woke.contains("<swarm_message from=\"scout\" to=\"root\">\nscout says hi\n</swarm_message>"),
        "{woke}"
    );
    // The kept child was woken by the scout's message: its next turn answers it.
    wait_until("the kept child answers the scout", || {
        requests
            .lock()
            .unwrap()
            .iter()
            .any(|b| last_message(b).contains("Message from scout: look at the lexer"))
    });
    let agents = server.cell("a = await swarm.agents()\nprint(sorted(a), a['scout']['topics'])");
    assert!(agents.ends_with("['lexer', 'scout'] ['lexing']"), "{agents}");
    server.close();
}

/// What reached the root's inbox: the cell results `seen` since it was sent, else (once it arrives) the inbox.
fn root_inbox<const N: usize>(server: &Server, seen: [&str; N]) -> String {
    let seen = seen.concat();
    if seen.contains("<swarm_message") {
        return seen;
    }
    let opened = server.running.opened().unwrap();
    wait_until("the wake reaches the root's inbox", || opened.root.pending_events() > 0);
    opened.root.take_events().unwrap()
}

/// A FAKE `claude -p` (never the real CLI): it records the subagent server its MCP config names (token, parent
/// socket and lane) and its brief in `<dir>/child.json`, then waits until `<dir>/go` (a FIFO) is opened and closed
/// before it prints its stream-json result. The test runs the subagent's `ultron mcp --child` server itself, in
/// process, from that record.
fn fake_claude(dir: &Path) -> PathBuf {
    let script = dir.join("claude");
    let text = format!(
        r#"#!{python}
import json, os, sys
args = sys.argv[1:]
server = json.loads(args[args.index('--mcp-config') + 1])['mcpServers']['ultron']
def flag(name):
    return server['args'][server['args'].index(name) + 1]
info = {{'token': server['env']['ULTRON_PARENT_TOKEN'], 'socket': flag('--parent-socket'), 'lane': flag('--parent-lane'), 'prompt': sys.stdin.read()}}
os.mkfifo('{dir}/go')
with open('{dir}/child.json.tmp', 'w') as f:
    json.dump(info, f)
os.rename('{dir}/child.json.tmp', '{dir}/child.json')
with open('{dir}/go') as f:
    f.read()
print(json.dumps({{'type': 'result', 'result': 'claude child done', 'num_turns': 1, 'usage': {{'input_tokens': 1, 'output_tokens': 1}}}}))
"#,
        python = python(),
        dir = dir.display()
    );
    std::fs::write(&script, text).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn claude_root_a_claude_subagents_server_forwards_the_shared_world_to_the_root() {
    let root = Root::new();
    let fake = root.path("tmp");
    let claude = fake_claude(&fake);
    let (port, _requests) = fake_provider(Arc::new(|body: &Value| {
        if has_tool_result(body) {
            Reply::Text("deep done".into())
        } else {
            Reply::Cell("print((await board.add('deep task'))['by'])".into())
        }
    }));
    let bin = [("ULTRON_CLAUDE_BIN", claude.to_str().unwrap())];
    let mut server = Server::with_child_model(&root, port, &bin, &["--children", "claude"]);
    let spawned = server.cell("h = await rlm.spawn('CLAUDE-BRIEF: help out', name='scout')\nprint(h.rlm_child_id)");
    let record = fake.join("child.json");
    wait_until("the fake claude records its server", || record.exists());
    let child: Value = serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    assert!(child["prompt"].as_str().unwrap().contains("CLAUDE-BRIEF"), "{child}");
    assert_eq!(child["lane"], format!("ultron.rlm-child.{spawned}"));
    // The subagent's own `ultron mcp --child` server, as Claude Code would start it from its MCP config.
    let token = child["token"].as_str().unwrap().to_string();
    let socket = child["socket"].as_str().unwrap().to_string();
    let lane = child["lane"].as_str().unwrap().to_string();
    let mut sub = Server::launch(
        root.env(&[("ULTRON_PARENT_TOKEN", &token)]),
        &[
            "--child",
            "--parent-socket",
            &socket,
            "--parent-lane",
            &lane,
            "--children",
            "ultron",
            "--ultron-child-model",
            CHILD_MODEL,
        ],
    );
    assert_eq!(sub.cell("print((await board.add('claude task'))['by'])"), "scout");
    assert_eq!(sub.cell("print(await notes.append('claude note'))"), "{'version': 1}");
    assert_eq!(
        sub.cell("print(await swarm.send('root', 'claude says hi', wake=True))"),
        "{'id': 1, 'woke': True}"
    );
    // Its own in-memory subagent goes by its own name, through its server and the parent's.
    let deep = sub.cell("h = await rlm.spawn('DEEP-BRIEF', name='deep')\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['value'])");
    assert_eq!(deep, "deep done");
    // Persistent children stay the root's.
    assert_ends(
        &sub.attempt("await rlm.ask('1', 'q', timeout=5)"),
        "rlm.ask is not available to a subagent in ultron claude: persistent children belong to the root and its persistent children; use swarm.send, board and notes to reach them, and put what the root needs in your answer",
    );
    // The root sees the subagent's work, by its spawn names, and its wake in Claude Code's inbox.
    let board = server.cell("print([(t['title'], t['by']) for t in await board.list()])");
    assert!(
        board.ends_with("[('claude task', 'scout'), ('deep task', 'deep')]"),
        "{board}"
    );
    let notes = server.cell("print(repr((await notes.read())['text']))");
    assert!(notes.ends_with("'[scout] claude note\\n'"), "{notes}");
    let woke = root_inbox(&server, [board.as_str(), notes.as_str()]);
    assert!(
        woke.contains("<swarm_message from=\"scout\" to=\"root\">\nclaude says hi\n</swarm_message>"),
        "{woke}"
    );
    // Only a running subagent's token is accepted.
    assert_eq!(
        server.control("shared", json!({"token": "not-a-token", "kind": "board.list"})),
        Err("the session's shared world: unknown subagent".to_string())
    );
    sub.close();
    // The fake claude prints its result: the subagent's answer.
    drop(std::fs::OpenOptions::new().write(true).open(fake.join("go")).unwrap());
    let answer = server.cell("r = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['value'])");
    assert!(answer.ends_with("claude child done"), "{answer}");
    server.close();
}

#[test]
fn claude_root_schedules_survive_claude_resume_and_the_missed_slots_fire_once() {
    let root = Root::new();
    let clock = root.path("clock");
    std::fs::write(&clock, "0").unwrap();
    let env = [
        (kernel_apis::prompt_schedule::CLOCK_FILE_ENV, clock.to_str().unwrap()),
        ("CLAUDE_CODE_SESSION_ID", "claude-session-schedules"),
    ];
    let (port, _requests) = fake_provider(Arc::new(|_: &Value| Reply::Text("unused".into())));
    let mut server = Server::start(&root, port, &env, &[]);
    let id = server.cell("print(await schedule.every('1m', 'poll ci', name='ci'))");
    assert!(id.starts_with("sch-"), "{id}");
    let session_path = server.running.opened().unwrap().session_path.clone();
    server.close();
    // The schedule is kept in the session, where the Ultron server's timer does not look: it never fires a Claude
    // Code session's schedule as a native turn.
    assert!(
        std::fs::read_to_string(&session_path)
            .unwrap()
            .contains("ultron.claude.schedule")
    );
    assert!(daemon::prompt_schedules::scan_journal(&session_path).is_empty());
    // Claude Code is down for five slots; `claude --resume` reopens the session, and its schedule.
    std::fs::write(&clock, "300000").unwrap();
    let mut server = Server::start(&root, port, &env, &[]);
    let opened = server.running.opened().unwrap();
    assert_eq!(opened.session_path, session_path);
    wait_until("the missed slots fire", || opened.root.pending_events() > 0);
    assert_eq!(
        opened.root.take_events().unwrap(),
        format!(
            "<scheduled_prompt schedule=\"ci\" id=\"{id}\" run=\"1\">\nThis session's schedule fired: do this now as a new task.\npoll ci\n</scheduled_prompt>"
        )
    );
    // One run for the five missed slots; the next slot is after now.
    let listed = server.cell("s = (await schedule.list())[0]\nprint(s['id'], s['runs'], s['state'])");
    // (The kernel restarted from its checkpoint: the note after the output says what it kept.)
    assert!(listed.starts_with(&format!("{id} 1 active\n")), "{listed}");
    assert!(opened.schedules_pending());
    std::fs::write(&clock, "360000").unwrap();
    wait_until("the next slot fires", || opened.root.pending_events() > 0);
    assert!(opened.root.take_events().unwrap().contains("run=\"2\""));
    server.close();
}
