//! Single ownership of a storage file: one process at a time may write a session's storage. The owner holds an
//! exclusive `flock` on `<file>.owner` for as long as it lives and records itself there (`{pid, acquiredAt}`); the
//! kernel drops the lock when the owner dies, however it dies, so a dead owner's lock is simply taken over (its
//! stale record is overwritten). A second claimant is refused with the live owner's pid (`SessionInUseError`).

use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Why ownership was not taken.
#[derive(Debug)]
pub enum OwnerError {
    /// Another live process owns the file (its recorded pid, when readable).
    InUse {
        pid: Option<u32>,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for OwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnerError::InUse { pid: Some(pid) } => write!(f, "Storage is owned by process {pid}"),
            OwnerError::InUse { pid: None } => f.write_str("Storage is owned by another process"),
            OwnerError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for OwnerError {}

/// The owner record of `target` (`<target>.owner`).
pub fn owner_path(target: &Path) -> PathBuf {
    PathBuf::from(format!("{}.owner", target.display()))
}

/// The pid recorded as `target`'s owner, whether or not it still holds the lock.
pub fn recorded_owner(target: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(owner_path(target)).ok()?;
    let record: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    record["pid"].as_u64().map(|pid| pid as u32)
}

/// Exclusive ownership of one storage file, held until dropped (or the process ends).
#[derive(Debug)]
pub struct OwnerLock {
    file: File,
    path: PathBuf,
}

impl OwnerLock {
    /// Take ownership of `target` for this process, or report who holds it. Never waits.
    pub fn acquire(target: &Path) -> Result<OwnerLock, OwnerError> {
        let path = owner_path(target);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(OwnerError::Io)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                // A new owner records itself just after it locks: give its record a moment to appear.
                let mut pid = None;
                for _ in 0..50 {
                    let mut text = String::new();
                    let _ = file.seek(SeekFrom::Start(0));
                    let _ = file.read_to_string(&mut text);
                    pid = serde_json::from_str::<serde_json::Value>(text.trim())
                        .ok()
                        .and_then(|r| r["pid"].as_u64())
                        .map(|pid| pid as u32);
                    if pid.is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                return Err(OwnerError::InUse { pid });
            }
            Err(error) => return Err(OwnerError::Io(error.into())),
        }
        let acquired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let record = serde_json::json!({"pid": std::process::id(), "acquiredAt": acquired_at});
        file.set_len(0).map_err(OwnerError::Io)?;
        file.seek(SeekFrom::Start(0)).map_err(OwnerError::Io)?;
        file.write_all(format!("{record}\n").as_bytes())
            .map_err(OwnerError::Io)?;
        file.sync_data().map_err(OwnerError::Io)?;
        Ok(OwnerLock { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OwnerLock {
    /// The record goes first, then the lock: a reader never sees this process's pid after it let go.
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}
