//! A session's kernels and commands run in the session's environment, and cancelling a cell stops its shell work.
//!
//! The stack is composed as the session worker composes it (`Runtime::stack`), with an injected environment whose
//! PATH is a directory of fake tools only:
//! - the kernel, a cell's bash (blocking and as a job) and the host's own bash (the user's `!`) run the fake tool
//!   found on that PATH, and a tool only the server process's PATH has (`ls` in /usr/bin) is not found;
//! - a cell blocked in a host bash command (blocking, the default yield path, or waiting on a job it started) ends
//!   at once when cancelled: the command's own process group is stopped, the cell raises, and the kernel is not
//!   killed, so the lane keeps the variables the cancelled cell set before the command. A job started by a cell that
//!   ended normally keeps running when a later cell is cancelled.
//!
//! The blocking fake tool writes its pid to a FIFO the test reads (an event, no polling) and then blocks reading a
//! second FIFO nobody writes. Nothing here signals a process: the host stops what it started, and a command still
//! blocked when a test fails is released by opening its FIFO (the reader then sees end of file).

use delegation::lanes::CellOutcome;
use serde_json::json;
use session_core::RunEnv;
use session_core::compose::{CELL_CANCELLED_SHELL, Runtime, Services, Stack};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const HANG_GUARD: Duration = Duration::from_secs(120);
const O_NONBLOCK: i32 = 0o4000;

/// The session's PATH: only `<root>/bin`, holding the fake tools.
fn tools(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let write = |name: &str, body: &str| {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write("w16tool", "#!/bin/sh\necho fake-session-tool\n");
    // $1: a FIFO for the pid; $2: a FIFO to block on (nobody writes it).
    write("w16block", "#!/bin/sh\necho $$ > \"$1\"\nexec /usr/bin/cat \"$2\"\n");
    bin
}

fn stack(root: &Path) -> Arc<Stack> {
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".ultron/agent")).unwrap();
    std::fs::write(
        home.join(".ultron/agent/settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let bin = tools(root);
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("PATH".into(), bin.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            home.join(".ultron/agent").display().to_string(),
        ),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    runtime.stack(&model, None, "w16f").unwrap()
}

/// One cell on `lane`, its output and value.
fn run(stack: &Stack, lane: &str, code: &str) -> String {
    stack.lanes.open(lane).unwrap();
    match stack.lanes.run(lane, code, HANG_GUARD).unwrap() {
        CellOutcome::Done(Ok(result)) => {
            assert_eq!(result.status, kernel::Status::Ok, "{result:?}");
            format!("{}{}", result.stdout, result.result.unwrap_or_default())
        }
        other => panic!("cell did not end: {other:?}"),
    }
}

/// A blocking fake command: its pid FIFO, read on a thread, and the FIFO it blocks on (released on drop).
struct Blocker {
    block: PathBuf,
    pid: mpsc::Receiver<i32>,
    command: String,
}

impl Blocker {
    fn new(root: &Path, name: &str) -> Blocker {
        let (pid_fifo, block) = (root.join(format!("{name}.pid")), root.join(format!("{name}.block")));
        for fifo in [&pid_fifo, &block] {
            let status = std::process::Command::new("/usr/bin/mkfifo")
                .arg(fifo)
                .stdin(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }
        let (send, pid) = mpsc::channel();
        let reader = pid_fifo.clone();
        std::thread::spawn(move || {
            // Opening blocks until the command opens it to write its pid.
            let mut text = String::new();
            if let Ok(mut file) = std::fs::File::open(&reader) {
                let _ = file.read_to_string(&mut text);
            }
            if let Ok(pid) = text.trim().parse::<i32>() {
                let _ = send.send(pid);
            }
        });
        let command = format!("w16block {} {}", pid_fifo.display(), block.display());
        Blocker { block, pid, command }
    }

    /// The pid the command wrote: it is running and blocked.
    fn started(&self) -> i32 {
        self.pid
            .recv_timeout(HANG_GUARD)
            .expect("the fake command never started")
    }
}

impl Drop for Blocker {
    fn drop(&mut self) {
        // A command still blocked (a failing test) sees end of file and exits: nothing is signalled.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open(&self.block);
    }
}

/// Whether `pid` still runs (a zombie or a missing /proc entry is gone).
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat.rfind(')').and_then(|at| stat.get(at + 2..at + 3));
        !matches!(state, Some("Z" | "X") | None)
    })
}

/// Waits (hang guard only) until `pid` no longer runs; the host's kill is sent before the cancel returns.
fn wait_gone(pid: i32) {
    let deadline = Instant::now() + HANG_GUARD;
    while alive(pid) {
        assert!(Instant::now() < deadline, "process {pid} still runs after the cancel");
        std::thread::yield_now();
    }
}

/// Starts `code` on main without waiting, waits for `blocker`'s command, cancels the cell; the cancelled cell's
/// error value and the blocked pid.
fn cancel_while_blocked(stack: &Stack, code: &str, blocker: &Blocker) -> (String, i32) {
    let CellOutcome::Running { cell, .. } = stack.lanes.run("main", code, Duration::ZERO).unwrap() else {
        panic!("the cell ended at once");
    };
    let pid = blocker.started();
    assert!(alive(pid));
    let outcome = stack.lanes.cancel(&cell, "test cancel").unwrap().unwrap();
    assert_eq!(outcome.status, kernel::Status::Error, "{outcome:?}");
    (outcome.error.map(|error| error.evalue).unwrap_or_default(), pid)
}

#[test]
fn session_core_w16f_kernel_cell_bash_and_host_bash_take_path_from_the_session_env() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let stack = stack(&root);
    let bin = root.join("bin").display().to_string();
    // The kernel's own environment is the session's: its PATH, and only tools on it are found.
    assert_eq!(
        run(
            &stack,
            "main",
            "import os, shutil\n(os.environ['PATH'], shutil.which('w16tool') is not None, shutil.which('ls'))"
        ),
        format!("('{bin}', True, None)")
    );
    // A cell's bash: the default (a job past its yield), blocking (yield_after=None) and a job.
    assert_eq!(
        run(&stack, "main", "(await bash('w16tool')).output.strip()"),
        "'fake-session-tool'"
    );
    assert_eq!(
        run(
            &stack,
            "main",
            "(await bash('w16tool', yield_after=None)).output.strip()"
        ),
        "'fake-session-tool'"
    );
    assert_eq!(
        run(
            &stack,
            "main",
            "j = await bash('w16tool', yield_after=0)\n(await j.result()).text.strip()"
        ),
        "'fake-session-tool'"
    );
    // A tool only the server process's PATH has is not found.
    assert_eq!(
        run(
            &stack,
            "main",
            "(await bash('command -v ls || echo missing')).output.strip()"
        ),
        "'missing'"
    );
    assert_eq!(
        run(
            &stack,
            "main",
            "(await bash('echo $PATH', yield_after=None)).output.strip()"
        ),
        format!("'{bin}'")
    );
    // The host's own bash (the user's `!` command).
    let signal = kernel::Signal::new();
    let ran = stack
        .host
        .user_bash("w16tool; command -v ls || echo missing", &signal, &mut |_| {})
        .unwrap();
    assert_eq!(ran["output"], "fake-session-tool\nmissing\n");
    stack.close();
}

#[test]
fn session_core_w16f_cancelling_a_cell_stops_its_blocking_bash_and_keeps_the_lane_kernel_and_variables() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let stack = stack(&root);
    let kernel_pid = run(&stack, "main", "import os\nx = 41\nos.getpid()");
    let blocker = Blocker::new(&root, "blocking");
    let (error, pid) = cancel_while_blocked(
        &stack,
        &format!(
            "y = 'set before the command'\nawait bash({:?}, yield_after=None)\nz = 'after'",
            blocker.command
        ),
        &blocker,
    );
    assert_eq!(error, CELL_CANCELLED_SHELL);
    wait_gone(pid);
    // The same kernel: variables of the cancelled cell's first line survive (a restore would have lost them).
    assert_eq!(
        run(&stack, "main", "(x, y, 'z' in globals(), os.getpid())"),
        format!("(41, 'set before the command', False, {kernel_pid})")
    );
    stack.close();
}

#[test]
fn session_core_w16f_cancelling_a_cell_stops_a_default_bash_and_a_job_it_waits_on() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let stack = stack(&root);
    let kernel_pid = run(&stack, "main", "import os\nx = 1\nos.getpid()");
    // The default bash: a job the cell waits on for up to its yield time.
    let default = Blocker::new(&root, "default");
    let (error, pid) = cancel_while_blocked(
        &stack,
        &format!("a = 'kept'\nawait bash({:?})", default.command),
        &default,
    );
    assert_eq!(error, CELL_CANCELLED_SHELL);
    wait_gone(pid);
    // A job the cell started and then waits on.
    let job = Blocker::new(&root, "job");
    let (error, pid) = cancel_while_blocked(
        &stack,
        &format!("j = await bash({:?}, yield_after=0)\nawait j.result()", job.command),
        &job,
    );
    assert_eq!(error, CELL_CANCELLED_SHELL);
    wait_gone(pid);
    assert_eq!(
        run(
            &stack,
            "main",
            "jobs = await rlm.host_request('shell.list', {})\n(x, a, sorted(j['status'] for j in jobs if 'w16block' in j['command']), os.getpid())"
        ),
        format!("(1, 'kept', ['cancelled', 'cancelled'], {kernel_pid})")
    );
    stack.close();
}

#[test]
fn session_core_w16f_a_job_of_a_cell_that_ended_keeps_running_when_a_later_cell_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let stack = stack(&root);
    let kept = Blocker::new(&root, "kept");
    run(
        &stack,
        "main",
        &format!("keep = await bash({:?}, yield_after=0)", kept.command),
    );
    let kept_pid = kept.started();
    // A later cell is cancelled while it starts and waits on its own job: only that job is stopped.
    let own = Blocker::new(&root, "own");
    let (error, own_pid) = cancel_while_blocked(
        &stack,
        &format!(
            "mine = await bash({:?}, yield_after=0)\nawait mine.result()",
            own.command
        ),
        &own,
    );
    assert_eq!(error, CELL_CANCELLED_SHELL);
    wait_gone(own_pid);
    assert!(alive(kept_pid), "the earlier cell's job was stopped too");
    assert_eq!(run(&stack, "main", "(await keep.result(wait=0)).status"), "'running'");
    // Stopped by the model's own cancel.
    assert_eq!(run(&stack, "main", "(await keep.cancel()).status"), "'cancelled'");
    wait_gone(kept_pid);
    stack.close();
}
