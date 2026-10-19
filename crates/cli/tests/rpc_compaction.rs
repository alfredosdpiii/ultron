//! Compaction over RPC as AgentSession runs it: `abort` cancels a running compaction (manual `compact` and an
//! automatic one inside a run) and `get_state` reports `isCompacting` meanwhile. The Rust binary runs in RPC mode in a
//! private home against a scripted Anthropic server that can hold a summary request open until the client drops it.
//! (Pi extensions' compaction and tree hooks are not supported by ultron-rs.)
//! A `compact` during a run aborts the run first; tree navigation's branch summary (driven on the session actor
//! directly, RPC has no navigate command) is `isCompacting`, abortable and exclusive with compaction.

mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A hang guard, never a pass condition.
const HANG: Duration = Duration::from_secs(120);

/// An Anthropic SSE reply of `text`, reporting `input` tokens.
fn reply(input: u64, text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// A model reply that streams `Partial answer` and is then held open until the client drops it (reported on the
/// held channel like a held summary request).
const HOLD_MODEL: &str = "(hold)";

fn is_summary(body: &Value) -> bool {
    body["system"].to_string().contains("context summarization assistant")
}

/// What happened to a held summary request.
#[derive(Debug, PartialEq)]
enum Held {
    /// It arrived (and is held open).
    Seen,
    /// Its client closed the connection: the request was cancelled.
    Dropped,
}

/// A scripted provider: model requests get `replies` in order; summary requests get `summaries` in order, then
/// `summary`, except that with `hold` the first one is held open until the client drops it (reported on the
/// channel).
fn provider_scripted(
    replies: Vec<String>,
    summaries: Vec<String>,
    summary: &str,
    mut hold: bool,
) -> (u16, Arc<Mutex<Vec<Value>>>, Receiver<Held>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (seen, summary) = (requests.clone(), summary.to_string());
    let (held_tx, held): (Sender<Held>, Receiver<Held>) = channel();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        let mut summaries = summaries.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            seen.lock().unwrap().push(body.clone());
            let text = if body["system"].to_string().contains(common::NAMING_SYSTEM) {
                reply(10, "A title")
            } else if is_summary(&body) {
                if std::mem::take(&mut hold) {
                    let held_tx = held_tx.clone();
                    // Held on its own thread: the server keeps answering (the run's next requests).
                    std::thread::spawn(move || {
                        let _ = held_tx.send(Held::Seen);
                        let mut byte = [0u8; 1];
                        while matches!(stream.read(&mut byte), Ok(n) if n > 0) {}
                        let _ = held_tx.send(Held::Dropped);
                    });
                    continue;
                }
                summaries.next().unwrap_or_else(|| reply(10, &summary))
            } else {
                let next = replies.next().unwrap_or_else(|| reply(10, "(no more replies)"));
                if next == HOLD_MODEL {
                    let partial: String = reply(10, "Partial answer").split_inclusive("\n\n").take(3).collect();
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(partial.as_bytes());
                    let _ = stream.flush();
                    let held_tx = held_tx.clone();
                    std::thread::spawn(move || {
                        let _ = held_tx.send(Held::Seen);
                        let mut byte = [0u8; 1];
                        while matches!(stream.read(&mut byte), Ok(n) if n > 0) {}
                        let _ = held_tx.send(Held::Dropped);
                    });
                    continue;
                }
                next
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                text.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(text.as_bytes());
        }
    });
    (port, requests, held)
}

struct Fixture {
    client: RpcClient,
    requests: Arc<Mutex<Vec<Value>>>,
    held: Receiver<Held>,
    root: PathBuf,
    _reaper: Reaper,
    _tmp: tempfile::TempDir,
}

/// The binary in RPC mode against [`provider`].
fn start(replies: Vec<String>, hold: bool) -> Fixture {
    start_full(replies, Vec::new(), hold, None)
}

/// [`start`] whose first summary requests get `summaries`, with `settings` as settings.json.
fn start_full(replies: Vec<String>, summaries: Vec<String>, hold: bool, settings: Option<Value>) -> Fixture {
    let (port, requests, held) = provider_scripted(replies, summaries, "## Goal\nSummary from the model.", hold);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}).to_string(),
    )
    .unwrap();
    if let Some(settings) = settings {
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    }
    let env = [
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("HOME", root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR", root.join("server").display().to_string()),
        ("ANTHROPIC_API_KEY", "test-key".into()),
        ("ULTRON_HINDSIGHT_URL", "off".into()),
        ("PI_OFFLINE", "1".into()),
        ("NO_COLOR", "1".into()),
        ("TZ", "UTC".into()),
        // Requests are compared with TS recordings: the TS runtime guide.
        ("ULTRON_GUIDE", "ts".into()),
    ];
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(cwd),
        env_clear: true,
        env: env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        args: vec!["--model".into(), "anthropic/claude-sonnet-4-5".into()],
        ..Default::default()
    };
    Fixture {
        client: RpcClient::start(&options).unwrap(),
        requests,
        held,
        _reaper: Reaper(root.clone()),
        root,
        _tmp: tmp,
    }
}

/// Kills every process started under a root (the client's coordinator and session worker name it) when the test
/// ends, passed or failed: a worker busy with a held summary would otherwise outlive a failed test.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let needle = self.0.display().to_string();
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            if pid != std::process::id() as i32 && String::from_utf8_lossy(&cmdline).contains(&needle) {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}

impl Fixture {
    fn held(&self) -> Held {
        self.held.recv_timeout(HANG).expect("the held summary request")
    }

    /// The committed writes of the session's journal, in order.
    fn writes(&self) -> Vec<Value> {
        let files = common::session_files(&self.root.join("agent").join("experimental").join("sessions"));
        assert_eq!(files.len(), 1, "{files:?}");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        text.lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
            .collect()
    }

    fn compaction_entries(&self) -> Vec<Value> {
        self.writes()
            .into_iter()
            .filter(|w| w["kind"] == "entry" && w["type"] == "compaction")
            .collect()
    }

    /// `(kind, status)` of every settled operation, oldest first.
    fn results(&self) -> Vec<(String, String)> {
        self.writes()
            .iter()
            .filter(|w| w["namespace"] == session_journal::run::OP_RESULT && w["op"] == "set")
            .map(|w| {
                let text = |key: &str| w["value"][key].as_str().unwrap_or_default().to_string();
                (text("kind"), text("status"))
            })
            .collect()
    }

    /// Preparations written and not removed, and operations still open: both 0 for a consistent session.
    fn open_state(&self) -> (i64, Value) {
        let writes = self.writes();
        let preparations = writes
            .iter()
            .filter(|w| w["namespace"] == "pi.op.preparation")
            .map(|w| if w["op"] == "set" { 1 } else { -1 })
            .sum();
        let lane = writes
            .iter()
            .rfind(|w| w["namespace"] == "pi.lane.state")
            .map_or(Value::Null, |w| w["value"]["currentOperationId"].clone());
        (preparations, lane)
    }

    /// The response to the command with `id`, sent after the last mark.
    fn response(&mut self, id: &str) -> Value {
        self.client
            .wait_for(&|l| l["type"] == "response" && l["id"] == id, HANG)
            .unwrap()
    }

    /// The journal as a worker killed now leaves it, reopened from a copy and settled (`recovery::settle`): what
    /// it recovered, and the settled session.
    fn settled_copy(&self) -> (session_core::recovery::Recovered, session_journal::Session) {
        let files = common::session_files(&self.root.join("agent").join("experimental").join("sessions"));
        let copy = self.root.join("killed").join("--cwd--");
        std::fs::create_dir_all(&copy).unwrap();
        let path = copy.join(files[0].file_name().unwrap());
        std::fs::copy(&files[0], &path).unwrap();
        let metadata = session_journal::repo::read_metadata(&path).unwrap();
        let repo = session_journal::SessionRepo::new(&self.root.join("killed"), session_journal::SystemClock::shared());
        let mut session = repo.open(&metadata).unwrap();
        let recovered = session_core::recovery::settle(&mut session).unwrap();
        (recovered, session)
    }

    /// Index in the transcript of the first line of `kind`.
    fn first(&self, kind: &str) -> usize {
        self.client
            .transcript
            .iter()
            .position(|l| l["type"] == kind)
            .unwrap_or_else(|| panic!("no {kind}"))
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.client
            .transcript
            .iter()
            .filter(|l| l["type"] == kind)
            .cloned()
            .collect()
    }
}

/// `abort` during a manual `compact` cancels it at once: `get_state` reports `isCompacting` while the summary
/// request is in flight, the request is dropped, `compact` answers "Compaction cancelled" with `compaction_end`
/// `aborted`, the operation ends `aborted` with the branch, preparation and lane unchanged, and the session prompts
/// on. Before, the actor blocked on the summary: `get_state` and `abort` waited for it and `isCompacting` was false.
#[test]
fn cli_rpc_compaction_abort_cancels_a_manual_compaction() {
    let mut f = start(vec![reply(10, "Hello."), reply(10, "After.")], true);
    f.client.prompt_and_wait("hello", HANG).unwrap();
    f.client.mark();
    f.client
        .write_line(&json!({"type": "compact", "id": "compact-1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Seen);
    let state = f.client.get_state().unwrap();
    assert_eq!(state["isCompacting"], true, "{state}");
    assert_eq!(state["isStreaming"], false, "{state}");
    // A prompt meanwhile is refused, as AgentSession refuses one during a manual compaction.
    let refused = f
        .client
        .send(json!({"type": "prompt", "message": "too early"}))
        .unwrap();
    assert_eq!(refused["success"], false, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("compaction is in progress"),
        "{refused}"
    );
    let aborted = f.client.send(json!({"type": "abort"})).unwrap();
    assert_eq!(aborted["success"], true, "{aborted}");
    assert_eq!(f.held(), Held::Dropped, "the summary request was cancelled");
    let compact = f.response("compact-1");
    assert_eq!(compact["success"], false, "{compact}");
    assert_eq!(compact["error"], "Compaction cancelled");
    let end = f.client.wait_for(&|l| l["type"] == "compaction_end", HANG).unwrap();
    assert_eq!(
        end,
        json!({"type": "compaction_end", "reason": "manual", "aborted": true, "willRetry": false})
    );
    assert_eq!(f.client.get_state().unwrap()["isCompacting"], false);
    assert!(f.compaction_entries().is_empty());
    assert_eq!(
        f.results().last().unwrap(),
        &("compaction".to_string(), "aborted".to_string())
    );
    assert_eq!(f.open_state(), (0, Value::Null));
    // The session goes on with its whole history.
    f.client.prompt_and_wait("after", HANG).unwrap();
    assert_eq!(f.client.get_last_assistant_text().unwrap().as_deref(), Some("After."));
    let last = f.requests.lock().unwrap().last().cloned().unwrap();
    assert!(last["messages"].to_string().contains("Hello."), "{last}");
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// `abort` during a run's threshold compaction (its summary in flight) cancels the compaction with the run:
/// `isCompacting` and `isStreaming` meanwhile, the summary request dropped, `compaction_end` `aborted`, the run
/// ended `aborted` with no compaction entry and nothing left open; the next prompt runs.
#[test]
fn cli_rpc_compaction_abort_cancels_an_automatic_compaction_with_its_run() {
    let mut f = start(vec![reply(990_000, "Big."), reply(10, "Next.")], true);
    f.client.mark();
    f.client
        .write_line(&json!({"type": "prompt", "message": "first", "id": "p1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Seen);
    let state = f.client.get_state().unwrap();
    assert_eq!(
        (&state["isCompacting"], &state["isStreaming"]),
        (&json!(true), &json!(true)),
        "{state}"
    );
    assert_eq!(f.client.send(json!({"type": "abort"})).unwrap()["success"], true);
    assert_eq!(f.held(), Held::Dropped, "the summary request was cancelled");
    f.client.wait_for(&|l| l["type"] == "agent_settled", HANG).unwrap();
    let ends = f.events("compaction_end");
    assert_eq!(
        ends,
        [json!({"type": "compaction_end", "reason": "threshold", "aborted": true, "willRetry": false})]
    );
    assert_eq!(f.client.get_state().unwrap()["isCompacting"], false);
    assert!(f.compaction_entries().is_empty());
    assert_eq!(f.results().last().unwrap(), &("run".to_string(), "aborted".to_string()));
    assert_eq!(f.open_state(), (0, Value::Null));
    // The context is still over the threshold: the next run compacts first (its summary answered), then asks.
    let events = f.client.prompt_and_wait("second", HANG).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "compaction_end" && e["aborted"] == false),
        "{events:#?}"
    );
    assert_eq!(f.client.get_last_assistant_text().unwrap().as_deref(), Some("Next."));
    assert_eq!(f.compaction_entries().len(), 1);
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// Committed `(type, role, stopReason)` of the journal's entries, in order.
fn entry_kinds(writes: &[Value]) -> Vec<(String, String, String)> {
    writes
        .iter()
        .filter(|w| w["kind"] == "entry")
        .map(|w| {
            let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
            (
                text(&w["type"]),
                text(&w["message"]["role"]),
                text(&w["message"]["stopReason"]),
            )
        })
        .collect()
}

/// TypeScript #7253 (`manual compaction during an active response`): `compact` while a response streams aborts
/// the run first (AgentSession `compact` awaits `abort()`), persists the aborted response, then compacts once:
/// one `compaction_start` / `compaction_end` (`manual`), the compaction entry after the aborted response. Before,
/// Rust refused the `compact` with "Agent is already processing".
#[test]
fn cli_rpc_compaction_compact_during_a_run_aborts_the_run_first() {
    let mut f = start(vec![HOLD_MODEL.into(), reply(10, "After.")], false);
    f.client.mark();
    f.client
        .write_line(
            &json!({"type": "prompt", "message": "Run the tool, then continue responding.", "id": "p1"}).to_string(),
        )
        .unwrap();
    assert_eq!(f.held(), Held::Seen);
    f.client
        .write_line(&json!({"type": "compact", "id": "compact-1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Dropped, "the response was aborted");
    let compact = f.response("compact-1");
    assert_eq!(compact["success"], true, "{compact}");
    assert_eq!(compact["data"]["summary"], "## Goal\nSummary from the model.");
    let reasons = |kind: &str| -> Vec<Value> { f.events(kind).iter().map(|e| e["reason"].clone()).collect() };
    assert_eq!(reasons("compaction_start"), [json!("manual")]);
    assert_eq!(reasons("compaction_end"), [json!("manual")]);
    assert!(
        f.first("agent_end") < f.first("compaction_start"),
        "the run ended before the compaction"
    );
    let kinds = entry_kinds(&f.writes());
    let aborted = kinds
        .iter()
        .position(|(t, role, stop)| t == "message" && role == "assistant" && stop == "aborted")
        .expect("the aborted response is persisted");
    let compaction = kinds.iter().position(|(t, ..)| t == "compaction").unwrap();
    assert!(compaction > aborted, "{kinds:?}");
    assert_eq!(f.compaction_entries().len(), 1);
    let results = f.results();
    assert_eq!(
        results[results.len() - 2..],
        [
            ("run".to_string(), "aborted".to_string()),
            ("compaction".to_string(), "completed".to_string())
        ]
    );
    assert_eq!(f.open_state(), (0, Value::Null));
    f.client.prompt_and_wait("after", HANG).unwrap();
    assert_eq!(f.client.get_last_assistant_text().unwrap().as_deref(), Some("After."));
    let last = f.requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        last["messages"].to_string().contains("Summary from the model."),
        "{last}"
    );
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// The compaction a `compact` during a run starts after aborting it: `isCompacting` (not streaming) while its
/// summary runs; a worker killed then leaves the aborted run settled and the compaction ended `failed`
/// (`interrupted`), the branch at the aborted response; a live `abort` cancels it like any manual compaction.
#[test]
fn cli_rpc_compaction_compact_during_a_run_recovers_and_cancels() {
    let mut f = start(vec![HOLD_MODEL.into(), reply(10, "After.")], true);
    f.client
        .write_line(&json!({"type": "prompt", "message": "first", "id": "p1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Seen);
    f.client.mark();
    f.client
        .write_line(&json!({"type": "compact", "id": "compact-1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Dropped, "the response was aborted");
    assert_eq!(f.held(), Held::Seen, "the summary request");
    let state = f.client.get_state().unwrap();
    assert_eq!(
        (&state["isCompacting"], &state["isStreaming"]),
        (&json!(true), &json!(false)),
        "{state}"
    );
    // Killed here: the run was settled `aborted` before the compaction began; the compaction is interrupted.
    let (recovered, session) = f.settled_copy();
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    assert!(!recovered.resume);
    let tip = session.branch_tip("main").flatten().unwrap();
    let entry = session.state().entry(&tip).unwrap().clone();
    assert_eq!(
        (&entry["message"]["role"], &entry["message"]["stopReason"]),
        (&json!("assistant"), &json!("aborted")),
        "{entry}"
    );
    let operation = recovered.operation_id.unwrap();
    let result = session.value(session_journal::run::OP_RESULT, &operation).unwrap();
    assert_eq!(
        (&result["kind"], &result["error"]["code"]),
        (&json!("compaction"), &json!("interrupted"))
    );
    assert_eq!(session.state().scan_values("pi.op.preparation", "").len(), 0);
    // Live: abort cancels the compaction.
    assert_eq!(f.client.send(json!({"type": "abort"})).unwrap()["success"], true);
    assert_eq!(f.held(), Held::Dropped, "the summary request was cancelled");
    let compact = f.response("compact-1");
    assert_eq!(compact["error"], "Compaction cancelled", "{compact}");
    let results = f.results();
    assert_eq!(
        results[results.len() - 2..],
        [
            ("run".to_string(), "aborted".to_string()),
            ("compaction".to_string(), "aborted".to_string())
        ]
    );
    assert_eq!(f.open_state(), (0, Value::Null));
    assert!(f.compaction_entries().is_empty());
    f.client.prompt_and_wait("after", HANG).unwrap();
    assert_eq!(f.client.get_last_assistant_text().unwrap().as_deref(), Some("After."));
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// The session actor itself (what the session worker runs) against [`provider`], for the operations RPC has no
/// command for (tree navigation is the agent controller's `navigate`): ops go in through `core::call`, its events
/// are collected.
struct Actor {
    tx: std::sync::mpsc::Sender<session_core::core::Input>,
    actor: Option<std::thread::JoinHandle<()>>,
    events: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    held: Receiver<Held>,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

fn actor(replies: Vec<String>, hold: bool) -> Actor {
    actor_full(replies, Vec::new(), hold, None)
}

/// [`actor`] whose first summary requests get `summaries`, with `settings` as settings.json.
fn actor_full(replies: Vec<String>, summaries: Vec<String>, hold: bool, settings: Option<Value>) -> Actor {
    let (port, requests, held) = provider_scripted(replies, summaries, "## Goal\nSummary from the model.", hold);
    let sandbox = common::Sandbox::new(port);
    if let Some(settings) = settings {
        std::fs::write(sandbox.agent_dir.join("settings.json"), settings.to_string()).unwrap();
    }
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        (
            "ULTRON_SERVER_DIR".into(),
            sandbox.root.join("server").display().to_string(),
        ),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    let env = cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let root = sandbox.sessions_dir();
    let repo = session_journal::SessionRepo::new(&root, session_journal::SystemClock::shared());
    let created = repo.create(&sandbox.cwd.display().to_string(), None, None).unwrap();
    let open = session_core::core::OpenOptions {
        env,
        flags: session_core::ModelFlags {
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..Default::default()
        },
        session_root: root,
        metadata: created.metadata().clone(),
        mode: "rpc".into(),
        services: cli::compose::Services::default(),
        auto_name: false,
    };
    drop(created);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let (tx, actor, _) = session_core::core::spawn(open, Box::new(move |e| sink.lock().unwrap().push(e))).unwrap();
    Actor {
        tx,
        actor: Some(actor),
        events,
        requests,
        held,
        root: sandbox.root.clone(),
        _tmp: sandbox.tmp,
    }
}

impl Actor {
    fn call(&self, op: session_core::core::Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    /// `op` on its own thread (it waits for a held request); its answer arrives on the receiver.
    fn start(&self, op: session_core::core::Op) -> Receiver<Result<Option<Value>, String>> {
        let (answer, answered) = channel();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = answer.send(session_core::core::call(&tx, op));
        });
        answered
    }

    fn held(&self) -> Held {
        self.held.recv_timeout(HANG).expect("the held request")
    }

    fn prompt(&self, text: &str) {
        let ran = self
            .call(session_core::core::Op::PromptAndWait {
                message: text.into(),
                images: Vec::new(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(ran["error"], Value::Null, "{ran}");
    }

    fn entries(&self) -> (Vec<Value>, Value) {
        let read = self
            .call(session_core::core::Op::Entries { since: None })
            .unwrap()
            .unwrap();
        (read["entries"].as_array().unwrap().clone(), read["leafId"].clone())
    }

    fn is_compacting(&self) -> bool {
        self.call(session_core::core::Op::State).unwrap().unwrap()["isCompacting"] == true
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == kind)
            .cloned()
            .collect()
    }

    /// The committed writes of the session's journal, in order.
    fn writes(&self) -> Vec<Value> {
        let files = common::session_files(&self.root.join("agent").join("experimental").join("sessions"));
        let text = std::fs::read_to_string(&files[0]).unwrap();
        text.lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
            .collect()
    }

    /// The `at` of every state the operation `id` went through, then its result's status.
    fn operation_trace(&self, id: &str) -> (Vec<String>, Value) {
        let writes = self.writes();
        let states = writes
            .iter()
            .filter(|w| w["namespace"] == session_journal::run::OP_STATE && w["key"] == id && w["op"] == "set")
            .map(|w| w["value"]["at"].as_str().unwrap().to_string())
            .collect();
        let result = writes
            .iter()
            .rfind(|w| w["namespace"] == session_journal::run::OP_RESULT && w["key"] == id)
            .map_or(Value::Null, |w| w["value"].clone());
        (states, result)
    }

    /// The label of tree entry `id`, if any.
    fn label(&self, id: &str) -> Value {
        let tree = self.call(session_core::core::Op::Tree).unwrap().unwrap();
        let mut stack: Vec<Value> = tree["tree"].as_array().unwrap().clone();
        while let Some(node) = stack.pop() {
            if node["entry"]["id"] == id {
                return node.get("label").cloned().unwrap_or(Value::Null);
            }
            stack.extend(node["children"].as_array().cloned().unwrap_or_default());
        }
        Value::Null
    }

    /// The journal as a worker killed now leaves it, reopened from a copy and settled.
    fn settled_copy(&self) -> (session_core::recovery::Recovered, session_journal::Session) {
        let files = common::session_files(&self.root.join("agent").join("experimental").join("sessions"));
        assert_eq!(files.len(), 1, "{files:?}");
        let copy = self.root.join("killed").join("--cwd--");
        std::fs::create_dir_all(&copy).unwrap();
        let path = copy.join(files[0].file_name().unwrap());
        std::fs::copy(&files[0], &path).unwrap();
        let metadata = session_journal::repo::read_metadata(&path).unwrap();
        let repo = session_journal::SessionRepo::new(&self.root.join("killed"), session_journal::SystemClock::shared());
        let mut session = repo.open(&metadata).unwrap();
        let recovered = session_core::recovery::settle(&mut session).unwrap();
        (recovered, session)
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        let (done, closed) = channel();
        // A failed test may leave the actor stuck: it is joined only once it has closed.
        if self.tx.send(session_core::core::Input::Close(done)).is_ok()
            && closed.recv_timeout(HANG).is_ok()
            && let Some(actor) = self.actor.take()
        {
            let _ = actor.join();
        }
    }
}

/// A session of two exchanges (`first ask` / `One.`, `second ask` / `Two.`); returns the first answer's entry id
/// and the leaf.
fn two_exchanges(a: &Actor) -> (String, Value) {
    a.prompt("first ask");
    a.prompt("second ask");
    let (entries, leaf) = a.entries();
    let first_answer = entries
        .iter()
        .find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    (first_answer, leaf)
}

fn navigate(target: &str, summarize: bool) -> session_core::core::Op {
    session_core::core::Op::Navigate {
        target_id: Some(target.into()),
        summarize,
        instructions: None,
        label: None,
    }
}

/// TypeScript `should handle abort during summarization` (agent-session-tree-navigation.test.ts) and #9178: while a
/// navigation's branch summary runs the session answers, `isCompacting` is true, another navigation is refused
/// ("Wait for the current compaction or tree navigation to finish ..."); a worker killed then leaves the
/// navigation's operation to settle `failed` (`interrupted`) with the session where it was; `abort` cancels the summary: the navigation ends `aborted` with no
/// summary entry and the leaf unchanged, and `isCompacting` is false again. Before, the actor blocked on the
/// summary and `isCompacting` stayed false.
#[test]
fn cli_rpc_compaction_branch_summary_is_compacting_and_abortable() {
    let a = actor(vec![reply(10, "One."), reply(10, "Two.")], true);
    let (target, leaf) = two_exchanges(&a);
    let (entries_before, _) = a.entries();
    let navigation = a.start(navigate(&target, true));
    assert_eq!(a.held(), Held::Seen);
    assert!(a.is_compacting());
    assert_eq!(
        a.call(navigate(&target, false)).unwrap_err(),
        "Wait for the current compaction or tree navigation to finish before navigating the session tree."
    );
    // Killed here: the navigation's operation settles `failed` (`interrupted`), nothing moved.
    let (recovered, session) = a.settled_copy();
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    assert!(!recovered.resume);
    let result = session
        .value(session_journal::run::OP_RESULT, &recovered.operation_id.unwrap())
        .unwrap()
        .clone();
    assert_eq!(
        (&result["kind"], &result["error"]["code"]),
        (&json!("navigation"), &json!("interrupted"))
    );
    assert!(session.state().scan_values("pi.op.preparation", "").is_empty());
    assert_eq!(json!(session.branch_tip("main").flatten()), leaf);
    assert!(session.state().entries().iter().all(|e| e["type"] != "branch_summary"));
    a.call(session_core::core::Op::Abort).unwrap();
    assert_eq!(a.held(), Held::Dropped, "the summary request was cancelled");
    let answer = navigation.recv_timeout(HANG).unwrap();
    assert_eq!(answer.unwrap_err(), "Branch summarization cancelled");
    assert!(!a.is_compacting());
    let end = a.events("navigation_end").pop().unwrap();
    assert_eq!(
        (&end["status"], &end["tipId"], &end["fromTipId"]),
        (&json!("aborted"), &leaf, &leaf),
        "{end}"
    );
    let (entries_after, leaf_after) = a.entries();
    assert_eq!(entries_after.len(), entries_before.len());
    assert_eq!(leaf_after, leaf);
    // The session goes on: the navigation with its summary (answered this time), then without one.
    let summarized = a.call(navigate(&target, true)).unwrap().unwrap();
    let (entries, leaf_now) = a.entries();
    let summary = entries.last().unwrap();
    assert_eq!(
        (&summary["type"], &summary["parentId"], &summary["summary"]),
        (
            &json!("branch_summary"),
            &json!(target),
            &json!(
                "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n## Goal\nSummary from the model."
            )
        ),
        "{summary}"
    );
    assert_eq!(summarized["tipId"], leaf_now);
    assert_eq!(
        a.requests.lock().unwrap().len(),
        4,
        "two answers, the cancelled summary and the summary"
    );
}

/// TypeScript #9178 (`rejects navigation before the active leaf can change`): a navigation during a manual
/// compaction is refused with AgentSession's message and the compaction commits on the original leaf; and a
/// `compact` during a branch summary aborts the summary first (AgentSession `compact` awaits `abort()`, which
/// `abortBranchSummary`s), then compacts.
#[test]
fn cli_rpc_compaction_navigation_and_compaction_exclude_each_other() {
    let a = actor(vec![reply(10, "One."), reply(10, "Two.")], true);
    let (target, leaf) = two_exchanges(&a);
    let compaction = a.start(session_core::core::Op::Compact { instructions: None });
    assert_eq!(a.held(), Held::Seen);
    assert_eq!(
        a.call(navigate(&target, false)).unwrap_err(),
        "Wait for the current compaction or tree navigation to finish before navigating the session tree."
    );
    a.call(session_core::core::Op::Abort).unwrap();
    assert_eq!(a.held(), Held::Dropped);
    assert_eq!(
        compaction.recv_timeout(HANG).unwrap().unwrap_err(),
        "Compaction cancelled"
    );
    assert_eq!(a.entries().1, leaf);

    // A held summary again (the provider holds only its first summary request): navigate, then compact.
    let b = actor(vec![reply(10, "One."), reply(10, "Two.")], true);
    let (target, leaf) = two_exchanges(&b);
    let navigation = b.start(navigate(&target, true));
    assert_eq!(b.held(), Held::Seen);
    let compacted = b.start(session_core::core::Op::Compact { instructions: None });
    assert_eq!(b.held(), Held::Dropped, "the branch summary was aborted");
    assert_eq!(
        navigation.recv_timeout(HANG).unwrap().unwrap_err(),
        "Branch summarization cancelled"
    );
    let compacted = compacted.recv_timeout(HANG).unwrap().unwrap().unwrap();
    assert_eq!(compacted["summary"], "## Goal\nSummary from the model.");
    let (entries, _) = b.entries();
    let last = entries.last().unwrap();
    assert_eq!(
        (&last["type"], &last["parentId"]),
        (&json!("compaction"), &leaf),
        "{last}"
    );
    assert!(entries.iter().all(|e| e["type"] != "branch_summary"));
}

fn summarizing(target: &str, label: Option<&str>) -> session_core::core::Op {
    session_core::core::Op::Navigate {
        target_id: Some(target.into()),
        summarize: true,
        instructions: None,
        label: label.map(str::to_string),
    }
}

/// A navigation journaled as the harness journals it: with a summary, `summary.deciding` (its `branch_summary`
/// preparation), `summary.ready`, `summary.effect_pending` (before and with its request, then with its usage), and
/// the terminal transaction (`navigation` / `completed`), the label on the summary entry; without one,
/// `navigation.ready_to_commit` then its commit. `navigation_start` / `navigation_end` carry the operation's id.
#[test]
fn cli_rpc_compaction_navigation_is_journaled_as_an_operation() {
    let a = actor(vec![reply(10, "One."), reply(10, "Two.")], false);
    let (target, leaf) = two_exchanges(&a);
    let answer = a.call(summarizing(&target, Some("checkpoint"))).unwrap().unwrap();
    let start = a.events("navigation_start").pop().unwrap();
    let operation = start["runId"].as_str().unwrap().to_string();
    let (states, result) = a.operation_trace(&operation);
    assert_eq!(
        states,
        [
            "summary.deciding",
            "summary.ready",
            "summary.effect_pending",
            "summary.effect_pending",
            "summary.effect_pending"
        ]
    );
    assert_eq!(
        (
            &result["kind"],
            &result["status"],
            &result["fromTipId"],
            &result["tipId"]
        ),
        (&json!("navigation"), &json!("completed"), &leaf, &answer["tipId"])
    );
    let end = a.events("navigation_end").pop().unwrap();
    assert_eq!(
        (&end["runId"], &end["status"]),
        (&json!(operation), &json!("completed"))
    );
    let preparations: Vec<Value> = a
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == "pi.op.preparation")
        .collect();
    assert_eq!(preparations.len(), 2, "set at admission, removed at the end");
    assert_eq!(preparations[0]["value"]["kind"], "branch_summary");
    assert_eq!(a.label(answer["tipId"].as_str().unwrap()), "checkpoint");

    // Without a summary, to the first answer.
    let (entries, _) = a.entries();
    let answer_id = entries[1]["id"].as_str().unwrap().to_string();
    let answer = a
        .call(session_core::core::Op::Navigate {
            target_id: Some(answer_id.clone()),
            summarize: false,
            instructions: None,
            label: Some("start".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(answer, json!({"tipId": answer_id}));
    let operation = a.events("navigation_start").pop().unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();
    let (states, result) = a.operation_trace(&operation);
    assert_eq!(states, ["navigation.ready_to_commit"]);
    assert_eq!(
        (&result["status"], &result["tipId"]),
        (&json!("completed"), &json!(answer_id))
    );
    assert_eq!(a.label(&answer_id), "start");
}

/// AgentSession `navigateTree` resolves the selected entry: selecting a user message moves the tip to before it
/// (here the root) and answers its text for the editor; `session_before_tree` sees the selected entry as
/// `targetId`; a label goes on the selected message (`labelEntryId` in `navigation.ready_to_commit`), or on the
/// branch summary when there is one, which hangs under the message's parent. Selecting the current tip is a no-op.
#[test]
fn cli_rpc_compaction_navigation_resolves_a_selected_user_message() {
    let a = actor(vec![reply(10, "One."), reply(10, "Two.")], false);
    let (first_answer, leaf) = two_exchanges(&a);
    let (entries, _) = a.entries();
    let (first_ask, second_ask) = (
        entries[0]["id"].as_str().unwrap().to_string(),
        entries[2]["id"].as_str().unwrap().to_string(),
    );
    // Already there: nothing happens.
    assert_eq!(
        a.call(navigate(leaf.as_str().unwrap(), false)).unwrap().unwrap(),
        json!({"tipId": leaf})
    );
    assert!(a.events("navigation_start").is_empty());

    let answer = a
        .call(session_core::core::Op::Navigate {
            target_id: Some(first_ask.clone()),
            summarize: false,
            instructions: None,
            label: Some("start".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(answer, json!({"tipId": null, "editorText": "first ask"}));
    assert_eq!(a.entries().1, Value::Null);
    assert_eq!(a.label(&first_ask), "start");
    let operation = a.events("navigation_start").pop().unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();
    let state = a
        .writes()
        .into_iter()
        .find(|w| w["namespace"] == session_journal::run::OP_STATE && w["key"] == operation.as_str())
        .unwrap()["value"]
        .clone();
    assert_eq!(
        (&state["targetId"], &state["label"], &state["labelEntryId"]),
        (&Value::Null, &json!("start"), &json!(first_ask))
    );

    // Back to the old leaf, then summarize on the way to the second ask: the summary hangs under its parent.
    a.call(navigate(leaf.as_str().unwrap(), false)).unwrap();
    let answer = a
        .call(session_core::core::Op::Navigate {
            target_id: Some(second_ask.clone()),
            summarize: true,
            instructions: None,
            label: Some("summary".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(answer["editorText"], "second ask");
    let summary = a.entries().0.last().cloned().unwrap();
    assert_eq!(
        (&summary["type"], &summary["parentId"], &answer["tipId"]),
        (&json!("branch_summary"), &json!(first_answer), &summary["id"])
    );
    assert_eq!(a.label(summary["id"].as_str().unwrap()), "summary");
    assert_eq!(a.label(&second_ask), Value::Null);
    assert_eq!(a.call(navigate("nope", false)).unwrap_err(), "Entry nope not found");
}

/// The error message the Anthropic provider gives [`overloaded`]'s response (the stream's error event).
const OVERLOADED_ERROR: &str = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

/// An Anthropic stream that fails with a transient provider error (retryable: `isRetryableAssistantError`).
fn overloaded() -> String {
    let error = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    format!("event: error\ndata: {error}\n\n")
}

/// An Anthropic stream that fails with an error that is not retryable.
fn invalid() -> String {
    let error = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Bad summary request"}});
    format!("event: error\ndata: {error}\n\n")
}

impl Actor {
    /// Waits until the actor emitted an event of `kind` (a hang guard, never a pass condition).
    fn wait_event(&self, kind: &str) -> Value {
        let end = std::time::Instant::now() + HANG;
        loop {
            if let Some(event) = self.events(kind).pop() {
                return event;
            }
            assert!(std::time::Instant::now() < end, "timed out waiting for {kind}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The operation whose result was recorded last, and its trace.
    fn last_operation(&self) -> (String, Vec<String>, Value) {
        let id = self
            .writes()
            .iter()
            .rfind(|w| w["namespace"] == session_journal::run::OP_RESULT)
            .map(|w| w["key"].as_str().unwrap().to_string())
            .unwrap();
        let (states, result) = self.operation_trace(&id);
        (id, states, result)
    }

    fn summaries(&self) -> usize {
        self.requests.lock().unwrap().iter().filter(|b| is_summary(b)).count()
    }
}

/// The harness's summary retry (`publishAttemptResult`, `runStructuralRetryWait`) for a manual compaction: a
/// retryable provider error (`Overloaded`) journals `summary.retry_wait` (attempt 2, its `notBefore` and error),
/// waits `retryDelayMs` (settings `retry.baseDelayMs`), then `summary.ready` / `summary.effect_pending` for attempt 2,
/// which commits; `retry_scheduled`, `retry_start` and `retry_end` carry the operation, the task and the attempt.
#[test]
fn cli_rpc_compaction_manual_summary_retries_a_transient_error() {
    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 1}})),
    );
    two_exchanges(&a);
    let compacted = a
        .call(session_core::core::Op::Compact { instructions: None })
        .unwrap()
        .unwrap();
    assert_eq!(compacted["summary"], "## Goal\nSummary from the model.");
    assert_eq!(a.summaries(), 2);
    let (operation, states, result) = a.last_operation();
    assert_eq!(
        states,
        [
            "summary.deciding",
            "summary.ready",
            "summary.effect_pending",
            "summary.effect_pending",
            "summary.retry_wait",
            "summary.ready",
            "summary.effect_pending",
            "summary.effect_pending",
            "summary.effect_pending"
        ]
    );
    assert_eq!(
        (&result["kind"], &result["status"]),
        (&json!("compaction"), &json!("completed"))
    );
    let waiting = a
        .writes()
        .into_iter()
        .find(|w| w["namespace"] == session_journal::run::OP_STATE && w["value"]["at"] == "summary.retry_wait")
        .unwrap()["value"]
        .clone();
    assert_eq!(waiting["nextAttempt"], 2);
    assert!(
        waiting["errorMessage"].as_str().unwrap().contains("Overloaded"),
        "{waiting}"
    );
    assert_eq!(
        waiting["summaryContext"]["retryPolicy"],
        json!({"maxAttempts": 4, "baseDelayMs": 1, "maxAgentDelayMs": 60000})
    );
    let task = waiting["task"]["taskId"].clone();
    let scheduled = a.events("retry_scheduled").pop().unwrap();
    assert_eq!(
        (
            &scheduled["runId"],
            &scheduled["step"],
            &scheduled["attempt"],
            &scheduled["maxAttempts"],
            &scheduled["delayMs"]
        ),
        (&json!(operation), &task, &json!(2), &json!(4), &json!(1))
    );
    assert_eq!(scheduled["notBefore"], waiting["notBefore"]);
    let start = a.events("retry_start").pop().unwrap();
    assert_eq!((&start["step"], &start["attempt"]), (&task, &json!(2)));
    let end = a.events("retry_end").pop().unwrap();
    assert_eq!((&end["attempt"], &end["success"]), (&json!(2), &json!(true)));
}

/// A worker killed while a summary waits to retry (`summary.retry_wait`) leaves the compaction to settle
/// `failed` (`interrupted`), nothing moved and its preparation removed; a live `abort` during the wait ends it
/// `aborted` at once, with no second request.
#[test]
fn cli_rpc_compaction_retry_wait_is_recoverable_and_abortable() {
    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 600000}})),
    );
    let (_, leaf) = two_exchanges(&a);
    let compaction = a.start(session_core::core::Op::Compact { instructions: None });
    let scheduled = a.wait_event("retry_scheduled");
    // `retryDelayMs` caps the wait at `maxAgentDelayMs`.
    assert_eq!(scheduled["delayMs"], 60000);
    assert!(a.is_compacting());
    let (recovered, session) = a.settled_copy();
    assert_eq!(recovered.status.as_deref(), Some("failed"));
    let result = session
        .value(session_journal::run::OP_RESULT, &recovered.operation_id.unwrap())
        .unwrap()
        .clone();
    assert_eq!(
        (&result["kind"], &result["error"]["code"]),
        (&json!("compaction"), &json!("interrupted"))
    );
    assert_eq!(json!(session.branch_tip("main").flatten()), leaf);
    assert!(session.state().scan_values("pi.op.preparation", "").is_empty());
    a.call(session_core::core::Op::Abort).unwrap();
    assert_eq!(
        compaction.recv_timeout(HANG).unwrap().unwrap_err(),
        "Compaction cancelled"
    );
    let (_, states, result) = a.last_operation();
    assert_eq!(states.last().unwrap(), "summary.retry_wait");
    assert_eq!(result["status"], "aborted");
    assert_eq!(a.summaries(), 1);
    assert!(a.events("retry_start").is_empty());
    assert!(!a.is_compacting());
}

/// A branch summary retries the same way, and an automatic compaction inside a run too: the run's operation goes
/// through `summary.retry_wait` and its compaction commits.
#[test]
fn cli_rpc_compaction_branch_summary_and_run_compaction_retry() {
    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 1}})),
    );
    let (target, _) = two_exchanges(&a);
    a.call(summarizing(&target, None)).unwrap().unwrap();
    let (_, states, result) = a.last_operation();
    assert!(states.contains(&"summary.retry_wait".to_string()), "{states:?}");
    assert_eq!(
        (&result["kind"], &result["status"]),
        (&json!("navigation"), &json!("completed"))
    );
    assert_eq!(a.entries().0.last().unwrap()["type"], "branch_summary");
    assert_eq!(a.events("retry_end").pop().unwrap()["success"], true);

    let a = actor_full(
        vec![reply(990_000, "Big.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 1}})),
    );
    a.prompt("first");
    let (operation, states, result) = a.last_operation();
    assert!(states.contains(&"summary.retry_wait".to_string()), "{states:?}");
    assert_eq!(
        (&result["kind"], &result["status"]),
        (&json!("run"), &json!("completed"))
    );
    // The run's compaction task waits for attempt 2, then runs it.
    let summary_states: Vec<Value> = a
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == session_journal::run::OP_STATE && w["key"] == operation.as_str())
        .map(|w| w["value"].clone())
        .filter(|v| v["at"].as_str().is_some_and(|at| at.starts_with("summary.")))
        .collect();
    let wait = summary_states
        .iter()
        .position(|v| v["at"] == "summary.retry_wait")
        .unwrap();
    assert_eq!(summary_states[wait]["nextAttempt"], 2);
    assert_eq!(summary_states[wait]["task"]["reason"], "threshold");
    assert_eq!(
        (
            &summary_states[wait + 1]["at"],
            &summary_states[wait + 1]["nextAttempt"]
        ),
        (&json!("summary.ready"), &json!(2))
    );
    assert_eq!(summary_states[wait + 2]["attempt"], 2);
    assert_eq!(a.summaries(), 2);
    assert!(a.entries().0.iter().any(|e| e["type"] == "compaction"));
}

/// No retry for an error that is not transient, nor when `retry.enabled` is off (one attempt): the compaction
/// fails with the provider's error after one request.
#[test]
fn cli_rpc_compaction_summary_retry_needs_a_transient_error_and_retries_on() {
    for (summaries, settings) in [
        (vec![invalid()], json!({"retry": {"baseDelayMs": 1}})),
        (
            vec![overloaded()],
            json!({"retry": {"enabled": false, "baseDelayMs": 1}}),
        ),
    ] {
        let a = actor_full(
            vec![reply(10, "One."), reply(10, "Two.")],
            summaries,
            false,
            Some(settings),
        );
        two_exchanges(&a);
        assert!(a.call(session_core::core::Op::Compact { instructions: None }).is_err());
        assert_eq!(a.summaries(), 1);
        let (_, states, result) = a.last_operation();
        assert!(!states.contains(&"summary.retry_wait".to_string()), "{states:?}");
        assert_eq!(result["status"], "failed");
        assert!(a.events("retry_scheduled").is_empty());
    }
}

/// The summarization retry events AgentSession emits (`_summarizationRetryCallbacks` over `retryAssistantCall`)
/// and their order: `summarization_retry_scheduled` with the retry's number (1), the retries allowed (`maxRetries`,
/// 3), the delay and the provider's error message; `summarization_retry_attempt_start` with the source (`compaction` and its
/// reason, or `branchSummary`); `summarization_retry_finished` once the retries end, also after an abort during the
/// wait (then with no attempt start).
#[test]
fn cli_rpc_compaction_summary_retry_emits_agent_session_events() {
    let retry_events = |a: &Actor| -> Vec<Value> {
        a.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"].as_str().is_some_and(|t| t.starts_with("summarization_retry")))
            .cloned()
            .collect()
    };
    let settings = Some(json!({"retry": {"baseDelayMs": 1}}));
    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        settings.clone(),
    );
    two_exchanges(&a);
    a.call(session_core::core::Op::Compact { instructions: None })
        .unwrap()
        .unwrap();
    assert_eq!(
        retry_events(&a),
        [
            json!({"type": "summarization_retry_scheduled", "lane": "main", "attempt": 1, "maxAttempts": 3, "delayMs": 1, "errorMessage": OVERLOADED_ERROR}),
            json!({"type": "summarization_retry_attempt_start", "lane": "main", "source": "compaction", "reason": "manual"}),
            json!({"type": "summarization_retry_finished", "lane": "main"}),
        ]
    );

    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        settings.clone(),
    );
    let (target, _) = two_exchanges(&a);
    a.call(summarizing(&target, None)).unwrap().unwrap();
    assert_eq!(
        retry_events(&a)[1],
        json!({"type": "summarization_retry_attempt_start", "lane": "main", "source": "branchSummary"})
    );

    let a = actor_full(vec![reply(990_000, "Big.")], vec![overloaded()], false, settings);
    a.prompt("first");
    assert_eq!(
        retry_events(&a)[1],
        json!({"type": "summarization_retry_attempt_start", "lane": "main", "source": "compaction", "reason": "threshold"})
    );

    let a = actor_full(
        vec![reply(10, "One."), reply(10, "Two.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 600000, "maxRetries": 1}})),
    );
    two_exchanges(&a);
    let compaction = a.start(session_core::core::Op::Compact { instructions: None });
    let scheduled = a.wait_event("summarization_retry_scheduled");
    assert_eq!(
        (&scheduled["maxAttempts"], &scheduled["delayMs"]),
        (&json!(1), &json!(60000))
    );
    a.call(session_core::core::Op::Abort).unwrap();
    compaction.recv_timeout(HANG).unwrap().unwrap_err();
    let types: Vec<Value> = retry_events(&a).iter().map(|e| e["type"].clone()).collect();
    assert_eq!(
        types,
        [
            json!("summarization_retry_scheduled"),
            json!("summarization_retry_finished")
        ]
    );
}

/// Over RPC the summarization retry events reach the client as AgentSession emits them, between the
/// compaction's `compaction_start` and `compaction_end`.
#[test]
fn cli_rpc_compaction_summary_retry_events_on_rpc() {
    let mut f = start_full(
        vec![reply(10, "Hello.")],
        vec![overloaded()],
        false,
        Some(json!({"retry": {"baseDelayMs": 1}})),
    );
    f.client.prompt_and_wait("hello", HANG).unwrap();
    f.client.mark();
    let compact = f.client.send(json!({"type": "compact"})).unwrap();
    assert_eq!(compact["success"], true, "{compact}");
    f.client.wait_for(&|l| l["type"] == "compaction_end", HANG).unwrap();
    let shown: Vec<Value> = f
        .client
        .transcript
        .iter()
        .filter(|l| {
            l["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("summarization_retry") || t.starts_with("compaction_"))
        })
        .cloned()
        .collect();
    let types: Vec<&str> = shown.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "compaction_start",
            "summarization_retry_scheduled",
            "summarization_retry_attempt_start",
            "summarization_retry_finished",
            "compaction_end"
        ]
    );
    assert_eq!(
        shown[1],
        json!({"type": "summarization_retry_scheduled", "attempt": 1, "maxAttempts": 3, "delayMs": 1, "errorMessage": OVERLOADED_ERROR})
    );
    assert_eq!(
        shown[2],
        json!({"type": "summarization_retry_attempt_start", "source": "compaction", "reason": "manual"})
    );
    assert_eq!(shown[3], json!({"type": "summarization_retry_finished"}));
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}

/// rpc-native.ts:256-261: `new_session` during a run creates a session and switches to it; the old turn keeps
/// running, detached (nothing aborts it). Before, Rust refused with "Agent is already processing". Switching back
/// (`switch_session`, also allowed during a run) finds it still streaming, and an abort there ends it.
#[test]
fn cli_rpc_new_session_during_a_run_switches_and_the_old_turn_runs_on() {
    let mut f = start(vec![HOLD_MODEL.into(), reply(10, "Fresh.")], false);
    f.client
        .write_line(&json!({"type": "prompt", "message": "first", "id": "p1"}).to_string())
        .unwrap();
    assert_eq!(f.held(), Held::Seen);
    let before = f.client.get_state().unwrap();
    assert_eq!(before["isStreaming"], true, "{before}");
    let created = f.client.send(json!({"type": "new_session", "id": "n1"})).unwrap();
    assert_eq!(created["success"], true, "{created}");
    assert_eq!(created["data"], json!({"cancelled": false}));
    let state = f.client.get_state().unwrap();
    assert_ne!(state["sessionId"], before["sessionId"]);
    assert_eq!(
        (&state["isStreaming"], &state["messageCount"]),
        (&json!(false), &json!(0)),
        "{state}"
    );
    // The new session runs its own turn while the old one is held.
    f.client.prompt_and_wait("fresh", HANG).unwrap();
    assert_eq!(f.client.get_last_assistant_text().unwrap().as_deref(), Some("Fresh."));
    let switched = f
        .client
        .send(json!({"type": "switch_session", "sessionPath": before["sessionId"]}))
        .unwrap();
    assert_eq!(switched["success"], true, "{switched}");
    let back = f.client.get_state().unwrap();
    assert_eq!(back["sessionId"], before["sessionId"]);
    assert_eq!(back["isStreaming"], true, "the old turn still runs: {back}");
    // Attached to a running session again, a prompt without streamingBehavior is refused as in TS.
    let refused = f.client.send(json!({"type": "prompt", "message": "again"})).unwrap();
    assert_eq!(
        refused["error"],
        "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
    );
    assert_eq!(f.client.send(json!({"type": "abort"})).unwrap()["success"], true);
    assert_eq!(f.held(), Held::Dropped, "only the abort ended the held response");
    let (_, code) = f.client.finish(HANG);
    assert_eq!(code, Some(0));
}
