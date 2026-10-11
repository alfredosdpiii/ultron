//! Supervisor for Ultron's persistent Python kernel: a port of the host side of
//! `packages/coding-agent/src/ultron/rlm/kernel.ts`. The Python side is the unchanged `runtime.py`.
//!
//! The kernel runs `python runtime.py --protocol-fds` in its own process group. Protocol frames (one JSON object per
//! line, at most 1 MiB) arrive on the kernel's fd 3 and host messages go to its fd 4; stdin is /dev/null, and fd 1 and
//! fd 2 are plain output attributed to the running cell. After a cell, runtime.py writes a flush marker naming the
//! cell's request id to fd 1 and fd 2 and then sends `done`, listing the markers it wrote; the cell resolves once the
//! host has read them, so the cell's raw output is complete. With [`ProtocolChannel::Stdio`] frames use stdout and stdin.
//!
//! Calls block; cells run one at a time in call order. A [`Signal`] cancels a call: a queued call is rejected without
//! touching the kernel, a running one kills the kernel's process tree. Any kernel failure (exit, protocol violation,
//! startup timeout, CPU backstop) rejects every pending call with an explicit error and loses the Python state; the
//! next call starts a fresh kernel. Host requests from Python go to a [`HostHandler`], each on its own thread.
//!
//! Snapshots (kernel.ts `snapshot`/`restore`): runtime.py writes a data-only snapshot of the namespace; the host
//! signs it with an HMAC key the kernel never sees and verifies the signature before the kernel may read it back.
//! With `snapshot_path` a new kernel restores it before its first call; once a started kernel failed or a running
//! call was cancelled it does not (kernel.ts never silently resurrects an older checkpoint). A [`ContinuityPolicy`]
//! replaces that rule: checkpoints follow every cell, a dead kernel is restarted and restored at once, and the next
//! cell result reports what was restored and lost (see [`continuity`]).

mod config;
pub mod continuity;
mod output;
mod process;
mod sandboxed;
pub mod snapshot;

pub use continuity::{ContinuityPolicy, ContinuityReport, LostName};
pub use snapshot::{
    RestoreReport, SnapshotAuthError, SnapshotReport, load_or_create_snapshot_key, process_snapshot_key, sign_snapshot,
    verify_snapshot,
};

pub use config::{
    DEFAULT_KERNEL_LIMITS, KernelResourceLimits, LimitOverrides, kernel_environment, kernel_resource_limits,
    rlm_output_budget,
};

use command_fds::{CommandFdExt, FdMapping};
use continuity::{Crash, recovery_report};
use output::{
    MAX_FRAME_BYTES, MAX_ORPHAN_CHARS, RawCapture, Utf8Stream, append_bounded, find, flush_marker, keep_tail,
    marker_prefix_len, orphan_text, utf16_len,
};
use rustix::process::{WaitId, WaitIdOptions};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

/// A failed call: the kernel failed, was shut down, the call was cancelled, or its request could not be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelError {
    message: String,
}

impl KernelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for KernelError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellError {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

/// What a cell (or another request) produced. Serializes as kernel.ts `KernelExecutionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionResult {
    pub status: Status,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CellError>,
}

/// A call's full reply: the result plus the snapshot or restore report a done frame carried (kernel.ts
/// `KernelExecutionResult` with `snapshot?` / `restore?`), and, on the first cell after a recovery, what it
/// restored and lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KernelReply {
    #[serde(flatten)]
    pub result: ExecutionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ContinuityReport>,
}

impl KernelReply {
    fn from_result(result: ExecutionResult) -> Self {
        Self {
            result,
            snapshot: None,
            restore: None,
            recovery: None,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

type Listener = Box<dyn FnOnce(String) + Send>;

#[derive(Default)]
struct SignalState {
    reason: Option<String>,
    listeners: Vec<(u64, Listener)>,
    next: u64,
}

/// Cancellation, like an `AbortSignal`: aborted once, with a reason. A call's signal cancels it; the signal a
/// [`HostHandler`] receives is aborted when its cell completes or fails.
#[derive(Clone, Default)]
pub struct Signal {
    inner: Arc<(Mutex<SignalState>, Condvar)>,
}

impl Signal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Aborts with `reason`; later calls do nothing.
    pub fn abort(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let listeners = {
            let mut state = lock(&self.inner.0);
            if state.reason.is_some() {
                return;
            }
            state.reason = Some(reason.clone());
            std::mem::take(&mut state.listeners)
        };
        self.inner.1.notify_all();
        for (_, listener) in listeners {
            listener(reason.clone());
        }
    }

    pub fn reason(&self) -> Option<String> {
        lock(&self.inner.0).reason.clone()
    }

    pub fn is_aborted(&self) -> bool {
        lock(&self.inner.0).reason.is_some()
    }

    /// Blocks until aborted or `timeout` passes; whether it is aborted.
    pub fn wait(&self, timeout: Duration) -> bool {
        let state = lock(&self.inner.0);
        let (state, _) = self
            .inner
            .1
            .wait_timeout_while(state, timeout, |state| state.reason.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.reason.is_some()
    }

    /// Runs `listener` on abort; None (and `listener` is dropped) when already aborted.
    fn listen(&self, listener: Listener) -> Option<u64> {
        let mut state = lock(&self.inner.0);
        if state.reason.is_some() {
            return None;
        }
        state.next += 1;
        let id = state.next;
        state.listeners.push((id, listener));
        Some(id)
    }

    fn unlisten(&self, id: u64) {
        lock(&self.inner.0).listeners.retain(|(listener, _)| *listener != id);
    }
}

/// Serves host requests from Python (`await rlm.host_request(type, payload)` and every API built on it). Runs on its
/// own thread per request; `signal` is aborted when the requesting cell completes or the kernel fails, and a reply
/// after that is dropped. `Err` becomes a `RuntimeError` with that message in Python.
pub trait HostHandler: Send + Sync {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String>;
}

impl<F> HostHandler for F
where
    F: Fn(&str, Map<String, Value>, &Signal) -> Result<Value, String> + Send + Sync,
{
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self(kind, payload, signal)
    }
}

/// Test handler: replies `{"type": kind, "payload": payload}`, and refuses `test.fail` with "refused test.fail".
pub struct EchoHandler;

impl HostHandler for EchoHandler {
    fn handle(&self, kind: &str, payload: Map<String, Value>, _signal: &Signal) -> Result<Value, String> {
        if kind == "test.fail" {
            return Err(format!("refused {kind}"));
        }
        let mut reply = Map::new();
        reply.insert("type".into(), Value::String(kind.into()));
        reply.insert("payload".into(), Value::Object(payload));
        Ok(Value::Object(reply))
    }
}

/// Where protocol frames travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolChannel {
    /// Frames on fd 3, host messages on fd 4, stdin /dev/null; fd 1 and fd 2 are the running cell's output.
    PrivateFd,
    /// Frames on stdout and messages on stdin (kernel.ts's Windows channel): raw writes to fd 1 corrupt the stream.
    Stdio,
}

pub struct KernelOptions {
    pub cwd: PathBuf,
    pub runtime_path: PathBuf,
    /// Interpreter; default `ULTRON_PYTHON`, else /usr/bin/python3 when it exists, else `python3`.
    pub python: Option<PathBuf>,
    /// Bounds initialization, not execution.
    pub startup_timeout: Duration,
    /// Overrides `ULTRON_RLM_MAX_MEMORY_MB` / `ULTRON_RLM_MAX_CPU_SECONDS`; 0 disables a limit.
    pub limits: LimitOverrides,
    /// Extra environment for the kernel, applied after the credential filter.
    pub env: Vec<(String, String)>,
    pub protocol_channel: ProtocolChannel,
    /// The default path of [`Kernel::snapshot`] / [`Kernel::restore`]; a new kernel restores it before its first
    /// call (kernel.ts `snapshotPath`).
    pub snapshot_path: Option<PathBuf>,
    /// HMAC key that signs and verifies snapshots in the host; the kernel never receives it. Default: a key of this
    /// process, so snapshots then restore only within it.
    pub snapshot_key: Option<Vec<u8>>,
    /// Checkpoint after cells and recover from kernel death; requires `snapshot_path`.
    pub continuity: Option<ContinuityPolicy>,
    /// Start runtime.py inside this launcher's sandbox (the session's bash policy, plus the snapshot and spill
    /// directories writable and the interpreter readable), so a cell's direct `open()` is contained too. Docker
    /// cannot carry the protocol descriptors: with it (or nothing working) `auto` runs unsandboxed and `required`
    /// refuses to start. None: unsandboxed.
    pub sandbox: Option<Arc<sandbox::Launcher>>,
}

impl KernelOptions {
    pub fn new(cwd: impl Into<PathBuf>, runtime_path: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            runtime_path: runtime_path.into(),
            python: None,
            startup_timeout: Duration::from_secs(10),
            limits: LimitOverrides::default(),
            env: Vec::new(),
            protocol_channel: ProtocolChannel::PrivateFd,
            snapshot_path: None,
            snapshot_key: None,
            continuity: None,
            sandbox: None,
        }
    }
}

/// runtime.py exit codes for limits it could not report in-band.
const EXIT_MEMORY: i32 = 86;
const EXIT_CPU: i32 = 87;
const SIGKILL: i32 = 9;
const SIGXCPU: i32 = 24;
const MAX_HOST_REQUESTS: usize = 16;
/// How long a finished cell waits for its flush markers on fd 1 and fd 2.
const RAW_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a closed protocol channel waits for the exit status, so a limit's exit code is what gets reported.
const CLOSED_CHANNEL_GRACE: Duration = Duration::from_millis(200);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);
const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000000";
const SHUT_DOWN: &str = "RLM kernel is shut down";
const NOT_RUNNING: &str = "RLM kernel is not running";

fn memory_limit_message(limits: &KernelResourceLimits) -> String {
    format!(
        "RLM kernel exceeded its memory limit ({} MiB per process, ULTRON_RLM_MAX_MEMORY_MB) and was stopped; the next cell starts a fresh kernel without earlier Python state",
        limits.max_memory_mb
    )
}

fn cpu_limit_message(limits: &KernelResourceLimits) -> String {
    format!(
        "RLM kernel exceeded its CPU limit ({} CPU-seconds per cell, ULTRON_RLM_MAX_CPU_SECONDS) and was stopped; the next cell starts a fresh kernel without earlier Python state",
        limits.max_cpu_seconds
    )
}

/// A frame value as text: strings as they are, null as empty, anything else as JSON.
fn frame_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn encode_frame(frame: &Map<String, Value>) -> Result<String, KernelError> {
    let line = serde_json::to_string(frame)
        .map_err(|error| KernelError::new(format!("RLM kernel cannot encode outbound frame: {error}")))?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(KernelError::new(format!(
            "RLM kernel outbound frame exceeds 1 MiB ({MAX_FRAME_BYTES} bytes)"
        )));
    }
    Ok(line + "\n")
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

/// A random (version 4) UUID: the request id, which also names the cell's flush marker.
fn random_uuid() -> Result<String, KernelError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| KernelError::new(format!("RLM kernel request id: {error}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Lone UTF-16 surrogate escapes (which Python's json.dumps emits for surrogate-escaped text) become U+FFFD: a Rust
/// string cannot hold them, and refusing the frame would end the kernel.
fn replace_lone_surrogates(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("\\u") {
        return line.into();
    }
    let hex = |index: usize| -> Option<u16> {
        let digits = line.get(index..index + 6)?.strip_prefix("\\u")?;
        u16::from_str_radix(digits, 16).ok()
    };
    let bytes = line.as_bytes();
    let mut text = String::with_capacity(line.len());
    let (mut index, mut copied) = (0, 0);
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        match hex(index) {
            Some(0xd800..=0xdbff) if matches!(hex(index + 6), Some(0xdc00..=0xdfff)) => index += 12,
            Some(0xd800..=0xdfff) => {
                text.push_str(&line[copied..index]);
                text.push_str("\\ufffd");
                index += 6;
                copied = index;
            }
            _ => index += 2,
        }
    }
    if copied == 0 {
        return line.into();
    }
    text.push_str(&line[copied..]);
    text.into()
}

/// Exit of the kernel's root process. `exited` is set (under the state lock) before the process is reaped, so a
/// kill under that lock that sees it unset cannot hit a reused pid.
#[derive(Default)]
struct Exit {
    state: Mutex<(bool, Option<ExitStatus>)>,
    changed: Condvar,
}

impl Exit {
    fn exited(&self) -> bool {
        lock(&self.state).0
    }

    /// Waits up to `timeout` for the exit status; whether it is known.
    fn wait_reaped(&self, timeout: Duration) -> bool {
        let state = lock(&self.state);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.1.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.1.is_some()
    }
}

#[derive(Default)]
struct RawStream {
    decoder: Utf8Stream,
    /// Bytes that may start the running cell's flush marker, held until the next chunk.
    held: Vec<u8>,
    /// Output from while no cell owned the stream; reported at the start of the next cell.
    orphan: String,
    orphan_dropped: bool,
    /// Whether the runtime's startup marker has arrived; output before it is startup diagnostics.
    started: bool,
}

struct Waiter {
    op: u64,
    state: KernelReply,
    controller: Signal,
    handler: Option<Arc<dyn HostHandler>>,
}

/// Raw output of the execute cell that owns fd 1 and fd 2 until its flush markers arrive (or time out).
struct CellOutput {
    request_id: String,
    marker: Vec<u8>,
    captures: [RawCapture; 2],
    orphan: [String; 2],
    /// Set by the done frame: the markers runtime.py wrote and the host still waits for.
    awaiting: Option<[bool; 2]>,
    deadline: Option<Instant>,
    /// The finished cell, once its done frame has arrived.
    finished: Option<Waiter>,
}

struct Generation {
    id: u64,
    pid: i32,
    private_protocol: bool,
    writer: mpsc::Sender<String>,
    exit: Arc<Exit>,
    raw: [RawStream; 2],
    output: Option<CellOutput>,
    pending: HashMap<String, Waiter>,
    active_cell: Option<String>,
    host_requests: HashSet<String>,
    ready_received: bool,
    initialized: bool,
    startup_deadline: Instant,
    cell_cpu_start: Option<f64>,
    /// The process the CPU backstop measures: the root, or under a sandbox the Python process found at the first cell.
    cpu_pid: Option<i32>,
    /// The sandbox this generation runs in.
    sandbox: Option<sandbox::Mechanism>,
    buffer: Vec<u8>,
    /// The snapshot this generation restores before it counts as started.
    restore_path: Option<PathBuf>,
    /// After `ready`, startup is completed by a waiting caller (restore, recovery report) instead of the reader.
    host_completes_startup: bool,
    /// A caller is completing startup.
    completing: bool,
}

struct Op {
    running: bool,
    outcome: Option<Result<KernelReply, KernelError>>,
    /// The controller of the cell this call runs.
    cell: Option<Signal>,
    handler: Option<Arc<dyn HostHandler>>,
    /// A recovery report taken by this execute call when its cell was sent.
    recovery: Option<ContinuityReport>,
}

/// Continuity bookkeeping (see [`continuity`]).
#[derive(Default)]
struct Continuity {
    /// The latest checkpoint written to the configured snapshot path.
    checkpoint: Option<SnapshotReport>,
    /// Cells completed, and the count at the latest checkpoint.
    cells: u64,
    checkpoint_cells: u64,
    last_checkpoint: Option<Instant>,
    /// A cell completed since the latest checkpoint.
    dirty: bool,
    snapshot_queued: bool,
    idle_flush_scheduled: bool,
    /// The failure the next recovery reports.
    crash: Option<Crash>,
    /// A recovery report for the next cell.
    notice: Option<ContinuityReport>,
    last_recovery: Option<ContinuityReport>,
    /// The failed generation was running: start a new one without waiting for a call.
    restart: bool,
}

#[derive(Default)]
struct State {
    generation: Option<Generation>,
    next_generation: u64,
    /// The error the latest failed generation reported.
    failure: Option<(u64, KernelError)>,
    closed: bool,
    stderr: String,
    ops: HashMap<u64, Op>,
    queue: VecDeque<u64>,
    next_op: u64,
    /// kernel.ts `autoRestore = false`: never restore the configured snapshot into a later generation.
    no_auto_restore: bool,
    continuity: Continuity,
}

impl State {
    fn current(&mut self, generation: u64) -> Option<&mut Generation> {
        self.generation.as_mut().filter(|current| current.id == generation)
    }

    fn diagnostics(&self) -> String {
        if self.stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", self.stderr)
        }
    }

    fn resolve(&mut self, waiter: Waiter) {
        if let Some(op) = self.ops.get_mut(&waiter.op)
            && op.outcome.is_none()
        {
            op.outcome = Some(Ok(waiter.state));
        }
    }

    fn reject_ops(&mut self, error: &KernelError) {
        for op in self.ops.values_mut() {
            op.outcome.get_or_insert_with(|| Err(error.clone()));
        }
        self.queue.clear();
    }
}

struct Shared {
    options: KernelOptions,
    handler: Arc<dyn HostHandler>,
    limits: KernelResourceLimits,
    snapshot_key: Vec<u8>,
    state: Mutex<State>,
    changed: Condvar,
}

/// One kernel: a sequence of Python processes (generations), each replaced after a failure.
pub struct Kernel {
    shared: Arc<Shared>,
}

impl Kernel {
    pub fn new(options: KernelOptions, handler: Arc<dyn HostHandler>) -> Result<Self, KernelError> {
        let limits = kernel_resource_limits(|name| std::env::var(name).ok(), options.limits)?;
        if options.startup_timeout.is_zero() || options.startup_timeout > Duration::from_millis(2_147_483_647) {
            return Err(KernelError::new(
                "RLM startupTimeoutMs must be a positive, finite timer duration",
            ));
        }
        if options.continuity.is_some() && options.snapshot_path.is_none() {
            return Err(KernelError::new("RLM kernel continuity requires a snapshot path"));
        }
        let snapshot_key = options.snapshot_key.clone().unwrap_or_else(process_snapshot_key);
        Ok(Self {
            shared: Arc::new(Shared {
                options,
                handler,
                limits,
                snapshot_key,
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        })
    }

    pub fn limits(&self) -> KernelResourceLimits {
        self.shared.limits
    }

    /// Whether a kernel process is up and initialized.
    pub fn is_running(&self) -> bool {
        let state = lock(&self.shared.state);
        !state.closed
            && state
                .generation
                .as_ref()
                .is_some_and(|generation| generation.initialized)
    }

    /// The bounded tail of the kernel's raw stderr, for diagnostics.
    pub fn stderr_tail(&self) -> String {
        lock(&self.shared.state).stderr.clone()
    }

    /// The pid of the running kernel process.
    pub fn pid(&self) -> Option<u32> {
        let state = lock(&self.shared.state);
        state
            .generation
            .as_ref()
            .and_then(|generation| u32::try_from(generation.pid).ok())
    }

    /// The sandbox the running kernel is in; None when unsandboxed or not running.
    pub fn sandbox(&self) -> Option<sandbox::Mechanism> {
        let state = lock(&self.shared.state);
        state.generation.as_ref().and_then(|generation| generation.sandbox)
    }

    /// Starts the kernel process if none runs and waits until it is ready.
    pub fn start(&self) -> Result<(), KernelError> {
        let mut state = lock(&self.shared.state);
        let generation = self.shared.ensure_started(&mut state, true)?;
        self.shared.wait_started(state, generation, None).map(drop)
    }

    /// Runs a cell. `handler`, when given, serves this cell's host requests instead of the kernel's handler. The first
    /// cell after a recovery starts its stderr with the recovery notice ([`ContinuityReport::render`]).
    pub fn execute(
        &self,
        code: &str,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<ExecutionResult, KernelError> {
        self.execute_cell(code, signal, handler).map(|reply| reply.result)
    }

    /// [`Kernel::execute`] with the structured recovery report of the first cell after a recovery.
    pub fn execute_cell(
        &self,
        code: &str,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<KernelReply, KernelError> {
        let frame = object([("request", Value::from("execute")), ("code", Value::from(code))]);
        self.shared.enqueue(frame, signal, handler)
    }

    /// Clears invocation scratch while keeping the API bindings and the declared `state` dict.
    pub fn reset_scratch(&self) -> Result<ExecutionResult, KernelError> {
        self.shared
            .enqueue(object([("request", Value::from("reset_scratch"))]), None, None)
            .map(|reply| reply.result)
    }

    /// Saves the namespace's restorable names to `path` (default: the configured snapshot path), signed by the host.
    /// The reply's `snapshot` lists saved names and every skipped name with a reason; a failure leaves the previous
    /// file intact.
    pub fn snapshot(&self, path: Option<&Path>) -> Result<KernelReply, KernelError> {
        let path = self.snapshot_target(path)?;
        self.shared.enqueue(
            object([("request", Value::from("snapshot")), ("path", Value::from(path))]),
            None,
            None,
        )
    }

    /// Restores a host-verified snapshot from `path` (default: the configured snapshot path). Unsigned, tampered or
    /// foreign-key snapshots are refused before the kernel reads them, and the namespace stays untouched.
    pub fn restore(&self, path: Option<&Path>) -> Result<KernelReply, KernelError> {
        let path = self.snapshot_target(path)?;
        self.shared.enqueue(
            object([("request", Value::from("restore")), ("path", Value::from(path))]),
            None,
            None,
        )
    }

    fn snapshot_target(&self, path: Option<&Path>) -> Result<String, KernelError> {
        path.or(self.shared.options.snapshot_path.as_deref())
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| KernelError::new("RLM snapshot path is not configured"))
    }

    /// The latest recovery's report, also after it was delivered with a cell.
    pub fn last_recovery(&self) -> Option<ContinuityReport> {
        lock(&self.shared.state).continuity.last_recovery.clone()
    }

    /// The latest checkpoint written to the configured snapshot path.
    pub fn checkpoint(&self) -> Option<SnapshotReport> {
        lock(&self.shared.state).continuity.checkpoint.clone()
    }

    /// Permanently closes this kernel: kills its process tree, rejects every call, and waits (bounded) for the exit.
    pub fn shutdown(&self) {
        let exit = {
            let mut state = lock(&self.shared.state);
            state.closed = true;
            let error = KernelError::new(SHUT_DOWN);
            let exit = match state
                .generation
                .as_ref()
                .map(|generation| (generation.id, generation.exit.clone()))
            {
                Some((id, exit)) => {
                    self.shared.fail(&mut state, id, error);
                    Some(exit)
                }
                None => {
                    state.reject_ops(&error);
                    None
                }
            };
            self.shared.changed.notify_all();
            exit
        };
        if let Some(exit) = exit {
            exit.wait_reaped(SHUTDOWN_TIMEOUT);
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Shared {
    fn with_state(&self, run: impl FnOnce(&mut State)) {
        let mut state = lock(&self.state);
        run(&mut state);
        drop(state);
        self.changed.notify_all();
    }

    fn enqueue(
        self: &Arc<Self>,
        frame: Map<String, Value>,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<KernelReply, KernelError> {
        if lock(&self.state).closed {
            return Err(KernelError::new(SHUT_DOWN));
        }
        if let Some(reason) = signal.and_then(Signal::reason) {
            return Err(KernelError::new(reason));
        }
        let mut probe = frame.clone();
        probe.insert("id".into(), Value::from(PLACEHOLDER_ID));
        encode_frame(&probe)?;
        let op = {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(KernelError::new(SHUT_DOWN));
            }
            state.next_op += 1;
            let op = state.next_op;
            state.ops.insert(
                op,
                Op {
                    running: false,
                    outcome: None,
                    cell: None,
                    handler,
                    recovery: None,
                },
            );
            state.queue.push_back(op);
            op
        };
        let kind = frame
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let listener = signal.map(|signal| {
            let shared: Weak<Self> = Arc::downgrade(self);
            let id = signal.listen(Box::new(move |reason| {
                if let Some(shared) = shared.upgrade() {
                    shared.abort_op(op, reason);
                }
            }));
            if id.is_none() {
                self.abort_op(op, signal.reason().unwrap_or_default());
            }
            (signal, id)
        });
        let mut result = self.run(op, frame);
        if let Some((signal, Some(id))) = listener {
            signal.unlisten(id);
        }
        let mut state = lock(&self.state);
        let recovery = state.ops.remove(&op).and_then(|entry| entry.recovery);
        if state.queue.front() == Some(&op) {
            state.queue.pop_front();
        }
        match &mut result {
            Ok(reply) => {
                if let Some(report) = recovery {
                    reply.result.stderr = format!("{}{}", report.render(), reply.result.stderr);
                    reply.recovery = Some(report);
                }
                if kind == "execute" || kind == "reset_scratch" {
                    self.cell_completed(&mut state);
                }
            }
            // Not delivered: the next cell reports it, unless a newer recovery replaced it.
            Err(_) => {
                if state.continuity.notice.is_none() {
                    state.continuity.notice = recovery;
                }
            }
        }
        drop(state);
        self.changed.notify_all();
        result
    }

    /// Continuity: a cell may have changed the namespace; checkpoint now, or once the debounce interval has passed.
    fn cell_completed(self: &Arc<Self>, state: &mut State) {
        let Some(policy) = self.options.continuity else {
            return;
        };
        let continuity = &mut state.continuity;
        continuity.cells += 1;
        continuity.dirty = true;
        if continuity.snapshot_queued || state.closed {
            return;
        }
        let due = continuity
            .last_checkpoint
            .map(|last| last + policy.min_interval)
            .filter(|due| *due > Instant::now());
        match due {
            // Ahead of any queued cell: a crash in the next cell must find this cell's state saved.
            None => self.queue_checkpoint(state, true),
            Some(due) if !continuity.idle_flush_scheduled => {
                continuity.idle_flush_scheduled = true;
                let shared = Arc::downgrade(self);
                std::thread::spawn(move || {
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    let Some(shared) = shared.upgrade() else {
                        return;
                    };
                    let mut state = lock(&shared.state);
                    state.continuity.idle_flush_scheduled = false;
                    if state.continuity.dirty && !state.continuity.snapshot_queued && !state.closed {
                        shared.queue_checkpoint(&mut state, false);
                    }
                });
            }
            Some(_) => {}
        }
    }

    /// Queues a checkpoint of the configured snapshot path, at the front of the queue or at its end, and runs it on
    /// its own thread so the cell that triggered it returns at once.
    fn queue_checkpoint(self: &Arc<Self>, state: &mut State, front: bool) {
        let Some(path) = self.options.snapshot_path.as_ref() else {
            return;
        };
        state.next_op += 1;
        let op = state.next_op;
        state.ops.insert(
            op,
            Op {
                running: false,
                outcome: None,
                cell: None,
                handler: None,
                recovery: None,
            },
        );
        if front {
            state.queue.push_front(op);
        } else {
            state.queue.push_back(op);
        }
        state.continuity.snapshot_queued = true;
        let frame = object([
            ("request", Value::from("snapshot")),
            ("path", Value::from(path.to_string_lossy().into_owned())),
        ]);
        let shared = self.clone();
        std::thread::spawn(move || {
            // The outcome is recorded by signed_snapshot; a failed checkpoint keeps `dirty` for the next cell.
            let _ = shared.run(op, frame);
            let mut state = lock(&shared.state);
            state.ops.remove(&op);
            if state.queue.front() == Some(&op) {
                state.queue.pop_front();
            }
            state.continuity.snapshot_queued = false;
            drop(state);
            shared.changed.notify_all();
        });
    }

    fn abort_op(&self, op: u64, reason: String) {
        self.with_state(|state| {
            let error = KernelError::new(reason);
            let Some(entry) = state.ops.get_mut(&op) else {
                return;
            };
            if entry.outcome.is_some() {
                return;
            }
            if !entry.running {
                // Still queued: the process serving the preceding cell is left alone.
                entry.outcome = Some(Err(error));
                state.queue.retain(|queued| *queued != op);
                return;
            }
            if let Some(cell) = &entry.cell {
                cell.abort(error.message());
            }
            if self.options.continuity.is_none() {
                state.no_auto_restore = true;
            }
            match state.generation.as_ref().map(|generation| generation.id) {
                Some(generation) => self.fail(state, generation, error),
                None => entry_reject(state, op, error),
            }
        });
    }

    fn take_outcome(state: &mut State, op: u64) -> Option<Result<KernelReply, KernelError>> {
        state.ops.get_mut(&op).and_then(|entry| entry.outcome.take())
    }

    fn run(self: &Arc<Self>, op: u64, frame: Map<String, Value>) -> Result<KernelReply, KernelError> {
        let mut state = lock(&self.state);
        loop {
            if let Some(outcome) = Self::take_outcome(&mut state, op) {
                return outcome;
            }
            if state.queue.front() == Some(&op) {
                break;
            }
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(entry) = state.ops.get_mut(&op) {
            entry.running = true;
        }
        let kind = frame
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // An explicit restore can also recover from a broken configured snapshot.
        let generation = self.ensure_started(&mut state, kind != "restore")?;
        let state = self.wait_started(state, generation, Some(op))?;
        drop(state);
        let path = frame.get("path").and_then(Value::as_str).unwrap_or_default();
        match kind.as_str() {
            "snapshot" => self.signed_snapshot(op, generation, Path::new(path)),
            "restore" => self.verified_restore(op, generation, Path::new(path)),
            _ => self.request(op, generation, frame),
        }
    }

    /// Sends one request of call `op` to `generation` and waits for its done frame.
    fn request(
        self: &Arc<Self>,
        op: u64,
        generation: u64,
        mut frame: Map<String, Value>,
    ) -> Result<KernelReply, KernelError> {
        let mut state = lock(&self.state);
        let execute = frame.get("request") == Some(&Value::from("execute"));
        let request_id = random_uuid()?;
        frame.insert("id".into(), Value::from(request_id.clone()));
        let line = encode_frame(&frame)?;
        let handler = state.ops.get(&op).and_then(|entry| entry.handler.clone());
        let controller = Signal::new();
        let Some(current) = state.current(generation) else {
            return Err(Self::take_outcome(&mut state, op)
                .and_then(Result::err)
                .unwrap_or_else(|| KernelError::new(NOT_RUNNING)));
        };
        let state_ok = KernelReply::from_result(ExecutionResult {
            status: Status::Ok,
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: None,
        });
        current.pending.insert(
            request_id.clone(),
            Waiter {
                op,
                state: state_ok,
                controller: controller.clone(),
                handler,
            },
        );
        if execute {
            if current.cpu_pid.is_none() {
                current.cpu_pid = sandboxed::python_pid(current.pid, &self.python(), &self.options.runtime_path);
            }
            current.cell_cpu_start = current.cpu_pid.and_then(process::process_cpu_seconds);
            current.active_cell = Some(request_id.clone());
            if current.private_protocol {
                take_raw_output(current, &request_id);
            }
            let notice = state.continuity.notice.take();
            if let Some(entry) = state.ops.get_mut(&op) {
                entry.cell = Some(controller);
                entry.recovery = notice;
            }
        }
        if let Err(error) = self.send(&mut state, generation, line) {
            if let Some(current) = state.current(generation) {
                current.pending.remove(&request_id);
                if current.active_cell.as_deref() == Some(request_id.as_str()) {
                    current.active_cell = None;
                }
            }
            entry_reject(&mut state, op, error.clone());
            self.fail(&mut state, generation, error);
        }
        let mut cpu_checked = Instant::now();
        loop {
            if let Some(outcome) = Self::take_outcome(&mut state, op) {
                return outcome;
            }
            let now = Instant::now();
            let mut timeout = WATCHDOG_INTERVAL;
            if let Some(current) = state.current(generation) {
                // A startup restore is bounded by the startup timeout.
                if !current.initialized {
                    let deadline = current.startup_deadline;
                    if deadline <= now {
                        let error = self.startup_timeout(&state);
                        self.fail(&mut state, generation, error);
                        continue;
                    }
                    timeout = timeout.min(deadline - now);
                }
                let Some(current) = state.current(generation) else {
                    continue;
                };
                if let Some(deadline) = current.output.as_ref().and_then(|output| output.deadline) {
                    if deadline <= now {
                        finish_cell(&mut state, generation);
                        continue;
                    }
                    timeout = timeout.min(deadline - now);
                }
                if now.duration_since(cpu_checked) >= WATCHDOG_INTERVAL {
                    cpu_checked = now;
                    if self.cpu_exceeded(current) {
                        let error = KernelError::new(cpu_limit_message(&self.limits));
                        self.fail(&mut state, generation, error);
                        continue;
                    }
                }
            }
            state = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Backstop for the per-cell CPU limit: a cell stuck in C code or swallowing the limit error is killed after a
    /// grace period.
    fn cpu_exceeded(&self, generation: &Generation) -> bool {
        let limit = self.limits.max_cpu_seconds as f64;
        if limit <= 0.0 || generation.active_cell.is_none() {
            return false;
        }
        let Some(start) = generation.cell_cpu_start else {
            return false;
        };
        let grace = (limit * 0.1).clamp(2.0, 10.0);
        generation
            .cpu_pid
            .and_then(process::process_cpu_seconds)
            .is_some_and(|used| used - start > limit + grace)
    }

    /// Waits until `generation` is initialized, its startup times out, or it (or the call `op`) fails.
    fn wait_started<'a>(
        self: &'a Arc<Self>,
        mut state: MutexGuard<'a, State>,
        generation: u64,
        op: Option<u64>,
    ) -> Result<MutexGuard<'a, State>, KernelError> {
        loop {
            if let Some(op) = op
                && let Some(outcome) = Self::take_outcome(&mut state, op)
            {
                return Err(outcome.err().unwrap_or_else(|| KernelError::new(NOT_RUNNING)));
            }
            let Some(current) = state.current(generation) else {
                return Err(match &state.failure {
                    Some((failed, error)) if *failed == generation => error.clone(),
                    _ => KernelError::new(NOT_RUNNING),
                });
            };
            if current.initialized {
                return Ok(state);
            }
            if current.ready_received && !current.completing {
                // This caller completes startup: restore the configured snapshot, report a recovery.
                current.completing = true;
                let restore_path = current.restore_path.clone();
                drop(state);
                self.complete_startup(generation, restore_path);
                state = lock(&self.state);
                continue;
            }
            let deadline = current.startup_deadline;
            let now = Instant::now();
            if now >= deadline {
                let error = self.startup_timeout(&state);
                self.fail(&mut state, generation, error);
                continue;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn startup_timeout(&self, state: &State) -> KernelError {
        KernelError::new(format!(
            "RLM kernel startup timed out after {}ms{}",
            self.options.startup_timeout.as_millis(),
            state.diagnostics()
        ))
    }

    /// The running generation, or a new one; `restore` lets a new one restore the configured snapshot.
    fn ensure_started(self: &Arc<Self>, state: &mut State, restore: bool) -> Result<u64, KernelError> {
        if state.closed {
            return Err(KernelError::new(SHUT_DOWN));
        }
        if let Some(generation) = &state.generation {
            return Ok(generation.id);
        }
        self.spawn(state, restore)
    }

    /// Completes a generation's startup after `ready`: restores `restore_path` (kernel.ts `completeStartup`) and, with
    /// continuity, prepares the recovery report for the next cell.
    fn complete_startup(self: &Arc<Self>, generation: u64, restore_path: Option<PathBuf>) {
        let restored = restore_path.as_ref().map(|path| {
            let op = {
                let mut state = lock(&self.state);
                state.next_op += 1;
                let op = state.next_op;
                state.ops.insert(
                    op,
                    Op {
                        running: true,
                        outcome: None,
                        cell: None,
                        handler: None,
                        recovery: None,
                    },
                );
                op
            };
            let outcome = self.verified_restore(op, generation, path);
            lock(&self.state).ops.remove(&op);
            outcome
        });
        let mut state = lock(&self.state);
        if state.current(generation).is_none() {
            return;
        }
        let continuity = self.options.continuity.is_some();
        let skipped_restore = state
            .continuity
            .crash
            .as_ref()
            .and_then(|crash| crash.restore_error.clone());
        let restore: Result<Option<RestoreReport>, String> = match restored {
            // Continuity skips a checkpoint whose restore failed or killed the kernel; that failure is reported.
            None => skipped_restore.map_or(Ok(None), Err),
            Some(Ok(reply)) if reply.result.status == Status::Error => Err(reply
                .result
                .error
                .map_or_else(|| "unknown restore error".to_string(), |error| error.evalue)),
            Some(Ok(reply)) => Ok(reply.restore),
            Some(Err(error)) => {
                self.fail(&mut state, generation, error);
                drop(state);
                self.changed.notify_all();
                return;
            }
        };
        if let Err(message) = &restore
            && !continuity
        {
            let error = KernelError::new(format!("RLM kernel startup restore failed: {message}"));
            self.fail(&mut state, generation, error);
            drop(state);
            self.changed.notify_all();
            return;
        }
        if continuity {
            let crash = state.continuity.crash.take();
            let restored_something = matches!(&restore, Ok(Some(report)) if !report.missing);
            if crash.is_some() || restored_something || restore.is_err() {
                let crash = crash.unwrap_or_else(|| Crash {
                    reason: "kernel started from its checkpoint".into(),
                    interrupted_cell: false,
                    unsaved_cells: 0,
                    restore_error: None,
                });
                let path = self
                    .options
                    .snapshot_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                let report = recovery_report(crash, path, restore, state.continuity.checkpoint.as_ref());
                state.continuity.notice = Some(report.clone());
                state.continuity.last_recovery = Some(report);
            }
        }
        if let Some(current) = state.current(generation) {
            current.initialized = true;
        }
        drop(state);
        self.changed.notify_all();
    }

    /// kernel.ts `signedSnapshot`: the kernel writes an unsigned temporary file; the host checks it is exactly what
    /// the kernel reported, signs it, and atomically replaces `path`. A failure leaves the previous snapshot intact.
    fn signed_snapshot(self: &Arc<Self>, op: u64, generation: u64, path: &Path) -> Result<KernelReply, KernelError> {
        let directory = path.parent().unwrap_or(Path::new(""));
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let unsigned = directory.join(format!(".{name}.{}.unsigned", random_uuid()?));
        let frame = object([
            ("request", Value::from("snapshot")),
            ("path", Value::from(unsigned.to_string_lossy().into_owned())),
        ]);
        let outcome = self.request(op, generation, frame).and_then(|reply| {
            if reply.result.status != Status::Ok {
                return Ok(reply);
            }
            let failed = |error: String| KernelReply::from_result(snapshot::snapshot_error(error));
            let content = match std::fs::read(&unsigned) {
                Ok(content) => content,
                Err(error) => return Ok(failed(format!("snapshot signing failed: {error}"))),
            };
            let reported = reply
                .snapshot
                .as_ref()
                .and_then(|report| report.content_sha256.as_deref());
            if reported != Some(snapshot::sha256_hex(&content).as_str()) {
                return Ok(failed(
                    "snapshot changed after the kernel wrote it; refusing to sign it".into(),
                ));
            }
            let temporary = directory.join(format!(".{name}.{}.tmp", random_uuid()?));
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)
                .and_then(|mut file| file.write_all(&sign_snapshot(&self.snapshot_key, &content)))
                .and_then(|()| std::fs::rename(&temporary, path));
            let _ = std::fs::remove_file(&temporary);
            if let Err(error) = written {
                return Ok(failed(format!("snapshot signing failed: {error}")));
            }
            if self.options.snapshot_path.as_deref() == Some(path) {
                let mut state = lock(&self.state);
                let continuity = &mut state.continuity;
                continuity.checkpoint = reply.snapshot.clone();
                continuity.checkpoint_cells = continuity.cells;
                continuity.last_checkpoint = Some(Instant::now());
                continuity.dirty = false;
                // A good checkpoint may be restored again, even after a refused one.
                if self.options.continuity.is_some() {
                    state.no_auto_restore = false;
                }
            }
            Ok(reply)
        });
        let _ = std::fs::remove_file(&unsigned);
        outcome
    }

    /// kernel.ts `verifiedRestore`: refuses unsigned or tampered snapshots before the kernel reads them; the namespace
    /// stays untouched. The kernel re-hashes what it reads, so a file swapped after this check is refused too.
    fn verified_restore(self: &Arc<Self>, op: u64, generation: u64, path: &Path) -> Result<KernelReply, KernelError> {
        let shown = path.to_string_lossy().into_owned();
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            // A missing snapshot is reported by the kernel as `missing`; nothing is loaded.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let frame = object([("request", Value::from("restore")), ("path", Value::from(shown))]);
                return self.request(op, generation, frame);
            }
            Err(error) => {
                return Ok(KernelReply::from_result(snapshot::snapshot_error(format!(
                    "snapshot cannot be read: {error}"
                ))));
            }
        };
        match verify_snapshot(&self.snapshot_key, &raw) {
            Err(error) => Ok(KernelReply::from_result(snapshot::snapshot_error(error.0))),
            Ok(content_sha256) => {
                let frame = object([
                    ("request", Value::from("restore")),
                    ("path", Value::from(shown)),
                    ("content_sha256", Value::from(content_sha256)),
                ]);
                self.request(op, generation, frame)
            }
        }
    }

    /// The interpreter: `python`, else `ULTRON_PYTHON`, else /usr/bin/python3 when it exists, else `python3`.
    fn python(&self) -> PathBuf {
        self.options
            .python
            .clone()
            .unwrap_or_else(|| match std::env::var_os("ULTRON_PYTHON") {
                Some(python) => PathBuf::from(python),
                None if cfg!(target_os = "linux") && Path::new("/usr/bin/python3").exists() => {
                    "/usr/bin/python3".into()
                }
                None => "python3".into(),
            })
    }

    fn spawn(self: &Arc<Self>, state: &mut State, restore: bool) -> Result<u64, KernelError> {
        let options = &self.options;
        let private_protocol = options.protocol_channel == ProtocolChannel::PrivateFd;
        let python = self.python();
        let spawn_error = |error: std::io::Error| KernelError::new(format!("RLM kernel spawn failed: {error}"));
        let mechanism = match &options.sandbox {
            Some(launcher) => sandboxed::mechanism(launcher)?,
            None => None,
        };
        let mut argv: Vec<std::ffi::OsString> = vec![python.clone().into(), options.runtime_path.clone().into()];
        if private_protocol {
            argv.push("--protocol-fds".into());
        }
        let budget = rlm_output_budget(std::env::var("ULTRON_RLM_OUTPUT_BYTES").ok().as_deref());
        let fixed_env: Vec<(String, String)> = vec![
            ("NO_COLOR".into(), "1".into()),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ("ULTRON_RLM_MAX_MEMORY_MB".into(), self.limits.max_memory_mb.to_string()),
            (
                "ULTRON_RLM_MAX_CPU_SECONDS".into(),
                self.limits.max_cpu_seconds.to_string(),
            ),
            ("ULTRON_RLM_OUTPUT_BYTES".into(), budget.to_string()),
        ];
        let mut command = match (mechanism, &options.sandbox) {
            (Some(mechanism), Some(launcher)) => sandboxed::command(sandboxed::Launch {
                launcher,
                mechanism,
                python: &python,
                argv,
                cwd: &options.cwd,
                runtime: &options.runtime_path,
                snapshot_path: options.snapshot_path.as_deref(),
                fixed_env,
                extra_env: &options.env,
            }),
            _ => {
                let mut command = Command::new(&argv[0]);
                command
                    .args(&argv[1..])
                    .current_dir(&options.cwd)
                    .env_clear()
                    .envs(kernel_environment(std::env::vars_os()))
                    .envs(fixed_env)
                    .envs(options.env.iter().map(|(name, value)| (name, value)));
                command
            }
        };
        command.process_group(0).stdout(Stdio::piped()).stderr(Stdio::piped());
        let protocol = if private_protocol {
            let (from_kernel, kernel_out) = std::io::pipe().map_err(spawn_error)?;
            let (kernel_in, to_kernel) = std::io::pipe().map_err(spawn_error)?;
            command.stdin(Stdio::null());
            command
                .fd_mappings(vec![
                    FdMapping {
                        parent_fd: kernel_out.into(),
                        child_fd: 3,
                    },
                    FdMapping {
                        parent_fd: kernel_in.into(),
                        child_fd: 4,
                    },
                ])
                .map_err(|error| KernelError::new(error.to_string()))?;
            Some((from_kernel, to_kernel))
        } else {
            command.stdin(Stdio::piped());
            None
        };
        let mut child = if mechanism.is_some() {
            // Spawned (and the command dropped) on the process-lifetime spawner thread: see `sandboxed`.
            sandboxed::spawn(command).map_err(spawn_error)?
        } else {
            let child = command.spawn().map_err(spawn_error)?;
            // The command holds the kernel's ends of the protocol pipes; close them so the host sees end of file.
            drop(command);
            child
        };
        let pid = i32::try_from(child.id()).map_err(|error| KernelError::new(error.to_string()))?;
        let stderr = child.stderr.take();
        type Streams = (
            Box<dyn Read + Send>,
            Box<dyn Write + Send>,
            Option<std::process::ChildStdout>,
        );
        let (protocol_out, protocol_in, raw_stdout): Streams = match (protocol, child.stdout.take(), child.stdin.take())
        {
            (Some((from_kernel, to_kernel)), stdout, _) => (Box::new(from_kernel), Box::new(to_kernel), stdout),
            (None, Some(stdout), Some(stdin)) => (Box::new(stdout), Box::new(stdin), None),
            _ => return Err(KernelError::new(NOT_RUNNING)),
        };
        state.next_generation += 1;
        let id = state.next_generation;
        let restore_path = if restore && !state.no_auto_restore {
            options.snapshot_path.clone()
        } else {
            None
        };
        let host_completes_startup =
            restore_path.is_some() || (options.continuity.is_some() && state.continuity.crash.is_some());
        let (writer, lines) = mpsc::channel::<String>();
        let exit = Arc::new(Exit::default());
        state.generation = Some(Generation {
            id,
            pid,
            private_protocol,
            writer,
            exit: exit.clone(),
            raw: Default::default(),
            output: None,
            pending: HashMap::new(),
            active_cell: None,
            host_requests: HashSet::new(),
            ready_received: false,
            initialized: false,
            startup_deadline: Instant::now() + options.startup_timeout,
            cell_cpu_start: None,
            cpu_pid: mechanism.is_none().then_some(pid),
            sandbox: mechanism,
            buffer: Vec::new(),
            restore_path,
            host_completes_startup,
            completing: false,
        });
        state.stderr.clear();

        let shared = self.clone();
        let mut protocol_in = protocol_in;
        std::thread::spawn(move || {
            for line in lines {
                if let Err(error) = protocol_in
                    .write_all(line.as_bytes())
                    .and_then(|()| protocol_in.flush())
                {
                    shared.with_state(|state| shared.fail(state, id, KernelError::new(error.to_string())));
                    return;
                }
            }
        });
        let shared = self.clone();
        let channel_exit = exit.clone();
        std::thread::spawn(move || {
            let channel = if private_protocol {
                "protocol channel (fd 3)"
            } else {
                "stdout"
            };
            if let Err(error) = read_chunks(protocol_out, |chunk| {
                shared.with_state(|state| shared.handle_data(state, id, chunk));
            }) {
                shared.with_state(|state| shared.fail(state, id, KernelError::new(error.to_string())));
                return;
            }
            if !channel_exit.wait_reaped(CLOSED_CHANNEL_GRACE) {
                shared.with_state(|state| {
                    let error = KernelError::new(format!("RLM kernel {channel} closed{}", state.diagnostics()));
                    shared.fail(state, id, error);
                });
            }
        });
        if let Some(stdout) = raw_stdout {
            let shared = self.clone();
            std::thread::spawn(move || {
                let _ = read_chunks(stdout, |chunk| {
                    shared.with_state(|state| handle_raw(state, id, 0, chunk));
                });
            });
        }
        if let Some(stderr) = stderr {
            let shared = self.clone();
            std::thread::spawn(move || {
                let _ = read_chunks(stderr, |chunk| {
                    shared.with_state(|state| {
                        if state.current(id).is_none() {
                            return;
                        }
                        if private_protocol {
                            handle_raw(state, id, 1, chunk);
                        } else {
                            // Only a bounded tail is kept, for diagnostics.
                            let start = chunk.len().saturating_sub(4 * output::STDERR_TAIL_CHARS);
                            keep_tail(&mut state.stderr, &String::from_utf8_lossy(&chunk[start..]));
                        }
                    });
                });
            });
        }
        let shared = self.clone();
        std::thread::spawn(move || {
            if let Some(process) = rustix::process::Pid::from_raw(pid) {
                while let Err(rustix::io::Errno::INTR) =
                    rustix::process::waitid(WaitId::Pid(process), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT)
                {
                }
            }
            shared.with_state(|_| lock(&exit.state).0 = true);
            let status = child.wait();
            lock(&exit.state).1 = Some(status.as_ref().copied().unwrap_or_else(|_| ExitStatus::from_raw(0)));
            exit.changed.notify_all();
            shared.with_state(|state| {
                if state.current(id).is_some() {
                    let error = shared.exit_error(state, status.ok());
                    shared.fail(state, id, error);
                }
            });
            // Continuity: a kernel that died while serving is replaced and restored now, not at the next call.
            let mut state = lock(&shared.state);
            if std::mem::take(&mut state.continuity.restart)
                && !state.closed
                && state.generation.is_none()
                && let Ok(generation) = shared.ensure_started(&mut state, true)
            {
                drop(shared.wait_started(state, generation, None));
            }
        });
        Ok(id)
    }

    fn exit_error(&self, state: &State, status: Option<ExitStatus>) -> KernelError {
        let mut code = status.and_then(|status| status.code());
        let mut signal = status.and_then(|status| status.signal());
        // bubblewrap reports a command killed by a signal as exit code 128 + the signal.
        if state.generation.as_ref().and_then(|generation| generation.sandbox) == Some(sandbox::Mechanism::Bwrap)
            && let Some(number) = code.filter(|code| (129..=192).contains(code))
        {
            (code, signal) = (None, Some(number - 128));
        }
        if code == Some(EXIT_MEMORY) {
            return KernelError::new(memory_limit_message(&self.limits));
        }
        if code == Some(EXIT_CPU) || signal == Some(SIGXCPU) {
            return KernelError::new(cpu_limit_message(&self.limits));
        }
        // The host's own kills happen after fail(), so a SIGKILL seen here came from elsewhere.
        let hint = if signal == Some(SIGKILL) {
            "; the system out-of-memory killer may have stopped it"
        } else {
            ""
        };
        let how = match (signal, code) {
            (Some(signal), _) => process::signal_name(signal),
            (None, Some(code)) => code.to_string(),
            (None, None) => "unknown".into(),
        };
        KernelError::new(format!("RLM kernel exited ({how}){hint}{}", state.diagnostics()))
    }

    /// Ends `generation` with `error`: rejects every call, aborts its cells' host work, and kills its process tree.
    fn fail(&self, state: &mut State, generation: u64, error: KernelError) {
        if state.current(generation).is_none() {
            return;
        }
        let Some(failed) = state.generation.take() else {
            return;
        };
        state.failure = Some((generation, error.clone()));
        if self.options.continuity.is_some() {
            if !state.closed {
                let continuity = &mut state.continuity;
                let unsaved_cells = continuity.cells - continuity.checkpoint_cells;
                let interrupted_cell = failed.active_cell.is_some();
                let restoring = failed.completing && !failed.initialized;
                match continuity.crash.as_mut() {
                    // Died again before reporting: keep the first cause, add what this failure adds.
                    Some(crash) => {
                        crash.interrupted_cell |= interrupted_cell;
                        crash.unsaved_cells = unsaved_cells;
                    }
                    None => {
                        continuity.crash = Some(Crash {
                            reason: error.message().to_string(),
                            interrupted_cell,
                            unsaved_cells,
                            restore_error: None,
                        });
                    }
                }
                if restoring && failed.restore_path.is_some() {
                    // Restoring the checkpoint killed the kernel: do not try it again.
                    state.no_auto_restore = true;
                    if let Some(crash) = state.continuity.crash.as_mut() {
                        crash.restore_error = Some(format!("the kernel failed while restoring: {}", error.message()));
                    }
                }
                state.continuity.restart = failed.initialized;
            }
        } else if failed.initialized {
            // A failed startup keeps reporting a bad configured snapshot on retry; once user code could have run,
            // an older checkpoint is never silently resurrected.
            state.no_auto_restore = true;
        }
        for waiter in failed.pending.values() {
            waiter.controller.abort(error.message());
        }
        if let Some(waiter) = failed.output.as_ref().and_then(|output| output.finished.as_ref()) {
            waiter.controller.abort(error.message());
        }
        state.reject_ops(&error);
        process::kill_process_tree(failed.pid, true, !failed.exit.exited());
        // Dropping the generation closes the writer channel, which closes the kernel's input.
    }

    fn send(&self, state: &mut State, generation: u64, line: String) -> Result<(), KernelError> {
        let closed = state.closed;
        let failure = state.failure.clone();
        match state.current(generation) {
            Some(current) if !closed => current.writer.send(line).map_err(|_| KernelError::new(NOT_RUNNING)),
            _ => Err(match failure {
                Some((failed, error)) if failed == generation => error,
                _ => KernelError::new(NOT_RUNNING),
            }),
        }
    }

    fn handle_data(self: &Arc<Self>, state: &mut State, generation: u64, chunk: &[u8]) {
        let mut offset = 0;
        while offset < chunk.len() {
            let Some(current) = state.current(generation) else {
                return;
            };
            let newline = chunk[offset..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|index| offset + index);
            let end = newline.unwrap_or(chunk.len());
            // Bounded before decoding, including unterminated frames split across many chunks.
            if current.buffer.len() + (end - offset) > MAX_FRAME_BYTES {
                let error = format!("RLM kernel protocol frame exceeds 1 MiB ({MAX_FRAME_BYTES} bytes)");
                self.fail(state, generation, KernelError::new(error));
                return;
            }
            current.buffer.extend_from_slice(&chunk[offset..end]);
            if newline.is_none() {
                return;
            }
            let line = std::mem::take(&mut current.buffer);
            let handled = match std::str::from_utf8(&line) {
                Ok(text) => self.handle_line(state, generation, text),
                Err(_) => Err("The encoded data was not valid for encoding utf-8".to_string()),
            };
            if let Err(message) = handled {
                let error = KernelError::new(format!("RLM kernel invalid protocol frame: {message}"));
                self.fail(state, generation, error);
            }
            offset = end + 1;
        }
    }

    fn handle_line(self: &Arc<Self>, state: &mut State, generation: u64, line: &str) -> Result<(), String> {
        let parsed: Value =
            serde_json::from_str(&replace_lone_surrogates(line)).map_err(|error| format!("invalid JSON: {error}"))?;
        let Value::Object(frame) = parsed else {
            return Err("protocol frame must be a JSON object".into());
        };
        let Some(Value::String(event)) = frame.get("event") else {
            return Err("protocol frame is missing event".into());
        };
        let Some(current) = state.current(generation) else {
            return Ok(());
        };
        match event.as_str() {
            "ready" => {
                if current.ready_received {
                    return Err("duplicate ready event".into());
                }
                if frame.contains_key("id") {
                    return Err("ready event cannot have an id".into());
                }
                current.ready_received = true;
                current.initialized = !current.host_completes_startup;
                Ok(())
            }
            "host_request" => {
                let id = match frame.get("id") {
                    Some(Value::String(id)) if !id.is_empty() => id.clone(),
                    _ => return Err("host_request requires an id".into()),
                };
                let kind = match frame.get("type") {
                    Some(Value::String(kind)) if !kind.is_empty() => kind.clone(),
                    _ => return Err("host_request requires a type".into()),
                };
                let payload = match frame.get("payload") {
                    None => Map::new(),
                    Some(Value::Object(payload)) => payload.clone(),
                    Some(_) => return Err("host_request payload must be an object".into()),
                };
                self.handle_host_request(state, generation, id, kind, payload);
                Ok(())
            }
            "stdout" | "stderr" | "result" | "error" | "done" => {
                let id = match frame.get("id") {
                    Some(Value::String(id)) if !id.is_empty() => id.clone(),
                    _ => return Err(format!("{event} event requires an id")),
                };
                let status = frame.get("status").map(Value::as_str);
                if event == "done" && status.is_some_and(|status| status != Some("ok") && status != Some("error")) {
                    return Err("done event has invalid status".into());
                }
                route_execution_frame(state, generation, &id, event, &frame)
            }
            other => Err(format!("unknown protocol event: {other}")),
        }
    }

    fn handle_host_request(
        self: &Arc<Self>,
        state: &mut State,
        generation: u64,
        id: String,
        kind: String,
        payload: Map<String, Value>,
    ) {
        let Some(current) = state.current(generation) else {
            return;
        };
        if current.host_requests.contains(&id) {
            let error = KernelError::new(format!("RLM kernel protocol duplicate host request id: {id}"));
            self.fail(state, generation, error);
            return;
        }
        let cell = current.active_cell.as_ref().and_then(|cell| current.pending.get(cell));
        let Some(cell) = cell else {
            self.send_host_error(state, generation, &id, "RLM host request has no active cell");
            return;
        };
        if current.host_requests.len() >= MAX_HOST_REQUESTS {
            let message = format!("RLM host request concurrency exceeds {MAX_HOST_REQUESTS}");
            self.send_host_error(state, generation, &id, &message);
            return;
        }
        let handler = cell.handler.clone().unwrap_or_else(|| self.handler.clone());
        let signal = cell.controller.clone();
        current.host_requests.insert(id.clone());
        let shared = self.clone();
        std::thread::spawn(move || {
            let result = handler.handle(&kind, payload, &signal);
            shared.with_state(|state| {
                if state.current(generation).is_some() && !signal.is_aborted() {
                    match result {
                        Ok(value) => {
                            let reply = object([
                                ("request", Value::from("host_reply")),
                                ("id", Value::from(id.clone())),
                                ("payload", value),
                            ]);
                            shared.send_host_reply(state, generation, &id, &reply);
                        }
                        Err(message) => shared.send_host_error(state, generation, &id, &message),
                    }
                }
                if let Some(current) = state.current(generation) {
                    current.host_requests.remove(&id);
                }
            });
        });
    }

    fn send_host_reply(&self, state: &mut State, generation: u64, id: &str, reply: &Map<String, Value>) {
        let sent = encode_frame(reply).and_then(|line| self.send(state, generation, line));
        if let Err(error) = sent {
            if state.current(generation).is_none() {
                return;
            }
            self.send_host_error(state, generation, id, error.message());
        }
    }

    fn send_host_error(&self, state: &mut State, generation: u64, id: &str, message: &str) {
        if state.current(generation).is_none() {
            return;
        }
        let reply = object([
            ("request", Value::from("host_reply")),
            ("id", Value::from(id)),
            ("error", Value::from(message)),
        ]);
        if let Err(error) = encode_frame(&reply).and_then(|line| self.send(state, generation, line)) {
            self.fail(state, generation, error);
        }
    }
}

fn entry_reject(state: &mut State, op: u64, error: KernelError) {
    if let Some(entry) = state.ops.get_mut(&op) {
        entry.outcome.get_or_insert(Err(error));
    }
}

/// Reads `stream` to its end in chunks; an interrupted read is retried.
fn read_chunks(mut stream: impl Read, mut handle: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => handle(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn route_execution_frame(
    state: &mut State,
    generation: u64,
    request_id: &str,
    event: &str,
    frame: &Map<String, Value>,
) -> Result<(), String> {
    let Some(current) = state.current(generation) else {
        return Ok(());
    };
    let Some(waiter) = current.pending.get_mut(request_id) else {
        return Err(format!("protocol frame references unknown request id: {request_id}"));
    };
    let result = &mut waiter.state.result;
    match event {
        "stdout" => append_bounded(&mut result.stdout, &frame_string(frame.get("text"))),
        "stderr" => append_bounded(&mut result.stderr, &frame_string(frame.get("text"))),
        "result" => result.result = Some(frame_string(frame.get("result"))),
        "error" => {
            result.status = Status::Error;
            let traceback = match frame.get("traceback") {
                Some(Value::Array(lines)) => lines
                    .iter()
                    .filter_map(|line| line.as_str().map(String::from))
                    .collect(),
                _ => Vec::new(),
            };
            result.error = Some(CellError {
                ename: frame_string(frame.get("ename")),
                evalue: frame_string(frame.get("evalue")),
                traceback,
            });
        }
        _ => {
            if frame.get("status") == Some(&Value::from("error")) {
                result.status = Status::Error;
            }
            // Snapshot and restore failures arrive only on the done frame.
            if result.status == Status::Error
                && result.error.is_none()
                && let Some(error) = frame.get("error").filter(|error| !error.is_null())
            {
                result.error = Some(CellError {
                    ename: "RuntimeError".into(),
                    evalue: frame_string(Some(error)),
                    traceback: Vec::new(),
                });
            }
            let Some(mut waiter) = current.pending.remove(request_id) else {
                return Ok(());
            };
            if let Some(report) = snapshot::snapshot_report(frame.get("snapshot")) {
                waiter.state.snapshot = Some(report);
            }
            if let Some(report) = snapshot::restore_report(frame.get("restore")) {
                waiter.state.restore = Some(report);
            }
            if current.active_cell.as_deref() == Some(request_id) {
                current.active_cell = None;
            }
            waiter.controller.abort("RLM cell completed");
            let Some(output) = current.output.as_mut().filter(|output| output.request_id == request_id) else {
                state.resolve(waiter);
                return Ok(());
            };
            // fd 1 and fd 2 are separate pipes from the protocol: wait (bounded) for the flush markers the runtime
            // wrote after the cell's raw output, so the result is complete.
            let flush = frame.get("flush").and_then(Value::as_object);
            let flushed = |name: &str| flush.and_then(|flush| flush.get(name)) == Some(&Value::Bool(true));
            output.awaiting = Some([flushed("stdout"), flushed("stderr")]);
            output.deadline = Some(Instant::now() + RAW_FLUSH_TIMEOUT);
            output.finished = Some(waiter);
            finish_cell_if_flushed(state, generation);
        }
    }
    Ok(())
}

/// The cell owns fd 1 and fd 2 output from now on, and reports the output from outside a cell that came before it.
fn take_raw_output(generation: &mut Generation, request_id: &str) {
    let orphan = generation.raw.each_mut().map(|stream| {
        let text = orphan_text(&stream.orphan, stream.orphan_dropped);
        stream.orphan.clear();
        stream.orphan_dropped = false;
        text
    });
    generation.output = Some(CellOutput {
        request_id: request_id.to_string(),
        marker: flush_marker(request_id),
        captures: Default::default(),
        orphan,
        awaiting: None,
        deadline: None,
        finished: None,
    });
}

/// Raw bytes from fd 1 (`index` 0) or fd 2 (1). They belong to the cell that owns the output until that cell's flush
/// marker arrives on the stream; anything else is output from outside a cell.
fn handle_raw(state: &mut State, generation: u64, index: usize, chunk: &[u8]) {
    let State {
        generation: current,
        stderr,
        ..
    } = state;
    let Some(current) = current.as_mut().filter(|current| current.id == generation) else {
        return;
    };
    let Generation { raw, output, .. } = current;
    let stream = &mut raw[index];
    let mut data = std::mem::take(&mut stream.held);
    data.extend_from_slice(chunk);
    let mut data = &data[..];
    if !stream.started {
        // Before the startup marker: stderr only feeds the diagnostic tail, stdout is dropped.
        let startup = flush_marker("ready");
        let found = find(data, &startup);
        let end = found.unwrap_or_else(|| data.len() - marker_prefix_len(data, &startup));
        if index == 1 {
            keep_tail(stderr, &String::from_utf8_lossy(&data[..end]));
        }
        let Some(found) = found else {
            stream.held = data[end..].to_vec();
            return;
        };
        stream.started = true;
        data = &data[found + startup.len()..];
    }
    let Some(cell) = output.as_mut().filter(|cell| !cell.captures[index].marker_seen) else {
        route_raw(stream, stderr, index, data, None);
        return;
    };
    if let Some(found) = find(data, &cell.marker) {
        route_raw(stream, stderr, index, &data[..found], Some(&mut cell.captures[index]));
        cell.captures[index].marker_seen = true;
        route_raw(stream, stderr, index, &data[found + cell.marker.len()..], None);
        finish_cell_if_flushed(state, generation);
        return;
    }
    let held = marker_prefix_len(data, &cell.marker);
    stream.held = data[data.len() - held..].to_vec();
    route_raw(
        stream,
        stderr,
        index,
        &data[..data.len() - held],
        Some(&mut cell.captures[index]),
    );
}

fn route_raw(
    stream: &mut RawStream,
    stderr: &mut String,
    index: usize,
    bytes: &[u8],
    capture: Option<&mut RawCapture>,
) {
    if bytes.is_empty() {
        return;
    }
    let text = stream.decoder.decode(bytes);
    if index == 1 {
        keep_tail(stderr, &text);
    }
    if let Some(capture) = capture {
        capture.push(&text);
        return;
    }
    let used = utf16_len(&stream.orphan);
    let room = MAX_ORPHAN_CHARS.saturating_sub(used);
    if utf16_len(&text) > room {
        stream.orphan_dropped = true;
    }
    if room > 0 {
        let kept = output::utf16_head(&text, room);
        stream.orphan.push_str(kept);
    }
}

/// Resolves the finished cell once every flush marker its done frame announced has arrived.
fn finish_cell_if_flushed(state: &mut State, generation: u64) {
    let Some(output) = state.current(generation).and_then(|current| current.output.as_ref()) else {
        return;
    };
    let (Some(_), Some(awaiting)) = (&output.finished, output.awaiting) else {
        return;
    };
    if (0..2).any(|index| awaiting[index] && !output.captures[index].marker_seen) {
        return;
    }
    finish_cell(state, generation);
}

/// Combines the output from outside a cell, the cell's captured frames and its raw output, and resolves the cell.
fn finish_cell(state: &mut State, generation: u64) {
    let State {
        generation: current,
        stderr,
        ..
    } = state;
    let Some(current) = current.as_mut().filter(|current| current.id == generation) else {
        return;
    };
    let Some(output) = current.output.take() else {
        return;
    };
    // A held partial marker was output after all.
    for (index, stream) in current.raw.iter_mut().enumerate() {
        if stream.started {
            let held = std::mem::take(&mut stream.held);
            route_raw(stream, stderr, index, &held, None);
        }
    }
    let Some(mut waiter) = output.finished else {
        return;
    };
    let [orphan_out, orphan_err] = &output.orphan;
    waiter.state.result.stdout = format!(
        "{orphan_out}{}{}",
        waiter.state.result.stdout,
        output.captures[0].text(1)
    );
    waiter.state.result.stderr = format!(
        "{orphan_err}{}{}",
        waiter.state.result.stderr,
        output.captures[1].text(2)
    );
    state.resolve(waiter);
}
