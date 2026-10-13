//! A56 (partial): long-running work does not block the turn.
//! Ported from packages/coding-agent/test/ultron-shell-jobs.test.ts with its literal expectations: the real
//! runtime.py under the Rust kernel supervisor, its host requests served by `host::Host` (host-owned shell jobs and
//! the blocking bash), as session-worker.ts routes them.
//!
//! The persisted jobs journal across a worker restart ("the journal stays small and survives a worker restart") is
//! ported over `Host::with_journal`, with one deliberate difference: a job whose process still runs reads as running
//! after the restart (Rust keeps job processes past their worker), not `interrupted` as in TypeScript, where the
//! process dies with the worker. A TypeScript journal's running job still reads as `interrupted`
//! (crates/host/tests/durable_jobs.rs), and a SIGKILLed worker's jobs report their real state there too.
//!
//! Not ported (no Rust counterpart yet): job end reporting (`ShellJobs.onEnd`, the `awaited` / `rootAborted` flags
//! and `jobSummary` that feed `job_done` events), root ids and `cancelRoot`, per-lane job visibility (every job
//! reports lane "main"), a configurable retention cap (fixed at 4 MiB).

mod common;

use common::{alive, read_pids, runtime, wait_until};
use host::{AllowAll, Host};
use kernel::{Kernel, KernelOptions, Status};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A kernel wired like the worker: every request goes to the host, which runs `bash` with a yield_after as a job.
fn kernel(host: &Arc<Host>, cwd: &Path, env: &[(&str, &str)]) -> Kernel {
    let mut options = KernelOptions::new(cwd, runtime());
    options.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Kernel::new(options, host.clone()).expect("kernel")
}

fn host(cwd: &Path) -> Arc<Host> {
    Arc::new(Host::new(cwd, cwd.join(".jobs"), cwd, Box::new(AllowAll)))
}

fn exec(kernel: &Kernel, code: &str) -> kernel::ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|e| panic!("{code}: {e}"))
}

fn ok(kernel: &Kernel, code: &str) -> String {
    let result = exec(kernel, code);
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

#[test]
fn acceptance_a56_yield_after_handle_outlives_the_cell_and_result_waits_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[]);
    // The job runs until the test creates `go`, so it is still running when the cell reads it, however slow.
    assert_eq!(
        ok(
            &k,
            "job = await bash('''i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo done-$((20+22))''', yield_after=0)\n(job.running, job.status, job.exit_code, job.id.startswith('job-'))"
        ),
        "(True, 'running', None, True)"
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    assert_eq!(
        ok(
            &k,
            "r = await job.result()\n(r is job, job.running, job.ok, job.exit_code, job.text.strip())"
        ),
        "(True, False, True, 0, 'done-42')"
    );
    assert!(exec(&k, "print(job)").stdout.contains("done-42\n[job "));
    k.shutdown();
}

#[test]
fn acceptance_a56_quick_job_finishes_inline_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[]);
    assert_eq!(
        ok(
            &k,
            "q = await bash('''echo quick''', yield_after=120)\n(q.running, q.ok, q.text)"
        ),
        "(False, True, 'quick\\n')"
    );
    k.shutdown();
}

#[test]
fn acceptance_a56_job_survives_a_kernel_restart_and_is_recovered_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let first = kernel(&host, dir.path(), &[]);
    let id = ok(
        &first,
        "job = await bash('''sleep 0.5; echo survived''', yield_after=0); job.id",
    )
    .replace('\'', "");
    first.shutdown();
    let second = kernel(&host, dir.path(), &[]);
    assert_eq!(ok(&second, "'job' in globals()"), "False");
    assert_eq!(
        ok(
            &second,
            &format!(
                "j = await rlm.job({id:?})\nawait j.result()\n(j.ok, j.text.strip(), [x.id for x in await rlm.jobs()][:1] == [j.id])"
            )
        ),
        "(True, 'survived', True)"
    );
    assert!(
        ok(&second, "(await rlm.jobs())[0]").contains(&format!("ShellJob(id='{id}', status='completed', exit_code=0"))
    );
    second.shutdown();
}

#[test]
fn acceptance_a56_cancel_stops_the_tree_timeout_and_wait_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[]);
    let pid_file = dir.path().join("child.pid");
    ok(
        &k,
        &format!(
            "job = await bash('''sleep 300 & echo $! > {}; wait''', yield_after=0.3)",
            pid_file.display()
        ),
    );
    // The job may yield before its shell writes the pid: wait for it.
    let pid = read_pids(&pid_file, 1)[0];
    assert_eq!(
        ok(&k, "s = await job.result(wait=0.2); (s.running, s.status)"),
        "(True, 'running')"
    );
    assert_eq!(
        ok(
            &k,
            "await job.cancel(); (job.running, job.cancelled, job.status, job.exit_code)"
        ),
        "(False, True, 'cancelled', None)"
    );
    wait_until("the job's child to die", || !alive(pid));
    assert_eq!(
        ok(
            &k,
            "t = await bash('''sleep 300''', timeout=0.3, yield_after=0)\nawait t.result()\n(t.timed_out, t.ok, t.status)"
        ),
        "(True, False, 'timed_out')"
    );
    k.shutdown();
}

#[test]
fn acceptance_a56_job_output_is_bounded_head_and_tail_and_readable_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[]);
    let result = ok(
        &k,
        "big = await bash('''for i in $(seq 1 20000); do echo line-$i; done''', yield_after=120)\n(big.truncated, len(big.text.encode()) < 17000, big.text.startswith('line-1\\n'), big.text.rstrip().endswith('line-20000'), 'bytes omitted' in big.text, big.output_bytes)",
    );
    let prefix = "(True, True, True, True, True, ";
    assert!(result.starts_with(prefix) && result.ends_with(')'), "{result}");
    let output_bytes: usize = result[prefix.len()..result.len() - 1].parse().unwrap();
    assert_eq!(
        ok(
            &k,
            "c = await big.read(cursor=0, max_bytes=100); (c['text'][:7], c['next_cursor'], c['truncated'])"
        ),
        "('line-1\\n', 100, False)"
    );
    // TS sets a 64 KiB retention cap; the Rust host's is fixed at 4 MiB, above this output, so all of it is kept.
    let path = ok(&k, "big.output_path").replace('\'', "");
    assert_eq!(std::fs::read(path).unwrap().len(), output_bytes);
    assert_eq!(host::JOB_TEXT_BYTES, 16 * 1024);
    k.shutdown();
}

#[test]
fn acceptance_a56_plain_bash_still_blocks_and_returns_a_string_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let main = kernel(&host, dir.path(), &[]);
    ok(&main, "m = await bash('''echo main''', yield_after=120)");
    let id = ok(&main, "m.id").replace('\'', "");
    assert_eq!(ok(&main, &format!("(await rlm.job('{id}')).ok")), "True");
    assert_eq!(
        ok(
            &main,
            "out = await bash('''echo plain; exit 2'''); (type(out).__name__, str(out))"
        ),
        "('BashOutput', 'plain\\n[exit code 2]')"
    );
    assert!(dir.path().join(".jobs").exists());
    main.shutdown();
}

#[test]
fn acceptance_a56_plain_bash_auto_detaches_a_slow_command_ported_part() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    // The window must outlast `echo early` however loaded the machine is; the rest waits for the test's `go`.
    let k = kernel(&host, dir.path(), &[("ULTRON_BASH_YIELD_AFTER", "5")]);
    assert_eq!(
        ok(
            &k,
            "out = await bash('''echo early; i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo late-$((40+2))''')\n(isinstance(out, str), type(out).__name__, out.running, out.ok, out.exit_code, out.job.id.startswith('job-'), out.job.running)"
        ),
        "(True, 'BashOutput', True, False, None, True, True)"
    );
    let text = exec(&k, "print(out)").stdout;
    let rest = text.strip_prefix("early\n[still running as job job-").expect(&text);
    let hex = rest.chars().take_while(char::is_ascii_hexdigit).count();
    assert!(hex > 0, "{text}");
    assert_eq!(
        &rest[hex..],
        " after 5 s; its completion will arrive as a runtime event, or `await <result>.job.result()` to wait]\n"
    );
    assert_eq!(ok(&k, "(out['running'], out.get('job') is out.job)"), "(True, True)");
    std::fs::write(dir.path().join("go"), "").unwrap();
    assert_eq!(
        ok(
            &k,
            "j = await out.job.result(); (j.running, j.ok, j.exit_code, j.text.split())"
        ),
        "(False, True, 0, ['early', 'late-42'])"
    );
    k.shutdown();
}

#[test]
fn acceptance_a56_plain_bash_within_the_window_returns_the_whole_output() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[("ULTRON_BASH_YIELD_AFTER", "120")]);
    assert_eq!(
        ok(
            &k,
            "out = await bash('''echo plain; exit 2'''); (str(out), out.exit_code, out.ok, out.running, out.job)"
        ),
        "('plain\\n[exit code 2]', 2, False, False, None)"
    );
    assert_eq!(
        ok(
            &k,
            "big = await bash('''for i in $(seq 1 3000); do echo line-$i; done'''); (big.truncated, len(big.splitlines()), big.splitlines()[-1])"
        ),
        "(False, 3000, 'line-3000')"
    );
    assert_eq!(
        ok(
            &k,
            "t = await bash('''sleep 300''', timeout=0.3); (t.timed_out, t.ok, str(t).endswith('[timed out after 0.3s]'))"
        ),
        "(True, False, True)"
    );
    k.shutdown();
}

#[test]
fn acceptance_a56_yield_after_none_and_off_block_without_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let k = kernel(&host, dir.path(), &[("ULTRON_BASH_YIELD_AFTER", "0.2")]);
    let before = ok(&k, "len(await rlm.jobs())");
    let started = Instant::now();
    assert_eq!(
        ok(
            &k,
            "b = await bash('''sleep 0.8; echo blocked''', yield_after=None); (str(b), b.running, b.ok)"
        ),
        "('blocked', False, True)"
    );
    assert!(started.elapsed() >= Duration::from_millis(700));
    let off = kernel(&host, dir.path(), &[("ULTRON_BASH_YIELD_AFTER", "off")]);
    assert_eq!(
        ok(&off, "b = await bash('''sleep 0.5; echo off'''); (str(b), b.running)"),
        "('off', False)"
    );
    assert_eq!(ok(&off, "len(await rlm.jobs())"), before);
    k.shutdown();
    off.shutdown();
}

/// ultron-shell-jobs.test.ts "the journal stays small and survives a worker restart: finished jobs keep their text,
/// running ones read as interrupted" (Rust: running ones whose process runs read as running; see the module docs).
#[test]
fn acceptance_a56_the_journal_stays_small_and_survives_a_worker_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let journal = cwd.join("jobs.json");
    let open = || {
        Arc::new(
            Host::new(&cwd, cwd.join(".jobs2"), &cwd, Box::new(AllowAll))
                .with_journal(Arc::new(host::journal::FileJobJournal::new(&journal)))
                .unwrap(),
        )
    };
    let first = open();
    let k = kernel(&first, &cwd, &[]);
    assert_eq!(
        ok(
            &k,
            "done = await bash('echo kept-output', yield_after=0); (await done.result()).status"
        ),
        "'completed'"
    );
    ok(
        &k,
        "running = await bash('i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done', yield_after=0)",
    );
    k.shutdown();
    let saved = std::fs::read_to_string(&journal).unwrap();
    // Output lives in the spill files, not in the journal that is rewritten with every job.
    assert!(!saved.contains("kept-output\\n"), "{saved}");
    assert!(saved.len() < 2000, "{}", saved.len());
    // One owner at a time: the first worker hands its jobs over before the second opens the journal.
    first.release_jobs();
    let second = open();
    let k = kernel(&second, &cwd, &[]);
    assert_eq!(
        ok(
            &k,
            "[running, finished] = await rlm.jobs()\nfinished = await rlm.job(finished.id)\n(running.status, running.error, finished.status, finished.ok, finished.text)"
        ),
        "('running', None, 'completed', True, 'kept-output\\n')"
    );
    let restored = host::journal::restore(
        Some(&serde_json::from_str(&std::fs::read_to_string(&journal).unwrap()).unwrap()),
        0,
    )
    .unwrap();
    let finished = restored.records.iter().find(|r| r.status == "completed").unwrap();
    assert_eq!(host::journal::job_summary(finished, 240), "exit 0; kept-output");
    std::fs::write(cwd.join("go"), "").unwrap();
    assert_eq!(ok(&k, "(await running.result()).status"), "'completed'");
    k.shutdown();
}
