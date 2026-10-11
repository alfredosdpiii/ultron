//! Host operations the Python kernel API calls (RLM-first core), served as a [`kernel::HostHandler`].
//!
//! Ported from the TypeScript session worker's host handler (experimental/session-worker.ts), its blocking
//! `bash` (ultron/rlm/host-bash.ts over core/bash-executor.ts) and host-owned shell jobs (ultron/rlm/shell-jobs.ts).
//! `read`, `write` and `edit` run inside runtime.py; the host only answers their `files.before_write` check.
//! Requests that need a model, subagents or durable stores are refused with [`not_available`].

pub mod cell;
pub mod file_hooks;
pub mod journal;
pub mod loki_guard;
pub mod sandboxed;
pub mod secrets;
pub mod shell_env;
pub mod text;
pub mod view_image;

use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use text::{DEFAULT_MAX_BYTES, Decoder, clean, tail_bytes, truncate_tail};

pub const JOB_TEXT_BYTES: usize = 16 * 1024;
pub const JOB_RETAINED_BYTES: usize = 4 * 1024 * 1024;
pub const JOB_MAX_ACTIVE: usize = 32;
pub const JOB_MAX_RETAINED: usize = 64;
const JOB_MAX_WAIT_SECONDS: f64 = 3600.0;
const MAX_TIMEOUT_SECONDS: f64 = 86_400.0;
/// How often a waiting thread re-checks its cell's abort signal and a running command its stop condition.
// ponytail: 10 ms polling instead of wiring the kernel Signal into a condvar; fine for a handful of waiters.
const POLL: Duration = Duration::from_millis(10);

/// The before-write and before-shell guards (Loki, extension hooks). The default allows everything.
pub trait Guard: Send + Sync {
    /// `Some(reason)` blocks the command: "bash command was not run: {reason}".
    fn before_shell(&self, _command: &str) -> Option<String> {
        None
    }
    /// One result per proposed write: `{blocked, reason?, notes?}` as runtime.py reads it.
    fn before_write(&self, _path: &str, _content: &str) -> Value {
        json!({ "blocked": false })
    }
}

pub struct AllowAll;
impl Guard for AllowAll {}

pub fn not_available(kind: &str) -> String {
    format!("Ultron RLM host request is not available in this host yet: {kind}")
}

pub struct Host {
    cwd: PathBuf,
    jobs_dir: PathBuf,
    spill_dir: PathBuf,
    guard: Box<dyn Guard>,
    /// Runs shell commands isolated (see [`Host::sandboxed`]); None runs them as before.
    launcher: Option<Arc<sandbox::Launcher>>,
    jobs: Mutex<Vec<Arc<Job>>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed_out",
        }
    }
}

struct Record {
    status: Status,
    exit_code: Option<i32>,
    started_at: u64,
    ended_at: Option<u64>,
    output_bytes: usize,
    retained_bytes: usize,
    tail: String,
    error: Option<String>,
}

struct Job {
    id: String,
    command: String,
    timeout: Option<f64>,
    output_path: PathBuf,
    record: Mutex<Record>,
    cancel: Mutex<bool>,
    done: (Mutex<bool>, Condvar),
    waiters: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("system randomness");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

fn command_of(payload: &Map<String, Value>) -> Result<String, String> {
    match payload.get("command") {
        Some(Value::String(command)) if !command.trim().is_empty() => Ok(command.clone()),
        _ => Err("bash command must be a non-empty string".into()),
    }
}

/// bashTimeoutSeconds: absent or null is no limit; at most a day.
fn timeout_of(value: Option<&Value>) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(seconds) if seconds > 0.0 => Ok(Some(seconds.min(MAX_TIMEOUT_SECONDS))),
            _ => Err("bash timeout must be a positive number of seconds".into()),
        },
    }
}

fn wait_seconds(value: Option<&Value>, name: &str) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(seconds) if seconds >= 0.0 => Ok(Some(seconds.min(JOB_MAX_WAIT_SECONDS))),
            _ => Err(format!("{name} must be a non-negative number of seconds")),
        },
    }
}

fn safe_integer(value: Option<&Value>, default: u64) -> Option<u64> {
    match value {
        None => Some(default),
        Some(value) => value.as_u64().or_else(|| {
            value
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= 9_007_199_254_740_991.0)
                .map(|f| f as u64)
        }),
    }
}

/// Runs `command` with bash in its own process group, stdout and stderr on one pipe. `on_text` gets each decoded,
/// cleaned chunk and the raw byte count; `stop` is polled and kills the process group when it turns true.
/// Returns the exit code (None when killed by a signal).
fn run_shell(
    command: &str,
    cwd: &Path,
    launcher: Option<&sandbox::Launcher>,
    mut on_text: impl FnMut(&str, usize),
    stop: impl Fn() -> bool,
) -> Result<Option<i32>, String> {
    let mut process = match launcher {
        Some(launcher) => launcher.command(&["bash".into(), "-c".into(), command.into()], cwd)?,
        None => {
            let mut process = Command::new("bash");
            process.arg("-c").arg(command).current_dir(cwd);
            process
        }
    };
    let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
    let stderr = writer.try_clone().map_err(|error| error.to_string())?;
    let mut child = process
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    // The Command holds the pipe's write ends: drop it so the reader sees EOF when the command exits.
    drop(process);
    let pid = rustix::process::Pid::from_child(&child);
    let (sender, chunks) = mpsc::channel::<Vec<u8>>();
    let pump = std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut decoder = Decoder::default();
    let mut killed = false;
    loop {
        match chunks.recv_timeout(POLL) {
            Ok(data) => on_text(&clean(&decoder.decode(&data, true)), data.len()),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !killed && stop() {
            killed = true;
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
    let _ = pump.join();
    on_text(&clean(&decoder.decode(&[], false)), 0);
    let status = child.wait().map_err(|error| error.to_string())?;
    Ok(status.code())
}

impl Host {
    /// `cwd` is where commands run; job spill files go to `jobs_dir/<id>.log`, blocking-bash spill files to
    /// `spill_dir/pi-bash-<hex>.log` (TS: the OS temp dir).
    pub fn new(
        cwd: impl Into<PathBuf>,
        jobs_dir: impl Into<PathBuf>,
        spill_dir: impl Into<PathBuf>,
        guard: Box<dyn Guard>,
    ) -> Self {
        let jobs_dir = jobs_dir.into();
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&jobs_dir);
        Host {
            cwd: cwd.into(),
            jobs_dir,
            spill_dir: spill_dir.into(),
            guard,
            launcher: None,
            jobs: Mutex::new(Vec::new()),
        }
    }

    fn dispatch(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind == "files.before_write" {
            // session-worker.ts `proposedWrites`.
            let writes = match payload.get("writes") {
                Some(Value::Array(writes)) if (1..=64).contains(&writes.len()) => writes,
                _ => return Err("files.before_write needs 1 to 64 writes".into()),
            };
            let mut proposed = Vec::new();
            for write in writes {
                match (write.get("path"), write.get("content")) {
                    (Some(Value::String(path)), Some(Value::String(content))) if !path.is_empty() => {
                        proposed.push((path.as_str(), content.as_str()));
                    }
                    _ => return Err("files.before_write writes need a path and text content".into()),
                }
            }
            let results: Vec<Value> = proposed
                .into_iter()
                .map(|(path, content)| self.guard.before_write(path, content))
                .collect();
            return Ok(json!({ "results": results }));
        }
        if matches!(kind, "bash" | "shell.bash" | "shell.run")
            && let Some(Value::String(command)) = payload.get("command")
            && let Some(reason) = self.guard.before_shell(command)
        {
            return Err(format!("bash command was not run: {reason}"));
        }
        match kind {
            "bash" if payload.contains_key("yield_after") => self.shell_bash(&payload, signal),
            "bash" => self.blocking_bash(&payload, signal),
            "shell.bash" => self.shell_bash(&payload, signal),
            "shell.run" => {
                fields(&payload, &["command", "timeout", "yield_after"])?;
                let command = command_of(&payload)?;
                let timeout = timeout_of(payload.get("timeout"))?;
                let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?.unwrap_or(0.0);
                let job = self.start(command, timeout)?;
                wait(&job, Some(yield_after), signal);
                Ok(snapshot(&job))
            }
            "shell.result" => {
                fields(&payload, &["id", "wait"])?;
                let job = self.find(payload.get("id"))?;
                // TS validates `wait` only while the job runs.
                if lock(&job.record).status == Status::Running {
                    wait(&job, wait_seconds(payload.get("wait"), "wait")?, signal);
                }
                Ok(snapshot(&job))
            }
            "shell.get" => {
                fields(&payload, &["id"])?;
                let job = self.find(payload.get("id"))?;
                Ok(snapshot(&job))
            }
            "shell.cancel" => {
                fields(&payload, &["id"])?;
                let job = self.find(payload.get("id"))?;
                cancel(&job);
                Ok(snapshot(&job))
            }
            "shell.read" => {
                fields(&payload, &["id", "cursor", "max_bytes"])?;
                let job = self.find(payload.get("id"))?;
                read_output(&job, payload.get("cursor"), payload.get("max_bytes"))
            }
            "shell.list" => {
                fields(&payload, &[])?;
                let jobs = lock(&self.jobs).clone();
                Ok(Value::Array(
                    jobs.iter()
                        .rev()
                        .map(|job| {
                            let mut snapshot = snapshot(job);
                            snapshot["text"] = Value::Null;
                            snapshot
                        })
                        .collect(),
                ))
            }
            other if other.starts_with("shell.") => Err(format!("Ultron RLM host request is not wired: {other}")),
            other => Err(not_available(other)),
        }
    }

    /// host-bash.ts runHostBash over bash-executor.ts: wait for the command, keep the tail, spill the whole output.
    fn blocking_bash(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let command = command_of(payload)?;
        let timeout = timeout_of(payload.get("timeout"))?;
        let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
        let timed_out = std::cell::Cell::new(false);
        let mut chunks: std::collections::VecDeque<String> = Default::default();
        let mut kept_units = 0usize;
        let mut total_bytes = 0usize;
        let mut spill: Option<(PathBuf, File)> = None;
        let open_spill =
            |spill: &mut Option<(PathBuf, File)>, chunks: &std::collections::VecDeque<String>| -> Result<(), String> {
                if spill.is_none() {
                    let path = self.spill_dir.join(format!("pi-bash-{}.log", hex(8)));
                    let mut file = File::create(&path).map_err(|error| error.to_string())?;
                    for chunk in chunks {
                        let _ = file.write_all(chunk.as_bytes());
                    }
                    *spill = Some((path, file));
                }
                Ok(())
            };
        let mut failure = None;
        let exit = run_shell(
            &command,
            &self.cwd,
            self.launcher.as_deref(),
            |text, raw| {
                total_bytes += raw;
                if total_bytes > DEFAULT_MAX_BYTES
                    && let Err(error) = open_spill(&mut spill, &chunks)
                {
                    failure = Some(error);
                }
                if let Some((_, file)) = spill.as_mut() {
                    let _ = file.write_all(text.as_bytes());
                }
                chunks.push_back(text.to_owned());
                kept_units += text.encode_utf16().count();
                while kept_units > DEFAULT_MAX_BYTES * 2 && chunks.len() > 1 {
                    if let Some(removed) = chunks.pop_front() {
                        kept_units -= removed.encode_utf16().count();
                    }
                }
            },
            || {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    timed_out.set(true);
                }
                timed_out.get() || signal.is_aborted()
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
        let full: String = chunks.iter().map(String::as_str).collect();
        let truncated = truncate_tail(&full);
        if truncated.is_some() {
            open_spill(&mut spill, &chunks)?;
        }
        let stopped = timed_out.get() || signal.is_aborted();
        Ok(json!({
            "output": truncated.clone().unwrap_or(full),
            "exit_code": if stopped { None } else { exit },
            "cancelled": stopped && !timed_out.get(),
            "timed_out": timed_out.get(),
            "truncated": truncated.is_some(),
            "full_output_path": spill.map(|(path, _)| path.to_string_lossy().into_owned()),
        }))
    }

    /// Plain `await bash(cmd)` with a yield_after: wait like the blocking bash, then leave a slow command running.
    fn shell_bash(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        fields(payload, &["command", "timeout", "yield_after"])?;
        let command = command_of(payload)?;
        let timeout = timeout_of(payload.get("timeout"))?;
        let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?;
        let job = self.start(command, timeout)?;
        wait(&job, yield_after, signal);
        if lock(&job.record).status == Status::Running && signal.is_aborted() {
            job.waiters.fetch_add(1, Ordering::SeqCst);
            cancel(&job);
            job.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        bash_result(&job)
    }

    fn find(&self, id: Option<&Value>) -> Result<Arc<Job>, String> {
        let Some(Value::String(id)) = id.filter(|id| id.as_str().is_some_and(|id| !id.is_empty())) else {
            return Err("job id must be a nonempty string".into());
        };
        lock(&self.jobs)
            .iter()
            .find(|job| &job.id == id)
            .cloned()
            .ok_or_else(|| format!("Unknown shell job {id}"))
    }

    fn start(&self, command: String, timeout: Option<f64>) -> Result<Arc<Job>, String> {
        let mut jobs = lock(&self.jobs);
        let running = |job: &Arc<Job>| lock(&job.record).status == Status::Running;
        if jobs.iter().filter(|job| running(job)).count() >= JOB_MAX_ACTIVE {
            return Err(format!("At most {JOB_MAX_ACTIVE} shell jobs can run at once"));
        }
        let id = format!("job-{}", hex(4));
        let job = Arc::new(Job {
            output_path: self.jobs_dir.join(format!("{id}.log")),
            id,
            command,
            timeout,
            record: Mutex::new(Record {
                status: Status::Running,
                exit_code: None,
                started_at: now_ms(),
                ended_at: None,
                output_bytes: 0,
                retained_bytes: 0,
                tail: String::new(),
                error: None,
            }),
            cancel: Mutex::new(false),
            done: (Mutex::new(false), Condvar::new()),
            waiters: AtomicUsize::new(0),
        });
        jobs.push(job.clone());
        // Bounded retention: drop the oldest finished jobs and their spill files.
        let excess = jobs.len().saturating_sub(JOB_MAX_RETAINED);
        let pruned: Vec<String> = jobs
            .iter()
            .filter(|job| !running(job))
            .take(excess)
            .map(|job| job.id.clone())
            .collect();
        jobs.retain(|job| {
            let keep = !pruned.contains(&job.id);
            if !keep {
                let _ = std::fs::remove_file(&job.output_path);
            }
            keep
        });
        drop(jobs);
        let cwd = self.cwd.clone();
        let runner = job.clone();
        let launcher = self.launcher.clone();
        std::thread::spawn(move || run_job(&runner, &cwd, launcher.as_deref()));
        Ok(job)
    }
}

fn run_job(job: &Job, cwd: &Path, launcher: Option<&sandbox::Launcher>) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&job.output_path);
    let deadline = job
        .timeout
        .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    let timed_out = std::cell::Cell::new(false);
    let mut file = file.ok();
    let result = run_shell(
        &job.command,
        cwd,
        launcher,
        |text, _| {
            if text.is_empty() {
                return;
            }
            let mut record = lock(&job.record);
            record.output_bytes += text.len();
            let room = JOB_RETAINED_BYTES.saturating_sub(record.retained_bytes);
            if room > 0 {
                let kept = &text.as_bytes()[..room.min(text.len())];
                if let Some(file) = file.as_mut() {
                    let _ = file.write_all(kept);
                }
                record.retained_bytes += kept.len();
            }
            let tail = format!("{}{text}", record.tail);
            record.tail = tail_bytes(&tail, JOB_TEXT_BYTES / 2).to_owned();
        },
        || {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                timed_out.set(true);
            }
            timed_out.get() || *lock(&job.cancel)
        },
    );
    let aborted = timed_out.get() || *lock(&job.cancel);
    {
        let mut record = lock(&job.record);
        record.ended_at = Some(now_ms());
        record.exit_code = match &result {
            Ok(code) if !aborted => *code,
            _ => None,
        };
        record.status = if timed_out.get() {
            Status::TimedOut
        } else if aborted {
            Status::Cancelled
        } else if result.is_err() {
            Status::Failed
        } else {
            Status::Completed
        };
        if timed_out.get() {
            record.error = Some(format!(
                "timed out after {}s; process tree killed",
                js_number(job.timeout.unwrap_or(0.0))
            ));
        } else if let Err(error) = result
            && !aborted
        {
            record.error = Some(error);
        }
    }
    *lock(&job.done.0) = true;
    job.done.1.notify_all();
}

/// A number as JS prints it in a template string (integers without a fraction).
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// A JSON number as JS writes it: integral values without a fraction (Python then sees an int).
fn js_json_number(value: f64) -> Value {
    if value.fract() == 0.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// Wait for a job at most `seconds` (at least 20 ms; None: until it ends) or until the calling cell ends.
fn wait(job: &Job, seconds: Option<f64>, signal: &Signal) {
    job.waiters.fetch_add(1, Ordering::SeqCst);
    let deadline = seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds.max(0.02)));
    let mut done = lock(&job.done.0);
    while !*done && !signal.is_aborted() && deadline.is_none_or(|deadline| Instant::now() < deadline) {
        done = job
            .done
            .1
            .wait_timeout(done, POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    drop(done);
    job.waiters.fetch_sub(1, Ordering::SeqCst);
}

fn cancel(job: &Job) {
    if *lock(&job.done.0) {
        return;
    }
    *lock(&job.cancel) = true;
    let mut done = lock(&job.done.0);
    while !*done {
        done = job.done.1.wait(done).unwrap_or_else(PoisonError::into_inner);
    }
}

fn read_file(path: &Path, cursor: usize, max: usize) -> String {
    let mut buffer = Vec::new();
    if let Ok(mut file) = File::open(path)
        && file.seek(SeekFrom::Start(cursor as u64)).is_ok()
    {
        let _ = file.take(max as u64).read_to_end(&mut buffer);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Whole output when it fits, else the first and last halves around a marker naming how to read the rest.
fn job_text(job: &Job, record: &Record) -> (String, bool) {
    let half = JOB_TEXT_BYTES / 2;
    if record.output_bytes <= JOB_TEXT_BYTES && record.retained_bytes == record.output_bytes {
        return (read_file(&job.output_path, 0, JOB_TEXT_BYTES), false);
    }
    let head = read_file(&job.output_path, 0, half);
    let omitted = record.output_bytes.saturating_sub(half + record.tail.len());
    let marker = format!(
        "\n[... {omitted} bytes omitted; {} bytes kept in {}: await job.read(cursor={half}, max_bytes=65536) ...]\n",
        record.retained_bytes,
        job.output_path.display()
    );
    (format!("{head}{marker}{}", record.tail), true)
}

fn snapshot(job: &Job) -> Value {
    let record = lock(&job.record);
    let (text, truncated) = job_text(job, &record);
    let elapsed = record.ended_at.unwrap_or_else(now_ms).saturating_sub(record.started_at);
    json!({
        "id": job.id,
        "lane": "main",
        "command": job.command,
        "status": record.status.name(),
        "running": record.status == Status::Running,
        "exit_code": record.exit_code,
        "ok": record.status == Status::Completed && record.exit_code == Some(0),
        "timed_out": record.status == Status::TimedOut,
        "cancelled": record.status == Status::Cancelled,
        "text": text,
        "truncated": truncated,
        "output_path": job.output_path.to_string_lossy(),
        "output_bytes": record.output_bytes,
        "elapsed_seconds": js_json_number((elapsed as f64 / 100.0).round() / 10.0),
        "error": record.error,
    })
}

/// The blocking bash's reply shape for a command that ran as a job, plus `running` and the job while it runs.
fn bash_result(job: &Job) -> Result<Value, String> {
    let (status, exit_code, error, partial) = {
        let record = lock(&job.record);
        (
            record.status,
            record.exit_code,
            record.error.clone(),
            record.retained_bytes < record.output_bytes,
        )
    };
    if status == Status::Failed {
        return Err(error.unwrap_or_else(|| "bash command failed".into()));
    }
    let snapshot = snapshot(job);
    let truncated = snapshot["truncated"].as_bool() == Some(true);
    let running = status == Status::Running;
    Ok(json!({
        "output": snapshot["text"],
        "exit_code": exit_code,
        "cancelled": status == Status::Cancelled,
        "timed_out": status == Status::TimedOut,
        "truncated": truncated,
        "full_output_path": if truncated { snapshot["output_path"].clone() } else { Value::Null },
        "partial_file": partial,
        "running": running,
        "job": if running { snapshot } else { Value::Null },
    }))
}

fn read_output(job: &Job, cursor: Option<&Value>, max_bytes: Option<&Value>) -> Result<Value, String> {
    let cursor = safe_integer(cursor, 0).ok_or("cursor must be a non-negative integer")? as usize;
    let max = safe_integer(max_bytes, 65_536)
        .filter(|max| (1..=1_048_576).contains(max))
        .ok_or("max_bytes must be an integer between 1 and 1048576")? as usize;
    let record = lock(&job.record);
    let start = cursor.min(record.retained_bytes);
    let text = read_file(&job.output_path, start, max.min(record.retained_bytes - start));
    let next = start + text.len();
    Ok(json!({
        "text": text,
        "next_cursor": next,
        "done": record.status != Status::Running && next >= record.retained_bytes,
        "truncated": record.retained_bytes < record.output_bytes,
    }))
}

impl HostHandler for Host {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self.dispatch(kind, payload, signal)
    }
}

impl Drop for Host {
    /// Worker shutdown: stop every running job.
    fn drop(&mut self) {
        for job in lock(&self.jobs).iter() {
            cancel(job);
        }
    }
}
