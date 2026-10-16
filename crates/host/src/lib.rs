//! Host operations the Python kernel API calls (RLM-first core), served as a [`kernel::HostHandler`].
//!
//! Ported from the TypeScript session worker's host handler (experimental/session-worker.ts), its blocking
//! `bash` (ultron/rlm/host-bash.ts over core/bash-executor.ts) and host-owned shell jobs (ultron/rlm/shell-jobs.ts).
//! `read`, `write` and `edit` run inside runtime.py; the host only answers their `files.before_write` check.
//! Requests that need a model, subagents or durable stores are refused with [`not_available`].
//!
//! Shell jobs are durable (`Host::with_journal`): each runs under a small keeper shell in its own process group that
//! writes the raw output to `<id>.out` and the exit status to `<id>.exit`, so the job does not depend on the worker
//! that started it. The host follows those files into the cleaned, bounded spill file `<id>.log` and journals every
//! start and end. A reopened worker settles each job the journal lists as running from what really happened: still
//! running (the keeper is alive: it is followed again), completed with the status the keeper wrote, or `lost` (the
//! process ended without one); never silently gone.

pub mod cell;
pub mod file_hooks;
pub mod journal;
pub mod loki_guard;
pub mod sandboxed;
pub mod secrets;
pub mod shell_env;
pub mod text;
pub mod view_image;

use journal::{JobJournal, JobRecord};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// A cell on `lane` is starting (after-cell checks take their baseline).
    fn cell_started(&self, _lane: &str) {}
    /// A cell on `lane` ended (after-cell checks start in the background).
    fn cell_ended(&self, _lane: &str) {}
    /// What after-cell checks found for `lane` and it has not been shown yet.
    fn take_pending(&self, _lane: &str) -> Option<String> {
        None
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
    shared: Arc<Shared>,
}

/// A job that ended, as `ShellJobs` `onEnd` reports it: its record, whether a caller was waiting on it (the result
/// came inline, so nothing is announced) and whether its root turn's abort stopped it.
#[derive(Clone, Debug, PartialEq)]
pub struct JobEnd {
    pub record: JobRecord,
    pub awaited: bool,
    pub root_aborted: bool,
}

impl JobEnd {
    /// The `job_done` event (`jobEvent`), unless the end was awaited or its root turn aborted.
    pub fn event(&self) -> Option<Value> {
        journal::job_event(&self.record, self.awaited, self.root_aborted)
    }
}

/// Sees every job end (not those stopped by the host's shutdown).
pub type JobEnds = Arc<dyn Fn(JobEnd) + Send + Sync>;
/// The usage root a job started by `lane` belongs to (`host.rootOf(caller)`): the root turn running on the root
/// lane, or the root of the task that owns another lane.
pub type RootOf = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Why a job was stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StopReason {
    /// `job.cancel()`.
    Cancelled,
    /// Its root turn was aborted (Esc): stopped, and its end is not announced.
    RootAborted,
    /// The host shut down: nothing is reported.
    Closed,
}

/// What job threads share with the host: the jobs, the journal they are written to, whether the host released
/// its jobs to a next owner, and who hears of their ends.
struct Shared {
    jobs: Mutex<Vec<Arc<Job>>>,
    journal: Mutex<Option<Arc<dyn JobJournal>>>,
    cwd: String,
    released: AtomicBool,
    ends: Mutex<Option<JobEnds>>,
    roots: Mutex<Option<RootOf>>,
}

impl Shared {
    /// Writes the journal (every job, tails left out), serialized under the jobs lock so the newest state wins.
    fn persist(&self) {
        let jobs = lock(&self.jobs);
        let Some(journal) = lock(&self.journal).clone() else {
            return;
        };
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        let records: Vec<JobRecord> = jobs.iter().map(|job| job_record(job, &self.cwd)).collect();
        let _ = journal.write(&journal::persisted(&records));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    /// A TypeScript journal's running job after a restart (no process to follow).
    Interrupted,
    /// Its process ended while no worker followed it and left no exit status.
    Lost,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed_out",
            Status::Interrupted => "interrupted",
            Status::Lost => "lost",
        }
    }

    fn from_name(name: &str) -> Status {
        match name {
            "running" => Status::Running,
            "completed" => Status::Completed,
            "cancelled" => Status::Cancelled,
            "timed_out" => Status::TimedOut,
            "interrupted" => Status::Interrupted,
            "lost" => Status::Lost,
            _ => Status::Failed,
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
    /// The lane that started the job (it and the root lane see it) and that lane's root turn.
    lane: String,
    root_id: Option<String>,
    command: String,
    timeout: Option<f64>,
    /// The cleaned, bounded output the kernel reads.
    output_path: PathBuf,
    /// The keeper's raw output and the exit status it writes when the command ends.
    raw_path: PathBuf,
    exit_path: PathBuf,
    record: Mutex<Record>,
    /// The keeper process (process group leader) and its start time.
    process: Mutex<Option<(i32, Option<u64>)>>,
    /// Set by the waiter of this worker's own keeper when it exits (its status code); guarded by `cancel.0`'s lock
    /// for wake-ups.
    exited: Mutex<Option<Option<i32>>>,
    /// Cancel requested; its condvar wakes the follower (cancel, keeper exit, release).
    cancel: (Mutex<bool>, Condvar),
    /// Why the first stop asked for it.
    reason: Mutex<Option<StopReason>>,
    done: (Mutex<bool>, Condvar),
    waiters: AtomicUsize,
}

impl Job {
    fn new(
        id: String,
        (lane, root_id): (String, Option<String>),
        command: String,
        timeout: Option<f64>,
        jobs_dir: &Path,
        record: Record,
    ) -> Job {
        Job {
            output_path: jobs_dir.join(format!("{id}.log")),
            raw_path: jobs_dir.join(format!("{id}.out")),
            exit_path: jobs_dir.join(format!("{id}.exit")),
            id,
            lane,
            root_id,
            command,
            timeout,
            record: Mutex::new(record),
            process: Mutex::new(None),
            exited: Mutex::new(None),
            cancel: (Mutex::new(false), Condvar::new()),
            reason: Mutex::new(None),
            done: (Mutex::new(false), Condvar::new()),
            waiters: AtomicUsize::new(0),
        }
    }

    fn finished(&self) {
        *lock(&self.done.0) = true;
        self.done.1.notify_all();
    }
}

fn job_record(job: &Job, cwd: &str) -> JobRecord {
    let record = lock(&job.record);
    let process = *lock(&job.process);
    JobRecord {
        id: job.id.clone(),
        lane: job.lane.clone(),
        root_id: job.root_id.clone(),
        command: job.command.clone(),
        cwd: cwd.into(),
        timeout: job.timeout,
        status: record.status.name().into(),
        exit_code: record.exit_code.map(i64::from),
        started_at: record.started_at,
        ended_at: record.ended_at,
        output_path: job.output_path.to_string_lossy().into_owned(),
        output_bytes: record.output_bytes as u64,
        retained_bytes: record.retained_bytes as u64,
        tail: record.tail.clone(),
        error: record.error.clone(),
        pid: process.map(|(pid, _)| i64::from(pid)),
        pid_start: process.and_then(|(_, start)| start),
    }
}

/// Start time of a live (not zombie) process, in clock ticks since boot (`/proc/<pid>/stat` field 22).
fn process_start(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if fields.first() == Some(&"Z") {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// Whether the keeper recorded for a job still runs (same pid and start time).
fn keeper_alive(process: Option<(i32, Option<u64>)>) -> bool {
    match process {
        Some((pid, start)) => process_start(pid).is_some_and(|now| start.is_none_or(|start| start == now)),
        None => false,
    }
}

/// The exit status the keeper wrote, once the command has ended.
fn exit_status(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().last()?.parse().ok()
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
    /// A cell on `lane` is starting: the after-cell file hooks take their baseline picture if they have none.
    pub fn cell_started(&self, lane: &str) {
        self.guard.cell_started(lane);
    }

    /// A cell on `lane` ended: the after-cell file hooks check what it changed, in the background.
    pub fn cell_ended(&self, lane: &str) {
        self.guard.cell_ended(lane);
    }

    /// Findings of after-cell checks not yet shown to `lane` (and forget them), for its next cell result.
    pub fn take_pending(&self, lane: &str) -> Option<String> {
        self.guard.take_pending(lane)
    }

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
        let cwd = cwd.into();
        Host {
            shared: Arc::new(Shared {
                jobs: Mutex::new(Vec::new()),
                journal: Mutex::new(None),
                cwd: cwd.to_string_lossy().into_owned(),
                released: AtomicBool::new(false),
                ends: Mutex::new(None),
                roots: Mutex::new(None),
            }),
            cwd,
            jobs_dir,
            spill_dir: spill_dir.into(),
            guard,
            launcher: None,
        }
    }

    /// Makes shell jobs durable: restores the jobs journal from `journal` and journals every start and end from now
    /// on. A job the journal lists as running is settled from its process: followed again while its keeper runs,
    /// completed with the exit status the keeper wrote, or `lost` when the process ended without one. Records
    /// without a process (TypeScript journals) read as `interrupted`, as there. Call once, before the first request.
    pub fn with_journal(self, journal: Arc<dyn JobJournal>) -> Result<Host, String> {
        let now = now_ms();
        let restored = journal::restore_durable(journal.read()?.as_ref(), now)?;
        let mut adopted = Vec::new();
        {
            let mut jobs = lock(&self.shared.jobs);
            for saved in restored.records {
                let job = Arc::new(Job::new(
                    saved.id.clone(),
                    (saved.lane.clone(), saved.root_id.clone()),
                    saved.command.clone(),
                    saved.timeout,
                    // The record names its own spill file; the keeper's files sit next to it.
                    Path::new(&saved.output_path).parent().unwrap_or(&self.jobs_dir),
                    Record {
                        status: Status::from_name(&saved.status),
                        exit_code: saved.exit_code.and_then(|code| i32::try_from(code).ok()),
                        started_at: saved.started_at,
                        ended_at: saved.ended_at,
                        output_bytes: saved.output_bytes as usize,
                        retained_bytes: saved.retained_bytes as usize,
                        tail: saved.tail.clone(),
                        error: saved.error.clone(),
                    },
                ));
                if saved.status == "running" {
                    let pid = saved.pid.and_then(|pid| i32::try_from(pid).ok());
                    *lock(&job.process) = pid.map(|pid| (pid, saved.pid_start));
                    adopted.push(job.clone());
                } else {
                    job.finished();
                }
                jobs.push(job);
            }
        }
        *lock(&self.shared.journal) = Some(journal);
        for job in adopted {
            if keeper_alive(*lock(&job.process)) {
                let (runner, shared) = (job.clone(), self.shared.clone());
                std::thread::spawn(move || follow(&runner, None, &shared));
            } else {
                // Ended while no worker followed it: settle it now, so the first reply is already its real state.
                follow(&job, None, &self.shared);
            }
        }
        self.shared.persist();
        Ok(self)
    }

    /// Hands the running jobs to the next owner of the journal: this host stops following and journaling them, and
    /// dropping it no longer stops them (a worker handing over to its successor). Without a journal it changes
    /// nothing but the drop.
    pub fn release_jobs(&self) {
        self.shared.persist();
        self.shared.released.store(true, Ordering::SeqCst);
        for job in lock(&self.shared.jobs).iter() {
            job.cancel.1.notify_all();
        }
    }

    /// Stops every running job and waits until each has ended (worker shutdown). Dropping the host does the same
    /// unless [`Host::release_jobs`] was called. Nothing is reported of them.
    pub fn stop_jobs(&self) {
        let jobs = lock(&self.shared.jobs).clone();
        for job in jobs {
            cancel(&job, StopReason::Closed);
        }
    }

    /// Who hears of every job's end (`onEnd`): the completion events of jobs nobody waited on.
    pub fn on_job_end(&self, ends: JobEnds) {
        *lock(&self.shared.ends) = Some(ends);
    }

    /// Where a new job's usage root comes from (`host.rootOf(caller)`).
    pub fn set_roots(&self, roots: RootOf) {
        *lock(&self.shared.roots) = Some(roots);
    }

    /// `cancelRoot`: Esc aborted a root turn: stop every job started under it (their ends are not announced).
    pub fn cancel_root(&self, root: &str) {
        let jobs: Vec<Arc<Job>> = lock(&self.shared.jobs)
            .iter()
            .filter(|job| job.root_id.as_deref() == Some(root))
            .cloned()
            .collect();
        for job in jobs {
            cancel(&job, StopReason::RootAborted);
        }
    }

    /// How many callers are waiting on job `id` now (a waiter that sees the job end gets its result inline, so
    /// the end is not announced).
    pub fn job_waiters(&self, id: &str) -> usize {
        lock(&self.shared.jobs)
            .iter()
            .find(|job| job.id == id)
            .map_or(0, |job| job.waiters.load(Ordering::SeqCst))
    }

    /// Running jobs of every lane.
    pub fn running_jobs(&self) -> usize {
        lock(&self.shared.jobs)
            .iter()
            .filter(|job| lock(&job.record).status == Status::Running)
            .count()
    }

    /// Running jobs of `lane` whose root `excluded` does not exclude (`running`).
    pub fn running(&self, lane: &str, excluded: &dyn Fn(Option<&str>) -> bool) -> usize {
        lock(&self.shared.jobs)
            .iter()
            .filter(|job| job.lane == lane && !excluded(job.root_id.as_deref()))
            .filter(|job| lock(&job.record).status == Status::Running)
            .count()
    }

    /// A request of `lane`'s kernel: its jobs belong to it (and to its root turn), and a lane other than the root's
    /// sees only its own.
    pub fn handle_lane(
        &self,
        lane: &str,
        kind: &str,
        payload: Map<String, Value>,
        signal: &Signal,
    ) -> Result<Value, String> {
        self.dispatch(lane, kind, payload, signal)
    }

    fn dispatch(&self, lane: &str, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
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
            "bash" if payload.contains_key("yield_after") => self.shell_bash(lane, &payload, signal),
            "bash" => self.blocking_bash(&payload, signal),
            "shell.bash" => self.shell_bash(lane, &payload, signal),
            "shell.run" => {
                fields(&payload, &["command", "timeout", "yield_after"])?;
                let command = command_of(&payload)?;
                let timeout = timeout_of(payload.get("timeout"))?;
                let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?.unwrap_or(0.0);
                let job = self.start(lane, command, timeout)?;
                wait(&job, Some(yield_after), signal);
                Ok(snapshot(&job))
            }
            "shell.result" => {
                fields(&payload, &["id", "wait"])?;
                let job = self.find(lane, payload.get("id"))?;
                // TS validates `wait` only while the job runs.
                if lock(&job.record).status == Status::Running {
                    wait(&job, wait_seconds(payload.get("wait"), "wait")?, signal);
                }
                Ok(snapshot(&job))
            }
            "shell.get" => {
                fields(&payload, &["id"])?;
                let job = self.find(lane, payload.get("id"))?;
                Ok(snapshot(&job))
            }
            "shell.cancel" => {
                fields(&payload, &["id"])?;
                let job = self.find(lane, payload.get("id"))?;
                cancel(&job, StopReason::Cancelled);
                Ok(snapshot(&job))
            }
            "shell.read" => {
                fields(&payload, &["id", "cursor", "max_bytes"])?;
                let job = self.find(lane, payload.get("id"))?;
                read_output(&job, payload.get("cursor"), payload.get("max_bytes"))
            }
            "shell.list" => {
                fields(&payload, &[])?;
                let jobs = lock(&self.shared.jobs).clone();
                Ok(Value::Array(
                    jobs.iter()
                        .rev()
                        .filter(|job| visible(job, lane))
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
    fn shell_bash(&self, lane: &str, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        fields(payload, &["command", "timeout", "yield_after"])?;
        let command = command_of(payload)?;
        let timeout = timeout_of(payload.get("timeout"))?;
        let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?;
        let job = self.start(lane, command, timeout)?;
        wait(&job, yield_after, signal);
        if lock(&job.record).status == Status::Running && signal.is_aborted() {
            job.waiters.fetch_add(1, Ordering::SeqCst);
            cancel(&job, StopReason::Cancelled);
            job.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        bash_result(&job)
    }

    fn find(&self, lane: &str, id: Option<&Value>) -> Result<Arc<Job>, String> {
        let Some(Value::String(id)) = id.filter(|id| id.as_str().is_some_and(|id| !id.is_empty())) else {
            return Err("job id must be a nonempty string".into());
        };
        lock(&self.shared.jobs)
            .iter()
            .find(|job| &job.id == id && visible(job, lane))
            .cloned()
            .ok_or_else(|| format!("Unknown shell job {id}"))
    }

    fn start(&self, lane: &str, command: String, timeout: Option<f64>) -> Result<Arc<Job>, String> {
        let root = lock(&self.shared.roots).clone().and_then(|roots| roots(lane));
        let mut jobs = lock(&self.shared.jobs);
        let running = |job: &Arc<Job>| lock(&job.record).status == Status::Running;
        if jobs.iter().filter(|job| running(job)).count() >= JOB_MAX_ACTIVE {
            return Err(format!("At most {JOB_MAX_ACTIVE} shell jobs can run at once"));
        }
        let id = format!("job-{}", hex(4));
        let job = Arc::new(Job::new(
            id,
            (lane.to_string(), root),
            command,
            timeout,
            &self.jobs_dir,
            Record {
                status: Status::Running,
                exit_code: None,
                started_at: now_ms(),
                ended_at: None,
                output_bytes: 0,
                retained_bytes: 0,
                tail: String::new(),
                error: None,
            },
        ));
        jobs.push(job.clone());
        // Bounded retention: drop the oldest finished jobs and their files.
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
                for path in [&job.output_path, &job.raw_path, &job.exit_path] {
                    let _ = std::fs::remove_file(path);
                }
            }
            keep
        });
        drop(jobs);
        // The keeper is spawned on the thread that follows it (its waiter); its sandbox is persistent
        // (`Launcher::command_persistent`), so neither that thread nor this worker ending stops it.
        let (started, spawned) = mpsc::channel();
        let (runner, shared, cwd, launcher) = (
            job.clone(),
            self.shared.clone(),
            self.cwd.clone(),
            self.launcher.clone(),
        );
        std::thread::spawn(move || match spawn_keeper(&runner, &cwd, launcher.as_deref()) {
            Ok(child) => {
                let pid = child.id() as i32;
                *lock(&runner.process) = Some((pid, process_start(pid)));
                let _ = started.send(());
                follow(&runner, Some(child), &shared);
            }
            Err(error) => {
                let mut record = lock(&runner.record);
                record.status = Status::Failed;
                record.ended_at = Some(now_ms());
                record.error = Some(error);
                drop(record);
                let _ = started.send(());
                runner.finished();
            }
        });
        let _ = spawned.recv();
        self.shared.persist();
        Ok(job)
    }
}

/// The keeper: runs the command with bash (stdout and stderr together into the raw file, which is the keeper's
/// stdout), then writes the command's exit status to its own stderr, the exit file. Files, not pipes, so the command
/// keeps running and its end is recorded when the worker that started it is gone.
const KEEPER: &str = r#"bash -c "$0" 2>&1 </dev/null; printf '%s\n' "$?" >&2"#;

fn spawn_keeper(job: &Job, cwd: &Path, launcher: Option<&sandbox::Launcher>) -> Result<std::process::Child, String> {
    let create = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("{}: {error}", path.display()))
    };
    let raw = create(&job.raw_path)?;
    let exit = create(&job.exit_path)?;
    let argv = ["bash".to_string(), "-c".into(), KEEPER.into(), job.command.clone()];
    let mut process = match launcher {
        // A job outlives the worker that started it (a successor adopts it): its sandbox is not tied to us.
        Some(launcher) => launcher.command_persistent(&argv, cwd)?,
        None => {
            let mut process = Command::new(&argv[0]);
            process.args(&argv[1..]).current_dir(cwd);
            process
        }
    };
    process
        .stdin(Stdio::null())
        .stdout(raw)
        .stderr(exit)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())
}

/// Turns raw output into the cleaned, bounded spill file and the record's counts and tail, as the TypeScript job
/// does with its stream.
struct Digest {
    decoder: Decoder,
    log: Option<File>,
}

impl Digest {
    /// Starts the spill file over: a job followed again after a restart is digested from its first raw byte.
    fn new(job: &Job) -> Digest {
        let log = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&job.output_path)
            .ok();
        let mut record = lock(&job.record);
        record.output_bytes = 0;
        record.retained_bytes = 0;
        record.tail.clear();
        Digest {
            decoder: Decoder::default(),
            log,
        }
    }

    fn text(&mut self, job: &Job, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut record = lock(&job.record);
        record.output_bytes += text.len();
        let room = JOB_RETAINED_BYTES.saturating_sub(record.retained_bytes);
        if room > 0 {
            let kept = &text.as_bytes()[..room.min(text.len())];
            if let Some(log) = self.log.as_mut() {
                let _ = log.write_all(kept);
            }
            record.retained_bytes += kept.len();
        }
        let tail = format!("{}{text}", record.tail);
        record.tail = tail_bytes(&tail, JOB_TEXT_BYTES / 2).to_owned();
    }

    /// Reads every raw byte written so far.
    fn pump(&mut self, job: &Job, raw: &mut Option<File>) {
        let Some(file) = raw.as_mut() else {
            return;
        };
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = file.read(&mut buffer) {
            if read == 0 {
                break;
            }
            let text = clean(&self.decoder.decode(&buffer[..read], true));
            self.text(job, &text);
        }
    }

    fn finish(&mut self, job: &Job) {
        let text = clean(&self.decoder.decode(&[], false));
        self.text(job, &text);
    }
}

/// Follows a job until it ends: its own child (`child`), or a keeper started by an earlier worker (`None`), whose
/// end is its exit file or, without one, its process going away (`lost`). Stops the process group on cancel or
/// timeout. Returns without settling the job when the host released its jobs.
fn follow(job: &Arc<Job>, child: Option<std::process::Child>, shared: &Shared) {
    let own = child.is_some();
    if let Some(mut child) = child {
        // Waits for the keeper on a thread of its own so its exit wakes the follower at once.
        let waiter = job.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|status| status.code());
            let guard = lock(&waiter.cancel.0);
            *lock(&waiter.exited) = Some(code);
            drop(guard);
            waiter.cancel.1.notify_all();
        });
    }
    let mut digest = Digest::new(job);
    let mut raw = File::open(&job.raw_path).ok();
    let started_at = lock(&job.record).started_at;
    let deadline = job
        .timeout
        .map(|seconds| started_at.saturating_add((seconds * 1000.0) as u64));
    let process = *lock(&job.process);
    let mut stopped: Option<Status> = None;
    let ended = loop {
        let ended = match own {
            true => *lock(&job.exited),
            false => {
                let code = exit_status(&job.exit_path);
                (code.is_some() || !keeper_alive(process)).then_some(code)
            }
        };
        digest.pump(job, &mut raw);
        if let Some(ended) = ended {
            break ended;
        }
        if shared.released.load(Ordering::SeqCst) {
            return;
        }
        if stopped.is_none() {
            if deadline.is_some_and(|deadline| now_ms() >= deadline) {
                stopped = Some(Status::TimedOut);
            } else if *lock(&job.cancel.0) {
                stopped = Some(Status::Cancelled);
            }
            if stopped.is_some()
                && let Some(pid) = process.and_then(|(pid, _)| rustix::process::Pid::from_raw(pid))
            {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let cancel = lock(&job.cancel.0);
        if lock(&job.exited).is_none() {
            drop(
                job.cancel
                    .1
                    .wait_timeout(cancel, POLL)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    };
    digest.finish(job);
    // The keeper writes the command's status just before it exits.
    let code = exit_status(&job.exit_path);
    {
        let mut record = lock(&job.record);
        record.ended_at = Some(match own {
            true => now_ms(),
            // Ended while nobody followed it: the exit file's time, when there is one.
            false => std::fs::metadata(&job.exit_path)
                .and_then(|meta| meta.modified())
                .ok()
                .filter(|_| code.is_some())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or_else(now_ms, |d| d.as_millis() as u64),
        });
        match stopped {
            Some(status) => {
                record.status = status;
                record.exit_code = None;
                if status == Status::TimedOut {
                    record.error = Some(format!(
                        "timed out after {}s; process tree killed",
                        js_number(job.timeout.unwrap_or(0.0))
                    ));
                }
            }
            None => match (code, own) {
                (Some(code), _) => {
                    record.status = Status::Completed;
                    record.exit_code = Some(code);
                }
                // The keeper itself was killed: as a command killed by a signal, no exit code.
                (None, true) => {
                    record.status = Status::Completed;
                    record.exit_code = ended;
                }
                (None, false) => {
                    record.status = Status::Lost;
                    record.exit_code = None;
                    record.error =
                        Some("its process ended while no Ultron worker was running; the exit status is unknown".into());
                }
            },
        }
    }
    shared.persist();
    // `onEnd`, before the job's waiters wake: a caller still waiting gets the result inline, so nothing is announced.
    let reason = *lock(&job.reason);
    let end = JobEnd {
        record: job_record(job, &shared.cwd),
        awaited: job.waiters.load(Ordering::SeqCst) > 0,
        root_aborted: reason == Some(StopReason::RootAborted),
    };
    let ends = lock(&shared.ends).clone();
    job.finished();
    if reason != Some(StopReason::Closed)
        && let Some(ends) = ends
    {
        ends(end);
    }
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

fn cancel(job: &Job, reason: StopReason) {
    if *lock(&job.done.0) {
        return;
    }
    lock(&job.reason).get_or_insert(reason);
    *lock(&job.cancel.0) = true;
    job.cancel.1.notify_all();
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
        "lane": job.lane,
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
        self.dispatch("main", kind, payload, signal)
    }
}

/// The root lane sees every job; a child lane sees its own.
fn visible(job: &Job, lane: &str) -> bool {
    lane == "main" || job.lane == lane
}

impl Drop for Host {
    /// Worker shutdown: stop every running job, unless they were released to the next owner.
    fn drop(&mut self) {
        if !self.shared.released.load(Ordering::SeqCst) {
            self.stop_jobs();
        }
    }
}
