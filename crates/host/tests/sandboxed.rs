//! A sandboxed Host serving the real runtime.py through crates/kernel: cells' shell commands (blocking bash and
//! shell jobs) run isolated, writes outside the writable paths are blocked, and nothing asks for approval. Isolation
//! cases are skipped (with the reason printed) when neither bwrap nor unshare works here.

use host::{AllowAll, Host};
use kernel::{Kernel, KernelOptions};
use sandbox::launcher::{Launcher, Mechanism, Policy};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

struct Tree {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

/// Under the cargo target dir (not /tmp), so the read-only home is exercised.
fn tree() -> Tree {
    let dir = tempfile::Builder::new()
        .prefix("host-sandboxed-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test"), "SECRET-KEY\n").unwrap();
    Tree {
        _dir: dir,
        home,
        project,
    }
}

fn isolation_available() -> bool {
    let tree = tree();
    let policy = Policy::new(&sandbox::Settings::default(), &tree.project, &tree.home, &[], None, &[]);
    let launcher = Launcher::new(sandbox::settings::Mode::Auto, policy);
    let found = launcher.mechanism();
    if found.is_none() {
        println!("skip: neither bwrap nor unshare works on this machine");
    }
    matches!(found, Some(Mechanism::Bwrap | Mechanism::Unshare))
}

/// Runs `cells` with a Host made sandboxed by `settings`; returns (stdout, result, error) per cell.
fn run(tree: &Tree, settings: Value, cells: &[&str]) -> Vec<Value> {
    let jobs = tree.project.join(".jobs");
    let host =
        Host::new(&tree.project, &jobs, &tree.project, Box::new(AllowAll)).sandboxed(&settings, &tree.home, None);
    let mut options = KernelOptions::new(&tree.project, runtime());
    options.env.push(("ULTRON_RLM_SPILL_DIR".into(), String::new()));
    options.env.push(("ULTRON_FILE_HOOKS".into(), "1".into()));
    let kernel = Kernel::new(options, Arc::new(host)).unwrap();
    let results = cells
        .iter()
        .map(|code| {
            let result = kernel.execute(code, None, None).unwrap();
            json!({
                "stdout": result.stdout,
                "result": result.result,
                "error": result.error.map(|e| format!("{}: {}", e.ename, e.evalue)),
            })
        })
        .collect();
    kernel.shutdown();
    results
}

fn error(result: &Value) -> &str {
    result["error"].as_str().unwrap_or_default()
}

#[test]
fn host_sandboxed_bash_and_jobs_cannot_write_the_home() {
    if !isolation_available() {
        return;
    }
    let tree = tree();
    let results = run(
        &tree,
        json!({ "sandbox": { "mode": "required", "network": false } }),
        &[
            "r = await bash('echo in > in.txt; echo out > ../out.txt; cat ../.ssh/id_test; cat in.txt'); print(r.output)",
            "j = await bash('sleep 0.3; echo job > job.txt; echo x > ../job-out.txt', yield_after=0); print((await j.result(wait=30)).status)",
        ],
    );
    assert_eq!(error(&results[0]), "", "{results:?}");
    let output = results[0]["stdout"].as_str().unwrap();
    assert!(output.contains("in\n"), "{output}");
    assert!(!output.contains("SECRET"), "{output}");
    assert!(
        output.contains("Read-only file system") || output.contains("Permission denied"),
        "{output}"
    );
    assert_eq!(error(&results[1]), "", "{results:?}");
    assert_eq!(results[1]["stdout"], "completed\n", "{results:?}");
    assert_eq!(std::fs::read_to_string(tree.project.join("in.txt")).unwrap(), "in\n");
    assert_eq!(std::fs::read_to_string(tree.project.join("job.txt")).unwrap(), "job\n");
    assert!(!tree.home.join("out.txt").exists());
    assert!(!tree.home.join("job-out.txt").exists());
}

/// sandbox.mode off keeps today's behavior: the command runs unisolated and writes where it likes.
#[test]
fn host_sandboxed_mode_off_runs_commands_as_before() {
    let tree = tree();
    let results = run(
        &tree,
        json!({ "sandbox": { "mode": "off" } }),
        &[
            "await bash('echo out > ../out.txt')",
            "await write('../outside.txt', 'x')",
        ],
    );
    assert_eq!(error(&results[0]), "", "{results:?}");
    assert_eq!(error(&results[1]), "", "{results:?}");
    assert!(tree.home.join("out.txt").exists());
    assert!(tree.home.join("outside.txt").exists());
}

#[test]
fn host_sandboxed_writes_outside_the_writable_paths_raise_in_the_cell() {
    let tree = tree();
    let results = run(
        &tree,
        json!({ "sandbox": { "mode": "auto" } }),
        &[
            "await write('inside.txt', 'ok')",
            "await write('../outside.txt', 'x')",
            "import os; os.symlink('..', 'up'); await write('up/via-link.txt', 'x')",
            "await write('../.ssh/id_test', 'x')",
        ],
    );
    assert_eq!(error(&results[0]), "", "{results:?}");
    assert!(tree.project.join("inside.txt").exists());
    for (index, result) in results.iter().enumerate().skip(1) {
        assert!(
            error(result).starts_with("ValueError: ") && error(result).contains("outside the sandbox's writable paths"),
            "cell {index}: {results:?}"
        );
    }
    assert!(!tree.home.join("outside.txt").exists());
    assert!(!tree.home.join("via-link.txt").exists());
    assert_eq!(
        std::fs::read_to_string(tree.home.join(".ssh/id_test")).unwrap(),
        "SECRET-KEY\n"
    );
}

/// There are no approvals (user decision, wave 40): `approval.*` settings that once denied or prompted bash and writes
/// are ignored, and every operation runs.
#[test]
fn host_sandboxed_approval_settings_are_ignored_and_every_operation_runs() {
    let tree = tree();
    let settings = json!({
        "sandbox": { "mode": "off" },
        "approval": {
            "mode": "always-ask",
            "ops": { "bash": "deny", "write": "deny" },
            "bash": { "patterns": [{ "match": "echo blocked*", "approval": "deny" }] },
        },
    });
    let results = run(
        &tree,
        settings,
        &[
            "print((await bash('echo blocked')).output.strip())",
            "print((await bash('true || rm -rf /nonexistent-ultron-path; echo ran')).output.strip())",
            "j = await bash('echo job > job.txt', yield_after=0); print((await j.result(wait=30)).status)",
            "await write('a.txt', 'x')",
        ],
    );
    let stdout: Vec<&str> = results
        .iter()
        .map(|r| r["stdout"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(stdout, ["blocked\n", "ran\n", "completed\n", ""], "{results:?}");
    assert!(results.iter().all(|r| error(r).is_empty()), "{results:?}");
    assert_eq!(std::fs::read_to_string(tree.project.join("a.txt")).unwrap(), "x");
    assert_eq!(std::fs::read_to_string(tree.project.join("job.txt")).unwrap(), "job\n");
}
