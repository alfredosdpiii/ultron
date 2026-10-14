//! The kernel process inside the session's sandbox, with the real runtime.py, under every backend that works on this
//! machine (one that does not is skipped with the reason printed). Docker cannot carry the protocol descriptors and
//! is checked only for how it is refused.
//!
//! The scratch tree lives under the cargo target dir (inside /home on a usual checkout), never the real home:
//!   base/fakehome                      fake HOME: notes.txt, .ssh/id_test (hidden), .ultron (hidden agent dir)
//!   base/fakehome/.ultron/sessions/s1  the session dir (snapshots), writable although inside the hidden .ultron
//!   base/fakehome/.ultron/spill        the spill dir (ULTRON_RLM_SPILL_DIR), writable likewise
//!   base/fakehome/work/project         the project (writable, the kernel's cwd)
//!   base/outside                       outside every writable path

use kernel::{ContinuityPolicy, ExecutionResult, HostHandler, Kernel, KernelOptions, LimitOverrides, Signal, Status};
use sandbox::launcher::{Launcher, Mechanism, Policy};
use sandbox::settings::{Mode, Settings};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEY: [u8; 32] = [7; 32];

fn runtime() -> PathBuf {
    std::fs::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py"),
    )
    .unwrap()
}

struct Tree {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    outside: PathBuf,
    session: PathBuf,
    spill: PathBuf,
}

fn tree() -> Tree {
    tree_in(Path::new(env!("CARGO_TARGET_TMPDIR")))
}

fn tree_in(base: &Path) -> Tree {
    let dir = tempfile::Builder::new()
        .prefix("kernel-sandbox-")
        .tempdir_in(base)
        .unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("fakehome");
    let project = home.join("work/project");
    let outside = base.join("outside");
    let session = home.join(".ultron/sessions/s1");
    for path in [&project, &outside, &home.join(".ssh"), &session] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(home.join("notes.txt"), "original\n").unwrap();
    std::fs::write(home.join(".ssh/id_test"), "SECRET-KEY-MATERIAL\n").unwrap();
    std::fs::write(home.join(".ultron/auth.json"), "SECRET-AUTH\n").unwrap();
    Tree {
        _dir: dir,
        spill: home.join(".ultron/spill"),
        home,
        project,
        outside,
        session,
    }
}

fn policy(tree: &Tree) -> Policy {
    let settings = Settings {
        mode: Mode::Required,
        writable: Vec::new(),
        readable: Vec::new(),
        hidden: vec!["~/.ssh".into(), "~/.ultron".into()],
        network: false,
        env: Vec::new(),
        image: None,
    };
    let host_env: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), tree.home.to_string_lossy().into_owned()),
        ("SECRET_TOKEN".into(), "leaked-token".into()),
    ];
    // The bash policy as the host builds it; the kernel adds its snapshot and spill dirs itself.
    Policy::new(&settings, &tree.project, &tree.home, &[], None, &host_env)
}

fn launcher(tree: &Tree, mode: Mode, mechanism: Option<Mechanism>) -> Arc<Launcher> {
    Arc::new(Launcher::new(mode, policy(tree)).with_mechanism(mechanism))
}

/// The backends that work here; prints why the others are skipped.
fn backends() -> Vec<Mechanism> {
    let tree = tree();
    let mut out = Vec::new();
    for mechanism in [Mechanism::Bwrap, Mechanism::Unshare] {
        if Launcher::new(Mode::Required, policy(&tree)).probe(mechanism) {
            out.push(mechanism);
        } else {
            println!(
                "skip {}: not installed or not permitted on this machine",
                mechanism.name()
            );
        }
    }
    println!("skip docker: it cannot pass the kernel's protocol descriptors (refusal checked separately)");
    out
}

/// Runs `bash -c command` on the host for the kernel's `bash()` (the host side of the request, unsandboxed here:
/// what matters is that the request crosses the protocol).
fn host_handler(cwd: PathBuf) -> Arc<dyn HostHandler> {
    Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
        if kind != "bash" {
            return Err(format!("unexpected {kind}"));
        }
        let command = payload.get("command").and_then(Value::as_str).unwrap_or_default();
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "output": String::from_utf8_lossy(&output.stdout),
            "exit_code": output.status.code().unwrap_or(-1),
        }))
    })
}

fn options(tree: &Tree, launcher: Arc<Launcher>) -> KernelOptions {
    let mut options = KernelOptions::new(&tree.project, runtime());
    options.sandbox = Some(launcher);
    options.snapshot_key = Some(KEY.to_vec());
    options.snapshot_path = Some(tree.session.join("main.snapshot"));
    options.env = vec![("ULTRON_RLM_SPILL_DIR".into(), tree.spill.display().to_string())];
    options
}

fn sandboxed(tree: &Tree, mechanism: Mechanism) -> Kernel {
    let options = options(tree, launcher(tree, Mode::Required, Some(mechanism)));
    Kernel::new(options, host_handler(tree.project.clone())).unwrap()
}

fn run(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"))
}

fn value(kernel: &Kernel, code: &str) -> String {
    let result = run(kernel, code);
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

/// A hang guard, never a timing expectation: every wait in these tests is for a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}: not reached in {HANG_GUARD:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

/// Live descendants of `root` (host pids), breadth first.
fn descendants(root: i32) -> Vec<i32> {
    let mut parents: Vec<(i32, i32)> = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
        if let (Some(state), Some(ppid)) = (fields.first(), fields.get(1).and_then(|p| p.parse().ok()))
            && *state != "Z"
        {
            parents.push((pid, ppid));
        }
    }
    let mut out = Vec::new();
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for (pid, _) in parents.iter().filter(|(_, ppid)| *ppid == parent) {
            out.push(*pid);
            queue.push(*pid);
        }
    }
    out
}

/// The Python process of a sandboxed kernel (host pid): `kernel.pid()` is the launcher.
fn python_pid(kernel: &Kernel) -> i32 {
    let root = i32::try_from(kernel.pid().unwrap()).unwrap();
    let runtime = runtime();
    descendants(root)
        .into_iter()
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                cmdline.split(|byte| *byte == 0).nth(1) == Some(runtime.as_os_str().as_encoded_bytes())
            })
        })
        .expect("the sandboxed Python process")
}

fn kill(pid: i32) {
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
}

#[test]
fn kernel_sandbox_contains_direct_writes_and_reads_of_a_cell() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let kernel = sandboxed(&tree, mechanism);
        let host_tmp = PathBuf::from(format!("/tmp/kernel-sandbox-{}-{name}.txt", std::process::id()));
        let code = format!(
            r#"import os
def attempt(path, text='x'):
    try:
        with open(path, 'w') as handle:
            handle.write(text)
        return 'wrote'
    except OSError as error:
        return type(error).__name__
def read(path):
    try:
        with open(path) as handle:
            return handle.read()
    except OSError as error:
        return type(error).__name__
results = {{
    'project': attempt('inproject.txt', 'in'),
    'home_new': attempt(os.path.expanduser('~/new.txt')),
    'home_existing': attempt(os.path.expanduser('~/notes.txt'), 'clobbered'),
    'outside': attempt({outside:?}),
    'host_tmp': attempt({host_tmp:?}),
    'private_tmp': attempt('/tmp/private.txt'),
    'ssh': read(os.path.expanduser('~/.ssh/id_test')),
    'auth': read(os.path.expanduser('~/.ultron/auth.json')),
    'secret_env': os.environ.get('SECRET_TOKEN'),
    'home_env': os.environ.get('HOME'),
}}
results"#,
            outside = tree.outside.join("direct.txt").display().to_string(),
            host_tmp = host_tmp.display().to_string(),
        );
        let result = value(&kernel, &code);
        let leaked_tmp = host_tmp.exists();
        let _ = std::fs::remove_file(&host_tmp);
        assert_eq!(kernel.sandbox(), Some(mechanism), "{name}");
        assert!(result.contains("'project': 'wrote'"), "{name}: {result}");
        assert!(result.contains("'private_tmp': 'wrote'"), "{name}: {result}");
        assert!(!result.contains("SECRET"), "{name}: credentials readable: {result}");
        assert!(result.contains("'secret_env': None"), "{name}: {result}");
        assert!(
            result.contains(&format!("'home_env': '{}'", tree.home.display())),
            "{name}: {result}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.project.join("inproject.txt")).unwrap(),
            "in",
            "{name}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.home.join("notes.txt")).unwrap(),
            "original\n",
            "{name}: {result}"
        );
        assert!(!tree.home.join("new.txt").exists(), "{name}: {result}");
        assert!(!tree.outside.join("direct.txt").exists(), "{name}: {result}");
        assert!(!leaked_tmp, "{name}: a cell's write reached the host /tmp: {result}");
        for key in ["home_new", "home_existing", "outside"] {
            assert!(
                !result.contains(&format!("'{key}': 'wrote'")),
                "{name}: {key}: {result}"
            );
        }
    }
}

#[test]
fn kernel_sandbox_host_requests_reach_the_host_and_raw_output_is_captured() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let kernel = sandboxed(&tree, mechanism);
        // The host runs the command outside the sandbox: it can write the home the cell cannot.
        let marker = tree.home.join("from-host.txt");
        let result = run(
            &kernel,
            &format!(
                "import os\nout = await bash('echo host-side > {} && echo done')\nprint('printed')\nos.write(1, b'raw1\\n')\nos.write(2, b'raw2\\n')\nstr(out), out.exit_code",
                marker.display()
            ),
        );
        assert_eq!(result.status, Status::Ok, "{name}: {result:?}");
        assert_eq!(result.result.as_deref(), Some("('done', 0)"), "{name}");
        assert_eq!(result.stdout, "printed\nraw1\n", "{name}");
        assert_eq!(result.stderr, "raw2\n", "{name}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "host-side\n", "{name}");
        // A cut output is spilled whole into the spill dir inside the hidden agent dir.
        let long = run(&kernel, "print('y' * 100_000)");
        assert_eq!(long.status, Status::Ok, "{name}");
        let spilled: Vec<_> = std::fs::read_dir(&tree.spill).unwrap().flatten().collect();
        assert_eq!(spilled.len(), 1, "{name}: {}", long.stdout.len());
        assert_eq!(
            std::fs::read_to_string(spilled[0].path()).unwrap(),
            format!("{}\n", "y".repeat(100_000)),
            "{name}"
        );
    }
}

#[test]
fn kernel_sandbox_snapshot_restore_and_continuity_work_in_the_session_dir() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let snapshot = tree.session.join("main.snapshot");
        let writer = sandboxed(&tree, mechanism);
        value(&writer, "saved = {'rows': [1, 2]}");
        let reply = writer.snapshot(None).unwrap();
        assert_eq!(reply.result.status, Status::Ok, "{name}: {reply:?}");
        assert!(snapshot.exists(), "{name}");
        writer.shutdown();
        let reader = sandboxed(&tree, mechanism);
        assert_eq!(value(&reader, "saved"), "{'rows': [1, 2]}", "{name}");
        reader.shutdown();

        // Continuity: the Python process is killed; the supervisor restarts it from its exit-waiting thread and
        // restores the checkpoint. The restarted kernel outlives that thread (the parent-death signal follows the
        // spawning thread).
        let mut options = options(&tree, launcher(&tree, Mode::Required, Some(mechanism)));
        options.continuity = Some(ContinuityPolicy::default());
        let kernel = Kernel::new(options, host_handler(tree.project.clone())).unwrap();
        value(&kernel, "total = 41");
        wait_until("checkpoint", || {
            kernel
                .checkpoint()
                .is_some_and(|checkpoint| checkpoint.saved.iter().any(|saved| saved == "total"))
        });
        let old = kernel.pid().unwrap();
        kill(python_pid(&kernel));
        wait_until("restart", || {
            kernel.is_running() && kernel.pid().is_some_and(|pid| pid != old)
        });
        let restarted = kernel.pid().unwrap();
        // Settle, then check nothing happened: a late death could only let this check pass wrongly, never fail.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            kernel.pid(),
            Some(restarted),
            "{name}: the restarted kernel died with its spawning thread"
        );
        let reply = kernel.execute_cell("total + 1", None, None).unwrap();
        assert_eq!(reply.result.result.as_deref(), Some("42"), "{name}: {reply:?}");
        let recovery = reply.recovery.expect("recovery report");
        assert!(
            recovery.reason.starts_with("RLM kernel exited (SIGKILL)"),
            "{name}: {}",
            recovery.reason
        );
        // The checkpoint holds the earlier session's restored names too.
        assert_eq!(recovery.restored, ["saved", "state", "total"], "{name}");
    }
}

#[test]
fn kernel_sandbox_limits_and_exits_are_reported_as_unsandboxed() {
    for mechanism in backends() {
        let name = mechanism.name();
        let tree = tree();
        let mut options = options(&tree, launcher(&tree, Mode::Required, Some(mechanism)));
        options.limits = LimitOverrides {
            max_memory_mb: Some(256),
            max_cpu_seconds: Some(1),
        };
        let kernel = Arc::new(Kernel::new(options, host_handler(tree.project.clone())).unwrap());
        let error = kernel.execute("import os\nos._exit(86)", None, None).unwrap_err();
        assert!(
            error
                .message()
                .starts_with("RLM kernel exceeded its memory limit (256 MiB per process"),
            "{name}: {error}"
        );
        // The CPU backstop measures the sandboxed Python process, not the launcher.
        let running = kernel.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(running.execute("sum(range(10 ** 12))", None, None));
        });
        let outcome = receiver.recv_timeout(HANG_GUARD).unwrap_or_else(|_| {
            kernel.shutdown();
            panic!("{name}: no CPU backstop")
        });
        assert!(
            outcome
                .unwrap_err()
                .message()
                .starts_with("RLM kernel exceeded its CPU limit (1 CPU-seconds per cell"),
            "{name}"
        );
        assert_eq!(value(&kernel, "2 + 2"), "4", "{name}");
        // A kill from the host is reported by signal, through the launcher's exit status.
        let old = kernel.pid().unwrap();
        let entered = tree.project.join("entered");
        let (sender, receiver) = std::sync::mpsc::channel();
        let running = kernel.clone();
        let code = format!(
            "open({:?}, 'w').close()\nimport time\ntime.sleep(3600)",
            entered.display().to_string()
        );
        std::thread::spawn(move || {
            let _ = sender.send(running.execute(&code, None, None));
        });
        // The cell is running in Python before its process is killed.
        wait_until("the cell runs", || entered.exists());
        assert_eq!(kernel.pid(), Some(old), "{name}");
        kill(python_pid(&kernel));
        let error = receiver.recv_timeout(HANG_GUARD).unwrap().unwrap_err();
        assert!(
            error.message().starts_with("RLM kernel exited (SIGKILL)"),
            "{name}: {error}"
        );
    }
}

#[test]
fn kernel_sandbox_docker_off_and_missing_mechanisms() {
    let tree = tree();
    // Docker cannot pass the protocol descriptors: required refuses, auto runs unsandboxed.
    let required = Kernel::new(
        options(&tree, launcher(&tree, Mode::Required, Some(Mechanism::Docker))),
        host_handler(tree.project.clone()),
    )
    .unwrap();
    let error = required.execute("1", None, None).unwrap_err();
    assert_eq!(
        error.message(),
        "sandbox.mode is required but no sandbox can host the RLM kernel here (install bubblewrap or allow unprivileged user namespaces; Docker cannot pass the kernel's protocol descriptors)"
    );
    let nothing = Kernel::new(
        options(&tree, launcher(&tree, Mode::Required, None)),
        host_handler(tree.project.clone()),
    )
    .unwrap();
    assert_eq!(nothing.execute("1", None, None).unwrap_err(), error);
    for (mode, mechanism) in [
        (Mode::Auto, Some(Mechanism::Docker)),
        (Mode::Auto, None),
        (Mode::Off, Some(Mechanism::Bwrap)),
    ] {
        let kernel = Kernel::new(
            options(&tree, launcher(&tree, mode, mechanism)),
            host_handler(tree.project.clone()),
        )
        .unwrap();
        assert_eq!(value(&kernel, "6 * 7"), "42", "{mode:?} {mechanism:?}");
        assert_eq!(kernel.sandbox(), None, "{mode:?} {mechanism:?}");
    }
}

const HELPER_ENV: &str = "KERNEL_SANDBOX_HELPER_MECHANISM";
const HELPER_BASE_ENV: &str = "KERNEL_SANDBOX_HELPER_BASE";

/// Not a test on its own: the "host" process of `kernel_sandbox_kernel_dies_with_its_host`. Starts a sandboxed
/// kernel, prints its processes, then dies by SIGKILL (no cleanup runs).
#[test]
fn kernel_sandbox_helper_host() {
    let Some(mechanism) = std::env::var(HELPER_ENV).ok().and_then(|name| Mechanism::parse(&name)) else {
        return;
    };
    // The helper's tree lives in the test's temp dir, which the test removes.
    let tree = tree_in(Path::new(&std::env::var(HELPER_BASE_ENV).unwrap()));
    let kernel = sandboxed(&tree, mechanism);
    // Started from a thread that ends at once: only the host process's death may end the kernel.
    std::thread::scope(|scope| {
        scope.spawn(|| kernel.start().unwrap());
    });
    let started = kernel.pid();
    // Settle, then check the kernel started by the ended thread still serves (a late death could only make the
    // outer test pass wrongly, never fail).
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(value(&kernel, "6 * 7"), "42");
    assert_eq!(kernel.pid(), started, "the kernel died with its spawning thread");
    let root = i32::try_from(kernel.pid().unwrap()).unwrap();
    let mut pids = vec![root];
    pids.extend(descendants(root));
    let list: Vec<String> = pids.iter().map(i32::to_string).collect();
    println!("KERNEL-PIDS {}", list.join(" "));
    std::mem::forget(tree);
    kill(std::process::id().cast_signed());
}

#[test]
fn kernel_sandbox_kernel_dies_with_its_host() {
    for mechanism in backends() {
        let name = mechanism.name();
        let base = tempfile::Builder::new()
            .prefix("kernel-sandbox-host-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kernel_sandbox_helper_host",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, name)
            .env(HELPER_BASE_ENV, base.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = helper.stdout.take().unwrap();
        let pids: Vec<i32> = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                // libtest prints "test <name> ... " before the helper's own output on the same line.
                line.split_once("KERNEL-PIDS ")
                    .map(|(_, rest)| rest)
                    .map(|rest| rest.split(' ').filter_map(|pid| pid.parse().ok()).collect())
            })
            .unwrap_or_default();
        let status = helper.wait().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(rustix::process::Signal::KILL.as_raw()),
            "{name}: the helper host did not die by SIGKILL"
        );
        assert!(pids.len() >= 2, "{name}: no kernel processes reported: {pids:?}");
        let deadline = Instant::now() + HANG_GUARD;
        while pids.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors: Vec<i32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        for pid in &survivors {
            kill(*pid);
        }
        assert!(
            survivors.is_empty(),
            "{name}: kernel processes outlived their host: {survivors:?}"
        );
    }
}
