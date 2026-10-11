//! `proper-lockfile` locks: `mkdir <path>.lock` is the lock; a holder refreshes its mtime, and a lock whose mtime
//! is older than `stale` is taken over. TypeScript and Rust holders exclude each other.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub struct Lock {
    path: PathBuf,
    released: Arc<AtomicBool>,
}

impl Lock {
    /// Acquire `<target>.lock`, retrying every `retry` for up to `wait`.
    pub fn acquire(target: &Path, stale: Duration, retry: Duration, wait: Duration) -> io::Result<Lock> {
        let path = PathBuf::from(format!("{}.lock", target.display()));
        let deadline = Instant::now() + wait;
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => break,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| SystemTime::now().duration_since(m).ok());
                    if age.is_some_and(|age| age > stale) {
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(ErrorKind::WouldBlock, "Lock file is already being held"));
                    }
                    std::thread::sleep(retry);
                }
                Err(e) => return Err(e),
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let (touch, done) = (path.clone(), released.clone());
        let update = stale / 3;
        std::thread::spawn(move || {
            loop {
                let step = Duration::from_millis(50);
                let mut waited = Duration::ZERO;
                while waited < update {
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(step);
                    waited += step;
                }
                if std::fs::File::open(&touch)
                    .and_then(|f| f.set_modified(SystemTime::now()))
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(Lock { path, released })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir(&self.path);
    }
}
