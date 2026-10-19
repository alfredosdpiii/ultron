//! Per-turn working-tree checkpoints and the file-restoring rewind (core::checkpoints; Rust-only feature, no
//! TypeScript reference): the session actor in a private home against a scripted provider on loopback, its working
//! directory a temporary Git repository the test creates. Every root user turn records the tree before it runs as an
//! unreferenced commit stored under the prompt entry (`ultron.checkpoint`); `restoreFiles` writes a checkpoint's
//! files back after saving the current files as another checkpoint, and never touches HEAD, the index or ignored
//! files.

mod p_session_support;

use p_session_support::{Actor, Setup, actor, in_order, text};
use serde_json::{Value, json};
use session_core::core::Op;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with `a.txt` ("one") committed.
fn init_repo(cwd: &Path) -> String {
    git(cwd, &["init", "-q", "-b", "main"]);
    git(cwd, &["config", "user.name", "Test"]);
    git(cwd, &["config", "user.email", "test@example.com"]);
    git(cwd, &["config", "commit.gpgsign", "false"]);
    std::fs::write(cwd.join("a.txt"), "one").unwrap();
    git(cwd, &["add", "a.txt"]);
    git(cwd, &["commit", "-q", "-m", "init"]);
    git(cwd, &["rev-parse", "HEAD"])
}

fn read(cwd: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(cwd.join(name)).ok()
}

/// The checkpoint values written, in order.
fn checkpoints(a: &Actor) -> Vec<Value> {
    a.writes()
        .into_iter()
        .filter(|w| w["namespace"] == session_journal::checkpoints::CHECKPOINTS && w["op"] == "set")
        .collect()
}

/// The user prompt entries, in order.
fn prompt_entries(a: &Actor) -> Vec<String> {
    a.writes()
        .into_iter()
        .filter(|w| w["kind"] == "entry" && w["message"]["role"] == "user")
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect()
}

fn restore_entry(a: &Actor, entry: &str) -> Result<Option<Value>, String> {
    a.call(Op::RestoreFiles {
        entry_id: Some(entry.into()),
        checkpoint: None,
    })
}

fn prompt_ok(a: &Actor, text: &str) {
    let ran = a.prompt(text);
    assert_eq!(ran["error"], Value::Null, "{ran}");
}

/// Each root turn records the tree as it was before the turn ran (a file the "model" writes while the first request
/// is answered is not in the first checkpoint), stored under its prompt entry; a restore to the first prompt brings
/// back its files (modified file restored, files created later deleted) without moving HEAD or touching the real
/// index; the files from before the restore are a `restore:` checkpoint on the branch tip, so restoring the tip
/// again undoes it.
#[test]
fn session_core_checkpoints_record_each_turn_and_restore_is_undoable() {
    let cwd_slot: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
    let slot = cwd_slot.clone();
    let script = Box::new(move |_: &Value, index: usize| {
        // While the first turn runs, a file appears (what a cell would write).
        if index == 0
            && let Some(cwd) = slot.lock().unwrap().as_ref()
        {
            std::fs::write(cwd.join("during.txt"), "written by turn 1").unwrap();
        }
        text(&format!("Answer {index}."))
    });
    let a = actor(script, Setup::default());
    let head = init_repo(&a.cwd);
    *cwd_slot.lock().unwrap() = Some(a.cwd.clone());
    let index_before = std::fs::read(a.cwd.join(".git/index")).unwrap();

    prompt_ok(&a, "first");
    std::fs::write(a.cwd.join("a.txt"), "two").unwrap();
    std::fs::write(a.cwd.join("b.txt"), "bee").unwrap();
    prompt_ok(&a, "second");
    let prompts = prompt_entries(&a);
    assert_eq!(prompts.len(), 2);
    let stored = checkpoints(&a);
    assert_eq!(stored.len(), 2, "{stored:?}");
    for (value, prompt) in stored.iter().zip(&prompts) {
        assert_eq!(value["key"], prompt.as_str());
        assert_eq!(value["value"]["kind"], "turn");
        assert_eq!(value["value"]["entryId"], prompt.as_str());
        assert_eq!(value["value"]["vcs"], "git");
        assert_eq!(value["value"]["head"], head.as_str());
        assert_eq!(value["value"]["root"], a.cwd.display().to_string());
    }
    // The first turn's tree is HEAD's (nothing was dirty): the checkpoint is HEAD itself, no new commit.
    assert_eq!(stored[0]["value"]["commit"], head.as_str());
    let second = stored[1]["value"]["commit"].as_str().unwrap().to_string();
    assert_ne!(second, head);
    // The second checkpoint holds the files as they were before the second turn, including during.txt.
    let listed = git(&a.cwd, &["ls-tree", "-r", "--name-only", &second]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), ["a.txt", "b.txt", "during.txt"]);
    assert_eq!(git(&a.cwd, &["show", &format!("{second}:a.txt")]), "two");
    // Unreferenced: no branch or tag points at it.
    assert_eq!(git(&a.cwd, &["for-each-ref", "--contains", &second]), "");

    // More work after the second turn, then rewind the files to the first prompt.
    std::fs::write(a.cwd.join("a.txt"), "three").unwrap();
    std::fs::write(a.cwd.join("c.txt"), "sea").unwrap();
    std::fs::create_dir_all(a.cwd.join("sub/deep")).unwrap();
    std::fs::write(a.cwd.join("sub/deep/d.txt"), "dee").unwrap();
    let restored = restore_entry(&a, &prompts[0]).unwrap().unwrap();
    assert_eq!(restored["checkpoint"], prompts[0].as_str());
    assert_eq!(restored["commit"], head.as_str());
    assert_eq!(read(&a.cwd, "a.txt").as_deref(), Some("one"));
    for gone in ["b.txt", "c.txt", "during.txt", "sub/deep/d.txt"] {
        assert_eq!(read(&a.cwd, gone), None, "{gone}");
    }
    // Emptied directories are removed.
    assert!(!a.cwd.join("sub").exists());
    assert_eq!(restored["written"], 1);
    assert_eq!(restored["deleted"], 4);
    // HEAD, the branch and the real index are untouched.
    assert_eq!(git(&a.cwd, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&a.cwd, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(std::fs::read(a.cwd.join(".git/index")).unwrap(), index_before);

    // The files from before the restore are a checkpoint on the tip.
    let safety_key = restored["safety"]["key"].as_str().unwrap().to_string();
    assert!(safety_key.starts_with("restore:"), "{safety_key}");
    let safety = checkpoints(&a).pop().unwrap();
    assert_eq!(safety["key"], safety_key.as_str());
    assert_eq!(safety["value"]["kind"], "restore");
    assert_eq!(safety["value"]["restoredTo"], prompts[0].as_str());
    let tip = a.call(Op::Tree).unwrap().unwrap()["leafId"].clone();
    assert_eq!(safety["value"]["entryId"], tip);
    assert!(
        restored["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("Restored files to checkpoint {}", &head[..12])),
        "{restored}"
    );

    // Undo: restoring the tip brings every file back.
    let undone = restore_entry(&a, tip.as_str().unwrap()).unwrap().unwrap();
    assert_eq!(undone["checkpoint"], safety_key.as_str());
    assert_eq!(read(&a.cwd, "a.txt").as_deref(), Some("three"));
    assert_eq!(read(&a.cwd, "b.txt").as_deref(), Some("bee"));
    assert_eq!(read(&a.cwd, "c.txt").as_deref(), Some("sea"));
    assert_eq!(read(&a.cwd, "during.txt").as_deref(), Some("written by turn 1"));
    assert_eq!(read(&a.cwd, "sub/deep/d.txt").as_deref(), Some("dee"));

    // An entry inside a turn restores that turn's start: the first turn's answer means the first checkpoint.
    let entries = a.call(Op::Entries { since: None }).unwrap().unwrap();
    let first_answer = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let by_answer = restore_entry(&a, &first_answer).unwrap().unwrap();
    assert_eq!(by_answer["checkpoint"], prompts[0].as_str());
    assert_eq!(read(&a.cwd, "a.txt").as_deref(), Some("one"));
    assert_eq!(read(&a.cwd, "c.txt"), None);
    // The second prompt's checkpoint: its own files.
    let by_second = restore_entry(&a, &prompts[1]).unwrap().unwrap();
    assert_eq!(by_second["commit"], second.as_str());
    assert_eq!(read(&a.cwd, "a.txt").as_deref(), Some("two"));
    assert_eq!(read(&a.cwd, "b.txt").as_deref(), Some("bee"));
    // Restoring the same point again changes nothing and records nothing.
    let count = checkpoints(&a).len();
    let again = restore_entry(&a, &prompts[1]).unwrap().unwrap();
    assert_eq!(again["written"], 0);
    assert_eq!(again["safety"], Value::Null);
    assert_eq!(
        again["message"],
        format!("Files already match checkpoint {}", &second[..12])
    );
    assert_eq!(checkpoints(&a).len(), count);
}

/// A turn whose tree equals the last checkpoint's records that checkpoint again (`unchanged`), with no new commit;
/// the listing op reports every checkpoint with its kind and entry.
#[test]
fn session_core_checkpoints_unchanged_tree_reuses_the_last_commit() {
    let a = actor(in_order(vec![text("A."), text("B."), text("C.")]), Setup::default());
    init_repo(&a.cwd);
    std::fs::write(a.cwd.join("dirty.txt"), "x").unwrap();
    prompt_ok(&a, "first");
    prompt_ok(&a, "second");
    let stored = checkpoints(&a);
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0]["value"]["commit"], stored[1]["value"]["commit"]);
    assert_eq!(stored[0]["value"]["unchanged"], Value::Null);
    assert_eq!(stored[1]["value"]["unchanged"], true);
    let listed = a.call(Op::Checkpoints).unwrap().unwrap();
    assert_eq!(listed["enabled"], true);
    let items = listed["checkpoints"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["unchanged"], true);
    assert_eq!(items[0]["kind"], "turn");
    assert_eq!(items[0]["entryId"], prompt_entries(&a)[0].as_str());
}

/// No checkpoint outside a Git working tree, none with `checkpoints.enabled: false`, none while the shell sandbox is
/// on (whose restore is refused too); a restore without any checkpoint says so.
#[test]
fn session_core_checkpoints_are_off_outside_git_when_disabled_and_in_the_sandbox() {
    let plain = actor(in_order(vec![text("A.")]), Setup::default());
    prompt_ok(&plain, "first");
    assert!(checkpoints(&plain).is_empty());
    let entry = prompt_entries(&plain).remove(0);
    assert_eq!(
        restore_entry(&plain, &entry).unwrap_err(),
        "No checkpoint of the files at this point (checkpoints start with the first turn in a Git repository)"
    );

    let disabled = actor(
        in_order(vec![text("A.")]),
        Setup {
            settings: Some(json!({"checkpoints": {"enabled": false}})),
            ..Setup::default()
        },
    );
    init_repo(&disabled.cwd);
    prompt_ok(&disabled, "first");
    assert!(checkpoints(&disabled).is_empty());
    assert_eq!(disabled.call(Op::Checkpoints).unwrap().unwrap()["enabled"], false);

    let sandboxed = actor(
        in_order(vec![text("A.")]),
        Setup {
            settings: Some(json!({"sandbox": {"mode": "auto"}})),
            ..Setup::default()
        },
    );
    init_repo(&sandboxed.cwd);
    prompt_ok(&sandboxed, "first");
    assert!(checkpoints(&sandboxed).is_empty());
    let entry = prompt_entries(&sandboxed).remove(0);
    assert_eq!(
        restore_entry(&sandboxed, &entry).unwrap_err(),
        session_core::core::RESTORE_IN_SANDBOX
    );
}

/// Bounds: a tree listing more files than `checkpoints.maxFiles` keeps a note instead of a commit (a restore to it
/// names the note); untracked files larger than `checkpoints.maxFileBytes` are left out (`excluded`).
#[test]
fn session_core_checkpoints_bound_huge_trees_and_large_files() {
    let few = actor(
        in_order(vec![text("A.")]),
        Setup {
            settings: Some(json!({"checkpoints": {"maxFiles": 2}})),
            ..Setup::default()
        },
    );
    init_repo(&few.cwd);
    std::fs::write(few.cwd.join("b.txt"), "b").unwrap();
    std::fs::write(few.cwd.join("c.txt"), "c").unwrap();
    prompt_ok(&few, "first");
    let stored = checkpoints(&few);
    assert_eq!(stored.len(), 1);
    let note = "the working tree lists 3 files, more than checkpoints.maxFiles (2)";
    assert_eq!(stored[0]["value"]["skipped"], note);
    assert_eq!(stored[0]["value"]["commit"], Value::Null);
    let entry = prompt_entries(&few).remove(0);
    assert_eq!(
        restore_entry(&few, &entry).unwrap_err(),
        format!("The files at this point were not recorded: {note}")
    );

    let small = actor(
        in_order(vec![text("A.")]),
        Setup {
            settings: Some(json!({"checkpoints": {"maxFileBytes": 10}})),
            ..Setup::default()
        },
    );
    init_repo(&small.cwd);
    std::fs::write(small.cwd.join("big.bin"), "0123456789ABCDEF").unwrap();
    std::fs::write(small.cwd.join("small.txt"), "tiny").unwrap();
    prompt_ok(&small, "first");
    let value = checkpoints(&small).remove(0)["value"].clone();
    assert_eq!(value["excluded"], json!(["big.bin"]));
    let commit = value["commit"].as_str().unwrap();
    let listed = git(&small.cwd, &["ls-tree", "-r", "--name-only", commit]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), ["a.txt", "small.txt"]);
    // Restoring keeps the excluded file (neither side holds it).
    std::fs::remove_file(small.cwd.join("small.txt")).unwrap();
    restore_entry(&small, &prompt_entries(&small)[0]).unwrap();
    assert_eq!(read(&small.cwd, "small.txt").as_deref(), Some("tiny"));
    assert_eq!(read(&small.cwd, "big.bin").as_deref(), Some("0123456789ABCDEF"));
}

/// Never lose user work: a restore that would overwrite a file the safety checkpoint cannot hold (ignored now) is
/// refused and changes nothing.
#[test]
fn session_core_checkpoints_restore_refuses_to_overwrite_unsaved_files() {
    let a = actor(in_order(vec![text("A.")]), Setup::default());
    init_repo(&a.cwd);
    std::fs::write(a.cwd.join("notes.log"), "old notes").unwrap();
    prompt_ok(&a, "first");
    // Later the file becomes ignored, and is edited.
    std::fs::write(a.cwd.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(a.cwd.join("notes.log"), "precious new notes").unwrap();
    std::fs::write(a.cwd.join("a.txt"), "edited").unwrap();
    let before = checkpoints(&a).len();
    let error = restore_entry(&a, &prompt_entries(&a)[0]).unwrap_err();
    assert_eq!(
        error,
        "Restoring would overwrite files that cannot be saved first (ignored or too large): notes.log; nothing was restored"
    );
    assert_eq!(read(&a.cwd, "notes.log").as_deref(), Some("precious new notes"));
    assert_eq!(read(&a.cwd, "a.txt").as_deref(), Some("edited"));
    assert_eq!(read(&a.cwd, ".gitignore").as_deref(), Some("*.log\n"));
    assert_eq!(checkpoints(&a).len(), before);
}

/// A restore while a run streams is refused (the run's cells may be writing files).
#[test]
fn session_core_checkpoints_restore_waits_for_the_run() {
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (arrived, arrival) = std::sync::mpsc::channel::<()>();
    let released = Mutex::new(released);
    let script = Box::new(move |_: &Value, index: usize| {
        if index == 0 {
            let _ = arrived.send(());
            let _ = released.lock().unwrap().recv_timeout(p_session_support::HANG);
        }
        text("A.")
    });
    let a = actor(script, Setup::default());
    init_repo(&a.cwd);
    a.call(Op::Prompt {
        message: "first".into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap();
    arrival.recv_timeout(p_session_support::HANG).unwrap();
    let entry = prompt_entries(&a).remove(0);
    assert_eq!(
        restore_entry(&a, &entry).unwrap_err(),
        session_core::core::RESTORE_WHILE_STREAMING
    );
    release.send(()).unwrap();
    a.wait_event("run_end");
}

/// In a Jujutsu repository (a `.jj` directory) with `jj` on the session's PATH, jj's own snapshot is the checkpoint
/// (`--no-integrate-operation`, `@`'s commit id), and a restore saves the current files with jj first, then runs
/// `jj restore --from <commit>`. Without `jj` on the session's PATH the Git checkpoint is used.
#[test]
fn session_core_checkpoints_use_jj_from_the_session_path() {
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("jj.log");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$*\" in\n  *--no-integrate-operation*) n=$(grep -c no-integrate '{}'); echo \"snap$n\" ;;\n  *--ignore-working-copy*) echo head1 ;;\nesac\n",
        log.display(),
        log.display()
    );
    let jj = bin.path().join("jj");
    std::fs::write(&jj, script).unwrap();
    std::fs::set_permissions(&jj, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let a = actor(
        in_order(vec![text("A.")]),
        Setup {
            vars: vec![("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))],
            ..Setup::default()
        },
    );
    std::fs::create_dir_all(a.cwd.join(".jj")).unwrap();
    prompt_ok(&a, "first");
    let value = checkpoints(&a).remove(0)["value"].clone();
    assert_eq!(value["vcs"], "jj");
    assert_eq!(value["head"], "head1");
    assert_eq!(value["commit"], "snap1");
    assert_eq!(value["root"], a.cwd.display().to_string());
    let restored = restore_entry(&a, &prompt_entries(&a)[0]).unwrap().unwrap();
    assert_eq!(restored["safety"]["commit"], "snap2");
    let calls = std::fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert!(
        calls[1].starts_with("--no-integrate-operation --color=never --no-pager --config snapshot.max-new-file-size=")
    );
    assert!(calls[1].ends_with("log --no-graph -r @ -T commit_id"), "{}", calls[1]);
    assert_eq!(calls[4], "--color=never --no-pager restore --from snap1");

    // Without jj on the session's PATH, a jj directory alone does not make a jj checkpoint.
    let git_only = actor(in_order(vec![text("A.")]), Setup::default());
    init_repo(&git_only.cwd);
    std::fs::create_dir_all(git_only.cwd.join(".jj")).unwrap();
    prompt_ok(&git_only, "first");
    assert_eq!(checkpoints(&git_only).remove(0)["value"]["vcs"], "git");
}
