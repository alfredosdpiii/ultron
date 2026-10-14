//! How autoreview runs `gh` and `git` (`autoreview/runner.ts`): one injected [`Runner`], so tests replace GitHub
//! with a fake and assert on every call. A token reaches a child process only through `env`, never `argv`.
//!
//! Known differences from the TypeScript, outside recorded behaviour:
//! - The 64 MiB cap counts bytes per read (64 KiB reads) where Node counted per `data` chunk, so the bytes kept just
//!   below the cap can differ; past the cap the child is killed in both, and its exit code is then 1.
//! - A spawn failure's message is `<cmd>: spawn <cmd> ENOENT` for a missing binary (Node's text), else the OS error.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Variables added to a child's environment.
pub type Env = BTreeMap<String, String>;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub cwd: Option<String>,
    /// Added to the process environment of the child.
    pub env: Env,
    /// Written to the child's stdin.
    pub input: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs one process to completion. Never fails: a missing binary is code 127, a timeout 124.
pub trait Runner: Send + Sync {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult;
}

/// `argv` as owned strings.
pub fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| part.to_string()).collect()
}

/// Real processes: pagers, prompts, update notices and colour off; `input` on stdin; a deadline and an output cap.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

enum Event {
    Closed,
    Overflow,
}

fn reader(
    mut source: impl Read + Send + 'static,
    total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    events: mpsc::Sender<Event>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let size = total.fetch_add(read, std::sync::atomic::Ordering::SeqCst) + read;
                    if size <= MAX_OUTPUT_BYTES {
                        kept.extend_from_slice(&buffer[..read]);
                    } else {
                        let _ = events.send(Event::Overflow);
                    }
                }
            }
        }
        let _ = events.send(Event::Closed);
        kept
    })
}

impl Runner for ProcessRunner {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        let Some(command) = argv.first() else {
            return RunResult {
                code: 127,
                stdout: String::new(),
                stderr: "empty command".into(),
            };
        };
        let mut child = Command::new(command);
        child
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &options.cwd {
            child.current_dir(cwd);
        }
        for (key, value) in [
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GH_PROMPT_DISABLED", "1"),
            ("GH_NO_UPDATE_NOTIFIER", "1"),
            ("NO_COLOR", "1"),
        ] {
            child.env(key, value);
        }
        child.envs(&options.env);
        let mut child = match child.spawn() {
            Ok(child) => child,
            Err(error) => {
                let not_found = error.kind() == std::io::ErrorKind::NotFound;
                return RunResult {
                    code: if not_found { 127 } else { 1 },
                    stdout: String::new(),
                    stderr: if not_found {
                        format!("{command}: spawn {command} ENOENT")
                    } else {
                        format!("{command}: {error}")
                    },
                };
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let input = options.input.clone().unwrap_or_default();
            // A child that does not read its input must not block the run.
            std::thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }
        let total = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (events, received) = mpsc::channel();
        let stdout = child
            .stdout
            .take()
            .map(|out| reader(out, total.clone(), events.clone()));
        let stderr = child
            .stderr
            .take()
            .map(|err| reader(err, total.clone(), events.clone()));
        drop(events);
        let timeout_ms = options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut open = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
        let mut timed_out = false;
        let mut killed = false;
        // Like Node's `close`: the run ends when the output pipes have closed, then the exit status is read.
        while open > 0 {
            let event = if killed {
                received.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            } else {
                received.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            };
            match event {
                Ok(Event::Closed) => open -= 1,
                Ok(Event::Overflow) => {
                    if !killed {
                        let _ = child.kill();
                        killed = true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    timed_out = true;
                    let _ = child.kill();
                    killed = true;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = child.wait().ok();
        let text = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
            String::from_utf8_lossy(&handle.and_then(|h| h.join().ok()).unwrap_or_default()).into_owned()
        };
        let (stdout, stderr) = (text(stdout), text(stderr));
        RunResult {
            code: if timed_out {
                124
            } else {
                status.and_then(|status| status.code()).unwrap_or(1)
            },
            stdout,
            stderr: if timed_out {
                format!(
                    "{command}: timed out after {}s",
                    crate::js::number(crate::js::round(timeout_ms as f64 / 1000.0))
                )
            } else {
                stderr
            },
        }
    }
}
