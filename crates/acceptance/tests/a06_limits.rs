//! A06 (partial): output and process limits hold.
//! Ported from the kernel cases of packages/coding-agent/test/ultron-a06-limits.test.ts with their literal
//! expectations, against the real runtime.py under the Rust kernel supervisor. The two other evidence files
//! (ultron-kernel-long-session.test.ts, ultron-kernel-raw-output.test.ts) are ported in crates/kernel/tests/process.rs.
//! Not ported: artifact size and quota (NativeLocalServices `artifacts.*`), task `timeout_ms` and the root wall
//! deadline (timers and the usage ledger), and the admitted-task cap (usage ledger `maxAdmittedTasks`).

mod common;

use common::{alive, kernel_in, null_handler, read_pids, run, value, wait_until};
use kernel::{KernelError, Signal, Status};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PREVIEW_LIMIT: usize = 8192 + "\n... [truncated]".len();
const OUTPUT_LIMIT: usize = 20_000 + 64;

fn has_truncation_marker(text: &str) -> bool {
    text.split("[... ").skip(1).any(|rest| {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(" bytes truncated ...]")
    })
}

fn output_flood_stays_bounded(dir: &Path) {
    let kernel = kernel_in(dir, null_handler());
    let result = run(
        &kernel,
        "import sys\nfor _ in range(5000): print('o' * 10000)\nfor _ in range(5000): print('e' * 10000, file=sys.stderr)\n['r' * 10000 for _ in range(5000)]",
    );
    assert_eq!(result.status, Status::Ok);
    for stream in [&result.stdout, &result.stderr] {
        assert!(stream.len() <= OUTPUT_LIMIT, "{}", stream.len());
        assert!(has_truncation_marker(stream));
    }
    let preview = result.result.unwrap_or_default();
    assert!(preview.len() <= PREVIEW_LIMIT);
    assert!(preview.contains("<list: 5,000 items; item types str>"));
    let failed = run(&kernel, "raise ValueError('x' * 5_000_000)");
    assert_eq!(failed.status, Status::Error);
    let error = failed.error.unwrap();
    assert!(error.evalue.len() <= PREVIEW_LIMIT);
    assert!(error.traceback.join("\n").len() <= PREVIEW_LIMIT);
    kernel.shutdown();
}

fn raw_writes_are_bounded_output_not_protocol(dir: &Path) {
    let kernel = kernel_in(dir, null_handler());
    let pid = value(&kernel, "import os\nkept = 'intact'\nos.getpid()");
    let result = run(&kernel, "os.write(1, b'y' * (8 * 1024 * 1024))\n'after'");
    assert_eq!((result.status, result.result.as_deref()), (Status::Ok, Some("'after'")));
    assert!(result.stdout.len() <= 4 * 1024 * 1024 + 256);
    assert!(
        result
            .stdout
            .contains("over the RLM kernel's 4 MiB per-cell output limit")
    );
    assert_eq!(value(&kernel, "os.getpid(), kept"), format!("({pid}, 'intact')"));
    kernel.shutdown();
}

fn an_interrupted_runaway_cell_stops(dir: &Path) {
    let kernel = Arc::new(kernel_in(dir, null_handler()));
    run(&kernel, "marker = 1");
    let signal = Signal::new();
    let timer = signal.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        timer.abort("timeout");
    });
    let started = Instant::now();
    assert!(kernel.execute("while True:\n    pass", Some(&signal), None).is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    // Interrupt replaces the process; state is lost rather than silently restored.
    assert_eq!(value(&kernel, "'marker' in globals()"), "False");
    kernel.shutdown();
}

/// Kernel pid, a direct `sleep` child, a `sh` child, and the `sleep` grandchild it started.
fn spawn_tree_cell(pid_file: &Path) -> String {
    let path = serde_json::to_string(pid_file).unwrap();
    format!(
        "import os, subprocess, time\nchild = subprocess.Popen(['sleep', '60'])\nshell = subprocess.Popen(['sh', '-c', 'sleep 60 & echo $!; wait'], stdout=subprocess.PIPE, text=True)\ngrandchild = int(shell.stdout.readline())\nwith open({path}, 'w') as f: f.write(f'{{os.getpid()}} {{child.pid}} {{shell.pid}} {{grandchild}}')\ntime.sleep(60)"
    )
}

fn interrupting_a_cell_kills_its_children(dir: &Path) {
    let pid_file = dir.join("interrupt.pids");
    let kernel = Arc::new(kernel_in(dir, null_handler()));
    let signal = Signal::new();
    let (cell_kernel, cell_signal, code) = (kernel.clone(), signal.clone(), spawn_tree_cell(&pid_file));
    let running = std::thread::spawn(move || cell_kernel.execute(&code, Some(&cell_signal), None));
    let pids = read_pids(&pid_file, 4);
    assert!(pids.iter().all(|pid| alive(*pid)));
    signal.abort("interrupt");
    assert_eq!(running.join().unwrap(), Err(KernelError::new("interrupt")));
    wait_until("the interrupted tree to end", Duration::from_secs(5), || {
        !pids.iter().any(|pid| alive(*pid))
    });
    kernel.shutdown();
}

fn shutdown_kills_the_whole_tree(dir: &Path) {
    let pid_file = dir.join("shutdown.pids");
    let kernel = Arc::new(kernel_in(dir, null_handler()));
    let (cell_kernel, code) = (kernel.clone(), spawn_tree_cell(&pid_file));
    let running = std::thread::spawn(move || cell_kernel.execute(&code, None, None));
    let pids = read_pids(&pid_file, 4);
    assert!(pids.iter().all(|pid| alive(*pid)));
    kernel.shutdown();
    assert_eq!(
        running.join().unwrap(),
        Err(KernelError::new("RLM kernel is shut down"))
    );
    wait_until("the shut-down tree to end", Duration::from_secs(5), || {
        !pids.iter().any(|pid| alive(*pid))
    });
}

#[test]
fn acceptance_a06_output_and_process_limits_hold_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    output_flood_stays_bounded(dir.path());
    raw_writes_are_bounded_output_not_protocol(dir.path());
    an_interrupted_runaway_cell_stops(dir.path());
    interrupting_a_cell_kills_its_children(dir.path());
    shutdown_kills_the_whole_tree(dir.path());
}
