//! Single ownership of a storage file (durable::owner): a second claimant is refused with the owner's pid while
//! the owner lives, and a dead owner's lock is taken over. The dead owner is a real process killed with SIGKILL:
//! this test binary re-runs itself as the holder.

use durable::owner::{OwnerError, OwnerLock, owner_path, recorded_owner};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HOLDER_ENV: &str = "DURABLE_OWNER_TEST_HOLD";

/// Run as the holder process: take ownership of the file named by the environment, announce it, then wait to be
/// killed.
#[test]
fn durable_owner_holder_process() {
    let Ok(target) = std::env::var(HOLDER_ENV) else { return };
    let _lock = OwnerLock::acquire(std::path::Path::new(&target)).unwrap();
    println!("held");
    // Held until the parent kills this process.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn durable_owner_second_claimant_is_refused_with_the_owner_pid() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("s.jsonl");
    let first = OwnerLock::acquire(&target).unwrap();
    assert_eq!(first.path(), owner_path(&target));
    assert_eq!(recorded_owner(&target), Some(std::process::id()));
    match OwnerLock::acquire(&target) {
        Err(OwnerError::InUse { pid }) => assert_eq!(pid, Some(std::process::id())),
        other => panic!("a second claimant must be refused, got {other:?}"),
    }
    drop(first);
    assert_eq!(recorded_owner(&target), None, "a released lock leaves no owner record");
    let again = OwnerLock::acquire(&target).unwrap();
    assert_eq!(recorded_owner(&target), Some(std::process::id()));
    drop(again);
}

#[test]
fn durable_owner_dead_owner_lock_is_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("s.jsonl");
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "durable_owner_holder_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_ENV, &target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while recorded_owner(&target) != Some(holder.id()) {
        assert!(Instant::now() < deadline, "the holder never took ownership");
        std::thread::sleep(Duration::from_millis(20));
    }
    match OwnerLock::acquire(&target) {
        Err(OwnerError::InUse { pid }) => assert_eq!(pid, Some(holder.id())),
        other => panic!("a live owner must refuse, got {other:?}"),
    }
    holder.kill().unwrap();
    holder.wait().unwrap();
    // The dead owner's record is still there, but its lock went with it.
    assert_eq!(recorded_owner(&target), Some(holder.id()));
    let adopted = OwnerLock::acquire(&target).unwrap();
    assert_eq!(recorded_owner(&target), Some(std::process::id()));
    drop(adopted);
}
