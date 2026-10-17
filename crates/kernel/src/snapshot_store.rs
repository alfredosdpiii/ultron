//! Where the snapshot signing key lives (`snapshot-auth.ts` `loadSnapshotKey`, `secretToolKeyring`):
//! `ULTRON_RLM_SNAPSHOT_KEY_STORE` = `file` (default) keeps `<agentDir>/rlm-snapshot.key`; `auto` moves the key into
//! the OS keyring when one answers (through libsecret's `secret-tool`), leaving a marker file; `keyring` insists on
//! the keyring and otherwise uses a key for this process only. Exposure reduction, not secrecy: model code runs as
//! the same user. `secret-tool` is found on the session's `PATH` only, and runs with the session's desktop variables.

use crate::snapshot::{KEY_BYTES, SNAPSHOT_KEY_FILE, hex, load_or_create_snapshot_key, parse_key};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Marker left in place of the key file once the key lives in the OS keyring; it holds no secret.
pub const SNAPSHOT_KEYRING_MARKER: &str = "rlm-snapshot.key.keyring";

/// `SnapshotKeyStore`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyStore {
    Auto,
    Keyring,
    File,
}

impl KeyStore {
    /// `ULTRON_RLM_SNAPSHOT_KEY_STORE` (trimmed, any case): `auto` or `keyring`, anything else the file.
    pub fn from_setting(value: Option<&str>) -> KeyStore {
        match value.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("auto") => KeyStore::Auto,
            Some("keyring") => KeyStore::Keyring,
            _ => KeyStore::File,
        }
    }
}

/// Where a loaded key lives: the OS keyring, the profile file, or only this process (its snapshots do not outlive
/// it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Keyring,
    File,
    Process,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSnapshotKey {
    pub key: Vec<u8>,
    pub source: KeySource,
    pub warning: Option<String>,
}

/// What a keyring lookup found: `None` when the store cannot be reached (no Secret Service, locked keyring, a
/// timeout), `Some(None)` when it holds no key.
pub type Lookup = Option<Option<String>>;

/// A secret store outside the profile directory (`SnapshotKeyring`).
pub trait SnapshotKeyring {
    fn lookup(&self) -> Lookup;
    fn store(&self, hex: &str) -> bool;
}

/// The login keyring through `secret-tool` (GNOME Keyring, KWallet's Secret Service, KeePassXC), keyed by the
/// resolved profile directory. A call that does not answer within the timeout (an unlock prompt nobody answers)
/// counts as unavailable, and is killed.
pub struct SecretToolKeyring {
    /// The executable, or `None` when the session's `PATH` has none (every call is then unavailable).
    pub command: Option<PathBuf>,
    pub attributes: Vec<String>,
    pub timeout: Duration,
    /// The child's whole environment.
    pub env: Vec<(String, String)>,
}

/// The variables `secret-tool` needs to find the session's Secret Service, taken from the session environment.
const SECRET_TOOL_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];

/// `secretToolKeyring(agentDir)` over the session environment `var`: `secret-tool` from its `PATH`, a 5 s timeout.
pub fn secret_tool_keyring(agent_dir: &Path, var: &dyn Fn(&str) -> Option<String>) -> SecretToolKeyring {
    let profile = std::path::absolute(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    SecretToolKeyring {
        command: var("PATH").and_then(|path| find_on_path("secret-tool", &path)),
        attributes: ["application", "ultron", "purpose", "rlm-snapshot-key", "profile"]
            .iter()
            .map(|s| s.to_string())
            .chain([profile.display().to_string()])
            .collect(),
        timeout: Duration::from_secs(5),
        env: SECRET_TOOL_ENV
            .iter()
            .filter_map(|name| Some((name.to_string(), var(name)?)))
            .collect(),
    }
}

fn find_on_path(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// A finished `secret-tool` run: its exit code (`None`: killed by a signal or the timeout), stdout and stderr.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl SecretToolKeyring {
    fn run(&self, args: &[String], input: Option<&str>) -> Option<Run> {
        let command = self.command.as_ref()?;
        let mut child = Command::new(command)
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            let _ = stdin.write_all(input.as_bytes());
        }
        let mut stdout = child.stdout.take()?;
        let mut stderr = child.stderr.take()?;
        let out = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        });
        let err = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                // Our own child only: it did not answer in time.
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let (stdout, stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
        Some(Run {
            code: status.and_then(|s| s.code()),
            stdout,
            stderr,
        })
    }
}

impl SnapshotKeyring for SecretToolKeyring {
    fn lookup(&self) -> Lookup {
        let args: Vec<String> = std::iter::once("lookup".to_string())
            .chain(self.attributes.iter().cloned())
            .collect();
        let run = self.run(&args, None)?;
        match run.code {
            Some(0) => Some(Some(run.stdout)),
            // secret-tool exits 1 silently when nothing matches, and 1 with a message when the service fails.
            Some(1) if run.stderr.trim().is_empty() => Some(None),
            _ => None,
        }
    }

    fn store(&self, hex: &str) -> bool {
        let args: Vec<String> = ["store", "--label=Ultron RLM snapshot signing key"]
            .iter()
            .map(|s| s.to_string())
            .chain(self.attributes.iter().cloned())
            .collect();
        self.run(&args, Some(hex)).is_some_and(|run| run.code == Some(0))
    }
}

fn random_key() -> Result<Vec<u8>, String> {
    let mut key = vec![0u8; KEY_BYTES];
    getrandom::fill(&mut key).map_err(|error| error.to_string())?;
    Ok(key)
}

fn mark_keyring(marker: &Path) -> Result<(), String> {
    if marker.exists() {
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(marker)
        .and_then(|mut file| {
            file.write_all(
                b"The RLM snapshot signing key is stored in the OS keyring (secret-tool, application=ultron purpose=rlm-snapshot-key). This file holds no secret.\n",
            )
        })
        .map_err(|error| error.to_string())
}

/// `loadSnapshotKey`: the per-profile key from the store `store` selects. In auto mode a reachable keyring takes over
/// the key (an existing key file is copied in, read back, and deleted, and a marker records it); once the marker
/// exists an unreachable keyring gives a key for this process only, with a warning, so no second persistent key is
/// ever made.
pub fn load_snapshot_key(
    agent_dir: &Path,
    store: KeyStore,
    keyring: &dyn SnapshotKeyring,
) -> Result<LoadedSnapshotKey, String> {
    let file = |key: Vec<u8>| LoadedSnapshotKey {
        key,
        source: KeySource::File,
        warning: None,
    };
    if store == KeyStore::File {
        return Ok(file(load_or_create_snapshot_key(agent_dir)?));
    }
    let path = agent_dir.join(SNAPSHOT_KEY_FILE);
    let shown = path.display().to_string();
    let marker = agent_dir.join(SNAPSHOT_KEYRING_MARKER);
    let unreachable = |reason: &str| -> Result<LoadedSnapshotKey, String> {
        if store == KeyStore::Auto && !marker.exists() {
            return Ok(file(load_or_create_snapshot_key(agent_dir)?));
        }
        Ok(LoadedSnapshotKey {
            key: random_key()?,
            source: KeySource::Process,
            warning: Some(format!(
                "RLM snapshot key: {reason}; using a key for this process only, so its snapshots will not restore in later sessions"
            )),
        })
    };
    let Some(found) = keyring.lookup() else {
        return unreachable("the OS keyring holding the key is unreachable");
    };
    if let Some(value) = found.filter(|v| !v.trim().is_empty()) {
        let key = parse_key(&value, "in the OS keyring")?;
        mark_keyring(&marker)?;
        if path.exists() {
            let file_key = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| parse_key(&text, &shown).ok());
            if file_key.as_deref() == Some(key.as_slice()) {
                let _ = std::fs::remove_file(&path);
            } else {
                return Ok(LoadedSnapshotKey {
                    key,
                    source: KeySource::Keyring,
                    warning: Some(format!(
                        "RLM snapshot key: {shown} differs from the key in the OS keyring; the keyring key is used and the file was left in place"
                    )),
                });
            }
        }
        return Ok(LoadedSnapshotKey {
            key,
            source: KeySource::Keyring,
            warning: None,
        });
    }
    // Nothing stored yet: move the file key into the keyring (so existing snapshots keep verifying), or make one.
    let key = if path.exists() {
        parse_key(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?, &shown)?
    } else {
        random_key()?
    };
    let encoded = hex(&key);
    let read_back = if keyring.store(&encoded) {
        keyring.lookup()
    } else {
        None
    };
    if read_back.flatten().map(|v| v.trim().to_string()).as_deref() != Some(encoded.as_str()) {
        if store == KeyStore::Keyring {
            return unreachable("the OS keyring did not store the key");
        }
        return Ok(file(load_or_create_snapshot_key(agent_dir)?));
    }
    mark_keyring(&marker)?;
    let _ = std::fs::remove_file(&path);
    Ok(LoadedSnapshotKey {
        key,
        source: KeySource::Keyring,
        warning: None,
    })
}

/// The key a session's kernels sign snapshots with (session-worker.ts: `loadSnapshotKey(getAgentDir())`, its
/// warning on stderr): the store from `ULTRON_RLM_SNAPSHOT_KEY_STORE`, the keyring through the session
/// environment's `secret-tool`. `None` when the key cannot be loaded (the kernel then uses a process key).
pub fn session_snapshot_key(agent_dir: &Path, var: &dyn Fn(&str) -> Option<String>) -> Option<Vec<u8>> {
    let store = KeyStore::from_setting(var("ULTRON_RLM_SNAPSHOT_KEY_STORE").as_deref());
    let keyring = secret_tool_keyring(agent_dir, var);
    let loaded = load_snapshot_key(agent_dir, store, &keyring).ok()?;
    if let Some(warning) = &loaded.warning {
        eprintln!("{warning}");
    }
    Some(loaded.key)
}
