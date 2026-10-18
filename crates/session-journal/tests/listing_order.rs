//! The order of a directory's session listing (`ultron -r`, `/resume`): `listNativeSessions` in
//! packages/coding-agent/src/native-command.ts sorts by file mtime, newest first, then by `createdAt`, newest first,
//! and keeps only this directory's sessions. File times are set explicitly, so the order never depends on when a
//! test happened to write each file.

use session_journal::names;
use session_journal::{SessionRepo, TestClock};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn session_journal_lists_sessions_newest_modified_first() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(dir.path(), TestClock::shared(1_790_000_000_000, 1));
    // Created in this order (the clock steps), so `createdAt` grows: a < b < c < d < elsewhere.
    let mut paths = Vec::new();
    for (id, cwd) in [("a", "/w"), ("b", "/w"), ("c", "/w"), ("d", "/w"), ("e", "/elsewhere")] {
        let session = repo.create(cwd, Some(id), None).unwrap();
        paths.push((id, session.metadata().path.clone()));
    }
    let mtime = |id: &str, seconds: u64| {
        let path = &paths.iter().find(|(i, _)| *i == id).unwrap().1;
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
    };
    // `a` was written last although created first; `c` and `d` share a time, so the newer creation comes first.
    mtime("a", 3_000);
    mtime("b", 1_000);
    mtime("c", 2_000);
    mtime("d", 2_000);
    mtime("e", 9_000);
    let listed: Vec<String> = names::list_sessions(dir.path(), "/w")
        .into_iter()
        .map(|s| s.metadata.id)
        .collect();
    assert_eq!(listed, ["a", "d", "c", "b"]);
}
