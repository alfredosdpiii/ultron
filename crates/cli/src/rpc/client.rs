//! `modes/rpc/rpc-client.ts`: drive an agent process in RPC mode. Commands get `req_<n>` ids and wait for their
//! response; every other line is an event. A process that exits fails the waiting request with its exit status.

use super::jsonl::{JsonlLines, serialize_json_line};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long [`RpcClient::stop`] waits for the agent to exit before killing it: a hang guard, not a grace period.
pub const STOP_HANG_GUARD: Duration = Duration::from_secs(120);

/// `RpcClientOptions`; `program` runs with `program_args`, then `--mode rpc`, the model and `args`.
#[derive(Clone, Debug, Default)]
pub struct RpcClientOptions {
    pub program: PathBuf,
    pub program_args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Start from an empty environment instead of this process's.
    pub env_clear: bool,
    pub env: Vec<(String, String)>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub args: Vec<String>,
}

enum Line {
    Record(Value),
    Closed,
}

pub struct RpcClient {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Line>,
    stderr: Arc<Mutex<String>>,
    request_id: u64,
    /// Every line received, in order.
    pub transcript: Vec<Value>,
    exit_error: Option<String>,
    /// Lines not yet consumed by a wait.
    cursor: usize,
}

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

impl RpcClient {
    /// `start()`.
    pub fn start(options: &RpcClientOptions) -> Result<Self, String> {
        let mut command = Command::new(&options.program);
        command.args(&options.program_args).args(["--mode", "rpc"]);
        if let Some(provider) = &options.provider {
            command.args(["--provider", provider]);
        }
        if let Some(model) = &options.model {
            command.args(["--model", model]);
        }
        command.args(&options.args);
        if options.env_clear {
            command.env_clear();
        }
        command.envs(options.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Agent process error: {e}"))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut stderr_pipe = child.stderr.take().ok_or("no stderr")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let collected = stderr.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = stderr_pipe.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                collected
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in JsonlLines::new(stdout).map_while(Result::ok) {
                // Non-JSON lines are ignored.
                if let Ok(value) = serde_json::from_str(&line)
                    && tx.send(Line::Record(value)).is_err()
                {
                    return;
                }
            }
            let _ = tx.send(Line::Closed);
        });
        let stdin = child.stdin.take();
        Ok(RpcClient {
            child,
            stdin,
            lines,
            stderr,
            request_id: 0,
            transcript: Vec::new(),
            exit_error: None,
            cursor: 0,
        })
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn exit_error(&mut self) -> String {
        if let Some(error) = &self.exit_error {
            return error.clone();
        }
        let status = self.child.wait().ok();
        let code = status.and_then(|s| s.code()).map_or("null".into(), |c| c.to_string());
        let signal = status.and_then(|s| s.signal()).map_or("null".into(), |s| s.to_string());
        let error = format!(
            "Agent process exited (code={code} signal={signal}). Stderr: {}",
            self.stderr()
        );
        self.exit_error = Some(error.clone());
        error
    }

    /// Receive lines until one at or after the cursor matches; returns its index.
    fn wait_line(&mut self, matches: &dyn Fn(&Value) -> bool, timeout: Duration) -> Result<usize, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(offset) = self.transcript[self.cursor..].iter().position(matches) {
                return Ok(self.cursor + offset);
            }
            if self.exit_error.is_some() {
                return Err(self.exit_error());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(Line::Record(value)) => self.transcript.push(value),
                Ok(Line::Closed) | Err(RecvTimeoutError::Disconnected) => return Err(self.exit_error()),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("Timeout waiting for a line. Stderr: {}", self.stderr()));
                }
            }
        }
    }

    /// Write one raw line to the agent's stdin.
    pub fn write_line(&mut self, line: &str) -> Result<(), String> {
        if let Some(error) = &self.exit_error {
            return Err(error.clone());
        }
        let stdin = self.stdin.as_mut().ok_or("Client not started")?;
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush());
        written.map_err(|e| format!("Agent process stdin is not writable: {e}. Stderr: {}", self.stderr()))
    }

    /// `send`: the command with the next `req_<n>` id; waits for its response.
    pub fn send(&mut self, command: Value) -> Result<Value, String> {
        self.request_id += 1;
        let id = format!("req_{}", self.request_id);
        let mut command = command;
        command["id"] = id.clone().into();
        self.request(command)
    }

    /// Send a command as is (its own `id`, if any) and wait for the response carrying that id.
    pub fn request(&mut self, command: Value) -> Result<Value, String> {
        let id = command.get("id").cloned();
        let line = serialize_json_line(&command);
        self.write_line(line.trim_end_matches('\n'))?;
        let index = self.wait_line(
            &|line| line["type"] == "response" && line.get("id") == id.as_ref(),
            RESPONSE_TIMEOUT,
        )?;
        Ok(self.transcript[index].clone())
    }

    /// Wait for a line matching `matches` that arrived since the last [`mark`](Self::mark).
    pub fn wait_for(&mut self, matches: &dyn Fn(&Value) -> bool, timeout: Duration) -> Result<Value, String> {
        let index = self.wait_line(matches, timeout)?;
        Ok(self.transcript[index].clone())
    }

    /// Later waits only see lines from here on.
    pub fn mark(&mut self) {
        self.cursor = self.transcript.len();
    }

    /// `getData`: the response's data, or its error.
    pub fn data(response: &Value) -> Result<Value, String> {
        if response["success"] != true {
            return Err(response["error"].as_str().unwrap_or("request failed").to_string());
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }

    fn call(&mut self, command: Value) -> Result<Value, String> {
        let response = self.send(command)?;
        Self::data(&response)
    }

    /// `promptAndWait`: prompt, then collect events until `agent_settled`.
    pub fn prompt_and_wait(&mut self, message: &str, timeout: Duration) -> Result<Vec<Value>, String> {
        let from = self.transcript.len();
        self.mark();
        self.call(json!({"type": "prompt", "message": message}))?;
        self.wait_for(&|line| line["type"] == "agent_settled", timeout)?;
        let events = self.transcript[from..]
            .iter()
            .filter(|line| line["type"] != "response")
            .cloned()
            .collect();
        self.mark();
        Ok(events)
    }

    pub fn get_state(&mut self) -> Result<Value, String> {
        self.call(json!({"type": "get_state"}))
    }

    pub fn get_messages(&mut self) -> Result<Vec<Value>, String> {
        Ok(self.call(json!({"type": "get_messages"}))?["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    pub fn get_last_assistant_text(&mut self) -> Result<Option<String>, String> {
        Ok(self.call(json!({"type": "get_last_assistant_text"}))?["text"]
            .as_str()
            .map(str::to_string))
    }

    pub fn bash(&mut self, command: &str) -> Result<Value, String> {
        self.call(json!({"type": "bash", "command": command}))
    }

    pub fn clear_queue(&mut self) -> Result<Value, String> {
        self.call(json!({"type": "clear_queue"}))
    }

    pub fn clone_session(&mut self) -> Result<Value, String> {
        self.call(json!({"type": "clone"}))
    }

    pub fn set_session_name(&mut self, name: &str) -> Result<(), String> {
        self.call(json!({"type": "set_session_name", "name": name})).map(drop)
    }

    pub fn get_commands(&mut self) -> Result<Value, String> {
        self.call(json!({"type": "get_commands"}))
    }

    /// `stop()`: close stdin and wait for the exit. The agent exits on its own once its input closes; the kill after
    /// [`STOP_HANG_GUARD`] only guards a hung agent. (A 10 s grace killed agents still flushing on a loaded machine,
    /// so a clean exit read as `None`.)
    pub fn stop(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + STOP_HANG_GUARD;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                return self.child.wait().ok().and_then(|s| s.code());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Drain every remaining line until the agent closes stdout.
    pub fn drain(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while let Ok(Line::Record(value)) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.transcript.push(value);
        }
    }

    /// Close stdin, collect the remaining lines, and return the exit code.
    pub fn finish(mut self, timeout: Duration) -> (Vec<Value>, Option<i32>) {
        drop(self.stdin.take());
        self.drain(timeout);
        let transcript = std::mem::take(&mut self.transcript);
        (transcript, self.stop())
    }
}
