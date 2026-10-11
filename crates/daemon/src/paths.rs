//! Server directory profile (experimental/server.ts): private directory checks, the logical server ID in
//! `default-server-id`, the launcher and activation locks, IDs and the fallback ID beside another release.

use crate::lock::Lock;
use sha2::{Digest, Sha256};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_SERVER_ID_FILE: &str = "default-server-id";
const LOCK_STALE: Duration = Duration::from_millis(30_000);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const LOCK_WAIT: Duration = Duration::from_millis(30_000);
const ACTIVATION_TIMEOUT: Duration = Duration::from_millis(10_000);

/// `ensurePrivateServerDirectory`.
pub fn ensure_private_directory(directory: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let meta = std::fs::symlink_metadata(directory)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "Unix socket directory is not a directory: {}",
            directory.display()
        )));
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(io::Error::other(format!(
            "Unix socket directory is not owned by the current user: {}",
            directory.display()
        )));
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

/// A random canonical UUIDv4 (`randomUUID`).
pub fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS random source is available");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The held launcher lock of one logical server.
pub struct Profile {
    pub server_id: String,
    _lock: Lock,
}

/// `acquireServerProfile`: the requested ID, else the directory's default (created once), under its launcher lock.
pub fn acquire_profile(directory: &Path, requested: Option<&str>) -> io::Result<Profile> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let server_id = match requested {
        Some(id) if protocol::is_server_id(id) => id.to_string(),
        Some(id) => return Err(io::Error::other(format!("Invalid experimental server ID: {id}"))),
        None => default_server_id(directory)?,
    };
    let lock = Lock::acquire(
        &directory.join(format!("launcher-{server_id}")),
        LOCK_STALE,
        LOCK_RETRY,
        LOCK_WAIT,
    )?;
    Ok(Profile { server_id, _lock: lock })
}

fn default_server_id(directory: &Path) -> io::Result<String> {
    let path = directory.join(DEFAULT_SERVER_ID_FILE);
    let read = |path: &Path| -> io::Result<String> {
        let value = std::fs::read_to_string(path)?.trim().to_string();
        if !protocol::is_server_id(&value) {
            return Err(io::Error::other(format!(
                "Invalid default experimental server identity in {}",
                path.display()
            )));
        }
        Ok(value)
    };
    match read(&path) {
        Ok(id) => Ok(id),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let candidate = uuid_v4();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(candidate.as_bytes())?;
                    Ok(candidate)
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => read(&path),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// `acquireServerActivation`: serializes cold starts of one logical server.
pub fn acquire_activation(directory: &Path, server_id: &str) -> io::Result<Lock> {
    Lock::acquire(
        &directory.join(format!("activation-{server_id}")),
        ACTIVATION_TIMEOUT * 2,
        LOCK_RETRY,
        ACTIVATION_TIMEOUT,
    )
}

/// `fallbackServerId`: a stable UUIDv4 derived from the logical ID and the coordinator protocol.
pub fn fallback_server_id(server_id: &str) -> String {
    let digest = Sha256::digest(
        format!(
            "{server_id}\0ultron-coordinator-{}",
            crate::coordinator::COORDINATOR_PROTOCOL_VERSION
        )
        .as_bytes(),
    );
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let variant = (u8::from_str_radix(&hex[16..17], 16).unwrap_or(0) & 0x3) | 0x8;
    format!(
        "{}-{}-4{}-{variant:x}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}
