//! Process-level tests: the Rust supervisor drives the real runtime.py. Expectations are ported from the TypeScript
//! kernel tests (packages/coding-agent/test/ultron-kernel*.test.ts) with their literal values.

use kernel::{
    ExecutionResult, HostHandler, Kernel, KernelError, KernelOptions, LimitOverrides, ProtocolChannel, Signal, Status,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn null_handler() -> Arc<dyn HostHandler> {
    Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null))
}

fn options() -> KernelOptions {
    KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime())
}

fn kernel_with(options: KernelOptions, handler: Arc<dyn HostHandler>) -> Kernel {
    Kernel::new(options, handler).expect("kernel options")
}

fn kernel() -> Kernel {
    kernel_with(options(), null_handler())
}

fn run(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"))
}

/// The result text of a cell that must succeed.
fn value(kernel: &Kernel, code: &str) -> String {
    let result = run(kernel, code);
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

fn failure(kernel: &Kernel, code: &str) -> KernelError {
    kernel.execute(code, None, None).expect_err(code)
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'))
}

/// Whether `pid` ends (or is a zombie) within 5 s.
fn gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

/// A gate the test opens once; handlers block on it.
#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);

impl Gate {
    fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn wait(&self) {
        let open = self.0.lock().unwrap();
        drop(self.1.wait_while(open, |open| !*open).unwrap());
    }
}

// ultron-kernel.test.ts: "preserves Python state and returns host replies and Python errors"
#[test]
fn kernel_preserves_state_and_returns_host_replies_and_python_errors() {
    let handler: Arc<dyn HostHandler> = Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
        if kind == "agents.list" {
            return Ok(json!([{"id": "identity"}]));
        }
        Err("Unknown request".to_string())
    });
    let kernel = kernel_with(options(), handler);
    let first = run(&kernel, "answer = 42\nprint('ready')\nanswer");
    assert_eq!(
        (first.status, first.stdout.as_str(), first.result.as_deref()),
        (Status::Ok, "ready\n", Some("42"))
    );
    assert_eq!(value(&kernel, "answer + 1"), "43");
    assert_eq!(value(&kernel, "await agents.list()"), "[{'id': 'identity'}]");
    let error = run(&kernel, "raise ValueError('bad')");
    assert_eq!(error.status, Status::Error);
    let error = error.error.unwrap();
    assert_eq!((error.ename.as_str(), error.evalue.as_str()), ("ValueError", "bad"));
    assert_eq!(error.traceback.last().map(String::as_str), Some("ValueError: bad"));
    let refused = run(&kernel, "await rlm.host_request('other.thing', {})").error.unwrap();
    assert_eq!(
        (refused.ename.as_str(), refused.evalue.as_str()),
        ("RuntimeError", "Unknown request")
    );
}

// ultron-kernel.test.ts: "queued cancellation leaves the active cell running"
#[test]
fn kernel_queued_cancellation_leaves_the_active_cell_running() {
    let gate = Arc::new(Gate::default());
    let (entered, entered_rx) = mpsc::channel();
    let handler_gate = gate.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, _: Map<String, Value>, _: &Signal| {
        let _ = entered.send(());
        handler_gate.wait();
        Ok(Value::from("completed"))
    });
    let kernel = Arc::new(kernel_with(options(), handler));
    let active_kernel = kernel.clone();
    let active = std::thread::spawn(move || active_kernel.execute("await agents.list()", None, None));
    entered_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    let pid = kernel.pid().unwrap();
    let signal = Signal::new();
    let queued_kernel = kernel.clone();
    let queued_signal = signal.clone();
    let queued = std::thread::spawn(move || queued_kernel.execute("1 + 1", Some(&queued_signal), None));
    std::thread::sleep(Duration::from_millis(100));
    signal.abort("cancel queued");
    assert_eq!(queued.join().unwrap(), Err(KernelError::new("cancel queued")));
    gate.open();
    let active = active.join().unwrap().unwrap();
    assert_eq!(
        (active.status, active.result.as_deref()),
        (Status::Ok, Some("'completed'"))
    );
    assert_eq!(value(&kernel, "6 * 7"), "42");
    assert_eq!(kernel.pid(), Some(pid));
}

// ultron-kernel.test.ts: "active cancellation stops Python and propagates to host work"
#[test]
fn kernel_active_cancellation_kills_the_kernel_and_aborts_host_work() {
    let (entered, entered_rx) = mpsc::channel();
    let host_aborted = Arc::new(Mutex::new(None));
    let seen = host_aborted.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, _: Map<String, Value>, signal: &Signal| {
        let _ = entered.send(());
        signal.wait(Duration::from_secs(30));
        *seen.lock().unwrap() = signal.reason();
        Ok(json!([]))
    });
    let kernel = Arc::new(kernel_with(options(), handler));
    let pid_before = {
        assert_eq!(value(&kernel, "'started'"), "'started'");
        kernel.pid().unwrap()
    };
    let signal = Signal::new();
    let running = kernel.clone();
    let running_signal = signal.clone();
    let executing = std::thread::spawn(move || running.execute("await agents.list()", Some(&running_signal), None));
    entered_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    signal.abort("cancel active");
    assert_eq!(executing.join().unwrap(), Err(KernelError::new("cancel active")));
    let deadline = Instant::now() + Duration::from_secs(5);
    while host_aborted.lock().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(host_aborted.lock().unwrap().as_deref(), Some("cancel active"));
    assert!(gone(pid_before), "the cancelled kernel process is gone");
    assert_eq!(value(&kernel, "'fresh'"), "'fresh'");
    assert_ne!(kernel.pid(), Some(pid_before));
}

#[test]
fn kernel_cancellation_interrupts_a_busy_python_cell_and_the_next_cell_starts_fresh() {
    let kernel = Arc::new(kernel());
    assert_eq!(value(&kernel, "kept = 1\n'ok'"), "'ok'");
    let signal = Signal::new();
    let busy = kernel.clone();
    let busy_signal = signal.clone();
    let started = Instant::now();
    let executing = std::thread::spawn(move || busy.execute("while True:\n    pass", Some(&busy_signal), None));
    std::thread::sleep(Duration::from_millis(300));
    signal.abort("stop");
    assert_eq!(executing.join().unwrap(), Err(KernelError::new("stop")));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(value(&kernel, "'kept' in globals()"), "False");
    // A signal aborted before the call rejects it without touching the kernel.
    let pid = kernel.pid();
    assert_eq!(kernel.execute("1", Some(&signal), None), Err(KernelError::new("stop")));
    assert_eq!(kernel.pid(), pid);
}

// ultron-kernel.test.ts: "rejects malformed protocol and startup stalls"
#[test]
fn kernel_rejects_malformed_protocol_and_startup_stalls() {
    let directory = tempfile::tempdir().unwrap();
    let invalid = directory.path().join("invalid.py");
    std::fs::write(
        &invalid,
        "import os, time\nprint('plain output', flush=True)\nos.write(3, b'not JSON\\n')\ntime.sleep(30)\n",
    )
    .unwrap();
    let kernel = kernel_with(KernelOptions::new(directory.path(), &invalid), null_handler());
    let error = failure(&kernel, "1");
    assert!(
        error
            .message()
            .starts_with("RLM kernel invalid protocol frame: invalid JSON:"),
        "{error}"
    );
    let stalled = directory.path().join("stalled.py");
    std::fs::write(&stalled, "import time\ntime.sleep(30)\n").unwrap();
    let mut options = KernelOptions::new(directory.path(), &stalled);
    options.startup_timeout = Duration::from_millis(100);
    let hanging = kernel_with(options, null_handler());
    let started = Instant::now();
    assert_eq!(
        failure(&hanging, "1"),
        KernelError::new("RLM kernel startup timed out after 100ms")
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn kernel_protocol_violations_end_the_kernel_with_explicit_errors() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            "os.write(3, b'[1]\\n')",
            "RLM kernel invalid protocol frame: protocol frame must be a JSON object",
        ),
        (
            "os.write(3, b'{\"id\": 1}\\n')",
            "RLM kernel invalid protocol frame: protocol frame is missing event",
        ),
        (
            "os.write(3, b'{\"event\": \"nope\"}\\n')",
            "RLM kernel invalid protocol frame: unknown protocol event: nope",
        ),
        (
            "os.write(3, b'\\xff\\n')",
            "RLM kernel invalid protocol frame: The encoded data was not valid for encoding utf-8",
        ),
        (
            "os.write(3, b'x' * (1024 * 1024 + 1))",
            "RLM kernel protocol frame exceeds 1 MiB (1048576 bytes)",
        ),
        (
            "os.write(3, b'{\"event\": \"ready\"}\\n')",
            "RLM kernel invalid protocol frame: duplicate ready event",
        ),
        (
            "os.write(3, b'{\"event\": \"stdout\", \"id\": \"zzz\", \"text\": \"\"}\\n')",
            "RLM kernel invalid protocol frame: protocol frame references unknown request id: zzz",
        ),
        (
            "os.write(3, b'{\"event\": \"done\", \"id\": \"\"}\\n')",
            "RLM kernel invalid protocol frame: done event requires an id",
        ),
        (
            "os.write(3, b'{\"event\": \"host_request\", \"id\": \"h\", \"type\": \"t\", \"payload\": []}\\n')",
            "RLM kernel invalid protocol frame: host_request payload must be an object",
        ),
        ("os._exit(5)", "RLM kernel exited (5)"),
        (
            "os.close(3); time.sleep(5)",
            "RLM kernel protocol channel (fd 3) closed",
        ),
    ];
    for (action, expected) in cases {
        let script = directory.path().join("bad.py");
        std::fs::write(
            &script,
            format!("import os, time\nos.write(3, b'{{\"event\": \"ready\"}}\\n')\nos.read(4, 1)\n{action}\ntime.sleep(30)\n"),
        )
        .unwrap();
        let kernel = kernel_with(KernelOptions::new(directory.path(), &script), null_handler());
        assert_eq!(failure(&kernel, "1"), KernelError::new(expected), "{action}");
    }
    // A done frame with an unknown status, sent for the running cell.
    let script = directory.path().join("status.py");
    std::fs::write(
        &script,
        "import os, json, time\nos.write(3, b'{\"event\": \"ready\"}\\n')\nrequest = json.loads(os.fdopen(4).readline())\nos.write(3, json.dumps({'event': 'done', 'id': request['id'], 'status': 'maybe'}).encode() + b'\\n')\ntime.sleep(30)\n",
    )
    .unwrap();
    let kernel = kernel_with(KernelOptions::new(directory.path(), &script), null_handler());
    assert_eq!(
        failure(&kernel, "1"),
        KernelError::new("RLM kernel invalid protocol frame: done event has invalid status")
    );
}

// ultron-kernel.test.ts: "rejects oversized input without losing existing Python state"
#[test]
fn kernel_rejects_oversized_input_without_losing_state() {
    let kernel = kernel();
    run(&kernel, "answer = 42");
    let error = failure(&kernel, &format!("#{}", "x".repeat(MIB)));
    assert_eq!(
        error,
        KernelError::new("RLM kernel outbound frame exceeds 1 MiB (1048576 bytes)")
    );
    assert_eq!(value(&kernel, "answer"), "42");
}

#[test]
fn kernel_crash_rejects_the_running_and_queued_cells_and_the_next_cell_restarts() {
    let kernel = Arc::new(kernel());
    assert_eq!(
        value(&kernel, "kept = 'x'\nimport os\nos.getpid()"),
        kernel.pid().unwrap().to_string()
    );
    let pid = kernel.pid().unwrap();
    let crashing = kernel.clone();
    let crash =
        std::thread::spawn(move || crashing.execute("import time, os\ntime.sleep(0.5)\nos._exit(3)", None, None));
    std::thread::sleep(Duration::from_millis(150));
    let queued = kernel.clone();
    let queued = std::thread::spawn(move || queued.execute("'never runs'", None, None));
    let crashed = crash.join().unwrap().unwrap_err();
    assert!(crashed.message().starts_with("RLM kernel exited (3)"), "{crashed}");
    assert_eq!(queued.join().unwrap(), Err(crashed));
    assert!(!kernel.is_running());
    assert_eq!(value(&kernel, "'kept' in globals()"), "False");
    assert!(kernel.is_running());
    assert_ne!(kernel.pid(), Some(pid));
}

#[test]
fn kernel_exit_reports_the_stderr_tail_and_killed_processes() {
    let kernel = kernel();
    run(&kernel, "import os, sys\nos.write(2, b'diagnostic line\\n')");
    let error = failure(&kernel, "os._exit(4)");
    assert!(error.message().starts_with("RLM kernel exited (4): "), "{error}");
    assert!(error.message().ends_with("diagnostic line\n"), "{error}");
    let error = failure(&kernel, "import os, signal\nos.kill(os.getpid(), signal.SIGTERM)");
    assert!(error.message().starts_with("RLM kernel exited (SIGTERM)"), "{error}");
    assert!(!error.message().contains("out-of-memory"));
    let error = failure(&kernel, "import os, signal\nos.kill(os.getpid(), signal.SIGKILL)");
    assert!(
        error
            .message()
            .starts_with("RLM kernel exited (SIGKILL); the system out-of-memory killer may have stopped it"),
        "{error}"
    );
}

#[test]
fn kernel_shutdown_kills_the_process_tree_and_rejects_later_calls() {
    let kernel = kernel();
    let child = value(
        &kernel,
        "import subprocess\nchild = subprocess.Popen(['sleep', '60'])\nchild.pid",
    );
    let child: u32 = child.parse().unwrap();
    let pid = kernel.pid().unwrap();
    assert!(alive(pid) && alive(child));
    kernel.shutdown();
    assert!(gone(pid), "kernel process is gone");
    assert!(gone(child), "the cell's subprocess is gone");
    assert_eq!(
        kernel.execute("1", None, None),
        Err(KernelError::new("RLM kernel is shut down"))
    );
    assert_eq!(kernel.start(), Err(KernelError::new("RLM kernel is shut down")));
    assert!(!kernel.is_running());
}

#[test]
fn kernel_host_requests_round_trip_with_per_cell_handlers_and_limits() {
    let echo: Arc<dyn HostHandler> = Arc::new(kernel::EchoHandler);
    let kernel = kernel_with(options(), echo);
    assert_eq!(
        value(
            &kernel,
            "await rlm.host_request('test.echo', {'n': 1, 'list': [1, 'two']})"
        ),
        "{'type': 'test.echo', 'payload': {'n': 1, 'list': [1, 'two']}}"
    );
    let override_handler: Arc<dyn HostHandler> =
        Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| Ok(Value::from(format!("cell handler: {kind}"))));
    let result = kernel
        .execute("await rlm.host_request('x.y')", None, Some(override_handler))
        .unwrap();
    assert_eq!(result.result.as_deref(), Some("'cell handler: x.y'"));
    // A request after its cell has finished has no active cell.
    run(
        &kernel,
        "import asyncio\nasync def later():\n    await asyncio.sleep(0.2)\n    return await rlm.host_request('test.echo', {})\nlate = asyncio.ensure_future(later())",
    );
    std::thread::sleep(Duration::from_millis(800));
    let late = value(
        &kernel,
        "try:\n    await late\nexcept Exception as error:\n    message = str(error)\nmessage",
    );
    assert_eq!(late, "'RLM host request has no active cell'");
    // A reply that cannot be encoded within a frame becomes an error in Python.
    let big: Arc<dyn HostHandler> =
        Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::from("b".repeat(MIB))));
    let result = kernel
        .execute("await rlm.host_request('big')", None, Some(big))
        .unwrap()
        .error
        .unwrap();
    assert_eq!(result.evalue, "RLM kernel outbound frame exceeds 1 MiB (1048576 bytes)");
}

#[test]
fn kernel_host_requests_beyond_sixteen_in_flight_are_refused() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let counter = in_flight.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, _: Map<String, Value>, _: &Signal| {
        counter.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(20);
        while counter.load(Ordering::SeqCst) < 16 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Value::from("ok"))
    });
    let kernel = kernel_with(options(), handler);
    let result = value(
        &kernel,
        "import asyncio\nresults = await asyncio.gather(*[rlm.host_request('slow', {}) for _ in range(17)], return_exceptions=True)\n[str(r) for r in results if isinstance(r, Exception)], results.count('ok')",
    );
    assert_eq!(result, "(['RLM host request concurrency exceeds 16'], 16)");
    assert_eq!(in_flight.load(Ordering::SeqCst), 16);
}

#[test]
fn kernel_lone_surrogates_from_python_become_replacement_characters() {
    let kernel = kernel_with(options(), Arc::new(kernel::EchoHandler));
    assert_eq!(
        value(
            &kernel,
            "(await rlm.host_request('test.echo', {'s': '\\udcff', 'pair': '\\ud83c\\udf89'}))['payload']"
        ),
        "{'s': '\u{fffd}', 'pair': '🎉'}"
    );
}

#[test]
fn kernel_top_level_await_and_persistent_namespace() {
    let kernel = kernel();
    assert_eq!(
        value(
            &kernel,
            "import asyncio\nasync def twice(x):\n    await asyncio.sleep(0)\n    return 2 * x"
        ),
        ""
    );
    assert_eq!(value(&kernel, "await twice(21)"), "42");
    assert_eq!(
        value(&kernel, "results = await asyncio.gather(twice(1), twice(2))\nresults"),
        "[2, 4]"
    );
    assert_eq!(value(&kernel, "results"), "[2, 4]");
    run(&kernel, "state['keep'] = 1\nscratch = 2");
    assert_eq!(kernel.reset_scratch().unwrap().status, Status::Ok);
    assert_eq!(value(&kernel, "state, 'scratch' in globals()"), "({'keep': 1}, False)");
}

#[test]
fn kernel_start_and_running_state() {
    let kernel = kernel();
    assert!(!kernel.is_running());
    kernel.start().unwrap();
    assert!(kernel.is_running());
    let missing = kernel_with(
        KernelOptions {
            python: Some("/nonexistent/python".into()),
            ..options()
        },
        null_handler(),
    );
    assert!(
        missing
            .start()
            .unwrap_err()
            .message()
            .starts_with("RLM kernel spawn failed: ")
    );
    let mut zero = options();
    zero.startup_timeout = Duration::ZERO;
    assert_eq!(
        Kernel::new(zero, null_handler()).err(),
        Some(KernelError::new(
            "RLM startupTimeoutMs must be a positive, finite timer duration"
        ))
    );
}

// ultron-kernel-raw-output.test.ts
fn with_kernel(check: impl FnOnce(&Kernel)) {
    let kernel = kernel();
    let first = run(&kernel, "import os, subprocess, sys\nkept = 'intact'\nos.getpid()");
    // Interpreter output before the runtime is ready is startup diagnostics, not cell output.
    assert_eq!(
        (first.status, first.stdout.as_str(), first.stderr.as_str()),
        (Status::Ok, "", "")
    );
    let pid = first.result.unwrap();
    check(&kernel);
    assert_eq!(value(&kernel, "os.getpid(), kept"), format!("({pid}, 'intact')"));
}

#[test]
fn kernel_raw_write_of_3_mb_lands_whole_in_stdout() {
    with_kernel(|kernel| {
        let result = run(kernel, "os.write(1, b'<' + b'x' * 3_000_000 + b'>')\n'done'");
        assert_eq!(result.result.as_deref(), Some("'done'"));
        assert_eq!(result.stdout.len(), 3_000_002);
        assert!(result.stdout.starts_with("<xxx") && result.stdout.ends_with("xxx>"));
    });
}

#[test]
fn kernel_raw_output_over_4_mib_keeps_head_and_tail() {
    with_kernel(|kernel| {
        let result = run(kernel, "os.write(1, b'H' + b'y' * 6_000_000 + b'T')\n1");
        assert_eq!(result.result.as_deref(), Some("1"));
        assert!(result.stdout.len() <= 4 * MIB + 256);
        assert!(result.stdout.starts_with("Hyyy") && result.stdout.ends_with("yyyT"));
        let cut = 6_000_002 - 4 * MIB;
        assert!(result.stdout.contains(&format!(
            "\n[... {cut} characters of raw fd 1 output cut from the middle: over the RLM kernel's 4 MiB per-cell output limit ...]\n"
        )));
    });
}

#[test]
fn kernel_os_system_subprocesses_and_original_stdout_write_into_the_cell() {
    with_kernel(|kernel| {
        let shell = run(kernel, "os.system('yes | head -c 2000000')");
        assert_eq!(shell.result.as_deref(), Some("0"));
        assert_eq!(shell.stdout.len(), 2_000_000);
        assert!(shell.stdout.starts_with("y\ny\n"));
        let child = run(
            kernel,
            "subprocess.run([sys.executable, '-c', \"print('hi' * 500000)\"]).returncode",
        );
        assert_eq!(child.result.as_deref(), Some("0"));
        assert_eq!(child.stdout, format!("{}\n", "hi".repeat(500_000)));
        let original = run(kernel, "print('via __stdout__', file=sys.__stdout__)");
        assert_eq!(original.stdout, "via __stdout__\n");
    });
}

#[test]
fn kernel_c_stdio_output_is_flushed_into_the_cell() {
    with_kernel(|kernel| {
        let result = run(
            kernel,
            "import ctypes\nctypes.CDLL(None).printf(b'from C %d\\n', 42)\nNone",
        );
        assert_eq!((result.status, result.stdout.as_str()), (Status::Ok, "from C 42\n"));
    });
}

#[test]
fn kernel_fd_2_writes_land_in_stderr() {
    with_kernel(|kernel| {
        let result = run(
            kernel,
            "os.write(2, b'raw error\\n')\nprint('buffered error', file=sys.__stderr__)\nsubprocess.run(['sh', '-c', 'echo from child >&2'])\nNone",
        );
        assert_eq!(
            (result.stdout.as_str(), result.stderr.as_str()),
            ("", "raw error\nbuffered error\nfrom child\n")
        );
        let flood = run(kernel, "os.write(2, b'e' * 3_000_000)\n2");
        assert_eq!((flood.result.as_deref(), flood.stderr.len()), (Some("2"), 3_000_000));
    });
}

#[test]
fn kernel_print_and_raw_writes_are_ordered_deterministically() {
    with_kernel(|kernel| {
        for _ in 0..20 {
            let result = run(
                kernel,
                "for i in range(3):\n    print(f'print {i}')\n    os.write(1, f'raw {i}\\n'.encode())\n    subprocess.run(['echo', f'child {i}'])",
            );
            assert_eq!(
                result.stdout,
                "print 0\nprint 1\nprint 2\nraw 0\nchild 0\nraw 1\nchild 1\nraw 2\nchild 2\n"
            );
        }
    });
}

#[test]
fn kernel_failing_cell_still_collects_raw_output() {
    with_kernel(|kernel| {
        let result = run(kernel, "os.write(1, b'before failure\\n')\nraise ValueError('boom')");
        assert_eq!(result.status, Status::Error);
        assert_eq!(result.error.unwrap().ename, "ValueError");
        assert_eq!(result.stdout, "before failure\n");
    });
}

#[test]
fn kernel_output_from_outside_a_cell_is_reported_once_at_the_next_cell() {
    with_kernel(|kernel| {
        let directory = tempfile::tempdir().unwrap();
        let written = directory.path().join("written");
        let started = run(
            kernel,
            &format!(
                "background = subprocess.Popen(['sh', '-c', 'sleep 0.2; echo late output; touch {}'])\n'started'",
                written.display()
            ),
        );
        assert_eq!(
            (started.stdout.as_str(), started.result.as_deref()),
            ("", Some("'started'"))
        );
        while !written.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(300));
        let next = run(kernel, "background.wait()\nprint('next cell')");
        assert_eq!(
            next.stdout,
            "[kernel output written outside a cell, before this one (a background thread or process):]\nlate output\n[end of output from outside a cell]\nnext cell\n"
        );
        assert_eq!(run(kernel, "print('clean')").stdout, "clean\n");
    });
}

#[test]
fn kernel_protocol_descriptors_are_not_inherited() {
    with_kernel(|kernel| {
        let result = value(
            kernel,
            "probe = \"import os\\nfor fd in (3, 4):\\n    try:\\n        os.fstat(fd); print(fd, 'open')\\n    except OSError: print(fd, 'closed')\"\ninherited = subprocess.run([sys.executable, '-c', probe], close_fds=False, capture_output=True, text=True).stdout\nshell = os.system('echo junk >&3 2>/dev/null; echo junk >&4 2>/dev/null')\ninherited, shell != 0, os.get_inheritable(3), os.get_inheritable(4)",
        );
        assert_eq!(result, "('3 closed\\n4 closed\\n', True, False, False)");
    });
}

#[test]
fn kernel_stdin_is_empty() {
    with_kernel(|kernel| {
        let result = run(kernel, "input('name? ')");
        let error = result.error.unwrap();
        assert_eq!(error.ename, "EOFError");
        assert!(error.evalue.contains("no interactive stdin"));
        assert_eq!(
            value(
                kernel,
                "subprocess.run(['cat'], capture_output=True, timeout=10).stdout"
            ),
            "b''"
        );
        assert_eq!(value(kernel, "sys.stdin.readline()"), "''");
    });
}

#[test]
fn kernel_stdio_protocol_channel_works() {
    let kernel = kernel_with(
        KernelOptions {
            protocol_channel: ProtocolChannel::Stdio,
            ..options()
        },
        null_handler(),
    );
    let result = run(&kernel, "print('hello')\n6 * 7");
    assert_eq!(
        (result.stdout.as_str(), result.result.as_deref()),
        ("hello\n", Some("42"))
    );
    // On this channel stderr is only a diagnostic tail (read asynchronously).
    let result = run(&kernel, "import os\nos.write(2, b'tail text\\n')");
    assert_eq!(result.stderr, "");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !kernel.stderr_tail().ends_with("tail text\n") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(kernel.stderr_tail().ends_with("tail text\n"));
}

// ultron-kernel-long-session.test.ts
#[test]
fn kernel_200_cells_of_64_kb_keep_one_generation() {
    let kernel = kernel_with(
        KernelOptions {
            env: vec![("ULTRON_RLM_OUTPUT_BYTES".into(), "131072".into())],
            ..options()
        },
        null_handler(),
    );
    let pid = value(&kernel, "import os\nkept = {'answer': 42}\nos.getpid()");
    for cell in 0..200 {
        let result = run(
            &kernel,
            &format!("print('{}' * {}, end='')\nkept['answer']", cell % 10, 64 * KIB),
        );
        assert_eq!((result.stdout.len(), result.result.as_deref()), (64 * KIB, Some("42")));
    }
    assert_eq!(value(&kernel, "os.getpid()"), pid);
    assert_eq!(value(&kernel, "kept"), "{'answer': 42}");
}

#[test]
fn kernel_bridge_requests_over_4_mib_keep_one_generation() {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, payload: Map<String, Value>, _: &Signal| {
        counted.fetch_add(1, Ordering::SeqCst);
        let size = payload.get("text").and_then(Value::as_str).map_or(0, str::len);
        Ok(json!({"size": size, "echo": "r".repeat(64 * KIB)}))
    });
    let kernel = kernel_with(options(), handler);
    let pid = value(&kernel, "import os\nstate_value = 'survives'\nos.getpid()");
    for _ in 0..4 {
        let result = value(
            &kernel,
            &format!(
                "sizes = []\nfor _ in range(64):\n    reply = await rlm.host_request('test.echo', {{'text': 'q' * {}}})\n    sizes.append((reply['size'], len(reply['echo'])))\nset(sizes), len(sizes)",
                64 * KIB
            ),
        );
        assert_eq!(result, format!("({{({}, {})}}, 64)", 64 * KIB, 64 * KIB));
    }
    assert_eq!(requests.load(Ordering::SeqCst), 256);
    assert_eq!(value(&kernel, "os.getpid()"), pid);
    assert_eq!(value(&kernel, "state_value"), "'survives'");
}

#[test]
fn kernel_single_output_over_the_frame_is_cut_in_the_cell() {
    let kernel = kernel_with(
        KernelOptions {
            env: vec![("ULTRON_RLM_OUTPUT_BYTES".into(), (4 * MIB).to_string())],
            ..options()
        },
        null_handler(),
    );
    let pid = value(&kernel, "import os\nbefore = 'kept'\nos.getpid()");
    let result = run(
        &kernel,
        "import sys\nprint('h' + 'x' * (3 * 1024 * 1024) + 't')\nprint('\\u00e9' * (1024 * 1024), file=sys.stderr)\n7",
    );
    assert_eq!(result.result.as_deref(), Some("7"));
    assert!(result.stdout.len() < MIB);
    assert!(result.stdout.starts_with("hxxx") && result.stdout.trim_end().ends_with("xxxt"));
    assert!(result.stdout.contains("characters cut to fit the 1 MiB protocol frame"));
    assert!(result.stderr.contains("characters cut to fit the 1 MiB protocol frame"));
    assert_eq!(value(&kernel, "os.getpid(), before"), format!("({pid}, 'kept')"));
}

#[test]
fn kernel_raw_output_across_many_cells_keeps_one_generation() {
    let kernel = kernel();
    let pid = value(&kernel, "import os\nkept = 'raw'\nos.getpid()");
    for cell in 0..40 {
        let result = run(&kernel, &format!("os.write(1, b'{}' * {})\nkept", cell % 10, 512 * KIB));
        assert_eq!(result.result.as_deref(), Some("'raw'"));
        assert_eq!(result.stdout, (cell % 10).to_string().repeat(512 * KIB));
    }
    assert_eq!(value(&kernel, "os.getpid()"), pid);
}

#[test]
fn kernel_host_bounds_a_cells_accumulated_output() {
    let directory = tempfile::tempdir().unwrap();
    let flooding = directory.path().join("flooding.py");
    std::fs::write(
        &flooding,
        [
            "import json, os, sys",
            "private = '--protocol-fds' in sys.argv",
            "out = os.fdopen(3, 'w') if private else sys.stdout",
            "def emit(**frame):",
            "    out.write(json.dumps(frame) + '\\n'); out.flush()",
            "emit(event='ready')",
            "for line in (os.fdopen(4) if private else sys.stdin):",
            "    request = json.loads(line)",
            "    if request.get('request') == 'execute':",
            "        for _ in range(12): emit(event='stdout', id=request['id'], text='s' * (900 * 1024))",
            "        emit(event='result', id=request['id'], result=str(len(request['code'])))",
            "    emit(event='done', id=request['id'], status='ok')",
        ]
        .join("\n"),
    )
    .unwrap();
    let kernel = kernel_with(KernelOptions::new(directory.path(), &flooding), null_handler());
    for _ in 0..3 {
        let result = run(&kernel, "1234");
        assert_eq!(result.result.as_deref(), Some("4"));
        assert_eq!(
            result.stdout,
            format!(
                "{}\n[... further output dropped: over the RLM kernel's 4 MiB per-cell output limit ...]\n",
                "s".repeat(4 * MIB)
            )
        );
    }
    assert!(kernel.is_running());
}

// ultron-kernel-resource-limits.test.ts
fn limited(memory: u64, cpu: u64) -> Kernel {
    kernel_with(
        KernelOptions {
            limits: LimitOverrides {
                max_memory_mb: Some(memory),
                max_cpu_seconds: Some(cpu),
            },
            ..options()
        },
        null_handler(),
    )
}

#[test]
fn kernel_default_limits_reach_the_kernel() {
    let kernel = kernel_with(
        KernelOptions {
            limits: LimitOverrides {
                max_memory_mb: Some(4096),
                max_cpu_seconds: Some(1800),
            },
            ..options()
        },
        null_handler(),
    );
    let result = value(
        &kernel,
        "import resource\ndata = bytearray(200 * 1024 * 1024)\nsum(range(2_000_000))\n(resource.getrlimit(resource.RLIMIT_DATA), resource.getrlimit(resource.RLIMIT_CPU)[0] > 0, len(data))",
    );
    assert_eq!(
        result,
        format!("(({}, {}), True, {})", 4096 * MIB, 4096 * MIB, 200 * MIB)
    );
    let unlimited = limited(0, 0);
    assert_eq!(
        value(
            &unlimited,
            "import resource\nresource.getrlimit(resource.RLIMIT_DATA)[0] != 4096 * 1024 * 1024 and resource.getrlimit(resource.RLIMIT_CPU)[0] == resource.RLIM_INFINITY"
        ),
        "True"
    );
}

#[test]
fn kernel_memory_limit_fails_in_band_and_keeps_state() {
    let kernel = limited(256, 0);
    run(&kernel, "kept = 'still here'");
    let failed = run(&kernel, "hog = bytearray(512 * 1024 * 1024)").error.unwrap();
    assert_eq!(failed.ename, "MemoryError");
    assert!(
        failed
            .evalue
            .contains("RLM kernel exceeded its memory limit (256 MiB per process")
    );
    assert_eq!(
        value(&kernel, "(kept, 'hog' in globals(), 6 * 7)"),
        "('still here', False, 42)"
    );
    assert_eq!(failure_or_error(&kernel), Status::Error);
}

fn failure_or_error(kernel: &Kernel) -> Status {
    run(
        kernel,
        "import resource\nresource.setrlimit(resource.RLIMIT_DATA, (resource.RLIM_INFINITY, resource.RLIM_INFINITY))",
    )
    .status
}

#[test]
fn kernel_memory_exit_is_reported_and_replaced() {
    let kernel = limited(256, 1800);
    run(&kernel, "before = 1");
    let error = failure(&kernel, "import os\nos._exit(86)");
    assert_eq!(
        error.message(),
        "RLM kernel exceeded its memory limit (256 MiB per process, ULTRON_RLM_MAX_MEMORY_MB) and was stopped; the next cell starts a fresh kernel without earlier Python state"
    );
    assert_eq!(value(&kernel, "('before' in globals(), 1 + 1)"), "(False, 2)");
}

#[test]
fn kernel_cpu_limit_stops_a_python_loop_in_band() {
    let kernel = limited(4096, 1);
    run(&kernel, "kept = 5");
    let burned = run(&kernel, "while True:\n    pass").error.unwrap();
    assert_eq!(burned.ename, "RlmCpuLimitExceeded");
    assert!(burned.evalue.contains("exceeded its CPU limit of 1 CPU-seconds"));
    assert_eq!(value(&kernel, "sum(range(100_000)) and kept"), "5");
}

#[test]
fn kernel_cpu_backstop_kills_a_cell_that_swallows_the_limit_or_sits_in_c() {
    let kernel = limited(4096, 1);
    let expected = KernelError::new(
        "RLM kernel exceeded its CPU limit (1 CPU-seconds per cell, ULTRON_RLM_MAX_CPU_SECONDS) and was stopped; the next cell starts a fresh kernel without earlier Python state",
    );
    let swallow =
        "while True:\n    try:\n        while True:\n            pass\n    except BaseException:\n        pass";
    let kernel = Arc::new(kernel);
    // Bounded: without the backstop these cells never end; shut the kernel down instead of hanging.
    let bounded = |code: &'static str| {
        let (sender, receiver) = mpsc::channel();
        let running = kernel.clone();
        std::thread::spawn(move || {
            let _ = sender.send(running.execute(code, None, None));
        });
        receiver.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| {
            kernel.shutdown();
            panic!("no CPU backstop for {code}")
        })
    };
    assert_eq!(bounded(swallow), Err(expected.clone()));
    assert_eq!(value(&kernel, "1 + 1"), "2");
    assert_eq!(bounded("sum(range(10 ** 12))"), Err(expected));
    assert_eq!(value(&kernel, "2 + 2"), "4");
}

#[test]
fn kernel_limits_propagate_to_subprocesses() {
    let kernel = limited(256, 2);
    let result = value(
        &kernel,
        "import subprocess, sys\ncode = 'import resource; print(resource.getrlimit(resource.RLIMIT_DATA)[0], resource.getrlimit(resource.RLIMIT_CPU)[0], resource.getrlimit(resource.RLIMIT_CORE)[1]); bytearray(512 * 1024 * 1024)'\nchild = subprocess.run([sys.executable, '-c', code], capture_output=True, text=True)\ndata, cpu, core = map(int, child.stdout.split())\n(data, 0 < cpu < 60, child.returncode != 0, 'MemoryError' in child.stderr, core)",
    );
    assert_eq!(result, format!("({}, True, True, True, 0)", 256 * MIB));
}

#[test]
fn kernel_environment_reaches_python_without_credentials() {
    let kernel = kernel_with(
        KernelOptions {
            env: vec![
                ("EXTRA_TOKEN".into(), "explicit".into()),
                ("PLAIN_EXTRA".into(), "1".into()),
            ],
            ..options()
        },
        null_handler(),
    );
    assert_eq!(
        value(
            &kernel,
            "import os\n[os.environ.get(name) for name in ('NO_COLOR', 'PYTHONDONTWRITEBYTECODE', 'ULTRON_RLM_OUTPUT_BYTES', 'EXTRA_TOKEN', 'PLAIN_EXTRA')]"
        ),
        "['1', '1', '20000', 'explicit', '1']"
    );
    assert_eq!(
        value(&kernel, "os.getcwd()"),
        format!("'{}'", env!("CARGO_MANIFEST_DIR"))
    );
    // The kernel leads its own process group.
    assert_eq!(value(&kernel, "os.getpgrp() == os.getpid()"), "True");
}
