//! `proper-lockfile`-compatible locks: the lock is the directory `<file>.lock`, created atomically with `mkdir`.
//! A lock directory whose mtime is older than `stale` is taken over, as `proper-lockfile` does.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

/// Held lock; dropping it removes the lock directory.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

/// Settings and trust locks: `lockSync` retried 10 times, 20 ms apart, stale after 10 s.
pub const SYNC_ATTEMPTS: u32 = 10;
pub const SYNC_STALE: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_millis(20);

fn try_acquire(lock_path: &Path, stale: Duration) -> io::Result<LockGuard> {
    match fs::create_dir(lock_path) {
        Ok(()) => {
            return Ok(LockGuard {
                path: lock_path.to_path_buf(),
            });
        }
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        Err(_) => {}
    }
    let modified = match fs::metadata(lock_path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        // Released between our mkdir and stat: try once more without the stale check.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return try_acquire(lock_path, Duration::MAX),
        Err(e) => return Err(e),
    };
    let age = SystemTime::now().duration_since(modified).unwrap_or_default();
    if age <= stale {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("Lock file is already being held: {}", lock_path.display()),
        ));
    }
    fs::remove_dir_all(lock_path)?;
    try_acquire(lock_path, Duration::MAX)
}

/// Locks `lock_path` (the `.lock` directory), retrying `attempts` times while another holder has it.
pub fn acquire(lock_path: &Path, stale: Duration, attempts: u32) -> io::Result<LockGuard> {
    let mut attempt = 1;
    loop {
        match try_acquire(lock_path, stale) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && attempt < attempts => {
                attempt += 1;
                sleep(RETRY_DELAY);
            }
            result => return result,
        }
    }
}

/// The lock directory `proper-lockfile` uses for `file` with `realpath: false`.
pub fn lock_path_for(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}
