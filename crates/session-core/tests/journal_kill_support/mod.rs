//! The product's durable store for the kill/reopen process tests: a format-4 session file whose durable runtime
//! records go through [`JournalStorage`], opened anew from the file the way a restarted worker opens it.

#![allow(dead_code)]

use session_core::durable_store::{JournalStorage, SharedSession, stored_commits};
use session_journal::repo::read_metadata;
use session_journal::{Session, SessionRepo, SystemClock};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Create an empty session file under `dir` and return its path.
pub fn create_session(dir: &Path) -> PathBuf {
    let repo = SessionRepo::new(&dir.join("sessions"), SystemClock::shared());
    let session = repo
        .create(&dir.display().to_string(), None, None)
        .expect("create session");
    session.metadata().path.clone()
}

/// The session file at `path`, opened anew.
pub fn open_session(path: &Path) -> Session {
    let metadata = read_metadata(path).expect("a session file");
    let repo = SessionRepo::new(path.parent().expect("a session directory"), SystemClock::shared());
    repo.open(&metadata).expect("open session")
}

/// The durable storage of the session file at `path`, replayed from its journal.
pub fn open_journal(path: &Path) -> JournalStorage {
    let session: SharedSession = Arc::new(Mutex::new(open_session(path)));
    JournalStorage::open(session, None).expect("open journal storage")
}

/// How many durable commits the session file at `path` holds.
pub fn durable_commits(path: &Path) -> usize {
    stored_commits(&open_session(path))
        .expect("durable records parse")
        .len()
}

/// SIGKILL a process this test's child spawned and reported by pid in a file the test controls. Only a plain
/// positive pid is signalled, never a group (`-pid`), `0`, `-1`, or this process.
pub fn kill_reported_pid(pid: &str) {
    let pid = pid.trim();
    let valid = !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && pid.parse::<u32>().is_ok_and(|pid| pid > 1 && pid != std::process::id());
    assert!(valid, "refusing to signal {pid:?}");
    let _ = std::process::Command::new("kill").args(["-9", pid]).status();
}
