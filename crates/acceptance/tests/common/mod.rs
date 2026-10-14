//! Shared by the acceptance tests: the real runtime.py, kernels over recording handlers, process-tree checks, a
//! scripted OpenAI-compatible provider over real HTTP, and a runner for the `ultron` binary in private directories.
#![allow(dead_code)]

use kernel::{ExecutionResult, HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

pub fn kernel_in(cwd: &Path, handler: Arc<dyn HostHandler>) -> Kernel {
    Kernel::new(KernelOptions::new(cwd, runtime()), handler).expect("kernel options")
}

pub fn null_handler() -> Arc<dyn HostHandler> {
    Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null))
}

/// Records every host request type, then answers it with `answer`.
pub struct Recorder<F> {
    pub kinds: Arc<Mutex<Vec<String>>>,
    answer: F,
}

impl<F> Recorder<F>
where
    F: Fn(&str, Map<String, Value>, &Signal) -> Result<Value, String> + Send + Sync,
{
    pub fn new(answer: F) -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let kinds = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Recorder {
                kinds: kinds.clone(),
                answer,
            }),
            kinds,
        )
    }
}

impl<F> HostHandler for Recorder<F>
where
    F: Fn(&str, Map<String, Value>, &Signal) -> Result<Value, String> + Send + Sync,
{
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self.kinds.lock().unwrap().push(kind.to_string());
        (self.answer)(kind, payload, signal)
    }
}

pub fn run(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"))
}

/// The result text of a cell that must succeed.
pub fn value(kernel: &Kernel, code: &str) -> String {
    let result = run(kernel, code);
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

/// A running process; a zombie awaiting its reaper runs nothing.
pub fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

/// The deadline of every wait in these tests: a hang guard only, never a timing expectation. A test may slow down
/// under load but must not fail because of it.
pub const HANG: Duration = Duration::from_secs(120);

/// Polls `condition` until it holds; fails only after the [`HANG`] guard.
pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Waits for `path` to hold `count` whitespace-separated pids (or a JSON object of pids).
pub fn read_pids(path: &Path, count: usize) -> Vec<u32> {
    let mut pids = Vec::new();
    wait_until("the cell's pid file", || {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        pids = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map.values().filter_map(Value::as_u64).map(|p| p as u32).collect(),
            _ => text.split_whitespace().filter_map(|p| p.parse().ok()).collect(),
        };
        pids.len() == count && pids.iter().all(|pid| *pid > 0)
    });
    pids
}

/// One request the scripted provider received.
#[derive(Clone, Debug)]
pub struct Request {
    pub raw: String,
    pub body: Value,
}

impl Request {
    pub fn system(&self) -> String {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "system" || m["role"] == "developer")
            .map(|m| content_text(&m["content"]))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_frame(&self) -> bool {
        self.system().contains("inference frame")
    }

    pub fn users(&self) -> Vec<String> {
        self.messages("user")
    }

    /// Text of every message with `role`, in order.
    pub fn messages(&self, role: &str) -> Vec<String> {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == role)
            .map(|m| content_text(&m["content"]))
            .collect()
    }

    /// The `rlm` tool results this root request carries, in order.
    pub fn tool_results(&self) -> Vec<String> {
        self.messages("tool")
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A reply of the scripted provider.
pub enum Reply {
    Text(String),
    Rlm(String),
    /// An `rlm` call with its own call id (tests that tell cells apart by their tool call ids).
    RlmCall(String, String),
    /// Hold the response until the client closes the connection (a request cancelled mid-flight); after the
    /// [`HANG`] guard, answer with this text (the TS `delayMs` reply).
    Hang(String),
}

/// An OpenAI-compatible chat-completions endpoint on 127.0.0.1 that answers each request with `script(request)`
/// as an SSE stream, one connection per thread, and records every request.
pub struct Provider {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl Provider {
    pub fn start(script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Provider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let script = Arc::new(script);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let seen = seen.clone();
                let script = script.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = header.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let raw = String::from_utf8(body).unwrap();
                    let request = Request {
                        body: serde_json::from_str(&raw).unwrap(),
                        raw,
                    };
                    seen.lock().unwrap().push(request.clone());
                    let reply = match script(&request) {
                        Reply::Hang(text) => {
                            let _ = stream.set_read_timeout(Some(HANG));
                            let mut rest = [0u8; 256];
                            loop {
                                match reader.read(&mut rest) {
                                    Ok(0) => return,
                                    Ok(_) => continue,
                                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                    Err(e)
                                        if matches!(
                                            e.kind(),
                                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                        ) =>
                                    {
                                        break Reply::Text(text);
                                    }
                                    Err(_) => return,
                                }
                            }
                        }
                        reply => reply,
                    };
                    let sse = sse(reply);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        sse.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(sse.as_bytes());
                });
            }
        });
        Provider { port, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn roots(&self) -> Vec<Request> {
        self.requests().into_iter().filter(|r| !r.is_frame()).collect()
    }

    pub fn frames(&self) -> Vec<Request> {
        self.requests().into_iter().filter(Request::is_frame).collect()
    }
}

fn chunk(delta: Value, finish: Value, usage: Option<Value>) -> String {
    let mut event = json!({
        "id": "chatcmpl-acceptance", "object": "chat.completion.chunk", "created": 1, "model": "llama-3.3-70b-versatile",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(usage) = usage {
        event["usage"] = usage;
    }
    format!("data: {event}\n\n")
}

fn sse(reply: Reply) -> String {
    let usage = json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15});
    let (id, code) = match reply {
        Reply::Text(text) => {
            let mut out = chunk(json!({"role": "assistant", "content": text}), Value::Null, None)
                + &chunk(json!({}), json!("stop"), Some(usage));
            out.push_str("data: [DONE]\n\n");
            return out;
        }
        Reply::Hang(_) => {
            unreachable!("a held reply is answered by the connection thread")
        }
        Reply::Rlm(code) => ("call_acceptance".to_string(), code),
        Reply::RlmCall(id, code) => (id, code),
    };
    let call = json!([{"index": 0, "id": id, "type": "function",
        "function": {"name": "rlm", "arguments": json!({"code": code}).to_string()}}]);
    let mut out = chunk(
        json!({"role": "assistant", "content": null, "tool_calls": call}),
        Value::Null,
        None,
    ) + &chunk(json!({}), json!("tool_calls"), Some(usage));
    out.push_str("data: [DONE]\n\n");
    out
}

/// Private agent, server and project directories for one `ultron` run, with `models.json` pointing the built-in
/// `groq/llama-3.3-70b-versatile` (an OpenAI-completions model) at the scripted provider.
pub struct Install {
    pub root: tempfile::TempDir,
}

pub const MODEL: &str = "groq/llama-3.3-70b-versatile";

impl Install {
    pub fn new(provider: &Provider) -> Install {
        let root = tempfile::tempdir().unwrap();
        let install = Install { root };
        std::fs::create_dir_all(install.agent()).unwrap();
        std::fs::create_dir_all(install.project()).unwrap();
        std::fs::write(
            install.agent().join("models.json"),
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{}/v1", provider.port)}}}).to_string(),
        )
        .unwrap();
        install
    }

    pub fn agent(&self) -> PathBuf {
        self.root.path().join("agent")
    }

    pub fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    /// `ultron -p --no-session --model groq/... <prompt>` in the project directory with only `env` added to a
    /// minimal environment.
    pub fn prompt(&self, prompt: &str, env: &[(&str, &str)]) -> Output {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ultron-acceptance"));
        command
            .env_clear()
            .args(["-p", "--no-session", "--model", MODEL, prompt])
            .current_dir(self.project())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.path())
            .env("ULTRON_CODING_AGENT_DIR", self.agent())
            .env("ULTRON_SERVER_DIR", self.root.path().join("server"))
            .env("GROQ_API_KEY", "test-groq")
            .env("NO_COLOR", "1")
            .env("TZ", "UTC");
        if let Ok(python) = std::env::var("ULTRON_PYTHON") {
            command.env("ULTRON_PYTHON", python);
        }
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }
}

/// The printed text of a successful run.
pub fn stdout_of(output: &Output) -> String {
    assert!(
        output.status.success(),
        "ultron failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// The minimal environment of every `ultron` run of an [`Install`], plus `env`.
fn base_env(install: &Install, env: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), install.root.path().display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), install.agent().display().to_string()),
        (
            "ULTRON_SERVER_DIR".into(),
            install.root.path().join("server").display().to_string(),
        ),
        ("GROQ_API_KEY".into(), "test-groq".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
        ("NO_COLOR".into(), "1".into()),
        ("TZ".into(), "UTC".into()),
    ];
    if let Ok(python) = std::env::var("ULTRON_PYTHON") {
        out.push(("ULTRON_PYTHON".into(), python));
    }
    out.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    out
}

impl Install {
    /// `ultron --mode rpc --model groq/... <args>` in the project directory (the shipped path: this process's
    /// server generation and a session worker), driven through the Rust `RpcClient`.
    pub fn rpc(&self, args: &[&str], env: &[(&str, &str)]) -> cli::rpc::client::RpcClient {
        let options = cli::rpc::client::RpcClientOptions {
            program: env!("CARGO_BIN_EXE_ultron-acceptance").into(),
            cwd: Some(self.project()),
            env_clear: true,
            env: base_env(self, env),
            model: Some(MODEL.into()),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };
        cli::rpc::client::RpcClient::start(&options).expect("start ultron --mode rpc")
    }

    /// A private sessions directory for `--session-dir`.
    pub fn sessions(&self) -> PathBuf {
        self.root.path().join("sessions")
    }
}

/// Every `toolResult` text of the `rlm` tool in RPC `get_messages` output, in order.
pub fn rpc_tool_results(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m["role"] == "toolResult")
        .map(|m| content_text(&m["content"]))
        .collect()
}
