//! The Radius login's fixed loopback callback port (127.0.0.1:1456, the registered redirect URI), shared by every
//! test process on the machine. The recorded-flow replay (providers_rest.rs, whose pasted-redirect scenarios start the
//! callback server on it) holds this lock for the whole replay. The loopback-callback tests (behavior_rest.rs) listen
//! on a free port of their own instead (`RadiusOAuth::callback_port`): a lock only orders the processes that take
//! it, and copies of these tests in other worktrees, or a real login, bind 1456 without it.

use std::fs::File;

/// Holds the port for one test (an exclusive lock on a file beside it, released when the file is dropped). With
/// `probe`, also checks the port is free and fails with the reason when a process outside these tests holds it.
pub fn callback_port(probe: bool) -> File {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("ultron-test-oauth-callback-1456.lock"))
        .unwrap();
    lock.lock().unwrap();
    if probe && let Err(error) = std::net::TcpListener::bind(("127.0.0.1", 1456)) {
        panic!("OAuth callback port 1456 is held by a process outside these tests ({error}); they need it free");
    }
    lock
}
