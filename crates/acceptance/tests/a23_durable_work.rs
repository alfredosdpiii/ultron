//! A23 (durable work): schedules fire on their own and exactly once, and the session goal keeps going, across a
//! worker killed with SIGKILL. A child process (this test binary in a child role) owns the stores: the task journal
//! (`delegation::agents::Agents` over a file), the schedules and goal documents (`kernel_apis::FileDocumentStore`)
//! and the shell jobs journal. It is killed at a named phase; the parent reopens the same files through
//! `kernel_apis::DurableWork`, the composition root's entry point, and checks what recovery did.
//!
//! Plus the wiring harness: `DurableWork` driven the way the composition root drives it, with the model's calls going
//! through the real runtime.py (`schedules.*`, `goal.*`, `bash(..., yield_after=0)`, `rlm.jobs()`).
//!
//! Every wait is on an event (a tick report on a channel, a file, a process ending) with a 120 s hang guard.

mod agents_support;
mod common;

use agents_support::{Options, aborted, adapter};
use common::HANG;
use delegation::agents::Agents;
use delegation::task_store::TaskStore;
use host::journal::FileJobJournal;
use host::{AllowAll, Host};
use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use kernel_apis::goal::TaskGoalJobs;
use kernel_apis::{
    DocumentStore, DurableWork, DurableWorkOptions, FileDocumentStore, GoalDriver, GoalJobs, KernelApis,
    ManualTickClock, SchedulesModule, TaskHost,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const CHILD: &str = "ULTRON_A23_DURABLE_CHILD";
const T0: i64 = 1_000_000;

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG;
    while !condition() {
        assert!(Instant::now() < deadline, "hang guard: {what}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The task journal in one JSON file (the session value `ultron.rlm/tasks`).
struct FileTasks(PathBuf);

impl TaskStore for FileTasks {
    fn read(&mut self) -> Result<Option<Value>, String> {
        FileDocumentStore::new(&self.0).read()
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        FileDocumentStore::new(&self.0).write(document)
    }
}

/// One worker over the files in `dir`: the typed-agent host (identity@1 echoes its input; a background job's lane
/// waits until it is stopped), the schedules module and the goal driver on a manual clock at `now`.
struct Worker {
    agents: Agents,
    tasks: Arc<dyn TaskHost>,
    schedules: Arc<SchedulesModule>,
    goal: Arc<GoalDriver>,
    host: Arc<Host>,
    clock: Arc<ManualTickClock>,
}

fn worker(dir: &Path, now: i64, phase: Phase) -> Worker {
    let fixture = agents_support::fixture_with_stores(
        Options {
            deterministic: Some(adapter(|_, input, _, _| Ok(input.clone()))),
            script: Some(Arc::new(|_, _, signal, _| aborted(signal))),
            ..Options::default()
        },
        Box::new(FileTasks(dir.join("tasks.json"))),
        Box::new(delegation::task_store::MemoryStore::default()),
    );
    let agents = fixture.agents.clone();
    let tasks: Arc<dyn TaskHost> = Arc::new(Phased {
        inner: agents.clone(),
        phase,
        dir: dir.to_path_buf(),
    });
    let clock = ManualTickClock::new(now);
    let host = Arc::new(
        Host::new(dir, dir.join(".jobs"), dir, Box::new(AllowAll))
            .with_journal(Arc::new(FileJobJournal::new(dir.join("jobs.json"))))
            .unwrap(),
    );
    let schedules = Arc::new(SchedulesModule::new(
        Arc::new(FileDocumentStore::new(dir.join("schedules.json"))),
        Some(clock.as_clock()),
    ));
    let jobs: Arc<dyn GoalJobs> = Arc::new(PhasedGoalJobs {
        inner: TaskGoalJobs::new(tasks.clone(), host.clone()),
        phase,
        dir: dir.to_path_buf(),
    });
    let goal = Arc::new(GoalDriver::new(
        Arc::new(FileDocumentStore::new(dir.join("goal.json"))),
        jobs,
        clock.as_clock(),
    ));
    Worker {
        agents,
        tasks,
        schedules,
        goal,
        host,
        clock,
    }
}

impl Worker {
    /// The kernel-API handler of the root lane, as the composition root registers it.
    fn apis(&self) -> KernelApis {
        let inner: Arc<dyn HostHandler> = Arc::new(self.agents.handler("main", self.host.clone()));
        KernelApis::new(inner)
            .with_host(self.tasks.clone(), "main")
            .with(self.schedules.clone())
            .with(self.goal.clone())
    }

    /// `DurableWork::open` with every tick report sent to the returned channel.
    fn open(&self) -> (DurableWork, Receiver<Result<Value, String>>) {
        let (reports, ticks) = channel();
        let reports = Mutex::new(reports);
        let work = DurableWork::open(
            DurableWorkOptions::new(self.tasks.clone())
                .schedules(self.schedules.clone())
                .goal(self.goal.clone())
                .jobs(self.host.clone())
                .clock(self.clock.clone())
                .on_tick(Arc::new(move |report: &Result<Value, String>| {
                    let _ = kernel_apis::util::lock(&reports).send(report.clone());
                })),
        )
        .unwrap();
        (work, ticks)
    }

    fn journal(&self) -> Vec<Value> {
        self.agents.tasks().unwrap()
    }
}

fn call(apis: &KernelApis, kind: &str, payload: Value) -> Value {
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    apis.handle(kind, payload, &Signal::new())
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
}

/// Where a child worker stops (writes `phase` and blocks until killed).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    None,
    /// In a schedule firing: after the slot is claimed, before the task is admitted.
    Claimed,
    /// In a schedule firing: after the task is admitted, before the schedule records it.
    Admitted,
    /// In a goal run: after the job task is admitted, before the goal records it.
    GoalAdmitted,
}

fn reached(dir: &Path, phase: Phase) -> ! {
    std::fs::write(dir.join("phase.tmp"), format!("{phase:?}")).unwrap();
    std::fs::rename(dir.join("phase.tmp"), dir.join("phase")).unwrap();
    loop {
        std::thread::park();
    }
}

struct Phased {
    inner: Agents,
    phase: Phase,
    dir: PathBuf,
}

impl TaskHost for Phased {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        self.inner.caller_task_id(lane)
    }
    fn task_lane(&self, task_id: &str) -> Option<String> {
        TaskHost::task_lane(&self.inner, task_id)
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        self.inner.strategy(definition)
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        TaskHost::tasks(&self.inner)
    }
    fn spawn(&self, request: &Value, parent: Option<&str>, signal: &Signal) -> Result<Value, String> {
        let schedule = request["key"].as_str().is_some_and(|key| key.starts_with("schedule:"));
        if schedule && self.phase == Phase::Claimed {
            reached(&self.dir, self.phase);
        }
        let task = TaskHost::spawn(&self.inner, request, parent, signal)?;
        if schedule && self.phase == Phase::Admitted {
            reached(&self.dir, self.phase);
        }
        Ok(task)
    }
    fn result(&self, task_id: &str) -> Result<Value, String> {
        TaskHost::result(&self.inner, task_id)
    }
    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
        TaskHost::cancel(&self.inner, task_id, reason)
    }
    fn steer(&self, task_id: &str, message: &str, signal: &Signal) -> Result<bool, String> {
        TaskHost::steer(&self.inner, task_id, message, signal)
    }
    fn now(&self) -> i64 {
        TaskHost::now(&self.inner)
    }
}

struct PhasedGoalJobs {
    inner: TaskGoalJobs,
    phase: Phase,
    dir: PathBuf,
}

impl GoalJobs for PhasedGoalJobs {
    fn run_check(&self, command: &str, timeout: u64) -> Result<kernel_apis::goal::GoalCheckResult, String> {
        self.inner.run_check(command, timeout)
    }
    fn start_job(&self, prompt: &str, key: &str, root: &str, signal: &Signal) -> Result<String, String> {
        let job = self.inner.start_job(prompt, key, root, signal)?;
        if self.phase == Phase::GoalAdmitted {
            reached(&self.dir, self.phase);
        }
        Ok(job)
    }
    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        self.inner.stop_job(task_id, reason)
    }
    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        self.inner.lane_of_job(task_id)
    }
    fn job_state(&self, task_id: &str) -> Option<kernel_apis::JobState> {
        self.inner.job_state(task_id)
    }
    fn job_with_key(&self, key: &str) -> Option<String> {
        self.inner.job_with_key(key)
    }
}

/// Runs `test` in a child process with `role`, waits until it reaches a phase or writes `ready`, then SIGKILLs it.
fn run_child_until_killed(test: &str, role: &str, dir: &Path, signal_file: &str) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{role}\n{}", dir.display()))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&format!("the child's {signal_file}"), || dir.join(signal_file).exists());
    child.kill().unwrap();
    child.wait().unwrap();
}

/// The child role named by the environment, if this process is a child.
fn child_role() -> Option<(String, PathBuf)> {
    let value = std::env::var(CHILD).ok()?;
    let (role, dir) = value.split_once('\n')?;
    Some((role.to_string(), PathBuf::from(dir)))
}

fn ready(dir: &Path, value: &Value) -> ! {
    std::fs::write(dir.join("ready.tmp"), value.to_string()).unwrap();
    std::fs::rename(dir.join("ready.tmp"), dir.join("ready")).unwrap();
    loop {
        std::thread::park();
    }
}

/// Child: a schedule every second from T0; its first firing stops at `phase`.
fn schedule_child(dir: &Path, phase: Phase) -> ! {
    let w = worker(dir, T0, phase);
    let (_work, ticks) = w.open();
    call(
        &w.apis(),
        "schedules.create",
        json!({"definition": "identity@1", "input": {"n": 1}, "every_ms": 1000}),
    );
    if phase == Phase::None {
        // The ticker fired slot 0 on its own; keep its report for the parent.
        let report = ticks.recv_timeout(HANG).unwrap().unwrap();
        ready(dir, &report);
    }
    // The firing blocks at its phase; the parent kills this process there.
    loop {
        std::thread::park();
    }
}

fn schedule_of(dir: &Path) -> Value {
    FileDocumentStore::new(dir.join("schedules.json"))
        .read()
        .unwrap()
        .unwrap()["schedules"][0]
        .clone()
}

fn temp() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().canonicalize().unwrap();
    (temp, dir)
}

/// After a crash at `phase` of the first firing, the reopened worker's ticker delivers that slot exactly once.
fn crash_during_firing(test: &str, phase: Phase) {
    let (_temp, dir) = temp();
    run_child_until_killed(test, &format!("{phase:?}"), &dir, "phase");
    let saved = schedule_of(&dir);
    assert_eq!(
        (&saved["pending_slot"], &saved["last_slot"], &saved["runs"]),
        (&json!(0), &json!(0), &json!(0)),
        "the slot was claimed before the crash: {saved}"
    );
    let key = format!("schedule:{}:0", saved["id"].as_str().unwrap());
    let w = worker(&dir, T0 + 10, Phase::None);
    let admitted_before = w.journal().iter().filter(|t| t["key"] == key.as_str()).count();
    assert_eq!(
        admitted_before,
        usize::from(phase == Phase::Admitted),
        "admitted before the crash"
    );
    let (work, ticks) = w.open();
    assert_eq!(
        work.recovered().next_fire,
        Some(i64::MIN),
        "a claimed slot is due at once"
    );
    let report = ticks.recv_timeout(HANG).unwrap().unwrap();
    let tasks: Vec<Value> = w.journal().into_iter().filter(|t| t["key"] == key.as_str()).collect();
    assert_eq!(tasks.len(), 1, "the slot ran exactly once: {tasks:?}");
    assert_eq!(
        report,
        json!({"fired": [{"schedule_id": saved["id"], "slot": 0, "task_id": tasks[0]["id"], "skipped": 0}], "failed": []})
    );
    let recorded = schedule_of(&dir);
    assert_eq!(
        (&recorded["pending_slot"], &recorded["runs"], &recorded["last_task_id"]),
        (&Value::Null, &json!(1), &tasks[0]["id"])
    );
    work.close();
    w.agents.close();
}

#[test]
fn acceptance_a23_durable_a_fire_runs_exactly_once_across_a_crash_between_claim_and_admission() {
    if let Some((_, dir)) = child_role() {
        schedule_child(&dir, Phase::Claimed);
    }
    crash_during_firing(
        "acceptance_a23_durable_a_fire_runs_exactly_once_across_a_crash_between_claim_and_admission",
        Phase::Claimed,
    );
}

#[test]
fn acceptance_a23_durable_a_fire_runs_exactly_once_across_a_crash_between_admission_and_record() {
    if let Some((_, dir)) = child_role() {
        schedule_child(&dir, Phase::Admitted);
    }
    crash_during_firing(
        "acceptance_a23_durable_a_fire_runs_exactly_once_across_a_crash_between_admission_and_record",
        Phase::Admitted,
    );
}

/// Slots missed while no worker ran are due at once on reopen and coalesce into one run of the latest slot, fired
/// by the ticker without any request (TS semantics: `skipped` counts the missed slots).
#[test]
fn acceptance_a23_durable_missed_fires_while_down_coalesce_and_fire_on_their_own_after_reopen() {
    if let Some((_, dir)) = child_role() {
        schedule_child(&dir, Phase::None);
    }
    let (_temp, dir) = temp();
    run_child_until_killed(
        "acceptance_a23_durable_missed_fires_while_down_coalesce_and_fire_on_their_own_after_reopen",
        "None",
        &dir,
        "ready",
    );
    let first: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("ready")).unwrap()).unwrap();
    let id = first["fired"][0]["schedule_id"].as_str().unwrap().to_string();
    assert_eq!(first["fired"][0]["slot"], 0);

    let w = worker(&dir, T0 + 5500, Phase::None);
    let (work, ticks) = w.open();
    assert_eq!(
        work.recovered().next_fire,
        Some(T0 + 1000),
        "recomputed from the durable schedule"
    );
    let report = ticks.recv_timeout(HANG).unwrap().unwrap();
    assert_eq!(report["fired"][0]["slot"], 5);
    assert_eq!(report["fired"][0]["skipped"], 4);
    assert_eq!(report["failed"], json!([]));
    let saved = schedule_of(&dir);
    assert_eq!(
        (&saved["runs"], &saved["last_slot"], &saved["skipped_total"]),
        (&json!(2), &json!(5), &json!(4))
    );
    let keys: Vec<Value> = w.journal().iter().map(|t| t["key"].clone()).collect();
    assert_eq!(
        keys,
        vec![json!(format!("schedule:{id}:0")), json!(format!("schedule:{id}:5"))]
    );
    // The next slot fires when the clock reaches it.
    w.clock.set(T0 + 6000);
    work.wake();
    assert_eq!(ticks.recv_timeout(HANG).unwrap().unwrap()["fired"][0]["slot"], 6);
    work.close();
    w.agents.close();
}

/// Child: a session goal with a failing check; its first run is admitted and stops at `phase`, or runs (and waits)
/// after one check.
fn goal_child(dir: &Path, phase: Phase) -> ! {
    let w = worker(dir, T0, phase);
    let _work = w.open();
    let signal = Signal::new();
    w.goal.command("make the build pass", &signal).unwrap();
    w.goal.command("check exit 1", &signal).unwrap();
    let check = w.goal.handle_request("goal.check", &Map::new(), "main").unwrap();
    ready(dir, &json!({"goal": w.goal.get().unwrap(), "check": check}));
}

#[test]
fn acceptance_a23_durable_the_session_goal_keeps_its_checks_and_runs_on_after_the_worker_is_killed() {
    if let Some((_, dir)) = child_role() {
        goal_child(&dir, Phase::None);
    }
    let (_temp, dir) = temp();
    run_child_until_killed(
        "acceptance_a23_durable_the_session_goal_keeps_its_checks_and_runs_on_after_the_worker_is_killed",
        "None",
        &dir,
        "ready",
    );
    let before: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("ready")).unwrap()).unwrap();
    let goal_id = before["goal"]["id"].as_str().unwrap().to_string();
    let first_job = before["goal"]["job"].clone();
    assert_eq!(
        (
            &before["goal"]["status"],
            &before["goal"]["runs"],
            &before["check"]["exit_code"]
        ),
        (&json!("active"), &json!(1), &json!(1))
    );

    let w = worker(&dir, T0 + 60_000, Phase::None);
    let (work, _ticks) = w.open();
    let goal = work.recovered().goal.clone().unwrap();
    // The run the kill interrupted is followed by the next one; the goal stays active with its checks.
    assert_eq!(
        (&goal["id"], &goal["status"], &goal["revision"], &goal["runs"]),
        (&json!(goal_id), &json!("active"), &json!(2), &json!(2))
    );
    assert_eq!(
        (&goal["checks"], &goal["stuck"], &goal["last_check"]["exit_code"]),
        (&json!(1), &json!(1), &json!(1))
    );
    assert_eq!(
        goal["last_answer"],
        "The previous run was interrupted when Ultron restarted; continue from the goal's state and the files."
    );
    assert_ne!(goal["job"], first_job);
    let journal = w.journal();
    let state_of = |key: String| {
        journal
            .iter()
            .filter(|t| t["key"] == key.as_str())
            .map(|t| match t.get("result") {
                Some(result) => result["status"].clone(),
                None => json!("live"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(state_of(format!("{goal_id}:1")), vec![json!("interrupted")]);
    assert_eq!(state_of(format!("{goal_id}:2")), vec![json!("live")]);
    assert_eq!(journal.len(), 2);
    // Progress goes on from the recorded state: the same failure again counts as the second in a row.
    let check = w.goal.handle_request("goal.check", &Map::new(), "main").unwrap();
    assert_eq!(check["exit_code"], 1);
    let goal = w.goal.get().unwrap().unwrap();
    assert_eq!((&goal["checks"], &goal["stuck"]), (&json!(2), &json!(1 + 1)));
    work.close();
    w.agents.close();
}

#[test]
fn acceptance_a23_durable_a_goal_run_admitted_before_the_crash_is_adopted_not_started_twice() {
    if let Some((_, dir)) = child_role() {
        goal_child(&dir, Phase::GoalAdmitted);
    }
    let (_temp, dir) = temp();
    run_child_until_killed(
        "acceptance_a23_durable_a_goal_run_admitted_before_the_crash_is_adopted_not_started_twice",
        "GoalAdmitted",
        &dir,
        "phase",
    );
    let saved = FileDocumentStore::new(dir.join("goal.json")).read().unwrap().unwrap()["goal"].clone();
    assert_eq!(
        (&saved["status"], &saved["runs"], &saved["job"]),
        (&json!("active"), &json!(1), &Value::Null),
        "the run was written, its task not yet"
    );
    let goal_id = saved["id"].as_str().unwrap().to_string();
    let w = worker(&dir, T0, Phase::None);
    let (work, _ticks) = w.open();
    let goal = work.recovered().goal.clone().unwrap();
    let journal = w.journal();
    let keys: Vec<Value> = journal.iter().map(|t| t["key"].clone()).collect();
    // Run 1's task was adopted (and, interrupted by the kill, followed by run 2): one task per run.
    assert_eq!(keys, vec![json!(format!("{goal_id}:1")), json!(format!("{goal_id}:2"))]);
    assert_eq!((&goal["status"], &goal["runs"]), (&json!("active"), &json!(2)));
    assert_eq!(goal["job"], journal[1]["id"]);
    work.close();
    w.agents.close();
}

/// The wiring harness: the composition root's calls in order, with the model's calls made through runtime.py.
/// Open, the model schedules work and starts a shell job, the ticker fires on its own; the worker hands over;
/// a successor opens the same files and sees the schedule, the goal and the job, then closes.
#[test]
fn acceptance_a23_durable_work_wiring_drives_schedules_goal_and_jobs_like_the_composition_root() {
    let (_temp, dir) = temp();
    let kernel = |w: &Worker| {
        let mut options = KernelOptions::new(&dir, common::runtime());
        options.env.push(("ULTRON_RLM_SPILL_DIR".into(), String::new()));
        Kernel::new(options, Arc::new(w.apis())).unwrap()
    };
    let ok = |kernel: &Kernel, code: &str| {
        let result = kernel.execute(code, None, None).unwrap();
        assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
        result.result.unwrap_or_default()
    };

    let first = worker(&dir, T0, Phase::None);
    let (work, ticks) = first.open();
    assert_eq!(work.recovered().next_fire, None);
    let k = kernel(&first);
    ok(&k, "s = await schedules.create('identity@1', {'n': 1}, 1000)");
    let fired = ticks.recv_timeout(HANG).unwrap().unwrap();
    assert_eq!(fired["fired"][0]["slot"], 0, "fired by the tick, not by a request");
    assert_eq!(
        ok(
            &k,
            "j = await bash('i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo handed-over', yield_after=0); j.running"
        ),
        "True"
    );
    first.goal.command("keep the docs current", &Signal::new()).unwrap();
    k.shutdown();
    work.hand_over();
    first.agents.close();
    drop(first);

    let second = worker(&dir, T0 + 1000, Phase::None);
    let (work, ticks) = second.open();
    let recovered = work.recovered().clone();
    assert_eq!(recovered.next_fire, Some(T0 + 1000));
    assert_eq!(recovered.goal.as_ref().unwrap()["status"], "active");
    assert_eq!(recovered.goal.as_ref().unwrap()["runs"], 2);
    assert_eq!(recovered.jobs[0]["status"], "running");
    assert_eq!(ticks.recv_timeout(HANG).unwrap().unwrap()["fired"][0]["slot"], 1);
    let k = kernel(&second);
    assert_eq!(
        ok(&k, "[(x['runs'], x['last_slot']) for x in await schedules.list()]"),
        "[(2, 1)]"
    );
    assert_eq!(
        ok(&k, "g = await goal.get(); (g['status'], g['runs'])"),
        "('active', 2)"
    );
    std::fs::write(dir.join("go"), "").unwrap();
    assert_eq!(
        ok(
            &k,
            "j = (await rlm.jobs())[0]; await j.result(); (j.status, j.exit_code, j.text)"
        ),
        "('completed', 0, 'handed-over\\n')"
    );
    k.shutdown();
    work.close();
    second.agents.close();
}
