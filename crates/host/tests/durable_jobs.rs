//! Durable shell jobs across a killed worker: a child process (this test binary, re-run in the child role) owns a
//! `Host` with a file journal, starts four jobs and is SIGKILLed. While no worker runs, one job finishes on its own
//! and another job's process group is killed. A new `Host` over the same journal must report each job's real state:
//! the finished one completed with its text, the surviving one still running (and followed to its end), the one
//! that ended while nobody followed it completed with the exit status its keeper wrote, and the killed one `lost`
//! with its captured output. Nothing is reported as gone.
//!
//! Every wait is on a condition (a file, a process ending), with a 120 s hang guard only.

use host::journal::FileJobJournal;
use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHILD: &str = "ULTRON_HOST_DURABLE_JOBS_CHILD";
const HANG: Duration = Duration::from_secs(120);

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG;
    while !condition() {
        assert!(Instant::now() < deadline, "hang guard: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn call(host: &Host, kind: &str, payload: Value) -> Value {
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    host.handle(kind, payload, &Signal::new())
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
}

fn host_in(dir: &Path) -> Host {
    Host::new(dir, dir.join(".jobs"), dir, Box::new(AllowAll))
        .with_journal(Arc::new(FileJobJournal::new(dir.join("jobs.json"))))
        .expect("journal")
}

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

/// The child role: start the jobs, wait until each has written its first line, report their ids, then block until
/// killed.
fn child(dir: &Path) -> ! {
    let host = host_in(dir);
    let start = |command: &str| {
        call(&host, "shell.run", json!({ "command": command, "yield_after": 0 }))["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let done = start("echo kept");
    call(&host, "shell.result", json!({ "id": done }));
    let running =
        start("echo before; i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo after");
    let exited = start(
        "echo partial; i=0; while [ ! -e stop-exited ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; exit 3",
    );
    let lost = start("echo lost-partial; i=0; while [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done");
    for id in [&running, &exited, &lost] {
        let raw = dir.join(".jobs").join(format!("{id}.out"));
        wait_for("first line", || {
            std::fs::read_to_string(&raw).is_ok_and(|text| text.ends_with('\n'))
        });
    }
    let ready = json!({ "done": done, "running": running, "exited": exited, "lost": lost });
    std::fs::write(dir.join("ready.tmp"), ready.to_string()).unwrap();
    std::fs::rename(dir.join("ready.tmp"), dir.join("ready.json")).unwrap();
    loop {
        std::thread::park();
    }
}

fn spawn_child(test: &str, dir: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, dir)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

fn journal_record(dir: &Path, id: &str) -> Value {
    let journal: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("jobs.json")).unwrap()).unwrap();
    journal["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == id)
        .cloned()
        .unwrap()
}

fn subset(actual: &Value, expected: &Value) {
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&actual[key], value, "{key} of {actual}");
    }
}

#[test]
fn host_durable_jobs_report_their_real_state_after_the_worker_is_killed() {
    if let Some(dir) = std::env::var_os(CHILD) {
        child(&PathBuf::from(dir));
    }
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let mut worker = spawn_child(
        "host_durable_jobs_report_their_real_state_after_the_worker_is_killed",
        &dir,
    );
    wait_for("the child's jobs", || dir.join("ready.json").exists());
    worker.kill().unwrap();
    worker.wait().unwrap();
    let ids: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("ready.json")).unwrap()).unwrap();
    let id = |name: &str| ids[name].as_str().unwrap().to_string();
    for name in ["running", "exited", "lost"] {
        assert_eq!(journal_record(&dir, &id(name))["status"], "running", "{name}");
    }

    // While no worker runs: one job ends on its own, another job's whole process group is killed.
    std::fs::write(dir.join("stop-exited"), "").unwrap();
    let exit_file = dir.join(".jobs").join(format!("{}.exit", id("exited")));
    wait_for("the exited job's status", || {
        std::fs::read_to_string(&exit_file).is_ok_and(|text| text.ends_with('\n'))
    });
    let lost_pid = journal_record(&dir, &id("lost"))["pid"].as_i64().unwrap() as i32;
    rustix::process::kill_process_group(
        rustix::process::Pid::from_raw(lost_pid).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    wait_for("the lost job's process to go", || !alive(lost_pid));

    let host = host_in(&dir);
    let get = |name: &str| call(&host, "shell.get", json!({ "id": id(name) }));
    subset(
        &get("done"),
        &json!({ "status": "completed", "running": false, "ok": true, "exit_code": 0, "text": "kept\n" }),
    );
    subset(
        &get("running"),
        &json!({ "status": "running", "running": true, "exit_code": null }),
    );
    subset(
        &get("exited"),
        &json!({ "status": "completed", "running": false, "ok": false, "exit_code": 3, "text": "partial\n", "error": null }),
    );
    subset(
        &get("lost"),
        &json!({
            "status": "lost", "running": false, "ok": false, "exit_code": null, "text": "lost-partial\n",
            "error": "its process ended while no Ultron worker was running; the exit status is unknown",
        }),
    );
    // The reopened journal already says so.
    assert_eq!(journal_record(&dir, &id("exited"))["status"], "completed");
    assert_eq!(journal_record(&dir, &id("lost"))["status"], "lost");
    assert_eq!(journal_record(&dir, &id("running"))["status"], "running");

    // The surviving job is followed to its end by the new worker, with its whole output.
    std::fs::write(dir.join("go"), "").unwrap();
    subset(
        &call(&host, "shell.result", json!({ "id": id("running") })),
        &json!({ "status": "completed", "ok": true, "exit_code": 0, "text": "before\nafter\n" }),
    );
    assert_eq!(journal_record(&dir, &id("running"))["status"], "completed");
    let listed = call(&host, "shell.list", json!({}));
    let statuses: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|job| job["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["lost", "completed", "completed", "completed"]);
}

/// A job handed over (`release_jobs`) keeps running after its host is dropped, and the next owner of the journal
/// follows it; a host that is simply dropped stops its jobs and journals that.
#[test]
fn host_durable_jobs_release_hands_running_jobs_to_the_next_owner() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let first = host_in(&dir);
    let id = call(
        &first,
        "shell.run",
        json!({ "command": "i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo handed", "yield_after": 0 }),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let pid = journal_record(&dir, &id)["pid"].as_i64().unwrap() as i32;
    first.release_jobs();
    drop(first);
    assert!(alive(pid), "a released job keeps running");
    let second = host_in(&dir);
    assert_eq!(call(&second, "shell.get", json!({ "id": id }))["status"], "running");
    std::fs::write(dir.join("go"), "").unwrap();
    subset(
        &call(&second, "shell.result", json!({ "id": id })),
        &json!({ "status": "completed", "exit_code": 0, "text": "handed\n" }),
    );

    let stopped = call(
        &second,
        "shell.run",
        json!({ "command": "i=0; while [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done", "yield_after": 0 }),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    let pid = journal_record(&dir, &stopped)["pid"].as_i64().unwrap() as i32;
    drop(second);
    assert!(!alive(pid), "dropping the host stops its jobs");
    assert_eq!(journal_record(&dir, &stopped)["status"], "cancelled");
}

/// A TypeScript journal names no process: its running jobs read as interrupted, as there.
#[test]
fn host_durable_jobs_typescript_records_read_as_interrupted() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let jobs = dir.join(".jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("job-0000aaaa.log"), "half\n").unwrap();
    let record = json!({
        "id": "job-0000aaaa", "lane": "main", "rootId": null, "command": "sleep 30", "cwd": dir, "timeout": null,
        "status": "running", "exitCode": null, "startedAt": 1, "endedAt": null,
        "outputPath": jobs.join("job-0000aaaa.log"), "outputBytes": 5, "retainedBytes": 5, "tail": "",
    });
    std::fs::write(
        dir.join("jobs.json"),
        json!({ "version": 1, "jobs": [record] }).to_string(),
    )
    .unwrap();
    let host = host_in(&dir);
    subset(
        &call(&host, "shell.get", json!({ "id": "job-0000aaaa" })),
        &json!({ "status": "interrupted", "running": false, "error": "worker restarted", "text": "half\n" }),
    );
}
