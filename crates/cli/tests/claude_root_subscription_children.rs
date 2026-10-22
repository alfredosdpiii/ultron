//! Under `ultron claude` (`ultron mcp`, the server Claude Code runs), every Ultron child that names no model runs on
//! the Claude root's own subscription model, never on the settings' default model: a persistent child
//! (`rlm.spawn(keep=True)`) and its follow-ups (`rlm.ask`), and a retained typed agent's next invocation. A
//! `claude-code/<id>` request that declares `rlm` runs on the Claude subscription only
//! (ai::claude_subscription::claude_code_lane), inside the child's own Ultron transcript, so follow-ups replay the
//! earlier turns.
//!
//! The Rust binary, the real Python kernel, and the ai crate's FAKE `claude` (crates/ai/fixtures/claude-subscription;
//! named by ULTRON_CLAUDE_CODE_BIN, never the real CLI; PATH is only /usr/bin:/bin) answering offline
//! (FAKE_OFFLINE_BLOCKS) and logging each run (FAKE_LOG). The settings name a paid default model and a paid
//! `rlm.childModel` whose provider is a loopback listener that must never be reached. Rust-only behavior (the
//! TypeScript `ultron claude` refuses persistent children), so the expectations are literal.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// Hang guard only: every wait below ends on an answer.
const HANG_GUARD: Duration = Duration::from_secs(120);
/// What the fake subscription answers every run with (valid typed-agent output too).
const ANSWER: &str = r#"{"answer": "subscribed"}"#;
/// `claude-code/claude-opus-5-5` as the subscription runs it (its pinned 1M window).
const OPUS_NATIVE: &str = "claude-opus-5-5[1m]";

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

struct Server {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    next: u64,
}

impl Server {
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        loop {
            let text = self
                .lines
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
}

/// One control request to the server's socket.
fn inspect(socket: &Path, request: &str) -> Value {
    let stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    writeln!(
        writer,
        "{}",
        json!({"id": 1, "op": "inspect", "request": request, "payload": {}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// A loopback listener that counts connections and answers none: the paid provider nothing may reach.
fn paid_provider() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, hits)
}

/// The fake `claude` runs that answered a request (not `--version`, `auth status` or a handshake).
fn runs(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record.get("frames").is_some())
        .collect()
}

fn argv_value<'a>(record: &'a Value, flag: &str) -> &'a str {
    let argv = record["argv"].as_array().unwrap();
    let at = argv
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("{flag}: {record}"));
    argv[at + 1].as_str().unwrap()
}

/// A run went through the subscription relay: one native turn (`--max-turns 1`, `--permission-mode dontAsk`, the
/// relay's own settings) whose only tool is Ultron's `rlm`, advertised by the inert inventory server.
fn assert_subscription_run(record: &Value) {
    assert_eq!(argv_value(record, "--max-turns"), "1", "{record}");
    assert_eq!(argv_value(record, "--permission-mode"), "dontAsk", "{record}");
    let tools: Vec<&str> = record["mcp"]["ultron"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no inert inventory: {record}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["rlm"], "{record}");
}

fn frames_text(record: &Value) -> String {
    record["frames"].to_string()
}

fn start(root: &Path, paid: u16, args: &[&str]) -> (Server, PathBuf, PathBuf) {
    for sub in ["h", "a", "s", "r", "w", "tmp"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    // The user's default model and `rlm.childModel` are paid API models (as cliproxyapi/gpt-6-astra is): neither
    // may be used by a child of the Claude root.
    std::fs::write(
        root.join("a/settings.json"),
        json!({"defaultProvider": "groq", "defaultModel": "llama-3.3-70b-versatile",
               "rlm": {"childModel": "groq/llama-3.3-70b-versatile"}, "sessionNaming": {"enabled": false}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("a/models.json"),
        json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{paid}/v1")}}}).to_string(),
    )
    .unwrap();
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude");
    let log = root.join("claude.jsonl");
    let socket = root.join("r/s.sock");
    let p = |s: &str| root.join(s).display().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["mcp", "--no-instructions", "--socket", &socket.display().to_string()])
        .args(args)
        .current_dir(root.join("w"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", p("h"))
        .env("ULTRON_CODING_AGENT_DIR", p("a"))
        .env("ULTRON_SERVER_DIR", p("s"))
        .env("XDG_RUNTIME_DIR", p("r"))
        .env("TMPDIR", p("tmp"))
        .env("CLAUDE_CONFIG_DIR", p("h"))
        .env("ULTRON_SELF_COMMAND", r#"["/opt/ultron/bin/ultron"]"#)
        .env("ULTRON_LOKI", "off")
        .env("ULTRON_PYTHON", python())
        .env("PI_OFFLINE", "1")
        .env("GROQ_API_KEY", ["fake", "groq", "key"].join("-"))
        .env("ULTRON_CLAUDE_CODE_BIN", &fake)
        .env("FAKE_LOG", &log)
        .env(
            "FAKE_OFFLINE_BLOCKS",
            json!([{"type": "text", "text": ANSWER}]).to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if send.send(line).is_err() {
                return;
            }
        }
    });
    let mut server = Server {
        stdin: child.stdin.take(),
        child,
        lines,
        next: 0,
    };
    server.call("initialize", json!({"protocolVersion": "2025-06-18"}));
    (server, log, socket)
}

fn close(mut server: Server) {
    drop(server.stdin.take());
    assert!(server.child.wait().unwrap().success());
}

#[test]
fn claude_root_subscription_children_keep_children_run_on_the_roots_subscription_and_take_follow_ups() {
    let dir = tempfile::Builder::new().prefix("ucs-").tempdir_in("/tmp").unwrap();
    let (paid, hits) = paid_provider();
    let (mut server, log, socket) = start(dir.path(), paid, &[]);

    // No model= anywhere: the root's subscription model (no --root-model: the default, claude-opus-5-5).
    let first = server.cell(
        "h = await rlm.spawn('KEEP-BRIEF: say hi', name='kept', keep=True)\nr = await rlm.collect([h], timeout_ms=120000)\nprint(r[0]['result']['status'], r[0]['result']['value'])",
    );
    assert_eq!(first, format!("succeeded {ANSWER}"));
    let after_first = runs(&log);
    assert_eq!(after_first.len(), 1, "{after_first:?}");
    assert_subscription_run(&after_first[0]);
    assert_eq!(argv_value(&after_first[0], "--model"), OPUS_NATIVE);
    assert!(frames_text(&after_first[0]).contains("KEEP-BRIEF: say hi"));

    // A follow-up in the same conversation: its run replays the brief and the first answer.
    let asked =
        server.cell("a = await rlm.ask(h, 'QUESTION-2: and then?', timeout=120)\nprint(a['status'], a['value'])");
    assert_eq!(asked, format!("succeeded {ANSWER}"));
    let after_ask = runs(&log);
    assert_eq!(after_ask.len(), 2, "{after_ask:?}");
    assert_subscription_run(&after_ask[1]);
    let replayed = frames_text(&after_ask[1]);
    for wanted in ["KEEP-BRIEF: say hi", "subscribed", "QUESTION-2: and then?"] {
        assert!(replayed.contains(wanted), "{wanted}: {replayed}");
    }

    // A spawn naming a Claude Code model is accepted and runs on it, on the subscription.
    let named = server.cell(
        "n = await rlm.spawn('KEEP-HAIKU: brief', name='haiku', keep=True, model='claude-code/haiku')\nr = await rlm.collect([n], timeout_ms=120000)\nprint(n.model, r[0]['result']['status'])",
    );
    assert_eq!(named, "claude-code/haiku succeeded");
    let haiku = runs(&log)
        .into_iter()
        .find(|record| frames_text(record).contains("KEEP-HAIKU"))
        .expect("the haiku child's run");
    assert_subscription_run(&haiku);
    assert!(argv_value(&haiku, "--model").contains("haiku"), "{haiku}");

    // The children's responses are the session's sub-agent usage (`usage.report`, what `ultron usage` prints), on
    // the subscription models they ran as, priced as notional subscription usage: nothing reported as billed.
    let report = inspect(&socket, "usage.report");
    let usage = &report["result"]["usage"];
    let models: Vec<(&str, u64)> = usage["models"]
        .as_array()
        .unwrap_or_else(|| panic!("{report}"))
        .iter()
        .map(|m| (m["model"].as_str().unwrap(), m["responses"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        models,
        [
            ("claude-subscription/claude-opus-5-5", 2),
            ("claude-subscription/haiku", 1)
        ],
        "{report}"
    );
    let subagents = &usage["lanes"]["subagents"];
    assert_eq!(subagents["responses"], 3, "{report}");
    assert_eq!(
        subagents["cost"],
        json!({"reportedUsd": null, "subscriptionUsd": 0, "unpricedResponses": 0})
    );
    close(server);
    // Neither the default model nor `rlm.childModel` (both paid) was ever asked.
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn claude_root_subscription_children_a_retained_typed_agent_continues_on_the_subscription() {
    let dir = tempfile::Builder::new().prefix("uct-").tempdir_in("/tmp").unwrap();
    let (paid, hits) = paid_provider();
    let (mut server, log, _) = start(dir.path(), paid, &[]);
    let definition = json!({
        "id": "summ", "version": "1", "strategy": "rlm", "instructions": "TYPED-SUMMARY: summarize the text.",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"],
                        "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"],
                         "additionalProperties": false},
        "maxRepairs": 0, "inputDescription": "{text:string}", "outputDescription": "{answer:string}",
    })
    .to_string();
    // Neither the request nor the definition names a model: the root's subscription model. The retained
    // instance's next invocation continues the lane's conversation on it.
    let outcome = server.cell(&format!(
        "import json\nawait agents.register(json.loads({definition:?}))\nt = await agents.spawn('summ@1', {{'text': 'FIRST-TEXT'}})\nprint((await t.result())['status'])\ni = await instances.retain(t.id)\nn = await instances.invoke(i['id'], {{'text': 'AGAIN-TEXT'}})\nr = await agents.result(n['task_id'])\nprint(r['status'], r.get('error'), r.get('value'))"
    ));
    assert!(
        outcome.ends_with("succeeded\nsucceeded None {'answer': 'subscribed'}"),
        "{outcome}"
    );
    let all = runs(&log);
    assert_eq!(all.len(), 2, "{all:?}");
    for record in &all {
        assert_subscription_run(record);
        assert_eq!(argv_value(record, "--model"), OPUS_NATIVE);
    }
    let continued = frames_text(&all[1]);
    for wanted in ["TYPED-SUMMARY", "FIRST-TEXT", "subscribed", "AGAIN-TEXT"] {
        assert!(continued.contains(wanted), "{wanted}: {continued}");
    }
    close(server);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn claude_root_subscription_children_follow_the_roots_model_for_in_memory_and_kept_children() {
    let dir = tempfile::Builder::new().prefix("ucr-").tempdir_in("/tmp").unwrap();
    let (paid, hits) = paid_provider();
    // The launcher passes the root's `--model sonnet` as `--root-model sonnet`.
    let (mut server, log, _) = start(dir.path(), paid, &["--root-model", "sonnet", "--children", "ultron"]);
    let done = server.cell(
        "k = await rlm.spawn('KEPT-SONNET: brief', name='kept', keep=True)\nh = await rlm.spawn('PLAIN-SONNET: brief', name='plain')\nrk = await rlm.collect([k], timeout_ms=120000)\nrh = await rlm.collect([h], timeout_ms=120000)\nprint(h.model, [x['result']['status'] for x in rk + rh])",
    );
    assert!(
        done.ends_with("claude-code/sonnet ['succeeded', 'succeeded']"),
        "{done}"
    );
    let all = runs(&log);
    for brief in ["KEPT-SONNET", "PLAIN-SONNET"] {
        let run = all
            .iter()
            .find(|record| frames_text(record).contains(brief))
            .unwrap_or_else(|| panic!("{brief}: {all:?}"));
        assert_subscription_run(run);
        assert!(argv_value(run, "--model").contains("sonnet"), "{run}");
    }
    close(server);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
