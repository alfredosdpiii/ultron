//! Generic file-write hooks for the RLM REPL (ultron/file-hooks.ts):
//!
//! 1. Before a write (blocking): the kernel's `edit()`/`write()` send each proposed write to the host before touching
//!    the file (`files.before_write`). A guard that blocks makes `write()` raise ValueError with its diagnostic
//!    (runtime.py `_check_write`); a guard that fails lets the write proceed with a visible note: it fails open,
//!    never silently.
//! 2. After a cell (advisory): files a cell changed by other means (`bash('sed -i ...')`, `Path.write_text`) are found
//!    by comparing workspace snapshots taken between cells, minus the checked writes whose content is unchanged.
//!    Guards' `after_cell_changes` run in the background, so a cell returns as fast as before, and what they report
//!    reaches the model with the lane's next cell result ([`FileHooks::take_pending`]).
//! 3. Before a shell command (blocking): commands that may write a file go to the guards before they run; a guard
//!    that blocks makes the host refuse the command ("bash command was not run: ...").

use crate::Guard;
use fancy_regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// A guard's answer: block with a reason, or allow with an optional note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Block(String),
    Allow(Option<String>),
}

/// Files one cell changed (absolute paths).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellChanges {
    /// Added or modified files no before-write check saw (or whose checked content was changed again).
    pub files: Vec<PathBuf>,
    /// Files written through a checked `edit()`/`write()` whose content is what was checked.
    pub checked: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    /// False when a snapshot hit its size bound, so some changes may be missing.
    pub complete: bool,
}

/// Whether an after-cell report holds something to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterOutcome {
    /// Nothing, or only a note.
    Clean,
    /// Only for-your-information lines.
    Advisory,
    /// Something to fix.
    Findings,
}

/// What an after-cell check has to say (`AfterCellReport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCellReport {
    /// Text for the model, delivered with the lane's next cell result.
    pub message: Option<String>,
    pub outcome: AfterOutcome,
}

/// One registered guard (Loki, extensions). `Err` means the guard could not check (the write proceeds unchecked).
pub trait FileWriteGuard: Send + Sync {
    /// Shown in notes and statistics ("Loki").
    fn name(&self) -> &str;
    /// False while the guard has nothing to do (checked per call; file-hooks.ts `enabled`).
    fn enabled(&self) -> bool {
        true
    }
    /// Whether the guard checks proposed writes / shell commands at all (TS: whether `beforeWrite` / `beforeShell`
    /// is defined); only those calls are counted in its statistics.
    fn checks_writes(&self) -> bool {
        true
    }
    fn checks_shell(&self) -> bool {
        true
    }
    /// Per-call time limit of `before_write` and `before_shell` (default 5 s); `None` waits for the answer. A guard
    /// that does not answer in time lets the write (or command) proceed with a note.
    fn timeout(&self) -> Option<Duration> {
        Some(DEFAULT_BEFORE_WRITE_TIMEOUT)
    }
    /// A proposed write: the absolute, normalized path and the complete new text; `cwd` is the lane's directory.
    fn before_write(&self, _path: &Path, _content: &str, _cwd: &Path) -> Result<Verdict, String> {
        Ok(Verdict::Allow(None))
    }
    /// [`Self::before_write`] for a write of the cell on `lane` (the extension event carries the lane).
    fn before_write_on(&self, path: &Path, content: &str, cwd: &Path, _lane: &str) -> Result<Verdict, String> {
        self.before_write(path, content, cwd)
    }
    /// A shell command that may write files ([`may_write_files`]), before it runs in `cwd`.
    fn before_shell(&self, _command: &str, _cwd: &Path) -> Result<Verdict, String> {
        Ok(Verdict::Allow(None))
    }
    /// Whether the guard wants the files each cell changed (snapshots are taken only while one does); checked per
    /// cell.
    fn watches_cells(&self) -> bool {
        false
    }
    /// The files a cell on `lane` (working in `cwd`) changed by other means than checked writes.
    fn after_cell_changes(
        &self,
        _changes: &CellChanges,
        _cwd: &Path,
        _lane: &str,
    ) -> Result<Option<AfterCellReport>, String> {
        Ok(None)
    }
    /// How long `after_cell_changes` may take (default 120 s).
    fn after_timeout(&self) -> Duration {
        DEFAULT_AFTER_CELL_TIMEOUT
    }
}

/// A picture of a working directory: relative path (with `/`) to a metadata signature (workspace-snapshot.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub root: PathBuf,
    pub entries: BTreeMap<String, String>,
    pub complete: bool,
}

/// Takes a [`Snapshot`] of a directory.
pub type Snapshotter = Arc<dyn Fn(&Path) -> Result<Snapshot, String> + Send + Sync>;

pub const DEFAULT_AFTER_CELL_TIMEOUT: Duration = Duration::from_secs(120);
/// `DEFAULT_BEFORE_WRITE_TIMEOUT_MS`.
pub const DEFAULT_BEFORE_WRITE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// One guard's time and outcomes, for the RLM pane and diagnostics (`GuardStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardStats {
    pub name: String,
    /// Before-write (and before-shell) checks run, those that blocked, those that timed out or failed.
    pub checks: u64,
    pub blocked: u64,
    pub unchecked: u64,
    /// After-cell checks run, and how many reported findings to fix (or could not check).
    pub after_checks: u64,
    pub after_findings: u64,
    /// Time spent in the guard: in all, and since the current root turn began.
    pub total_ms: u64,
    pub turn_ms: u64,
}

impl GuardStats {
    /// The TS object (`fileHooks.stats()` entries).
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "checks": self.checks,
            "blocked": self.blocked,
            "unchecked": self.unchecked,
            "afterChecks": self.after_checks,
            "afterFindings": self.after_findings,
            "totalMs": self.total_ms,
            "turnMs": self.turn_ms,
        })
    }
}

/// Every guard outcome, for logs (`GuardRecord`, ULTRON_LOKI_LOG).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardRecord {
    pub guard: String,
    /// `before_write`, `before_shell` or `after_cell`.
    pub phase: &'static str,
    /// `allowed`, `blocked`, `unchecked`, `clean`, `advisory` or `findings`.
    pub outcome: &'static str,
    pub ms: u64,
    pub paths: Vec<String>,
    pub lane: String,
    pub detail: Option<String>,
}

impl GuardRecord {
    /// The record as TS writes it (`detail` left out when unset).
    pub fn to_json(&self) -> Value {
        let mut record = json!({
            "guard": self.guard,
            "phase": self.phase,
            "outcome": self.outcome,
            "ms": self.ms,
            "paths": self.paths,
            "lane": self.lane,
        });
        if let Some(detail) = &self.detail {
            record["detail"] = json!(detail);
        }
        record
    }
}

/// Receives every [`GuardRecord`].
pub type OnRecord = Arc<dyn Fn(&GuardRecord) + Send + Sync>;
/// Milliseconds on some clock (TS `now`, default `Date.now`): only differences are used.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// `Math.round(ms / 100) / 10` as JavaScript prints it ("5", "0.2").
fn js_seconds(timeout: Duration) -> String {
    let seconds = (timeout.as_millis() as f64 / 100.0).round() / 10.0;
    if seconds.fract() == 0.0 {
        format!("{}", seconds as i64)
    } else {
        format!("{seconds}")
    }
}

/// How one timed guard call ended.
enum Answer<T> {
    Value(T),
    Error(String),
    TimedOut,
}

/// Run `call` on its own thread and wait at most `timeout` (file-hooks.ts `#timed`): a call that does not answer in
/// time is left to finish on its own.
fn timed<T: Send + 'static>(
    timeout: Option<Duration>,
    call: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Answer<T> {
    let (sender, answer) = channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    let received = match timeout {
        Some(timeout) => answer.recv_timeout(timeout).ok(),
        None => answer.recv().ok(),
    };
    match received {
        Some(Ok(value)) => Answer::Value(value),
        Some(Err(error)) => Answer::Error(error),
        None => Answer::TimedOut,
    }
}

/// A guard call's answer, once it comes (or its time is up).
type Pending = std::sync::mpsc::Receiver<Answer<Verdict>>;

/// A registered guard and its statistics.
#[derive(Clone)]
struct Registered {
    guard: Arc<dyn FileWriteGuard>,
    stats: Arc<Mutex<GuardStats>>,
}
/// Findings delivered with one cell result are capped; the rest are summarized.
const MAX_PENDING_CHARS: usize = 6_000;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

// JavaScript's `\b` after `null` is "no ASCII word character next"; `\d` is ASCII.
static DEV_NULL: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>>?\s*/dev/null(?![A-Za-z0-9_])"));
static FD_DUP: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>&[0-9]+"));
static WRITERS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        re(r"(^|[^<>-])>{1,2}\|?(?!&)"),
        re(r"(^|[\s;&|(])(tee|cp|mv|install)\s"),
        re(r"(^|[\s;&|(])sed\s+(?:[^|;&]*\s)?(-i|--in-place)"),
        re(r"(^|[\s;&|(])perl\s+(?:[^|;&]*\s)?-[a-zA-Z]*i"),
        re(r"(^|[\s;&|(])dd\s[^|;&]*\bof="),
    ]
});

/// `mayWriteFiles`: whether a command may write a file, cheaply and generously (a redirect, `tee`, `sed -i`,
/// `perl -i`, `cp`/`mv`/`install`, `dd of=`). Redirects to `/dev/null` and fd duplications do not count.
pub fn may_write_files(command: &str) -> bool {
    let text = DEV_NULL.replace_all(command, " ");
    let text = FD_DUP.replace_all(&text, " ");
    WRITERS
        .iter()
        .any(|pattern| pattern.is_match(text.as_ref()).unwrap_or(false))
}

/// `path.resolve(cwd, path)`: lexical, `..` and `.` removed.
pub fn resolve_lexical(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            _ => {}
        }
    }
    out
}

/// `path.relative(from, to)` when `to` is inside `from`, as a `/` path; None for `from` itself or outside it.
pub fn inside(from: &Path, to: &Path) -> Option<String> {
    let relative = to.strip_prefix(from).ok()?.to_string_lossy().into_owned();
    (!relative.is_empty() && !relative.starts_with("..")).then_some(relative)
}

/// The guards of one working directory (the session's, or a worktree subagent's), answering the host's
/// `files.before_write` and shell checks, and checking what each cell changed (`FileHooks`).
#[derive(Clone)]
pub struct FileHooks {
    inner: Arc<Inner>,
}

struct Inner {
    cwd: PathBuf,
    guards: Mutex<Vec<Registered>>,
    on_record: Mutex<Option<OnRecord>>,
    now: Mutex<Clock>,
    snapshot: Option<Snapshotter>,
    /// Checked writes since the last after-cell snapshot: absolute path -> sha256 of the checked content.
    checked: Mutex<HashMap<PathBuf, String>>,
    /// Findings not yet shown, per lane.
    pending: Mutex<HashMap<String, Vec<String>>>,
    /// The last snapshot of the directory (taken before the first watched cell, then after each).
    baseline: Mutex<Option<Snapshot>>,
    /// After-cell checks queued or running, and the worker that runs them one at a time.
    queued: Mutex<usize>,
    idle: Condvar,
    worker: Mutex<Option<Sender<String>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `diffSnapshots`: added, modified and deleted paths (each sorted), and whether both pictures were complete.
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> (Vec<String>, Vec<String>, Vec<String>, bool) {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    for (path, value) in &after.entries {
        match before.entries.get(path) {
            None => added.push(path.clone()),
            Some(previous) if previous != value => modified.push(path.clone()),
            Some(_) => {}
        }
    }
    let deleted = before
        .entries
        .keys()
        .filter(|path| !after.entries.contains_key(*path))
        .cloned()
        .collect();
    (added, modified, deleted, before.complete && after.complete)
}

/// The first `limit` UTF-16 code units of `text` (JavaScript's `slice(0, limit)`), and how many there were in all.
fn js_cut(text: &str, limit: usize) -> (String, usize) {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= limit {
        return (text.to_string(), units.len());
    }
    (String::from_utf16_lossy(&units[..limit]), units.len())
}

impl FileHooks {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        FileHooks {
            inner: Arc::new(Inner {
                cwd: cwd.into(),
                guards: Mutex::new(Vec::new()),
                on_record: Mutex::new(None),
                now: Mutex::new({
                    let started = std::time::Instant::now();
                    Arc::new(move || started.elapsed().as_millis() as u64)
                }),
                snapshot: None,
                checked: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                baseline: Mutex::new(None),
                queued: Mutex::new(0),
                idle: Condvar::new(),
                worker: Mutex::new(None),
            }),
        }
    }

    /// Hooks that also check what each cell changed, picturing the directory with `snapshot`.
    pub fn with_snapshots(cwd: impl Into<PathBuf>, snapshot: Snapshotter) -> Self {
        let mut hooks = FileHooks::new(cwd);
        if let Some(inner) = Arc::get_mut(&mut hooks.inner) {
            inner.snapshot = Some(snapshot);
        }
        hooks
    }

    pub fn add(&mut self, guard: Box<dyn FileWriteGuard>) {
        let stats = GuardStats {
            name: guard.name().to_string(),
            ..GuardStats::default()
        };
        lock(&self.inner.guards).push(Registered {
            guard: Arc::from(guard),
            stats: Arc::new(Mutex::new(stats)),
        });
    }

    /// Every guard outcome goes to `on_record` (ULTRON_LOKI_LOG).
    pub fn on_record(&self, on_record: OnRecord) {
        *lock(&self.inner.on_record) = Some(on_record);
    }

    /// The clock guard times are measured with (tests).
    pub fn set_clock(&self, now: Clock) {
        *lock(&self.inner.now) = now;
    }

    /// A new root turn began: per-turn times start from zero.
    pub fn begin_turn(&self) {
        for registered in lock(&self.inner.guards).iter() {
            lock(&registered.stats).turn_ms = 0;
        }
    }

    /// Each guard's statistics, in registration order.
    pub fn stats(&self) -> Vec<GuardStats> {
        lock(&self.inner.guards)
            .iter()
            .map(|registered| lock(&registered.stats).clone())
            .collect()
    }

    /// True when some guard wants to see changes after each cell (snapshots are taken only then).
    pub fn watches_cells(&self) -> bool {
        self.inner.watches_cells()
    }

    /// A cell is starting: make sure a baseline picture of the directory exists to compare its end with.
    pub fn cell_started(&self, _lane: &str) {
        let inner = &self.inner;
        if !inner.watches_cells() || lock(&inner.baseline).is_some() {
            return;
        }
        let picture = inner.picture();
        let mut baseline = lock(&inner.baseline);
        if baseline.is_none() {
            *baseline = picture;
        }
    }

    /// A cell on `lane` ended: in the background, diff the directory against the previous picture and give the
    /// guards the files changed by other means than checked writes. Returns at once; [`Self::settled`] waits.
    pub fn cell_ended(&self, lane: &str) {
        let inner = &self.inner;
        if !inner.watches_cells() {
            return;
        }
        if lock(&inner.baseline).is_none() {
            // No baseline yet (a guard became interested mid-session): this cell's changes cannot be told apart.
            self.cell_started(lane);
            return;
        }
        *lock(&inner.queued) += 1;
        let mut worker = lock(&inner.worker);
        let sender = worker.get_or_insert_with(|| {
            let (sender, lanes) = channel::<String>();
            let inner = Arc::downgrade(&self.inner);
            std::thread::spawn(move || {
                for lane in lanes {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    inner.after_cell(&lane);
                    let mut queued = lock(&inner.queued);
                    *queued -= 1;
                    inner.idle.notify_all();
                }
            });
            sender
        });
        if sender.send(lane.to_string()).is_err() {
            *lock(&inner.queued) -= 1;
        }
    }

    /// Wait for the after-cell checks already started.
    pub fn settled(&self) {
        let mut queued = lock(&self.inner.queued);
        while *queued > 0 {
            queued = self.inner.idle.wait(queued).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Findings from after-cell checks not yet shown to `lane`, as one block of text (and forget them).
    pub fn take_pending(&self, lane: &str) -> Option<String> {
        let pending = lock(&self.inner.pending).remove(lane)?;
        if pending.is_empty() {
            return None;
        }
        let text = pending.join("\n");
        let (head, total) = js_cut(&text, MAX_PENDING_CHARS);
        Some(if total > MAX_PENDING_CHARS {
            format!("{head}\n… ({} more characters of findings)", total - MAX_PENDING_CHARS)
        } else {
            text
        })
    }

    /// A path for notes: relative to the session directory when inside it.
    fn display(&self, path: &Path) -> String {
        self.inner.display(path)
    }

    fn guards(&self) -> Vec<Registered> {
        lock(&self.inner.guards).clone()
    }

    /// `beforeWrite` of one proposed write on `lane`: every enabled guard, in parallel, each within its time limit.
    pub fn before_write_on(&self, lane: &str, path: &str, content: &str) -> Value {
        let path = resolve_lexical(&self.inner.cwd, path);
        let display = self.display(&path);
        let calls: Vec<(Registered, u64, Option<Duration>, Pending)> = self
            .guards()
            .into_iter()
            .filter(|r| r.guard.checks_writes() && r.guard.enabled())
            .map(|registered| {
                let started = self.inner.now();
                let timeout = registered.guard.timeout();
                let (guard, path, content, cwd, lane) = (
                    registered.guard.clone(),
                    path.clone(),
                    content.to_string(),
                    self.inner.cwd.clone(),
                    lane.to_string(),
                );
                // Started together; each answer is awaited below with its own limit.
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(timeout, move || {
                        guard.before_write_on(&path, &content, &cwd, &lane)
                    }));
                });
                (registered, started, timeout, receiver)
            })
            .collect();
        let mut reasons = Vec::new();
        let mut notes = Vec::new();
        for (registered, started, timeout, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.inner.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let mut stats = lock(&registered.stats);
            stats.total_ms += ms;
            stats.turn_ms += ms;
            stats.checks += 1;
            let (outcome, reason, note) = match answer {
                Answer::TimedOut => {
                    stats.unchecked += 1;
                    let limit = js_seconds(timeout.unwrap_or_default());
                    let note = format!(
                        "[{name}] {name} did not check this write to {display} (no answer within {limit} s); it was written unchecked."
                    );
                    ("unchecked", None, Some(note))
                }
                Answer::Error(error) => {
                    stats.unchecked += 1;
                    let note = format!(
                        "[{name}] {name} did not check this write to {display} ({error}); it was written unchecked."
                    );
                    ("unchecked", None, Some(note))
                }
                Answer::Value(Verdict::Block(reason)) => {
                    stats.blocked += 1;
                    ("blocked", Some(format!("[{name}] {reason}")), None)
                }
                Answer::Value(Verdict::Allow(Some(message))) if !message.is_empty() => {
                    ("allowed", None, Some(format!("[{name}] {message}")))
                }
                Answer::Value(Verdict::Allow(_)) => ("allowed", None, None),
            };
            drop(stats);
            self.inner.record(GuardRecord {
                guard: name,
                phase: "before_write",
                outcome,
                ms,
                paths: vec![display.clone()],
                lane: lane.to_string(),
                detail: reason.clone(),
            });
            reasons.extend(reason);
            notes.extend(note);
        }
        if reasons.is_empty() {
            // The write will happen with this content; an after-cell diff need not report it again.
            lock(&self.inner.checked).insert(path, sha256(content.as_bytes()));
            json!({ "blocked": false, "notes": notes })
        } else {
            json!({ "blocked": true, "reason": reasons.join("\n"), "notes": notes })
        }
    }

    /// `beforeShell` of a command on `lane`: commands that may write no file, or with no shell guard, are allowed
    /// at once. `Some(reasons)` blocks it. Notes (a guard that could not check, advise-only refusals) are not shown
    /// for shell commands.
    pub fn before_shell_on(&self, lane: &str, command: &str) -> Option<String> {
        let guards: Vec<Registered> = self
            .guards()
            .into_iter()
            .filter(|r| r.guard.checks_shell() && r.guard.enabled())
            .collect();
        if guards.is_empty() || !may_write_files(command) {
            return None;
        }
        let display = if command.encode_utf16().count() > 80 {
            format!("{}...", js_cut(command, 77).0)
        } else {
            command.to_string()
        };
        let calls: Vec<(Registered, u64, Pending)> = guards
            .into_iter()
            .map(|registered| {
                let started = self.inner.now();
                let timeout = registered.guard.timeout();
                let (guard, command, cwd) = (registered.guard.clone(), command.to_string(), self.inner.cwd.clone());
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(timeout, move || guard.before_shell(&command, &cwd)));
                });
                (registered, started, receiver)
            })
            .collect();
        let mut reasons = Vec::new();
        for (registered, started, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.inner.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let mut stats = lock(&registered.stats);
            stats.total_ms += ms;
            stats.turn_ms += ms;
            stats.checks += 1;
            let (outcome, reason) = match answer {
                Answer::TimedOut | Answer::Error(_) => {
                    stats.unchecked += 1;
                    ("unchecked", None)
                }
                Answer::Value(Verdict::Block(reason)) => {
                    stats.blocked += 1;
                    ("blocked", Some(format!("[{name}] {reason}")))
                }
                Answer::Value(Verdict::Allow(_)) => ("allowed", None),
            };
            drop(stats);
            self.inner.record(GuardRecord {
                guard: name,
                phase: "before_shell",
                outcome,
                ms,
                paths: vec![display.clone()],
                lane: lane.to_string(),
                detail: reason.clone(),
            });
            reasons.extend(reason);
        }
        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }
}

impl Inner {
    fn watching(guard: &dyn FileWriteGuard) -> bool {
        guard.enabled() && guard.watches_cells()
    }

    fn watches_cells(&self) -> bool {
        self.snapshot.is_some() && lock(&self.guards).iter().any(|r| Inner::watching(&*r.guard))
    }

    fn now(&self) -> u64 {
        let clock = lock(&self.now).clone();
        clock()
    }

    fn record(&self, record: GuardRecord) {
        let on_record = lock(&self.on_record).clone();
        if let Some(on_record) = on_record {
            on_record(&record);
        }
    }

    /// A path for records: relative to the session directory when inside it.
    fn display(&self, path: &Path) -> String {
        inside(&self.cwd, path).unwrap_or_else(|| path.to_string_lossy().into_owned())
    }

    fn picture(&self) -> Option<Snapshot> {
        self.snapshot.as_ref().and_then(|snapshot| snapshot(&self.cwd).ok())
    }

    /// `#afterCell`: diff against the previous picture, then ask every watching guard (each within its time limit).
    fn after_cell(&self, lane: &str) {
        let before = lock(&self.baseline).clone();
        let after = self.picture();
        if let Some(after) = &after {
            *lock(&self.baseline) = Some(after.clone());
        }
        let (Some(before), Some(after)) = (before, after) else {
            return;
        };
        if before.root != after.root {
            return;
        }
        let (added, modified, deleted, complete) = diff_snapshots(&before, &after);
        if added.is_empty() && modified.is_empty() && deleted.is_empty() {
            return;
        }
        let mut changes = CellChanges {
            complete,
            ..CellChanges::default()
        };
        for relative in added.iter().chain(&modified) {
            let path = resolve_lexical(&after.root, relative);
            let approved = lock(&self.checked).remove(&path);
            if let Some(approved) = approved
                && std::fs::read(&path).is_ok_and(|current| sha256(&current) == approved)
            {
                changes.checked.push(path);
                continue;
            }
            changes.files.push(path);
        }
        for relative in &deleted {
            let path = resolve_lexical(&after.root, relative);
            lock(&self.checked).remove(&path);
            changes.deleted.push(path);
        }
        let guards: Vec<Registered> = lock(&self.guards)
            .iter()
            .filter(|r| Inner::watching(&*r.guard))
            .cloned()
            .collect();
        let paths: Vec<String> = changes
            .files
            .iter()
            .chain(&changes.checked)
            .map(|path| self.display(path))
            .collect();
        let changes = Arc::new(changes);
        // Every watching guard starts at once; each is awaited with its own limit.
        let calls: Vec<_> = guards
            .into_iter()
            .map(|registered| {
                let started = self.now();
                let (task, cwd, lane_name, checked) = (
                    registered.guard.clone(),
                    self.cwd.clone(),
                    lane.to_string(),
                    changes.clone(),
                );
                let limit = registered.guard.after_timeout();
                let (sender, receiver) = channel();
                std::thread::spawn(move || {
                    let _ = sender.send(timed(Some(limit), move || {
                        task.after_cell_changes(&checked, &cwd, &lane_name)
                    }));
                });
                (registered, started, receiver)
            })
            .collect();
        let mut found = Vec::new();
        for (registered, started, receiver) in calls {
            let answer = receiver.recv().unwrap_or(Answer::TimedOut);
            let ms = self.now().saturating_sub(started);
            let name = registered.guard.name().to_string();
            let (text, outcome) = match answer {
                Answer::TimedOut => (
                    Some(format!("{name} did not finish checking the files this cell changed.")),
                    "unchecked",
                ),
                Answer::Error(error) => (
                    Some(format!("{name} could not check the files this cell changed: {error}")),
                    "unchecked",
                ),
                Answer::Value(None) => (None, "clean"),
                Answer::Value(Some(report)) => (
                    report.message.filter(|m| !m.is_empty()),
                    match report.outcome {
                        AfterOutcome::Clean => "clean",
                        AfterOutcome::Advisory => "advisory",
                        AfterOutcome::Findings => "findings",
                    },
                ),
            };
            {
                let mut stats = lock(&registered.stats);
                stats.total_ms += ms;
                stats.turn_ms += ms;
                stats.after_checks += 1;
                if matches!(outcome, "findings" | "unchecked") {
                    stats.after_findings += 1;
                }
            }
            self.record(GuardRecord {
                guard: name.clone(),
                phase: "after_cell",
                outcome,
                ms,
                paths: paths.clone(),
                lane: lane.to_string(),
                detail: text.clone(),
            });
            if let Some(text) = text {
                found.push(format!("[{name}] {text}"));
            }
        }
        if !found.is_empty() {
            lock(&self.pending).entry(lane.to_string()).or_default().extend(found);
        }
    }
}

impl Guard for FileHooks {
    fn before_shell(&self, command: &str) -> Option<String> {
        self.before_shell_on("main", command)
    }

    fn before_shell_on(&self, lane: &str, command: &str) -> Option<String> {
        FileHooks::before_shell_on(self, lane, command)
    }

    fn before_write(&self, path: &str, content: &str) -> Value {
        self.before_write_on("main", path, content)
    }

    fn before_write_on(&self, lane: &str, path: &str, content: &str) -> Value {
        FileHooks::before_write_on(self, lane, path, content)
    }

    fn guard_stats(&self) -> Vec<Value> {
        self.stats().iter().map(GuardStats::to_json).collect()
    }

    fn begin_turn(&self) {
        FileHooks::begin_turn(self);
    }

    fn cell_started(&self, lane: &str) {
        FileHooks::cell_started(self, lane);
    }

    fn cell_ended(&self, lane: &str) {
        FileHooks::cell_ended(self, lane);
    }

    fn take_pending(&self, lane: &str) -> Option<String> {
        FileHooks::take_pending(self, lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_inside_matches_path_relative() {
        let root = Path::new("/r");
        assert_eq!(inside(root, Path::new("/r/a/b.txt")).as_deref(), Some("a/b.txt"));
        assert_eq!(inside(root, Path::new("/r")), None);
        assert_eq!(inside(root, Path::new("/rx/a")), None);
        assert_eq!(inside(root, Path::new("/r/..x")), None);
        assert_eq!(resolve_lexical(root, "a/../b/./c"), Path::new("/r/b/c"));
        assert_eq!(resolve_lexical(root, "/abs/../x"), Path::new("/x"));
    }
}
