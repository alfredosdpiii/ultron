//! Sandboxed shell jobs survive their worker, as unsandboxed ones do (durable_jobs.rs): a child process (this test
//! binary in the child role) owns a sandboxed `Host` with a file journal, starts a job and is SIGKILLed. The job
//! keeps running (its sandbox's parent-death tie, bwrap's `--die-with-parent` or the `setpriv --pdeathsig` of unshare
//! and the Docker client, watches a keeper shell, not the worker), still isolated (it cannot write the home), and a
//! new sandboxed `Host` over the same journal adopts it and follows it to its end with its whole output. Run under
//! every backend that works here; the others are skipped with a `SKIPPED` line (`backends_support`).
//!
//! Every wait is on a condition (a file, a process ending), with a 120 s hang guard only.

#[path = "../../sandbox/tests/backends_support/mod.rs"]
mod backends_support;

use host::journal::FileJobJournal;
use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use sandbox::launcher::Mechanism;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHILD: &str = "ULTRON_HOST_SANDBOXED_HAND_OVER_CHILD";
const CHILD_MECHANISM: &str = "ULTRON_HOST_SANDBOXED_HAND_OVER_MECHANISM";
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

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

/// `base/home/project` is the project; the home is visible read-only inside the sandbox.
fn host_in(base: &Path, mechanism: Mechanism) -> Host {
    let (home, project) = (base.join("home"), base.join("home/project"));
    let settings = json!({ "sandbox": {
        "mode": "required",
        "network": false,
        "image": backends_support::docker_image().ok(),
    } });
    Host::new(&project, project.join(".jobs"), &project, Box::new(AllowAll))
        .sandboxed_with_mechanism(&settings, &home, None, Some(mechanism))
        .with_journal(Arc::new(FileJobJournal::new(base.join("jobs.json"))))
        .expect("journal")
}

/// The child role: start a sandboxed job that waits for `go`, report its id and pid once it printed its first lines,
/// then block until killed.
fn child(base: &Path) -> ! {
    let mechanism = std::env::var(CHILD_MECHANISM)
        .ok()
        .and_then(|name| Mechanism::parse(&name))
        .unwrap();
    let host = host_in(base, mechanism);
    let escape = base.join("home/escaped").display().to_string();
    let command = format!(
        "echo before; (echo x > {escape}) 2>/dev/null && echo escaped || echo blocked; while [ ! -e go ]; do sleep 0.05; done; echo after"
    );
    let job = call(&host, "shell.run", json!({ "command": command, "yield_after": 0 }));
    let id = job["id"].as_str().unwrap().to_string();
    let raw = base.join("home/project/.jobs").join(format!("{id}.out"));
    wait_for("the job's first lines", || {
        std::fs::read_to_string(&raw).is_ok_and(|text| text.lines().count() >= 2)
    });
    std::fs::write(base.join("ready.tmp"), json!({ "id": id }).to_string()).unwrap();
    std::fs::rename(base.join("ready.tmp"), base.join("ready.json")).unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn host_sandboxed_job_survives_a_killed_worker_and_the_next_host_adopts_it() {
    if let Some(dir) = std::env::var_os(CHILD) {
        child(&PathBuf::from(dir));
    }
    // Probed with the bare tools (`backends_support`), so a launcher bug fails instead of skipping.
    for mechanism in backends_support::machine_backends()
        .into_iter()
        .filter_map(Mechanism::parse)
    {
        let temp = tempfile::Builder::new()
            .prefix("host-hand-over-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(base.join("home/project")).unwrap();
        hand_over(&base, mechanism);
    }
}

fn hand_over(base: &Path, mechanism: Mechanism) {
    let name = mechanism.name();
    let mut worker = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "host_sandboxed_job_survives_a_killed_worker_and_the_next_host_adopts_it",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, base)
        .env(CHILD_MECHANISM, name)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the child's job", || base.join("ready.json").exists());
    worker.kill().unwrap();
    worker.wait().unwrap();
    let ready: Value = serde_json::from_str(&std::fs::read_to_string(base.join("ready.json")).unwrap()).unwrap();
    let id = ready["id"].as_str().unwrap().to_string();
    let journal: Value = serde_json::from_str(&std::fs::read_to_string(base.join("jobs.json")).unwrap()).unwrap();
    let record = journal["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == id.as_str())
        .cloned()
        .unwrap();
    let pid = record["pid"].as_i64().unwrap() as i32;
    // The worker is gone; its sandboxed job is not.
    assert!(alive(pid), "{name}: the sandboxed job died with its worker");

    let host = host_in(base, mechanism);
    let got = call(&host, "shell.get", json!({ "id": id }));
    assert_eq!(
        (&got["status"], &got["running"]),
        (&json!("running"), &json!(true)),
        "{name}: {got}"
    );
    std::fs::write(base.join("home/project/go"), "").unwrap();
    let done = call(&host, "shell.result", json!({ "id": id }));
    assert_eq!(
        (&done["status"], &done["ok"], &done["exit_code"], &done["text"]),
        (
            &json!("completed"),
            &json!(true),
            &json!(0),
            &json!("before\nblocked\nafter\n")
        ),
        "{name}: {done}"
    );
    // Still isolated after the hand-over: the home stayed read-only.
    assert!(!base.join("home/escaped").exists(), "{name}");
    wait_for("the job's process to go", || !alive(pid));

    // Not tied to its worker, a sandboxed job is still stopped whole by its owner: cancelling kills the sandbox's
    // process group, and with it every process inside.
    let marker = format!("hand-over-marker-{}-{name}", std::process::id());
    let job = call(
        &host,
        "shell.run",
        json!({ "command": format!("echo {marker}; while true; do sleep 0.05; done"), "yield_after": 0 }),
    );
    let id = job["id"].as_str().unwrap().to_string();
    wait_for("the second job's process", || !processes_with(&marker).is_empty());
    let cancelled = call(&host, "shell.cancel", json!({ "id": id }));
    assert_eq!(cancelled["status"], "cancelled", "{name}: {cancelled}");
    wait_for("every process of the cancelled sandbox to go", || {
        processes_with(&marker).is_empty()
    });
}

/// Live processes whose command line mentions `marker` (the sandbox's inner processes are visible here too).
fn processes_with(marker: &str) -> Vec<i32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != std::process::id() as i32 && alive(*pid))
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(marker))
        })
        .collect()
}
