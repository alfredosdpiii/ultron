//! The whole-tree memory cap (tree-memory.ts, kernel.ts). Pure functions replay crates/kernel/fixtures/tree-memory.json
//! (recorded from the TypeScript module by fixtures/generate-tree-memory.mjs); the kernel tests port
//! packages/coding-agent/test/ultron-kernel-tree-memory.test.ts with smaller allocations. The cgroup backend runs
//! under a fake `systemd-run` (it records its arguments and execs the command after `--`), never the real one.

use kernel::tree_memory::{
    cgroup_oom_kills, cgroup_pids, cgroup_scope_command, kernel_tree_memory_limit, tree_memory_backend,
    tree_memory_limit_message, watchdog_poll_ms,
};
use kernel::{
    HostHandler, Kernel, KernelOptions, LimitOverrides, Signal, Status, TreeMemoryBackend, TreeMemoryOptions,
};
use serde_json::{Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HANG_GUARD: Duration = Duration::from_secs(120);

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tree-memory.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn null_handler() -> Arc<dyn HostHandler> {
    Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null))
}

fn options(backend: &str, cap_mb: u64) -> KernelOptions {
    let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
    options.limits = LimitOverrides {
        max_memory_mb: Some(256),
        max_cpu_seconds: Some(0),
    };
    options.tree_memory = Some(TreeMemoryOptions {
        max_mb: Some(cap_mb),
        backend: Some(backend.into()),
        ..TreeMemoryOptions::default()
    });
    options
}

fn kernel(backend: &str, cap_mb: u64) -> Kernel {
    Kernel::new(options(backend, cap_mb), null_handler()).unwrap()
}

fn value(kernel: &Kernel, code: &str) -> String {
    let result = kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"));
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

fn stat(pid: i32) -> Option<(String, i32)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &text[text.rfind(')')? + 2..];
    let mut fields = rest.split(' ');
    Some((fields.next()?.to_string(), fields.next()?.parse().ok()?))
}

/// Whether `pid` ends (or is a zombie awaiting its reaper) within the hang guard.
fn gone(pid: i32) -> bool {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        match stat(pid) {
            None => return true,
            Some((state, _)) if state == "Z" => return true,
            _ if Instant::now() >= deadline => return false,
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The pid a double-forked daemon wrote, once it is there.
fn daemon_pid(file: &Path) -> i32 {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        if let Some(pid) = std::fs::read_to_string(file).ok().and_then(|text| text.parse().ok()) {
            return pid;
        }
        assert!(Instant::now() < deadline, "the daemon never wrote its pid");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kills an escaped daemon if a test fails before the kernel did: only a process whose command line names this
/// test's own pid file (a reused pid is never signalled).
struct Reaper(i32, PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        let marker = self.1.to_string_lossy().into_owned();
        let ours = std::fs::read(format!("/proc/{}/cmdline", self.0))
            .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(&marker));
        if ours && self.0 > 1 {
            let _ = std::process::Command::new("kill")
                .args(["-9", &self.0.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// Cell code that starts `code` as a daemon escaping the cell's process tree the classic way (fork, setsid, fork,
/// both intermediates exit); the daemon writes its pid to `pid_file`.
fn double_fork(pid_file: &Path, code: &str) -> String {
    let daemon = format!(
        "import os\nopen({:?}, 'w').write(str(os.getpid()))\n{code}",
        pid_file.to_str().unwrap()
    );
    let launcher = [
        "import os, sys".to_string(),
        "if os.fork() == 0:".into(),
        "    os.setsid()".into(),
        "    if os.fork() == 0:".into(),
        format!(
            "        os.execv(sys.executable, [sys.executable, '-c', {}])",
            json(&daemon)
        ),
        "    os._exit(0)".into(),
        "os.wait()".into(),
    ]
    .join("\n");
    format!(
        "import subprocess, sys\nsubprocess.run([sys.executable, '-c', {}], check=True)",
        json(&launcher)
    )
}

fn json(text: &str) -> String {
    Value::String(text.into()).to_string()
}

/// `count` subprocesses that each touch `mb` MiB (under the per-process limit) and then wait.
fn hogs(count: u32, mb: u32) -> String {
    [
        "import subprocess, sys, time".to_string(),
        format!("code = \"import time\\nb = b'x' * ({mb} * 1024 * 1024)\\ntime.sleep(60)\""),
        format!("procs = [subprocess.Popen([sys.executable, '-c', code]) for _ in range({count})]"),
        "for p in procs: p.wait()".into(),
        "'finished'".into(),
    ]
    .join("\n")
}

#[test]
fn kernel_tree_memory_configuration_matches_typescript() {
    let fixture = fixture();
    for case in fixture["limits"].as_array().unwrap() {
        let env = case["env"]["ULTRON_RLM_MAX_TREE_MEMORY_MB"].as_str();
        let got = kernel_tree_memory_limit(env, case["perProcess"].as_u64().unwrap(), case["override"].as_u64());
        let got = match got {
            Ok(value) => serde_json::json!({ "ok": value }),
            Err(error) => serde_json::json!({ "error": error.message() }),
        };
        assert_eq!(got, case["result"], "{case}");
    }
    for case in fixture["backends"].as_array().unwrap() {
        // These choices never probe: a probe here would be a bug.
        let got = tree_memory_backend(case["cap"].as_u64().unwrap(), case["requested"].as_str(), || {
            panic!("probed for {case}")
        });
        assert_eq!(got.name(), case["result"], "{case}");
    }
    for case in fixture["scopeCommand"].as_array().unwrap() {
        assert_eq!(
            serde_json::json!(cgroup_scope_command(case["cap"].as_u64().unwrap())),
            case["command"]
        );
    }
    for case in fixture["polls"].as_array().unwrap() {
        let got = watchdog_poll_ms(case["bytes"].as_u64().unwrap(), case["cap"].as_u64().unwrap());
        assert_eq!(got, case["result"].as_u64().unwrap(), "{case}");
    }
    // Choices that may probe: "cgroup", "auto" and anything else take the scope only when the probe allows one.
    assert_eq!(
        tree_memory_backend(100, Some("cgroup"), || true),
        TreeMemoryBackend::Cgroup
    );
    assert_eq!(tree_memory_backend(100, None, || true), TreeMemoryBackend::Cgroup);
    assert_eq!(
        tree_memory_backend(100, Some("bogus"), || false),
        TreeMemoryBackend::Watchdog
    );
    assert_eq!(
        tree_memory_backend(100, Some(" Auto "), || false),
        TreeMemoryBackend::Watchdog
    );
}

#[test]
fn kernel_tree_memory_cgroup_files_match_typescript() {
    let dir = tempfile::tempdir().unwrap();
    for case in fixture()["cgroups"].as_array().unwrap() {
        let path = dir.path().join(case["name"].as_str().unwrap());
        if case["name"] == "pids" {
            std::fs::create_dir_all(path.join("child/grandchild")).unwrap();
            std::fs::write(path.join("cgroup.procs"), "12\n34\nx\n\n56").unwrap();
            std::fs::write(path.join("child/cgroup.procs"), "78\n").unwrap();
            std::fs::write(path.join("child/grandchild/cgroup.procs"), "90\n").unwrap();
            let mut pids = cgroup_pids(&path);
            pids.sort_unstable();
            assert_eq!(serde_json::json!(pids), case["pids"]);
            assert_eq!(serde_json::json!(cgroup_pids(&dir.path().join("gone"))), case["gone"]);
            continue;
        }
        if let Some(events) = case["events"].as_str() {
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("memory.events"), events).unwrap();
        }
        assert_eq!(serde_json::json!(cgroup_oom_kills(&path)), case["kills"], "{case}");
    }
}

#[test]
fn kernel_tree_memory_limit_message_names_the_cap_and_how_it_is_enforced() {
    // kernel.ts `treeMemoryLimitMessage`, literally.
    assert_eq!(
        tree_memory_limit_message(320, TreeMemoryBackend::Watchdog),
        "RLM kernel process tree exceeded its memory limit (320 MiB total across the kernel and every process its cells started, ULTRON_RLM_MAX_TREE_MEMORY_MB, enforced by host watchdog) and was stopped; the next cell starts a fresh kernel without earlier Python state"
    );
    assert!(tree_memory_limit_message(64, TreeMemoryBackend::Cgroup).contains("(64 MiB total"));
    assert!(tree_memory_limit_message(64, TreeMemoryBackend::Cgroup).contains("enforced by cgroup MemoryMax)"));
}

#[test]
fn kernel_tree_memory_watchdog_stops_a_tree_over_the_cap_and_the_next_cell_works() {
    let kernel = kernel("watchdog", 160);
    assert_eq!(value(&kernel, "kept = 1\n'ok'"), "'ok'");
    assert_eq!(kernel.tree_memory_backend(), Some(TreeMemoryBackend::Watchdog));
    let (bytes, cap) = kernel.memory_usage().unwrap();
    assert!(bytes > 0);
    assert_eq!(cap, Some(160 * 1024 * 1024));
    // Each child stays under the 256 MiB per-process limit; three of them exceed 160 MiB together.
    let error = kernel
        .execute(&hogs(3, 70), None, None)
        .expect_err("the tree was not stopped");
    assert_eq!(
        error.message(),
        tree_memory_limit_message(160, TreeMemoryBackend::Watchdog)
    );
    // The whole tree is gone and a fresh kernel serves the next cell.
    assert_eq!(value(&kernel, "('kept' in globals(), 6 * 7)"), "(False, 42)");
}

#[test]
fn kernel_tree_memory_watchdog_leaves_modest_cells_alone() {
    let kernel = kernel("watchdog", 256);
    let code = [
        "import subprocess, sys",
        "data = bytearray(60 * 1024 * 1024)",
        "out = subprocess.run([sys.executable, '-c', \"b = b'x' * (60 * 1024 * 1024); print(len(b))\"], capture_output=True, text=True)",
        "(len(data) // 2**20, out.stdout.strip())",
    ]
    .join("\n");
    assert_eq!(value(&kernel, &code), format!("(60, '{}')", 60 * 1024 * 1024));
    assert_eq!(value(&kernel, "1 + 1"), "2");
}

#[test]
fn kernel_tree_memory_watchdog_counts_and_kills_a_double_forked_hog() {
    let kernel = kernel("watchdog", 120);
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("daemon.pid");
    // 130 MiB stays under the per-process limit but exceeds the 120 MiB cap on its own: it is caught only if the
    // escaped daemon is still counted as part of the tree.
    let cell = format!(
        "{}\nimport time\ntime.sleep(60)\n'finished'",
        double_fork(&pid_file, "import time\nb = b'x' * (130 * 1024 * 1024)\ntime.sleep(60)")
    );
    let error = kernel.execute(&cell, None, None).expect_err("the tree was not stopped");
    assert_eq!(
        error.message(),
        tree_memory_limit_message(120, TreeMemoryBackend::Watchdog)
    );
    let pid = daemon_pid(&pid_file);
    let _reaper = Reaper(pid, pid_file.clone());
    assert!(gone(pid), "the escaped daemon outlived the tree");
    assert_eq!(value(&kernel, "1 + 1"), "2");
}

#[test]
fn kernel_tree_memory_double_forked_daemon_is_adopted_and_stopped_with_the_kernel() {
    let kernel = kernel("watchdog", 512);
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("daemon.pid");
    let cell = format!("{}\n'started'", double_fork(&pid_file, "import time\ntime.sleep(60)"));
    assert_eq!(value(&kernel, &cell), "'started'");
    let pid = daemon_pid(&pid_file);
    let _reaper = Reaper(pid, pid_file.clone());
    // The kernel is a child subreaper, so the orphaned daemon is its child rather than init's.
    let kernel_pid = value(&kernel, "__import__('os').getpid()");
    assert_eq!(kernel_pid, stat(pid).unwrap().1.to_string());
    kernel.shutdown();
    assert!(gone(pid));
}

#[test]
fn kernel_tree_memory_copy_on_write_pages_count_once() {
    let kernel = kernel("watchdog", 160);
    // One 100 MiB buffer shared by a parent and four forked children: 500 MiB resident summed per process, 100 MiB
    // actually used, within the 160 MiB cap.
    let worker = [
        "import os, time",
        "b = bytearray(100 * 1024 * 1024)",
        "for i in range(0, len(b), 4096): b[i] = 1",
        "kids = []",
        "for _ in range(4):",
        "    pid = os.fork()",
        "    if pid == 0:",
        "        time.sleep(3)",
        "        os._exit(0)",
        "    kids.append(pid)",
        "for pid in kids: os.waitpid(pid, 0)",
        "print('done')",
    ]
    .join("\n");
    let code = format!(
        "import subprocess, sys\nout = subprocess.run([sys.executable, '-c', {}], capture_output=True, text=True)\n(out.returncode, out.stdout.strip())",
        json(&worker)
    );
    assert_eq!(value(&kernel, &code), "(0, 'done')");
}

/// A fake `systemd-run`: records its arguments (one per line, `--end--` after each call) and execs the command after
/// `--`; the probe's `/bin/sh -c cat .../memory.max` is answered with `memory_max`.
fn fake_launcher(dir: &Path, memory_max: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("launcher.log");
    let script = dir.join("systemd-run");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{log}'; done\necho --end-- >> '{log}'\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift\nif [ \"$1\" = /bin/sh ]; then echo {memory_max}; exit 0; fi\nexec \"$@\"\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn calls(log: &Path) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.split("--end--\n")
        .filter(|call| !call.is_empty())
        .map(|call| call.lines().map(str::to_string).collect())
        .collect()
}

#[test]
fn kernel_tree_memory_cgroup_scope_wraps_the_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let (launcher, log) = fake_launcher(dir.path(), "67108864");
    let mut options = options("auto", 320);
    options.tree_memory.as_mut().unwrap().launcher = launcher;
    let python = kernel_python();
    options.python = Some(python.clone().into());
    let kernel = Kernel::new(options, null_handler()).unwrap();
    assert_eq!(value(&kernel, "6 * 7"), "42");
    assert_eq!(kernel.tree_memory_backend(), Some(TreeMemoryBackend::Cgroup));
    let recorded = calls(&log);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    // The probe: a 64 MiB scope that reads back its own memory.max.
    let mut probe = cgroup_scope_command(64)[1..].to_vec();
    probe.extend([
        "/bin/sh".into(),
        "-c".into(),
        r#"cat "/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.max""#.into(),
    ]);
    assert_eq!(recorded[0], probe);
    // The kernel itself runs inside a scope capped at the tree limit.
    let mut kernel_call = cgroup_scope_command(320)[1..].to_vec();
    kernel_call.extend([
        python,
        runtime().to_string_lossy().into_owned(),
        "--protocol-fds".into(),
    ]);
    assert_eq!(recorded[1], kernel_call);
    // A second kernel with the same launcher does not probe again.
    let mut options = options_with_launcher(&dir.path().join("systemd-run"));
    options.python = Some(kernel_python().into());
    let second = Kernel::new(options, null_handler()).unwrap();
    assert_eq!(value(&second, "1"), "1");
    assert_eq!(calls(&log).len(), 3);
}

fn options_with_launcher(launcher: &Path) -> KernelOptions {
    let mut options = options("cgroup", 64);
    options.tree_memory.as_mut().unwrap().launcher = launcher.to_path_buf();
    options
}

fn kernel_python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

#[test]
fn kernel_tree_memory_without_a_usable_scope_falls_back_to_the_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    // The probe's scope did not get the cap (no delegated memory controller): no scope for the kernel.
    let (launcher, log) = fake_launcher(dir.path(), "max");
    let kernel = Kernel::new(options_with_launcher(&launcher), null_handler()).unwrap();
    assert_eq!(value(&kernel, "1 + 1"), "2");
    assert_eq!(kernel.tree_memory_backend(), Some(TreeMemoryBackend::Watchdog));
    assert_eq!(calls(&log).len(), 1, "only the probe ran");
    // A launcher that does not exist is no scope either.
    let kernel = Kernel::new(options_with_launcher(&dir.path().join("missing")), null_handler()).unwrap();
    assert_eq!(value(&kernel, "1 + 1"), "2");
    assert_eq!(kernel.tree_memory_backend(), Some(TreeMemoryBackend::Watchdog));
}

#[test]
fn kernel_tree_memory_is_off_without_options_or_with_a_zero_cap() {
    let plain = Kernel::new(
        KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime()),
        null_handler(),
    )
    .unwrap();
    assert_eq!(value(&plain, "1"), "1");
    assert_eq!(plain.tree_memory_backend(), Some(TreeMemoryBackend::Off));
    assert_eq!(plain.memory_usage().unwrap().1, None);
    let zero = kernel("cgroup", 0);
    assert_eq!(value(&zero, "1"), "1");
    assert_eq!(zero.tree_memory_backend(), Some(TreeMemoryBackend::Off));
    assert_eq!(zero.memory_usage().unwrap().1, None);
    zero.shutdown();
    assert_eq!(zero.memory_usage(), None);
}
