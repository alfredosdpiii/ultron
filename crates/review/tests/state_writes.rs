//! The state file is rewritten only when the state changed: every poll updates each pull request it sees (a merged
//! one is marked closed again), and ~70 whole-file rewrites a poll came to about 15 GB a day.

use std::os::unix::fs::MetadataExt;

use review::state::StateStore;

const KEY: &str = "ultron-bot@github.com/o/r#1";

#[test]
fn state_an_update_that_changes_nothing_leaves_the_file_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("state.json");
    let store = StateStore::new(path.to_str().expect("path"));
    store
        .update_pull(KEY, |pull, _| pull.closed = Some(true))
        .expect("first write");
    // A write is a temporary file renamed over the state: a new inode.
    let inode = || std::fs::metadata(&path).expect("state").ino();
    let (first, text) = (inode(), std::fs::read_to_string(&path).expect("text"));
    store
        .update_pull(KEY, |pull, _| pull.closed = Some(true))
        .expect("same change");
    store.update(|_| ()).expect("no change");
    assert_eq!(inode(), first, "an unchanged state is not written");
    assert_eq!(std::fs::read_to_string(&path).expect("text"), text);
    store
        .update_pull(KEY, |pull, _| pull.last_verdict = Some("approve".into()))
        .expect("a change");
    assert_ne!(inode(), first, "a changed state is written");
    assert_eq!(store.read().pulls[KEY].last_verdict.as_deref(), Some("approve"));
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name != "state.json")
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
