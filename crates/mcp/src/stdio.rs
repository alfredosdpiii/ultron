//! Newline-delimited JSON-RPC over a child process's stdin/stdout (`transports/stdio.ts`).
//!
//! The server runs in its own process group, so closing the transport also ends the processes it started
//! (`npx`, `uvx` wrappers). Shutdown follows the spec: close stdin, wait, SIGTERM the group, wait, SIGKILL.

use crate::jsonrpc::{Error, parse_message};
use crate::{Incoming, Sink, Transport};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a server gets to exit on its own after stdin closes, before it is sent SIGTERM.
const STDIN_CLOSE_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Default)]
pub struct StdioOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Added to the host's environment, or the whole environment when `inherit_env` is false.
    pub env: Vec<(String, String)>,
    /// Default (None): true.
    pub inherit_env: Option<bool>,
    pub max_message_bytes: Option<usize>,
    pub max_stderr_bytes: Option<usize>,
    /// Time to wait after SIGTERM before SIGKILL. Default: 2 s.
    pub close_timeout: Option<Duration>,
}

#[derive(Default)]
struct State {
    started: bool,
    closed: bool,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
}

pub struct StdioTransport {
    options: StdioOptions,
    state: Mutex<State>,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Set once the child has exited and its stdout is drained.
    exited: Arc<(Mutex<bool>, Condvar)>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn signal_group(pid: u32, signal: Signal) {
    if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(pid, signal);
    }
}

/// Splits complete lines off `buffer` and turns each into a message or an error, like `handleStdout`.
fn drain_lines(buffer: &mut Vec<u8>, max: usize, emit: &dyn Fn(Incoming)) {
    loop {
        let Some(newline) = buffer.iter().position(|&b| b == b'\n') else {
            if buffer.len() > max {
                buffer.clear();
                emit(Incoming::Error(format!("MCP stdio message exceeds {max} bytes")));
            }
            return;
        };
        let line: Vec<u8> = buffer.drain(..=newline).take(newline).collect();
        if line.len() > max {
            emit(Incoming::Error(format!("MCP stdio message exceeds {max} bytes")));
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(text)
            .map_err(|e| Error::Other(e.to_string()))
            .and_then(parse_message)
        {
            Ok(message) => emit(Incoming::Message(message)),
            Err(error) => emit(Incoming::Error(error.to_string())),
        }
    }
}

impl StdioTransport {
    pub fn new(options: StdioOptions) -> Self {
        Self {
            options,
            state: Mutex::new(State::default()),
            stderr: Arc::new(Mutex::new(Vec::new())),
            exited: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        lock(&self.state).pid
    }

    /// The last `max_stderr_bytes` the server wrote to stderr.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&lock(&self.stderr)).into_owned()
    }

    /// Waits up to `timeout` for the child to exit; true when it has.
    fn wait_exit(&self, timeout: Duration) -> bool {
        let (done, condvar) = &*self.exited;
        let end = Instant::now() + timeout;
        let mut guard = lock(done);
        while !*guard {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            guard = condvar.wait_timeout(guard, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        true
    }
}

impl Transport for StdioTransport {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        let mut state = lock(&self.state);
        if state.started {
            return Err(Error::Other("MCP stdio transport already started".into()));
        }
        if state.closed {
            return Err(Error::closed());
        }
        state.started = true;
        let mut command = Command::new(&self.options.command);
        command
            .args(&self.options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group, so closing the transport can terminate the server's children too.
        command.process_group(0);
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        if self.options.inherit_env == Some(false) {
            command.env_clear();
        }
        command.envs(self.options.env.iter().map(|(k, v)| (k, v)));
        let mut child = command
            .spawn()
            .map_err(|e| Error::Other(format!("spawn {} failed: {e}", self.options.command)))?;
        state.pid = Some(child.id());
        state.stdin = child.stdin.take();
        drop(state);

        if let Some(mut stderr) = child.stderr.take() {
            let buffer = self.stderr.clone();
            let max = self.options.max_stderr_bytes.unwrap_or(DEFAULT_MAX_STDERR_BYTES);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                    let mut buffer = lock(&buffer);
                    buffer.extend_from_slice(&chunk[..n]);
                    if buffer.len() > max {
                        let excess = buffer.len() - max;
                        buffer.drain(..excess);
                    }
                }
            });
        }
        let mut stdout = child.stdout.take();
        let max = self.options.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        let exited = self.exited.clone();
        let close_sink = sink.clone();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut chunk = vec![0u8; 65536];
            if let Some(stdout) = stdout.as_mut() {
                loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buffer.extend_from_slice(&chunk[..n]);
                            drain_lines(&mut buffer, max, &*sink);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            sink(Incoming::Error(e.to_string()));
                            break;
                        }
                    }
                }
            }
            let _ = child.wait();
            if !String::from_utf8_lossy(&buffer).trim().is_empty() {
                sink(Incoming::Error(
                    "MCP stdio server closed with an incomplete JSON-RPC message".into(),
                ));
            }
            let (done, condvar) = &*exited;
            *lock(done) = true;
            condvar.notify_all();
            close_sink(Incoming::Closed);
        });
        Ok(())
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        let mut state = lock(&self.state);
        let ready = state.started && !state.closed;
        let Some(stdin) = state.stdin.as_mut().filter(|_| ready) else {
            return Err(Error::closed());
        };
        let mut payload = message.to_string();
        payload.push('\n');
        stdin
            .write_all(payload.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| Error::Other(e.to_string()))
    }

    fn close(&self) {
        let (pid, stdin) = {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            state.closed = true;
            (state.pid, state.stdin.take())
        };
        // Never started: there is nothing to stop and no reader to report the close.
        let Some(pid) = pid else { return };
        drop(stdin);
        let close_timeout = self.options.close_timeout.unwrap_or(DEFAULT_CLOSE_TIMEOUT);
        if !self.wait_exit(STDIN_CLOSE_GRACE.min(close_timeout)) {
            signal_group(pid, Signal::TERM);
            if !self.wait_exit(close_timeout) {
                signal_group(pid, Signal::KILL);
                while !self.wait_exit(Duration::from_secs(60)) {}
            }
        }
        // Children of the server that ignored stdin closing would otherwise outlive it.
        signal_group(pid, Signal::TERM);
    }
}
