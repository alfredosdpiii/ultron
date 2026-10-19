//! The host's shell commands (blocking bash and shell jobs) under every sandbox backend that works on this machine,
//! not only the one `auto` picks (bwrap where it works): bwrap, rootless `unshare`, and Docker on a local image.
//! Each must give the same guarantees: the project writable, the home read-only with credentials hidden, a private
//! /tmp, the environment cleared to the allowlist, no network when it is off, the exit status carried through, and a
//! stopped command (timeout, cancel) really stopped. A backend that does not work here is skipped with a `SKIPPED`
//! line (`backends_support`; ULTRON_SANDBOX_TEST_REQUIRE makes it a failure).
//!
//! The scratch tree lives under the cargo target dir (not /tmp, so the read-only home is exercised):
//!   base/home              fake HOME: notes.txt, .ssh/id_test (hidden)
//!   base/home/project      the project
//! Every wait is on a condition, with a 120 s hang guard only.

#[path = "../../sandbox/tests/backends_support/mod.rs"]
mod backends_support;

use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use sandbox::launcher::Mechanism;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);

struct Tree {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::Builder::new()
        .prefix("host-backends-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test"), "SECRET-KEY\n").unwrap();
    std::fs::write(home.join("notes.txt"), "original\n").unwrap();
    Tree {
        _dir: dir,
        home,
        project,
    }
}

fn settings(network: bool) -> Value {
    json!({ "sandbox": {
        "mode": "required",
        "network": network,
        "image": backends_support::docker_image().ok(),
    } })
}

fn host(tree: &Tree, mechanism: Mechanism, network: bool) -> Host {
    let host = Host::new(
        &tree.project,
        tree.project.join(".jobs"),
        &tree.project,
        Box::new(AllowAll),
    )
    .sandboxed_with_mechanism(&settings(network), &tree.home, None, Some(mechanism));
    assert_eq!(host.launcher().unwrap().mechanism(), Some(mechanism));
    host
}

/// The backends this machine runs (probed with the bare tools, so a launcher bug fails instead of skipping).
fn backends() -> Vec<Mechanism> {
    backends_support::machine_backends()
        .into_iter()
        .filter_map(Mechanism::parse)
        .collect()
}

fn call(host: &Host, kind: &str, payload: Value) -> Value {
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    host.handle(kind, payload, &Signal::new())
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
}

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG;
    while !condition() {
        assert!(Instant::now() < deadline, "hang guard: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait until `tick` (rewritten every 50 ms while the loop lives) holds still for 400 ms: the loop is gone. A loop
/// that outlived its stop never holds still (hang guard 120 s).
fn wait_until_still(what: &str, tick: &Path) {
    let deadline = Instant::now() + HANG;
    loop {
        let before = std::fs::read_to_string(tick).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(400));
        if before == std::fs::read_to_string(tick).unwrap_or_default() {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: the command outlived its stop");
    }
}

const TICK: &str = "while :; do date +%s%N > tick; sleep 0.05; done";

/// Names the sandboxed environment may hold: the allowlist (sandbox::settings::DEFAULT_ENV, LC_*), TMPDIR, and what
/// the shell or the container runtime set themselves.
fn allowed(name: &str) -> bool {
    sandbox::settings::DEFAULT_ENV.contains(&name)
        || name.starts_with("LC_")
        || ["TMPDIR", "PWD", "OLDPWD", "SHLVL", "_", "HOSTNAME"].contains(&name)
}

#[test]
fn host_sandboxed_backends_bash_keeps_the_policy() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let host_tmp = std::env::temp_dir().join(format!("ultron-host-backends-{}-{name}", std::process::id()));
        std::fs::write(&host_tmp, "host\n").unwrap();
        let inside_tmp = format!("/tmp/ultron-host-backends-inside-{}-{name}", std::process::id());
        let command = format!(
            r#"echo in > in.txt
echo x > {home}/notes.txt
echo x > {home}/new.txt
cat {home}/.ssh/id_test 2>/dev/null
ls -A {home}/.ssh
[ -e {host_tmp} ] && echo host-tmp-visible
echo inside > {inside_tmp} && echo tmp-writable
echo "ENV-NAMES $(env | sed 's/=.*//' | tr '\n' ' ')"
exit 3"#,
            host_tmp = host_tmp.display(),
            home = tree.home.display(),
        );
        let result = call(&host(&tree, mechanism, false), "bash", json!({ "command": command }));
        let _ = std::fs::remove_file(&host_tmp);
        let output = result["output"].as_str().unwrap();
        assert_eq!(result["exit_code"], 3, "{name}: {result}");
        assert_eq!(
            std::fs::read_to_string(tree.project.join("in.txt")).unwrap_or_default(),
            "in\n",
            "{name}: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.home.join("notes.txt")).unwrap(),
            "original\n",
            "{name}"
        );
        assert!(!tree.home.join("new.txt").exists(), "{name}: {output}");
        assert!(
            !output.contains("SECRET") && !output.contains("id_test"),
            "{name}: {output}"
        );
        assert!(!output.contains("host-tmp-visible"), "{name}: {output}");
        assert!(
            output.contains("tmp-writable"),
            "{name}: /tmp is not usable inside: {output}"
        );
        assert!(
            !Path::new(&inside_tmp).exists(),
            "{name}: a write to /tmp reached the host's /tmp"
        );
        let names = output
            .lines()
            .find_map(|line| line.strip_prefix("ENV-NAMES "))
            .unwrap_or_else(|| panic!("{name}: {output}"));
        for variable in names.split_whitespace() {
            assert!(allowed(variable), "{name}: {variable} passed the allowlist: {names}");
        }
        assert!(names.split_whitespace().any(|v| v == "HOME"), "{name}: {names}");
    }
}

#[test]
fn host_sandboxed_backends_network_follows_the_setting() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let command = format!(
            "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then echo reachable; else echo unreachable; fi"
        );
        let off = call(&host(&tree, mechanism, false), "bash", json!({ "command": command }));
        assert_eq!(off["output"].as_str().unwrap().trim(), "unreachable", "{name}: {off}");
        // Docker's network is a bridge: the host's loopback is not the container's.
        if mechanism != Mechanism::Docker {
            let on = call(&host(&tree, mechanism, true), "bash", json!({ "command": command }));
            assert_eq!(on["output"].as_str().unwrap().trim(), "reachable", "{name}: {on}");
        }
    }
}

#[test]
fn host_sandboxed_backends_jobs_keep_the_policy_and_their_status() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let host = host(&tree, mechanism, false);
        // A command that lasts (`sleep 1`) runs to its end: nothing stops it early (under Docker, the watcher that
        // ends the container with its client must not read end-of-input at once).
        let job = call(
            &host,
            "shell.run",
            json!({ "command": "sleep 1; echo job > job.txt; (echo x > ../job-out.txt) 2>/dev/null && echo escaped || echo blocked; exit 4", "yield_after": 0 }),
        );
        let done = call(&host, "shell.result", json!({ "id": job["id"] }));
        assert_eq!(
            (&done["status"], &done["exit_code"], &done["text"]),
            (&json!("completed"), &json!(4), &json!("blocked\n")),
            "{name}: {done}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.project.join("job.txt")).unwrap(),
            "job\n",
            "{name}"
        );
        assert!(!tree.home.join("job-out.txt").exists(), "{name}");
    }
}

/// A command the host stops is stopped inside the sandbox too: a timed-out blocking bash and a cancelled job. Under
/// Docker the container is no process of the host's, and used to keep running after its client was killed.
#[test]
fn host_sandboxed_backends_stopped_commands_stop_inside_the_sandbox() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let host = host(&tree, mechanism, false);
        let tick = tree.project.join("tick");
        // A sandbox can take longer than a short timeout to start under load: grow the timeout until the loop ran
        // before it fired (the last step is the 120 s hang guard).
        let mut timed_out = json!(null);
        for timeout in [2, 8, 30, 120] {
            timed_out = call(&host, "bash", json!({ "command": TICK, "timeout": timeout }));
            assert_eq!(timed_out["timed_out"], true, "{name}: {timed_out}");
            if tick.exists() {
                break;
            }
        }
        assert!(tick.exists(), "{name}: the loop never ran: {timed_out}");
        wait_until_still(&format!("{name} timed-out bash"), &tick);

        std::fs::remove_file(&tick).unwrap();
        let job = call(&host, "shell.run", json!({ "command": TICK, "yield_after": 0 }));
        wait_for(&format!("{name}: the job's loop"), || tick.exists());
        let cancelled = call(&host, "shell.cancel", json!({ "id": job["id"] }));
        assert_eq!(cancelled["status"], "cancelled", "{name}: {cancelled}");
        wait_until_still(&format!("{name} cancelled job"), &tick);
    }
}

/// A worktree subagent's variables (`withShellEnv(operations, laneWorkspace(lane).env)`: `ULTRON_WORKTREE`,
/// `ULTRON_PARENT_REPO`, `ULTRON_WORKTREE_BRANCH`) reach its commands inside the sandbox, which clears the
/// environment to its allowlist: blocking bash and shell jobs. Docker is not run here (no real docker in tests); its
/// container receives the same `env NAME=VALUE ...` command line (`host_sandboxed_argv_sets_the_commands_variables`).
#[test]
fn host_sandboxed_backends_worktree_variables_reach_the_command() {
    for mechanism in backends().into_iter().filter(|m| *m != Mechanism::Docker) {
        let name = mechanism.name();
        let tree = tree();
        let worktree = vec![
            ("ULTRON_WORKTREE".to_string(), tree.project.display().to_string()),
            ("ULTRON_PARENT_REPO".to_string(), "/parent/repo".to_string()),
            ("ULTRON_WORKTREE_BRANCH".to_string(), "ultron/s1/child".to_string()),
        ];
        let host = host(&tree, mechanism, false).with_env(worktree.clone());
        let print = r#"printf '%s|%s|%s' "$ULTRON_WORKTREE" "$ULTRON_PARENT_REPO" "$ULTRON_WORKTREE_BRANCH""#;
        let expected = format!("{}|/parent/repo|ultron/s1/child", tree.project.display());
        let result = call(&host, "bash", json!({ "command": print }));
        assert_eq!(result["output"], json!(expected), "{name}: {result}");
        let job = call(&host, "shell.run", json!({ "command": print, "yield_after": 0 }));
        let done = call(&host, "shell.result", json!({ "id": job["id"] }));
        assert_eq!(done["text"], json!(expected), "{name}: {done}");
    }
}

#[test]
fn host_sandboxed_argv_sets_the_commands_variables() {
    let argv = vec!["bash".to_string(), "-c".into(), "echo hi".into()];
    assert_eq!(host::sandboxed_argv(argv.clone(), &[]), argv);
    let env = [
        ("ULTRON_WORKTREE".to_string(), "/w t".to_string()),
        ("PWD".to_string(), "/w t".to_string()),
    ];
    assert_eq!(
        host::sandboxed_argv(argv, &env),
        ["env", "ULTRON_WORKTREE=/w t", "PWD=/w t", "bash", "-c", "echo hi"]
    );
}
