//! What autoreview remembers (`autoreview/state.ts`), in `<agentDir>/autoreview/state.json`: per account, the
//! notification cursor; per account and pull request, the last reviewed and acknowledged commits, attempts per
//! commit and the findings it posted (for re-reviews); and the recent reviews with their timings. No token is ever
//! stored.
//!
//! Writes are atomic (a temporary file renamed over the state) and serialized: within a process by a lock per state
//! file (threads wait on it), across processes by the lock directory `proper-lockfile` uses (`<state>.lock`, stale
//! after 10 s), so a one-off `ultron autoreview review`, the daemon and the TypeScript implementation share the
//! state. The daemon holds `daemon.lock` (refreshed every 5 s, stale after 15 s), so one runs per agent directory.
//!
//! Known differences, outside recorded behaviour: fields of the state are typed, so a pull request entry, a recent
//! record or a fate whose fields have the wrong JSON type is dropped on read (the TypeScript kept it as it was);
//! unknown fields are kept. Object keys are written in the state's own order (JavaScript writes integer-like keys
//! first).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::checkout::make_private_dir;
use crate::github::Error;
use crate::js::Num;
use crate::ledger::Fate;

pub const RECENT_LIMIT: usize = 50;
pub const MAX_ATTEMPT_SHAS: usize = 8;
pub const STATE_STALE_MS: u64 = 10_000;
pub const DAEMON_STALE_MS: u64 = 15_000;
pub const DAEMON_UPDATE_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostedFinding {
    /// Stable within the pull request: `<short sha>-<n>`.
    pub id: String,
    pub file: String,
    pub line: i64,
    /// The finding's level (older state files hold the old four-name severity).
    pub severity: String,
    pub claim: String,
    pub claim_hash: String,
    /// The finding's category and the kind of pass that raised it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The commit the finding was posted on.
    pub sha: String,
    /// The inline comment, when the finding was posted inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_id: Option<i64>,
    /// The review thread of that comment (the only thread this finding may resolve).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// `open`, `fixed` or `not_applicable`.
    pub status: String,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

impl PostedFinding {
    /// The ledger's view of the finding.
    pub fn ledger(&self) -> crate::ledger::PostedFinding {
        crate::ledger::PostedFinding {
            id: self.id.clone(),
            file: self.file.clone(),
            line: self.line,
            claim: self.claim.clone(),
            severity: self.severity.clone(),
            category: self.category.clone(),
            kind: self.kind.clone(),
            comment_id: self.comment_id,
            status: self.status.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_review_id: Option<i64>,
    /// The last "review" was a dry run: nothing was posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_review_dry_run: Option<bool>,
    /// `approve`, `request_changes` or `comment`: after `request_changes`, new commits are reviewed unasked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<String>,
    /// The pull request was seen closed or merged after the last review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ack_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ack_at: Option<String>,
    /// The acknowledgement line used last on this pull request (not repeated next time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ack_line: Option<String>,
    /// Failed attempts by head commit.
    #[serde(default)]
    pub attempts: IndexMap<String, u32>,
    /// The commit a "could not review" comment was posted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gave_up_sha: Option<String>,
    #[serde(default)]
    pub findings: Vec<PostedFinding>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<Num>,
    /// Nothing is sent for this account before this time (a rate limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRecord {
    pub account: String,
    pub pull: String,
    pub sha: String,
    pub at: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<Num>,
    /// Where the time went, by stage (the pipeline's stages, then `postMs`), in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages: Option<IndexMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pickup_to_post_ms: Option<Num>,
    /// From the notification's update time to the acknowledgement comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_to_ack_ms: Option<Num>,
    /// From the acknowledgement comment to the posted review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_to_post_ms: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<Num>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub pid: Num,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_start_ms: Option<Num>,
    #[serde(flatten)]
    pub extra: IndexMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutoreviewState {
    pub version: u32,
    pub accounts: IndexMap<String, AccountState>,
    pub pulls: IndexMap<String, PullState>,
    pub recent: Vec<ReviewRecord>,
    /// The daemon's queue as last written, for `ultron autoreview status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<Vec<String>>,
    /// Per `host/owner/repo`: the fates of findings posted there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lessons: Option<IndexMap<String, Vec<Fate>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonInfo>,
}

impl Default for AutoreviewState {
    fn default() -> Self {
        empty_state()
    }
}

pub fn empty_state() -> AutoreviewState {
    AutoreviewState {
        version: 1,
        accounts: IndexMap::new(),
        pulls: IndexMap::new(),
        recent: Vec::new(),
        queue: None,
        lessons: None,
        daemon: None,
    }
}

/// The first 16 hex digits of the SHA-256 of the claim, lowercased with its whitespace collapsed.
pub fn claim_hash(claim: &str) -> String {
    let normalized = crate::js::flatten(&claim.to_lowercase());
    let digest = ring::digest::digest(&ring::digest::SHA256, normalized.as_bytes());
    digest
        .as_ref()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn typed<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
    serde_json::from_value(value.clone()).ok()
}

fn normalize(value: &Value) -> AutoreviewState {
    let mut state = empty_state();
    let Value::Object(raw) = value else { return state };
    if let Some(Value::Object(accounts)) = raw.get("accounts") {
        state.accounts = accounts
            .iter()
            .filter_map(|(key, account)| typed(account).map(|account| (key.clone(), account)))
            .collect();
    }
    if let Some(Value::Object(pulls)) = raw.get("pulls") {
        for (key, pull) in pulls {
            let Value::Object(fields) = pull else { continue };
            let mut fields = fields.clone();
            // attempts and findings of the wrong type read as empty.
            if !matches!(fields.get("attempts"), Some(Value::Object(_))) {
                fields.insert("attempts".into(), Value::Object(Default::default()));
            }
            if !matches!(fields.get("findings"), Some(Value::Array(_))) {
                fields.insert("findings".into(), Value::Array(Vec::new()));
            }
            if let Some(pull) = typed(&Value::Object(fields)) {
                state.pulls.insert(key.clone(), pull);
            }
        }
    }
    if let Some(Value::Array(recent)) = raw.get("recent") {
        state.recent = recent.iter().filter_map(typed).collect();
    }
    if let Some(Value::Array(queue)) = raw.get("queue") {
        state.queue = Some(
            queue
                .iter()
                .map(|item| item.as_str().map_or_else(|| item.to_string(), String::from))
                .collect(),
        );
    }
    if let Some(daemon @ Value::Object(_)) = raw.get("daemon") {
        state.daemon = typed(daemon);
    }
    if let Some(Value::Object(lessons)) = raw.get("lessons") {
        let mut out = IndexMap::new();
        for (repo, fates) in lessons {
            if let Value::Array(fates) = fates {
                out.insert(repo.clone(), fates.iter().filter_map(typed).collect());
            }
        }
        state.lessons = Some(out);
    }
    state
}

/// `JSON.stringify(value, null, 1)`.
pub fn to_json_1<T: Serialize>(value: &T) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    let _ = value.serialize(&mut serializer);
    String::from_utf8(out).unwrap_or_default()
}

// ---- Locks ----

fn process_lock(path: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(path.into())
        .or_default()
        .clone()
}

fn is_stale(lock_dir: &str, stale_ms: u64) -> bool {
    std::fs::metadata(lock_dir)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > Duration::from_millis(stale_ms))
}

/// One attempt at `proper-lockfile`'s lock: the directory `<file>.lock`, taken over when stale.
fn try_lock_dir(lock_dir: &str, stale_ms: u64) -> std::io::Result<bool> {
    match std::fs::create_dir(lock_dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if is_stale(lock_dir, stale_ms) {
                let _ = std::fs::remove_dir(lock_dir);
                return Ok(std::fs::create_dir(lock_dir).is_ok());
            }
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

pub struct StateStore {
    pub path: String,
}

impl StateStore {
    pub fn new(path: &str) -> Self {
        Self { path: path.into() }
    }

    pub fn read(&self) -> AutoreviewState {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map_or_else(empty_state, |value| normalize(&value))
    }

    /// Read, change and write the state under the state lock.
    pub fn update<T>(&self, change: impl FnOnce(&mut AutoreviewState) -> T) -> Result<T, Error> {
        let io = |error: std::io::Error| Error::Other(format!("{}: {error}", self.path));
        let parent = Path::new(&self.path).parent().map(|p| p.to_string_lossy().into_owned());
        if let Some(parent) = parent.filter(|p| !p.is_empty()) {
            make_private_dir(&parent).map_err(io)?;
        }
        let local = process_lock(&self.path);
        let _local = local.lock().unwrap_or_else(|e| e.into_inner());
        // Another process (another `ultron autoreview`) may hold it: retried as proper-lockfile does (50 times,
        // 20 ms apart), since a lock directory has no event to wait on.
        let lock_dir = format!("{}.lock", self.path);
        let mut tries = 0;
        while !try_lock_dir(&lock_dir, STATE_STALE_MS).map_err(io)? {
            tries += 1;
            if tries > 50 {
                return Err(Error::Other(format!("Lock file is already being held: {}", self.path)));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = self.write_locked(change);
        let _ = std::fs::remove_dir(&lock_dir);
        result.map_err(io)
    }

    fn write_locked<T>(&self, change: impl FnOnce(&mut AutoreviewState) -> T) -> std::io::Result<T> {
        let mut state = self.read();
        let result = change(&mut state);
        if state.recent.len() > RECENT_LIMIT {
            state.recent.drain(..state.recent.len() - RECENT_LIMIT);
        }
        for pull in state.pulls.values_mut() {
            let excess = pull.attempts.len().saturating_sub(MAX_ATTEMPT_SHAS);
            pull.attempts.drain(..excess);
        }
        let temporary = format!("{}.{}.tmp", self.path, std::process::id());
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(format!("{}\n", to_json_1(&state)).as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, &self.path)?;
        Ok(result)
    }

    /// Change one pull request's entry (created when missing).
    pub fn update_pull<T>(
        &self,
        key: &str,
        change: impl FnOnce(&mut PullState, &mut AutoreviewState) -> T,
    ) -> Result<T, Error> {
        self.update(|state| {
            // A new entry goes last; an existing one keeps its place (as a JavaScript object key does).
            let mut pull = std::mem::take(state.pulls.entry(key.into()).or_default());
            let result = change(&mut pull, state);
            state.pulls.insert(key.into(), pull);
            result
        })
    }
}

/// The daemon lock of an autoreview directory: held until dropped (or [`DaemonLock::release`]).
pub struct DaemonLock {
    lock_dir: String,
    stop: Option<mpsc::Sender<()>>,
    refresher: Option<std::thread::JoinHandle<()>>,
}

impl DaemonLock {
    pub fn release(mut self) {
        self.stop_and_remove();
    }

    fn stop_and_remove(&mut self) {
        drop(self.stop.take());
        if let Some(refresher) = self.refresher.take() {
            let _ = refresher.join();
        }
        let _ = std::fs::remove_dir(&self.lock_dir);
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        if self.refresher.is_some() {
            self.stop_and_remove();
        }
    }
}

/// Take the daemon lock of an autoreview directory; fails with [`Error::DaemonRunning`] while another holds it.
pub fn acquire_daemon_lock(dir: &str) -> Result<DaemonLock, Error> {
    let io = |error: std::io::Error| Error::Other(format!("{dir}: {error}"));
    make_private_dir(dir).map_err(io)?;
    let target = Path::new(dir).join("daemon");
    if !target.exists() {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&target)
            .map_err(io)?;
    }
    let lock_dir = format!("{}.lock", target.to_string_lossy());
    if !try_lock_dir(&lock_dir, DAEMON_STALE_MS).map_err(io)? {
        return Err(Error::DaemonRunning(format!(
            "another ultron autoreview is already running for {dir}"
        )));
    }
    // Keep the lock fresh (its mtime) until released; the stop channel wakes the thread at once.
    let (stop, stopped) = mpsc::channel::<()>();
    let path = lock_dir.clone();
    let refresher = std::thread::spawn(move || {
        while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(Duration::from_millis(DAEMON_UPDATE_MS)) {
            if let Ok(dir) = std::fs::File::open(&path) {
                let _ = dir.set_modified(SystemTime::now());
            }
        }
    });
    Ok(DaemonLock {
        lock_dir,
        stop: Some(stop),
        refresher: Some(refresher),
    })
}
