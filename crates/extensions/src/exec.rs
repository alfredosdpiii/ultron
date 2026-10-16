//! `host/exec`: `pi.exec(command, args, { cwd, timeout, signal })` (`execCommand` of core/exec.ts). The command runs
//! without a shell in `cwd` (relative to the session directory), stdin closed; its output is collected whole. A
//! timeout or the request's cancellation sends SIGTERM, then SIGKILL after 5 s, and the result says `killed`. A command
//! that cannot start answers code 1 with empty output; one ended by a signal answers code 0 (`code ?? 0`).

use crate::rpc::{CancelToken, RpcError};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SIGTERM to SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// After the process exits, its pipes are read until they stay quiet this long (`EXIT_STDIO_GRACE_MS`).
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(5);

struct Pipe {
    data: Vec<u8>,
    ended: bool,
    last: Instant,
}

fn reader(mut stream: impl Read + Send + 'static) -> Arc<Mutex<Pipe>> {
    let pipe = Arc::new(Mutex::new(Pipe {
        data: Vec::new(),
        ended: false,
        last: Instant::now(),
    }));
    let shared = pipe.clone();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut pipe = shared.lock().unwrap_or_else(|e| e.into_inner());
                    pipe.data.extend_from_slice(&buffer[..n]);
                    pipe.last = Instant::now();
                }
            }
        }
        shared.lock().unwrap_or_else(|e| e.into_inner()).ended = true;
    });
    pipe
}

fn signal(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) {
        let _ = rustix::process::kill_process(pid, signal);
    }
}

fn text(pipe: &Mutex<Pipe>) -> String {
    String::from_utf8_lossy(&pipe.lock().unwrap_or_else(|e| e.into_inner()).data).into_owned()
}

/// Runs `{ command, args?, cwd?, timeoutMs? }`; answers `{ stdout, stderr, code, killed }`.
pub fn exec(params: &Value, session_cwd: &Path, cancel: &CancelToken) -> Result<Value, RpcError> {
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::remote(-32602, "host/exec needs a string \"command\""))?;
    let args: Vec<String> = match params.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|a| a.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| RpcError::remote(-32602, "host/exec \"args\" must be strings"))?,
        Some(_) => return Err(RpcError::remote(-32602, "host/exec \"args\" must be strings")),
    };
    let cwd = match params.get("cwd").and_then(Value::as_str) {
        Some(dir) => session_cwd.join(dir),
        None => session_cwd.to_path_buf(),
    };
    let timeout = params
        .get("timeoutMs")
        .and_then(Value::as_f64)
        .filter(|ms| *ms > 0.0)
        .map(|ms| Duration::from_millis(ms as u64));
    let spawned = Command::new(command)
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(_) => return Ok(json!({ "stdout": "", "stderr": "", "code": 1, "killed": false })),
    };
    let stdout = reader(child.stdout.take().expect("piped stdout"));
    let stderr = reader(child.stderr.take().expect("piped stderr"));
    let started = Instant::now();
    let mut killed_at: Option<Instant> = None;
    let mut sigkilled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        let expired = timeout.is_some_and(|t| started.elapsed() >= t);
        if killed_at.is_none() && (expired || cancel.is_cancelled()) {
            signal(child.id(), rustix::process::Signal::TERM);
            killed_at = Some(Instant::now());
        }
        if let Some(at) = killed_at
            && !sigkilled
            && at.elapsed() >= KILL_GRACE
        {
            let _ = child.kill();
            sigkilled = true;
        }
        std::thread::sleep(POLL);
    };
    // Descendants may keep the pipes open: read until both end or stay quiet for the grace period after the exit.
    let exited = Instant::now();
    loop {
        let (ended, last) = {
            let out = stdout.lock().unwrap_or_else(|e| e.into_inner());
            let err = stderr.lock().unwrap_or_else(|e| e.into_inner());
            (out.ended && err.ended, out.last.max(err.last).max(exited))
        };
        if ended || last.elapsed() >= EXIT_STDIO_GRACE {
            break;
        }
        std::thread::sleep(POLL);
    }
    let code = match status {
        Some(status) => status.code().unwrap_or(0),
        None => 1,
    };
    Ok(json!({
        "stdout": text(&stdout),
        "stderr": text(&stderr),
        "code": code,
        "killed": killed_at.is_some(),
    }))
}
