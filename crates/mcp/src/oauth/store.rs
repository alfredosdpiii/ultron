//! Per-server OAuth state in `<agentDir>/mcp-auth.json` (`McpOAuthCredentialStore` of extensions/mcp/oauth.ts):
//! keyed by server name and URL (`mcp__<name>|<url>`) so servers sharing a URL keep separate accounts; state an older
//! version stored by URL alone is taken over by the first server that loads it. Every access holds the file's
//! `proper-lockfile` lock; a refresh holds a per-server lock file in the agent directory, so processes sharing the
//! credentials never refresh a rotating refresh token twice. Tokens and client secrets are handed to the output
//! masker whenever they are read or written.

use super::OAuthError;
use super::flow::{StateStore, normalized_url};
use crate::connection::SecretSink;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A refresh lock its holder stopped renewing (the process was killed) is taken over after this.
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
/// How long to wait for another process's refresh: longer than a stale lock lives.
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
/// How often the holder renews the lock: half the staleness, as `proper-lockfile`'s default `update`.
const REFRESH_LOCK_RENEW: Duration = Duration::from_secs(10);

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The credential file of a profile.
pub struct CredentialStore {
    path: PathBuf,
    /// Directory for the refresh lock files; without one, refreshes are only serialized in this process.
    lock_dir: Option<PathBuf>,
    secrets: Option<SecretSink>,
    /// Serializes this process's refreshes (the lock file keeps other processes out).
    /// Serializes this process's refreshes of one server (the lock file keeps other processes out).
    refreshing: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    /// The refresh lock's staleness and how often its holder renews it (`proper-lockfile` `stale` and `update`).
    refresh_stale: Duration,
    refresh_renew: Duration,
}

fn other(error: impl std::fmt::Display) -> OAuthError {
    OAuthError::Other(error.to_string())
}

/// The stored states; an empty file is none, a non-object is none.
fn parse_states(content: Option<&str>) -> Result<Map<String, Value>, OAuthError> {
    let Some(content) = content.filter(|c| !c.trim().is_empty()) else {
        return Ok(Map::new());
    };
    match crate::js_json::parse(content).map_err(OAuthError::Other)? {
        Value::Object(map) => Ok(map),
        _ => Ok(Map::new()),
    }
}

fn serialize_states(states: &Map<String, Value>) -> String {
    format!(
        "{}\n",
        crate::edit::stringify_indented(&Value::Object(states.clone()), "  ")
    )
}

/// `mcp__<namespace>|<url>`, and the legacy key by URL alone.
fn store_keys(name: &str, server_url: &str) -> (String, String) {
    let legacy = normalized_url(server_url);
    (format!("{}|{legacy}", crate::config::mcp_namespace(name)), legacy)
}

impl CredentialStore {
    /// `<agent_dir>/mcp-auth.json`, with refresh locks in `agent_dir`.
    pub fn new(agent_dir: &Path, secrets: Option<SecretSink>) -> Self {
        Self {
            path: agent_dir.join("mcp-auth.json"),
            lock_dir: Some(agent_dir.to_path_buf()),
            secrets,
            refreshing: Mutex::default(),
            refresh_stale: REFRESH_LOCK_STALE,
            refresh_renew: REFRESH_LOCK_RENEW,
        }
    }

    /// The refresh lock's staleness and renewal period (tests shorten them).
    pub fn with_refresh_timing(mut self, stale: Duration, renew: Duration) -> Self {
        self.refresh_stale = stale;
        self.refresh_renew = renew;
        self
    }

    /// The lock directory of `key`'s refreshes.
    fn refresh_lock_path(&self, key: &str) -> Option<PathBuf> {
        let dir = self.lock_dir.as_ref()?;
        let hash: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()[..16]
            .to_string();
        Some(config::lock::lock_path_for(
            &dir.join(format!("mcp-auth-refresh-{hash}")),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Masked from cell output from now on.
    fn remember(&self, state: Option<&Value>) {
        let (Some(sink), Some(state)) = (&self.secrets, state) else {
            return;
        };
        let values: Vec<String> = [
            &state["tokens"]["access_token"],
            &state["tokens"]["refresh_token"],
            &state["clientInformation"]["client_secret"],
        ]
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
        sink(&values);
    }

    /// `withLock`: the file (created as `{}` when missing, readable by the user only) under its lock.
    fn with_lock<T>(
        &self,
        change: impl FnOnce(Option<&str>) -> Result<(T, Option<String>), OAuthError>,
    ) -> Result<T, OAuthError> {
        if let Some(dir) = self.path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(other)?;
        }
        if !self.path.exists() {
            write_private(&self.path, "{}")?;
        }
        let _lock = config::lock::acquire(
            &config::lock::lock_path_for(&self.path),
            config::lock::SYNC_STALE,
            config::lock::SYNC_ATTEMPTS,
        )
        .map_err(other)?;
        let current = std::fs::read_to_string(&self.path).ok();
        let (result, next) = change(current.as_deref())?;
        if let Some(next) = next {
            write_private(&self.path, &next)?;
        }
        Ok(result)
    }

    pub fn for_server(self: &Arc<Self>, name: &str, server_url: &str) -> ServerStore {
        let (key, legacy) = store_keys(name, server_url);
        ServerStore {
            store: self.clone(),
            key,
            legacy,
        }
    }

    /// The stored tokens of a server (for noticing sign-ins done elsewhere); does not take over legacy state.
    pub fn tokens(&self, name: &str, server_url: &str) -> Result<Option<Value>, OAuthError> {
        let (key, legacy) = store_keys(name, server_url);
        let states = self.with_lock(|current| Ok((parse_states(current)?, None)))?;
        Ok(states
            .get(&key)
            .or_else(|| states.get(&legacy))
            .and_then(|s| s.get("tokens"))
            .cloned())
    }

    /// Whether credentials were stored for the server; also removes legacy state it would take over.
    pub fn remove(&self, name: &str, server_url: &str) -> Result<bool, OAuthError> {
        let (key, legacy) = store_keys(name, server_url);
        self.with_lock(|current| {
            let mut states = parse_states(current)?;
            let stored = if states.contains_key(&key) {
                key.clone()
            } else if states.contains_key(&legacy) {
                legacy.clone()
            } else {
                return Ok((false, None));
            };
            states.shift_remove(&stored);
            Ok((true, Some(serialize_states(&states))))
        })
    }

    /// Runs `run` while no other refresh of the server's tokens runs, in this process or another. The lock is renewed
    /// while held (as `proper-lockfile` does), so a refresh slower than the staleness period is not taken over.
    fn with_refresh_lock<T>(&self, key: &str, run: impl FnOnce() -> Result<T, OAuthError>) -> Result<T, OAuthError> {
        let local = self
            .refreshing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.to_string())
            .or_default()
            .clone();
        let _local = local.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(dir), Some(lock_path)) = (&self.lock_dir, self.refresh_lock_path(key)) else {
            return run();
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(other)?;
        let attempts = u32::try_from(REFRESH_LOCK_WAIT.as_millis() / 20).unwrap_or(u32::MAX);
        let _lock = config::lock::acquire(&lock_path, self.refresh_stale, attempts).map_err(other)?;
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let renew = self.refresh_renew;
        let renewer = std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(renew) {
                // A lost lock at worst lets two refreshes overlap (`onCompromised` is ignored there too).
                if let Ok(lock) = std::fs::File::open(&lock_path) {
                    let _ = lock.set_modified(SystemTime::now());
                }
            }
        });
        let result = run();
        drop(stop);
        let _ = renewer.join();
        result
    }
}

fn write_private(path: &Path, content: &str) -> Result<(), OAuthError> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(other)?;
    file.write_all(content.as_bytes()).map_err(other)
}

/// One server's state in the credential file (`forServer`).
#[derive(Clone)]
pub struct ServerStore {
    store: Arc<CredentialStore>,
    key: String,
    legacy: String,
}

impl ServerStore {
    pub fn with_refresh_lock<T>(&self, run: impl FnOnce() -> Result<T, OAuthError>) -> Result<T, OAuthError> {
        self.store.with_refresh_lock(&self.key, run)
    }

    /// The lock directory this server's refreshes hold (`None` without a lock directory).
    pub fn refresh_lock_path(&self) -> Option<PathBuf> {
        self.store.refresh_lock_path(&self.key)
    }
}

impl StateStore for ServerStore {
    /// The first server to load legacy state takes it over; others with the same URL sign in again.
    fn load(&self) -> Result<Option<Value>, OAuthError> {
        let (key, legacy) = (&self.key, &self.legacy);
        let state = self.store.with_lock(|current| {
            let mut states = parse_states(current)?;
            if states.contains_key(key) || !states.contains_key(legacy) {
                return Ok((states.get(key).cloned(), None));
            }
            let moved = states.shift_remove(legacy);
            if let Some(moved) = moved.clone() {
                states.insert(key.clone(), moved);
            }
            Ok((moved, Some(serialize_states(&states))))
        })?;
        self.store.remember(state.as_ref());
        Ok(state)
    }

    fn save(&self, state: &Value) -> Result<(), OAuthError> {
        self.store.remember(Some(state));
        self.store.with_lock(|current| {
            let mut states = parse_states(current)?;
            states.insert(self.key.clone(), state.clone());
            Ok(((), Some(serialize_states(&states))))
        })
    }
}
