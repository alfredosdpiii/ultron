//! Shared helpers for the daemon process tests: a scripted Anthropic SSE server, private temp homes, the Rust server
//! process, and a check that every process a test started is gone.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// `textReply` of the fixture generators.
pub fn text_reply(text: &str) -> String {
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ])
}

/// A scripted provider: the n-th request gets `replies[n]`; request bodies are recorded. A reply may be delayed.
pub struct Provider {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

pub fn provider(replies: Vec<String>, delay: Duration) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let reply = replies
                .get(i)
                .cloned()
                .unwrap_or_else(|| text_reply("(no more replies)"));
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
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
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                std::thread::sleep(delay);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    Provider { port, requests }
}

/// A private home: `agent/` (with models.json pointing at the provider), `server/` and `cwd/`.
pub struct Home {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
}

impl Home {
    pub fn new(port: u16) -> Home {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::fs::create_dir_all(root.join("cwd")).unwrap();
        std::fs::write(
            root.join("agent/models.json"),
            format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
        )
        .unwrap();
        // These tests script the provider's replies in order; the session-naming request (a Rust addition, run
        // beside the first turn) would take one of them.
        std::fs::write(
            root.join("agent/settings.json"),
            r#"{"sessionNaming":{"enabled":false}}"#,
        )
        .unwrap();
        Home { tmp, root }
    }

    pub fn agent(&self) -> PathBuf {
        self.root.join("agent")
    }

    pub fn server_dir(&self) -> PathBuf {
        self.root.join("server")
    }

    pub fn cwd(&self) -> PathBuf {
        self.root.join("cwd")
    }

    pub fn sessions(&self) -> PathBuf {
        self.agent().join("experimental/sessions")
    }

    /// The private environment every process of a test runs with.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .args(args)
            .env_clear()
            .current_dir(self.cwd())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("ULTRON_CODING_AGENT_DIR", self.agent())
            .env("ULTRON_SERVER_DIR", self.server_dir())
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("NO_COLOR", "1")
            .env("TZ", "UTC");
        command
    }

    /// `ultron -p` (or any arguments), waited for.
    pub fn run(&self, args: &[&str]) -> std::process::Output {
        self.command(args).output().unwrap()
    }

    /// A command without `ULTRON_SERVER_DIR`, in `cwd`: it uses the project (or durable) instance under `HOME`.
    pub fn instance_command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = self.command(args);
        command.env_remove("ULTRON_SERVER_DIR").current_dir(cwd);
        command
    }
}

pub const SERVER_ID: &str = "00000000-0000-4000-8000-0000000000aa";

/// A detached Rust server process for `home` (`runServerProcess`, held open), reachable at its public socket.
pub struct ServerProcess {
    pub child: Child,
    pub server_id: String,
    pub socket: PathBuf,
}

pub fn start_server(home: &Home, server_id: &str) -> ServerProcess {
    let model = r#"{"model":"anthropic/claude-sonnet-4-5"}"#;
    let child = home
        .command(&[
            "__internal",
            "server",
            home.server_dir().to_str().unwrap(),
            server_id,
            home.sessions().to_str().unwrap(),
            model,
        ])
        .env("__ULTRON_SERVER_KEEP_ALIVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let socket = home.server_dir().join(format!("{server_id}.sock"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while client::runtime::Connection::connect(&socket, server_id).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    ServerProcess {
        child,
        server_id: server_id.to_string(),
        socket,
    }
}

impl ServerProcess {
    pub fn route(&self) -> client::runtime::Route {
        client::runtime::Route {
            server_id: self.server_id.clone(),
            path: self.socket.clone(),
        }
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// PIDs of live processes whose command line mentions `needle` (the test's private temp dir).
pub fn processes_mentioning(needle: &Path) -> Vec<u32> {
    let needle = needle.display().to_string();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        // A zombie has no command line left to match; skip exited processes awaiting their parent.
        if stat.split_whitespace().nth(2) == Some("Z") {
            continue;
        }
        if String::from_utf8_lossy(&cmdline).contains(&needle) {
            found.push(pid);
        }
    }
    found
}

/// Workers' option arguments carry their session path, so a session's worker is found by the temp root as well.
pub fn wait_gone(root: &Path, timeout: Duration) -> Vec<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = processes_mentioning(root);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Fail if anything this test started is still running, after killing it.
pub fn assert_no_processes(root: &Path, timeout: Duration) {
    assert_none_mentioning(&[root.to_path_buf()], timeout);
}

/// Like [`assert_no_processes`], for processes found by any of `needles` (temp roots, socket directories).
pub fn assert_none_mentioning(needles: &[PathBuf], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut left: Vec<u32>;
    loop {
        left = needles.iter().flat_map(|n| processes_mentioning(n)).collect();
        if left.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for pid in &left {
        if let Some(pid) = rustix::process::Pid::from_raw(*pid as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
    assert!(left.is_empty(), "processes left running: {left:?}");
}
