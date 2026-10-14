//! Scripts written by tests and then executed by path stay executable while other test threads fork
//! (`executable_support`): the race that made fake `claude`/`gh`/linter launches fail with ETXTBSY ("Text file
//! busy") under parallel load. Every wait here is a process ending; nothing sleeps.

mod executable_support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const WRITERS: usize = 4;
const FORKERS: usize = 4;
const ROUNDS: usize = 150;

/// Write-then-exec `ROUNDS` scripts in each of `WRITERS` threads while `FORKERS` threads fork without pause; the
/// failed launches, with their errors.
fn launch_while_forking(dir: &Path, write: fn(&Path, &str)) -> Vec<String> {
    let stop = Arc::new(AtomicBool::new(false));
    let forkers: Vec<_> = (0..FORKERS)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("/bin/true").status();
                }
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let dir: PathBuf = dir.to_path_buf();
            std::thread::spawn(move || {
                let mut failures = Vec::new();
                for round in 0..ROUNDS {
                    let script = dir.join(format!("s{writer}-{round}"));
                    write(&script, "#!/bin/sh\nexit 7\n");
                    match Command::new(&script).status() {
                        Ok(status) if status.code() == Some(7) => {}
                        Ok(status) => failures.push(format!("{}: {status}", script.display())),
                        Err(error) => failures.push(format!("{}: {error}", script.display())),
                    }
                }
                failures
            })
        })
        .collect();
    let failures = writers.into_iter().flat_map(|w| w.join().unwrap()).collect();
    stop.store(true, Ordering::Relaxed);
    for forker in forkers {
        forker.join().unwrap();
    }
    failures
}

/// The way the tests used to write them: a write descriptor in this process, then chmod.
fn write_in_process(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn cli_exec_race_installed_scripts_always_launch_while_other_threads_fork() {
    let dir = tempfile::tempdir().unwrap();
    let failures = launch_while_forking(dir.path(), executable_support::install);
    assert!(
        failures.is_empty(),
        "{} launches failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The race itself (evidence for the helper, not a gate: whether a given run loses it depends on scheduling):
/// scripts written in-process fail to launch with ETXTBSY while other threads fork.
#[test]
#[ignore = "demonstration of the race; run with `--ignored exec_race`"]
fn cli_exec_race_scripts_written_in_process_hit_text_file_busy() {
    let dir = tempfile::tempdir().unwrap();
    let failures = launch_while_forking(dir.path(), write_in_process);
    println!("{} of {} launches failed", failures.len(), WRITERS * ROUNDS);
    assert!(
        failures.iter().any(|f| f.contains("Text file busy")),
        "no ETXTBSY this run (the race depends on scheduling)"
    );
}
