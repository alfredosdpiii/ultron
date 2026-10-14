//! A56: the job-end cases of packages/coding-agent/test/ultron-shell-jobs.test.ts that the first port left out, with
//! their literal expectations: every job's end is reported once (`ShellJobs.onEnd`: `awaited`, `rootAborted`, the
//! record with its root and `jobSummary`), which is what the `job_done` completion events are made of; `cancelRoot`
//! stops the jobs an aborted root turn started and announces nothing; a child lane sees only its own jobs.
//!
//! The kernel is wired like the worker's: each lane's requests go to `host::Host` as that lane
//! (`Host::handle_lane`), and a job's root is the lane's root turn (`Host::set_roots`, TS `host.rootOf(caller)`).
//! Where the TypeScript test sleeps until a job ends, these jobs wait for a file the test writes.

mod common;

use common::{runtime, wait_until};
use host::{AllowAll, Host, JobEnd};
use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A lane's requests, as the worker routes them.
struct Lane {
    host: Arc<Host>,
    lane: String,
}

impl HostHandler for Lane {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self.host.handle_lane(&self.lane, kind, payload, signal)
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    host: Arc<Host>,
    ends: Arc<Mutex<Vec<JobEnd>>>,
    root: Arc<Mutex<Option<String>>>,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let host = Arc::new(Host::new(
            dir.path(),
            dir.path().join(".jobs"),
            dir.path(),
            Box::new(AllowAll),
        ));
        let ends = Arc::new(Mutex::new(Vec::new()));
        let seen = ends.clone();
        host.on_job_end(Arc::new(move |end| seen.lock().unwrap().push(end)));
        let root = Arc::new(Mutex::new(Some("turn:run-1".to_string())));
        let current = root.clone();
        host.set_roots(Arc::new(move |_| current.lock().unwrap().clone()));
        Fixture { dir, host, ends, root }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn kernel(&self, lane: &str, env: &[(&str, &str)]) -> Kernel {
        let mut options = KernelOptions::new(self.path(), runtime());
        options.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let handler = Arc::new(Lane {
            host: self.host.clone(),
            lane: lane.into(),
        });
        Kernel::new(options, handler).expect("kernel")
    }

    fn ends(&self) -> Vec<JobEnd> {
        self.ends.lock().unwrap().clone()
    }

    fn go(&self, name: &str) {
        std::fs::write(self.path().join(name), "").unwrap();
    }
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

/// A command that runs until the test writes `file`.
fn until(file: &str, then: &str) -> String {
    format!("while [ ! -e {file} ]; do sleep 0.05; done; {then}")
}

#[test]
fn acceptance_a56_yield_after_handle_outlives_the_cell_and_result_waits_and_reports_once() {
    let f = Fixture::new();
    let k = f.kernel("main", &[]);
    assert_eq!(
        ok(
            &k,
            &format!(
                "job = await bash('''{}''', yield_after=0)\n(job.running, job.status, job.exit_code, job.id.startswith('job-'))",
                until("go", "echo done-$((20+22))")
            )
        ),
        "(True, 'running', None, True)"
    );
    // The cell ended; the job keeps running in the host.
    assert!(f.ends().is_empty());
    let id = ok(&k, "job.id").replace('\'', "");
    let waiting = std::thread::scope(|scope| {
        let result = scope.spawn(|| {
            ok(
                &k,
                "r = await job.result()\n(r is job, job.running, job.ok, job.exit_code, job.text.strip())",
            )
        });
        // The job ends only once the cell waits on it.
        wait_until("the cell waits on the job", || f.host.job_waiters(&id) == 1);
        f.go("go");
        result.join().unwrap()
    });
    assert_eq!(waiting, "(True, False, True, 0, 'done-42')");
    // The cell was waiting when it ended: the result came inline, so no completion is announced.
    let ends = f.ends();
    assert_eq!(ends.len(), 1);
    assert!(ends[0].awaited && !ends[0].root_aborted, "{:?}", ends[0]);
    assert_eq!(ends[0].event(), None);
    assert!(exec(&k, "print(job)").stdout.contains("done-42\n[job "));
    k.shutdown();
}

#[test]
fn acceptance_a56_an_unawaited_job_reports_its_end_once_with_a_bounded_summary_and_a_quick_one_finishes_inline() {
    let f = Fixture::new();
    let k = f.kernel("main", &[]);
    ok(
        &k,
        &format!(
            "job = await bash('''{}''', yield_after=0)",
            until("go", "echo line one; echo 12 passed")
        ),
    );
    f.go("go");
    wait_until("the job's end", || f.ends().len() == 1);
    let end = &f.ends()[0];
    assert!(!end.awaited && !end.root_aborted);
    assert_eq!(
        (end.record.status.as_str(), end.record.exit_code),
        ("completed", Some(0))
    );
    assert_eq!(
        host::journal::job_summary(&end.record, 240),
        "exit 0; line one | 12 passed"
    );
    assert_eq!(end.record.root_id.as_deref(), Some("turn:run-1"));
    let event = end.event().expect("an unawaited end is announced");
    assert_eq!(event["kind"], "job_done");
    assert_eq!(event["summary"], "exit 0; line one | 12 passed");
    assert_eq!(event["rootId"], "turn:run-1");
    assert_eq!(event["lane"], "main");
    assert_eq!(
        ok(
            &k,
            "q = await bash('''echo quick''', yield_after=120)\n(q.running, q.ok, q.text)"
        ),
        "(False, True, 'quick\\n')"
    );
    assert_eq!(f.ends().iter().filter(|e| !e.awaited).count(), 1);
    k.shutdown();
}

#[test]
fn acceptance_a56_an_aborted_root_turn_cancels_its_jobs_and_announces_nothing_other_roots_keep_running() {
    let f = Fixture::new();
    let k = f.kernel("main", &[]);
    ok(&k, "a = await bash('''sleep 300''', yield_after=0)");
    *f.root.lock().unwrap() = Some("turn:run-2".into());
    ok(
        &k,
        &format!("b = await bash('''{}''', yield_after=0)", until("go-b", "echo b")),
    );
    f.host.cancel_root("turn:run-1");
    assert_eq!(ok(&k, "(await rlm.job(a.id)).status"), "'cancelled'");
    let ends = f.ends();
    assert_eq!(ends.len(), 1);
    assert!(ends[0].root_aborted);
    assert_eq!(ends[0].record.root_id.as_deref(), Some("turn:run-1"));
    assert_eq!(ends[0].event(), None, "an aborted root's job is not announced");
    f.go("go-b");
    wait_until("the other root's job ends", || f.ends().len() == 2);
    let other = &f.ends()[1];
    assert!(!other.root_aborted);
    assert_eq!(
        (other.record.status.as_str(), other.record.root_id.as_deref()),
        ("completed", Some("turn:run-2"))
    );
    k.shutdown();
}

#[test]
fn acceptance_a56_a_child_lane_sees_only_its_own_jobs_and_plain_bash_still_blocks() {
    let f = Fixture::new();
    let main = f.kernel("main", &[]);
    let child = f.kernel("ultron.rlm-child.task-1", &[]);
    ok(&main, "m = await bash('''echo main''', yield_after=120)");
    let id = ok(&main, "m.id").replace('\'', "");
    assert_eq!(ok(&child, "await rlm.jobs()"), "[]");
    let refused = exec(&child, &format!("await rlm.job('{id}')"));
    assert_eq!(refused.status, Status::Error);
    let evalue = refused.error.map(|e| e.evalue).unwrap_or_default();
    assert!(evalue.contains(&format!("Unknown shell job {id}")), "{evalue}");
    assert_eq!(ok(&main, &format!("(await rlm.job('{id}')).ok")), "True");
    // The child's own job is its own; the root lane sees every job.
    ok(&child, "c = await bash('''echo child''', yield_after=120)");
    assert_eq!(ok(&child, "[j.id for j in await rlm.jobs()] == [c.id]"), "True");
    assert_eq!(ok(&main, "len(await rlm.jobs())"), "2");
    assert_eq!(
        ok(
            &main,
            "out = await bash('''echo plain; exit 2'''); (type(out).__name__, str(out))"
        ),
        "('BashOutput', 'plain\\n[exit code 2]')"
    );
    assert!(f.path().join(".jobs").exists());
    main.shutdown();
    child.shutdown();
}

#[test]
fn acceptance_a56_plain_bash_auto_detaches_a_slow_command_then_announces_one_event() {
    let f = Fixture::new();
    let k = f.kernel("main", &[("ULTRON_BASH_YIELD_AFTER", "5")]);
    assert_eq!(
        ok(
            &k,
            &format!(
                "out = await bash('''{}''')\nout.running",
                until("go", "echo late-$((40+2))").replace("while", "echo early; while")
            )
        ),
        "True"
    );
    // Nobody waits on it: its end is announced once, as for any detached job.
    f.go("go");
    wait_until("the job's end", || f.ends().len() == 1);
    let end = &f.ends()[0];
    assert!(!end.awaited);
    assert_eq!(
        (end.record.status.as_str(), end.record.exit_code),
        ("completed", Some(0))
    );
    assert_eq!(host::journal::job_summary(&end.record, 240), "exit 0; early | late-42");
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
fn acceptance_a56_plain_bash_within_the_window_announces_nothing() {
    let f = Fixture::new();
    let k = f.kernel("main", &[("ULTRON_BASH_YIELD_AFTER", "120")]);
    assert_eq!(
        ok(
            &k,
            "out = await bash('''echo plain; exit 2'''); (str(out), out.exit_code, out.ok, out.running, out.job)"
        ),
        "('plain\\n[exit code 2]', 2, False, False, None)"
    );
    ok(
        &k,
        "big = await bash('''for i in $(seq 1 3000); do echo line-$i; done''')",
    );
    assert_eq!(
        ok(
            &k,
            "t = await bash('''sleep 300''', timeout=0.3); (t.timed_out, t.ok, str(t).endswith('[timed out after 0.3s]'))"
        ),
        "(True, False, True)"
    );
    let ends = f.ends();
    assert_eq!(ends.len(), 3);
    assert!(ends.iter().all(|e| e.awaited), "{ends:?}");
    k.shutdown();
}
