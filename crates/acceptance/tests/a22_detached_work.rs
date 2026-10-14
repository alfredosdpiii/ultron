//! A22 (partial): detached work and retained children survive UI loss.
//! Ported from packages/coding-agent/test/ultron-a22-detached-work.test.ts: real `ultron` RPC clients (the Rust
//! binary) drive their server generation, the coordinator and the session worker against a scripted provider in
//! private directories. Every test ends with no process of its own left and no socket in the server directory.
//!
//! The detached work is a subagent (`rlm.spawn`) instead of `background.start`, which has no Rust host; its result
//! is read with `rlm.collect`, not `background.result` / `background.inspect` (whose task record and lineage
//! assertions therefore are not ported). Subagents run on the session's durable runtime and their task journal is
//! in the session (`ultron.tasks/root`), so a later worker collects the result, and a worker that dies while a
//! child runs is restarted with the child reattached (a Rust Ultron Durable case beyond the TS file).

mod common;

use common::{Install, Provider, Reply, Request, rpc_tool_results};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const ROOT_PROMPT: &str = "start the slow job";
const CHILD_PROMPT: &str = "slow child work";
const CHILD_RESULT: &str = "CHILD_RESULT_A22";
const LONG_PROMPT: &str = "a long root turn";
const LONG_TOOL_OUTPUT: &str = "LONG_TOOL_RAN";
const LONG_RESULT: &str = "LONG_TURN_DONE";
/// A hang guard, never a pass condition: waits end on the condition they poll.
const HANG: Duration = Duration::from_secs(180);

/// A gate the scripted provider waits on.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
    fn wait(&self) {
        let mut open = self.0.0.lock().unwrap();
        while !*open {
            open = self.0.1.wait(open).unwrap();
        }
    }
}

#[derive(Clone, Default)]
struct Counts(Arc<Mutex<(usize, bool)>>);

/// The long root turn's provider side: its held model requests, the tool results it saw, whether it completed, and
/// whether the held reply was written.
#[derive(Clone, Default)]
struct Long(Arc<Mutex<LongState>>);

#[derive(Default)]
struct LongState {
    requests: usize,
    tool_results: usize,
    completed: bool,
    answered: bool,
}

/// Command lines of live, non-zombie processes mentioning `needle`.
fn owned(needle: &Path) -> Vec<(u32, String)> {
    let needle = needle.display().to_string();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if pid != std::process::id() && common::alive(pid) && cmdline.contains(&needle) {
            found.push((pid, cmdline));
        }
    }
    found
}

/// A message's text blocks, joined.
fn text_of(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        content => content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["text"].as_str())
            .collect(),
    }
}

fn worker_pids(root: &Path) -> Vec<u32> {
    owned(root)
        .into_iter()
        .filter(|(_, c)| c.contains("session-worker"))
        .map(|(pid, _)| pid)
        .collect()
}

fn poll<T>(timeout: Duration, mut read: impl FnMut() -> T, done: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let value = read();
        if done(&value) || Instant::now() >= deadline {
            return value;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sockets(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".sock"))
                .collect()
        })
        .unwrap_or_default()
}

/// The RPC client process of `session` (its own command line names the session id).
fn client_pid(session: &str) -> u32 {
    poll(
        HANG,
        || {
            std::fs::read_dir("/proc")
                .unwrap()
                .flatten()
                .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
                .find(|pid| {
                    let c = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                    let c = String::from_utf8_lossy(&c).replace('\0', " ");
                    c.contains("--mode rpc") && c.contains(session) && !c.contains("__internal")
                })
        },
        Option::is_some,
    )
    .expect("the client process")
}

fn signal(pid: u32, name: &str) {
    let status = std::process::Command::new("kill")
        .args([format!("-{name}"), pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

struct Fixture {
    provider: Provider,
    install: Install,
    release: Gate,
    child: Counts,
    release_long: Gate,
    long: Long,
    session: String,
}

impl Fixture {
    fn new(session: &str) -> Fixture {
        let release = Gate::default();
        let child = Counts::default();
        let (release_long, long) = (Gate::default(), Long::default());
        let (gate, counts) = (release.clone(), child.clone());
        let (long_gate, long_state) = (release_long.clone(), long.clone());
        let provider = Provider::start(move |request: &Request| {
            let users = request.users();
            let first = users.first().cloned().unwrap_or_default();
            let last_role = request.body["messages"].as_array().unwrap().last().unwrap()["role"].clone();
            if users.iter().any(|u| u == LONG_PROMPT) {
                if last_role == "tool" {
                    let mut state = long_state.0.lock().unwrap();
                    state.tool_results += 1;
                    state.completed = true;
                    let output = request.tool_results().last().cloned().unwrap_or_default();
                    return Reply::Text(format!("{LONG_RESULT}: {}", output.trim()));
                }
                if users.last().is_some_and(|u| u == LONG_PROMPT) {
                    // The root turn's model reply is held while its client goes away.
                    long_state.0.lock().unwrap().requests += 1;
                    long_gate.wait();
                    long_state.0.lock().unwrap().answered = true;
                    return Reply::Rlm(format!("print({LONG_TOOL_OUTPUT:?})"));
                }
            }
            if first.contains(CHILD_PROMPT) {
                counts.0.lock().unwrap().0 += 1;
                gate.wait();
                let reply = Reply::Text(CHILD_RESULT.into());
                counts.0.lock().unwrap().1 = true;
                return reply;
            }
            let last = users.last().cloned().unwrap_or_default();
            if last_role == "tool" {
                return Reply::Text("turn done".into());
            }
            if last == ROOT_PROMPT {
                return Reply::Rlm(format!(
                    "import json\nh = await rlm.spawn({CHILD_PROMPT:?}, name='slow')\nprint('JOB ' + json.dumps({{'id': h.rlm_child_id}}))"
                ));
            }
            if let Some(id) = last.strip_prefix("collect the job ") {
                return Reply::Rlm(format!(
                    "import json\nr = await rlm.collect([{id:?}])\nprint('RESULT ' + json.dumps(r[0]['result']))"
                ));
            }
            if last == "inspect the jobs" {
                return Reply::Rlm(
                    "import json\nprint('RESULTS ' + json.dumps(await rlm.results(), default=str))\nprint('LIST ' + json.dumps(await rlm.list_subagents(), default=str))".into(),
                );
            }
            Reply::Text("IDLE_OK".into())
        });
        let install = Install::new(&provider);
        Fixture {
            provider,
            install,
            release,
            child,
            release_long,
            long,
            session: session.into(),
        }
    }

    fn root(&self) -> &Path {
        self.install.root.path()
    }

    fn client(&self) -> cli::rpc::client::RpcClient {
        self.install.rpc(&["--session-id", &self.session], &[])
    }

    fn child_requests(&self) -> usize {
        self.child.0.lock().unwrap().0
    }

    fn child_completed(&self) -> bool {
        self.child.0.lock().unwrap().1
    }

    fn client_with(&self, env: &[(&str, &str)]) -> cli::rpc::client::RpcClient {
        self.install.rpc(&["--session-id", &self.session], env)
    }

    fn long(&self) -> (usize, usize, bool, bool) {
        let state = self.long.0.lock().unwrap();
        (state.requests, state.tool_results, state.completed, state.answered)
    }

    /// The long turn completed once: one held request, one tool result, and its answer carries the tool output.
    fn expect_long_turn_completed(&self, messages: &[Value]) {
        let answers: Vec<String> = messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(text_of)
            .collect();
        assert!(
            answers
                .iter()
                .any(|a| a == &format!("{LONG_RESULT}: {LONG_TOOL_OUTPUT}")),
            "{answers:?}"
        );
        assert!(
            rpc_tool_results(messages).iter().any(|t| t.trim() == LONG_TOOL_OUTPUT),
            "{messages:?}"
        );
        let (requests, tool_results, _, _) = self.long();
        assert_eq!((requests, tool_results), (1, 1));
    }

    /// Start the detached child from a real `rlm` call, then lose the UI (SIGTERM) while the child's model request
    /// is in flight. Returns the child id and the worker pid.
    fn start_job_and_lose_client(&self) -> (String, u32) {
        self.start_job_and_lose_client_by("TERM")
    }

    /// [`Fixture::start_job_and_lose_client`] with the client lost to `signal` (`TERM`: released; `KILL`: nothing
    /// sent, as from a crash or the OOM killer).
    fn start_job_and_lose_client_by(&self, lost: &str) -> (String, u32) {
        let mut first = self.client();
        first.mark();
        first
            .send(serde_json::json!({"type": "prompt", "message": ROOT_PROMPT}))
            .unwrap();
        poll(HANG, || self.child_requests(), |n| *n == 1);
        assert_eq!(self.child_requests(), 1);
        let line = poll(
            HANG,
            || {
                rpc_tool_results(&first.get_messages().unwrap())
                    .iter()
                    .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
                    .find(|l| l.starts_with("JOB "))
            },
            Option::is_some,
        )
        .expect("a JOB line");
        let job: Value = serde_json::from_str(&line[4..]).unwrap();
        let workers = worker_pids(self.root());
        assert_eq!(workers.len(), 1, "{workers:?}");

        // The UI goes away while the child is in flight: SIGTERM, as from a closed terminal, releases the worker;
        // SIGKILL sends nothing.
        let pid = client_pid(&self.session);
        let started = Instant::now();
        signal(pid, lost);
        // As in TS, the client handles SIGTERM: it releases the worker and exits 143.
        assert_eq!(first.stop(), if lost == "TERM" { Some(143) } else { None });
        assert!(started.elapsed() < Duration::from_secs(5));

        // The busy worker was handed off, not stopped: it still runs the child whose request is unanswered.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(worker_pids(self.root()), workers);
        assert!(!self.child_completed());
        (job["id"].as_str().unwrap().to_string(), workers[0])
    }

    /// Read the child's durable result through a scripted `rlm` call in `client`.
    fn collect(&self, client: &mut cli::rpc::client::RpcClient, id: &str) {
        client.prompt_and_wait(&format!("collect the job {id}"), HANG).unwrap();
        let tools = rpc_tool_results(&client.get_messages().unwrap());
        let line = tools
            .iter()
            .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
            .rfind(|l| l.starts_with("RESULT "))
            .unwrap_or_else(|| panic!("no RESULT line in {tools:?}"));
        let result: Value = serde_json::from_str(&line[7..]).unwrap();
        assert_eq!(
            (&result["status"], &result["value"]),
            (&"succeeded".into(), &CHILD_RESULT.into())
        );
    }

    /// `rlm.results()` and `rlm.list_subagents()` through a scripted `rlm` call: `(results, list)` as JSON.
    fn inspect(&self, client: &mut cli::rpc::client::RpcClient) -> (Value, Value) {
        client.prompt_and_wait("inspect the jobs", HANG).unwrap();
        let lines: Vec<String> = rpc_tool_results(&client.get_messages().unwrap())
            .iter()
            .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
            .collect();
        let find = |prefix: &str| -> Value {
            let line = lines
                .iter()
                .rfind(|l| l.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix} line in {lines:?}"));
            serde_json::from_str(&line[prefix.len()..]).unwrap()
        };
        (find("RESULTS "), find("LIST "))
    }

    /// Quit a client normally and require every process of this test and every socket to be gone.
    fn quit_and_expect_clean(&self, client: cli::rpc::client::RpcClient, timeout: Duration) {
        let (_, code) = client.finish(HANG);
        assert_eq!(code, Some(0));
        assert_eq!(poll(timeout, || owned(self.root()), Vec::is_empty), Vec::new());
        let server = self.root().join("server");
        assert_eq!(poll(HANG, || sockets(&server), Vec::is_empty), Vec::<String>::new());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.release.open();
        for (pid, _) in owned(self.install.root.path()) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let _ = &self.provider;
    }
}

#[test]
fn acceptance_a22_a_background_child_keeps_running_after_its_client_quits_and_a_new_client_reattaches() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000001");
    let (id, worker) = f.start_job_and_lose_client();

    // A second client reattaches to the same live worker and session.
    let mut second = f.client();
    let reattached = rpc_tool_results(&second.get_messages().unwrap());
    assert!(
        reattached.iter().any(|t| t.starts_with("JOB ") && t.contains(&id)),
        "{reattached:?}"
    );
    assert_eq!(worker_pids(f.root()), [worker]);

    // The child finishes at the provider after the UI loss.
    second.mark();
    f.release.open();
    assert!(poll(HANG, || f.child_completed(), |d| *d));
    // Its completion continues the root's run (a child_done event). Wait for this client to see that run settle:
    // the client refuses a prompt while it still sees a run, and `get_state` (the worker's view) can say idle
    // before the run's end reaches the client, which then refused the prompt as "already processing".
    second.wait_for(&|l| l["type"] == "agent_settled", HANG).unwrap();
    f.collect(&mut second, &id);
    f.quit_and_expect_clean(second, Duration::from_secs(15));
}

#[test]
fn acceptance_a22_detached_work_finishes_with_no_client_retires_and_stays_durable_for_a_later_client() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000002");
    let (id, worker) = f.start_job_and_lose_client();
    f.release.open();
    assert!(poll(HANG, || f.child_completed(), |d| *d));
    // Nobody is attached: once the work is committed the worker retires and the coordinator follows it.
    assert_eq!(poll(HANG, || owned(f.root()), Vec::is_empty), Vec::new());

    // A later client starts a fresh worker for the session: the child's completion was committed to the session
    // while nobody was attached, and its result is collected from the session's task journal.
    let mut later = f.client();
    let messages = later.get_messages().unwrap();
    let event = messages
        .iter()
        .filter(|m| m["role"] == "user" || m["role"] == "custom")
        .map(Value::to_string)
        .find(|m| m.contains("child_done"))
        .unwrap_or_else(|| panic!("no child_done event in {messages:#?}"));
    assert!(event.contains(&id), "{event}");
    assert!(event.contains("status=\\\"succeeded\\\""), "{event}");
    assert!(event.contains(CHILD_RESULT), "{event}");
    assert!(!worker_pids(f.root()).contains(&worker));
    f.collect(&mut later, &id);
    assert_eq!(f.child_requests(), 1);
    f.quit_and_expect_clean(later, HANG);
}

#[test]
fn acceptance_a22_quitting_with_no_active_work_leaves_no_processes() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000003");
    let mut client = f.client();
    client.prompt_and_wait("say hello", HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("IDLE_OK"));
    assert_eq!(worker_pids(f.root()).len(), 1);
    // Idle cleanup is prompt: no worker waits out the orphan grace.
    f.quit_and_expect_clean(client, Duration::from_secs(3));
}

/// A long root turn whose client quits (`TERM`, stdin closed, `KILL`) while its model reply is held: leaving is not
/// an abort. With a signal, a new client reattaches to the same worker and sees the turn finish; with stdin closed,
/// nobody is attached: the turn finishes, the worker retires, and a later client reads the result.
fn root_turn_in_flight_when_its_client_quits(quit: &str, session: &str) {
    let f = Fixture::new(session);
    let mut first = f.client();
    first
        .send(serde_json::json!({"type": "prompt", "message": LONG_PROMPT}))
        .unwrap();
    poll(HANG, || f.long().0, |n| *n == 1);
    let workers = worker_pids(f.root());
    assert_eq!(workers.len(), 1, "{workers:?}");
    let code = match quit {
        "stdin close" => first.stop(),
        signal_name => {
            signal(client_pid(&f.session), signal_name);
            first.stop()
        }
    };
    assert_eq!(
        code,
        match quit {
            "TERM" => Some(143),
            "stdin close" => Some(0),
            _ => None,
        }
    );
    assert_eq!(worker_pids(f.root()), workers);
    assert!(!f.long().2);
    if quit != "stdin close" {
        // Reattach while the turn is in flight, then watch it finish in the same worker.
        let mut second = f.client();
        assert_eq!(worker_pids(f.root()), workers);
        f.release_long.open();
        let messages = poll(
            HANG,
            || second.get_messages().unwrap(),
            |messages| {
                messages
                    .iter()
                    .any(|m| m["role"] == "assistant" && text_of(m).starts_with(LONG_RESULT))
            },
        );
        f.expect_long_turn_completed(&messages);
        f.quit_and_expect_clean(second, Duration::from_secs(15));
        return;
    }
    f.release_long.open();
    assert!(poll(HANG, || f.long().2, |done| *done));
    assert_eq!(
        poll(Duration::from_secs(15), || owned(f.root()), Vec::is_empty),
        Vec::new()
    );
    let mut later = f.client();
    let messages = later.get_messages().unwrap();
    f.expect_long_turn_completed(&messages);
    f.quit_and_expect_clean(later, Duration::from_secs(15));
}

#[test]
fn acceptance_a22_a_root_turn_in_flight_when_its_client_quits_sigterm_completes_and_a_later_client_sees_it() {
    root_turn_in_flight_when_its_client_quits("TERM", "a22a22a2-0000-4000-8000-000000000011");
}

#[test]
fn acceptance_a22_a_root_turn_in_flight_when_its_client_quits_stdin_close_completes_and_a_later_client_sees_it() {
    root_turn_in_flight_when_its_client_quits("stdin close", "a22a22a2-0000-4000-8000-000000000012");
}

#[test]
fn acceptance_a22_a_root_turn_in_flight_when_its_client_quits_sigkill_completes_and_a_later_client_sees_it() {
    root_turn_in_flight_when_its_client_quits("KILL", "a22a22a2-0000-4000-8000-000000000013");
}

#[test]
fn acceptance_a22_an_explicit_abort_still_stops_the_root_turn() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000014");
    let mut client = f.client();
    client
        .send(serde_json::json!({"type": "prompt", "message": LONG_PROMPT}))
        .unwrap();
    poll(HANG, || f.long().0, |n| *n == 1);
    client.send(serde_json::json!({"type": "abort"})).unwrap();
    assert!(poll(
        Duration::from_secs(10),
        || client.get_state().unwrap()["isStreaming"] == false,
        |idle| *idle
    ));
    f.release_long.open();
    // The held reply is written to an aborted request: nothing runs it.
    assert!(poll(Duration::from_secs(10), || f.long().3, |answered| *answered));
    let messages = client.get_messages().unwrap();
    assert_eq!(f.long().1, 0);
    assert!(
        !messages
            .iter()
            .any(|m| m["role"] == "assistant" && text_of(m).contains(LONG_RESULT)),
        "{messages:?}"
    );
    f.quit_and_expect_clean(client, Duration::from_secs(5));
}

#[test]
fn acceptance_a22_a_hard_killed_idle_client_leaves_no_process_behind_within_seconds() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000015");
    let mut client = f.client();
    client.prompt_and_wait("say hello", HANG).unwrap();
    assert_eq!(worker_pids(f.root()).len(), 1);
    signal(client_pid(&f.session), "KILL");
    assert_eq!(client.stop(), None);
    let killed = Instant::now();
    assert_eq!(
        poll(Duration::from_secs(10), || owned(f.root()), Vec::is_empty),
        Vec::new()
    );
    assert!(killed.elapsed() < Duration::from_secs(3), "{:?}", killed.elapsed());
}

#[test]
fn acceptance_a22_a_hard_killed_clients_background_job_completes_stays_durable_and_then_everything_exits() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000016");
    let (id, _) = f.start_job_and_lose_client_by("KILL");
    f.release.open();
    assert!(poll(Duration::from_secs(10), || f.child_completed(), |d| *d));
    assert_eq!(
        poll(Duration::from_secs(10), || owned(f.root()), Vec::is_empty),
        Vec::new()
    );
    let mut later = f.client();
    f.collect(&mut later, &id);
    assert_eq!(f.child_requests(), 1);
    f.quit_and_expect_clean(later, Duration::from_secs(15));
}

#[test]
fn acceptance_a22_a_client_restarted_right_after_a_hard_kill_reattaches_to_the_busy_worker() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000017");
    let (id, worker) = f.start_job_and_lose_client_by("KILL");
    let mut second = f.client();
    assert_eq!(worker_pids(f.root()), [worker]);
    second.mark();
    f.release.open();
    assert!(poll(HANG, || f.child_completed(), |d| *d));
    // The child_done run settles in this client before it prompts (see the reattach test).
    second.wait_for(&|l| l["type"] == "agent_settled", HANG).unwrap();
    f.collect(&mut second, &id);
    assert_eq!(f.child_requests(), 1);
    f.quit_and_expect_clean(second, Duration::from_secs(15));
}

#[test]
fn acceptance_a22_a_client_restarted_within_the_orphan_grace_reattaches_to_the_idle_worker() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000018");
    // A grace longer than CLI startup makes the restart land inside it deterministically.
    let grace = [("__PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS", "20000")];
    let mut first = f.client_with(&grace);
    first.prompt_and_wait("say hello", HANG).unwrap();
    let workers = worker_pids(f.root());
    assert_eq!(workers.len(), 1);
    signal(client_pid(&f.session), "KILL");
    assert_eq!(first.stop(), None);
    let mut second = f.client_with(&grace);
    assert_eq!(worker_pids(f.root()), workers);
    assert_eq!(second.get_last_assistant_text().unwrap().as_deref(), Some("IDLE_OK"));
    f.quit_and_expect_clean(second, Duration::from_secs(5));
}

/// Ultron Durable: the worker dies (SIGKILL) while a subagent's model request is in flight. Its server restarts it;
/// the restarted worker reattaches the child (its runtime resends the child's pinned request), the child ends, the
/// root answers its `child_done`, and the client collects the result and sees it in `rlm.results()` and
/// `rlm.list_subagents()` under the same id, from the session's task journal.
#[test]
fn acceptance_a22_a_running_child_survives_a_worker_kill_and_its_result_stays_collectable() {
    let f = Fixture::new("a22a22a2-0000-4000-8000-000000000019");
    let mut client = f.client();
    client
        .send(serde_json::json!({"type": "prompt", "message": ROOT_PROMPT}))
        .unwrap();
    poll(HANG, || f.child_requests(), |n| *n == 1);
    let id = poll(
        HANG,
        || {
            rpc_tool_results(&client.get_messages().unwrap_or_default())
                .iter()
                .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
                .find(|l| l.starts_with("JOB "))
        },
        Option::is_some,
    )
    .map(|line| {
        serde_json::from_str::<Value>(&line[4..]).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    })
    .expect("a JOB line");
    let workers = worker_pids(f.root());
    assert_eq!(workers.len(), 1, "{workers:?}");
    signal(workers[0], "KILL");
    // The reattached child asks its model again, with the request it had pinned.
    assert_eq!(poll(HANG, || f.child_requests(), |n| *n == 2), 2);
    let restarted = worker_pids(f.root());
    assert_eq!(restarted.len(), 1, "{restarted:?}");
    assert_ne!(restarted, workers);
    let child_bodies: Vec<Value> = f
        .provider
        .requests()
        .into_iter()
        .filter(|r| r.users().first().is_some_and(|u| u.contains(CHILD_PROMPT)))
        .map(|r| r.body)
        .collect();
    assert_eq!(child_bodies.len(), 2);
    assert_eq!(
        child_bodies[1], child_bodies[0],
        "the child's resent request is the pinned one"
    );
    f.release.open();
    // The child's end continues the root's run: its child_done event is answered.
    let messages = poll(
        HANG,
        || client.get_messages().unwrap_or_default(),
        |messages| {
            messages
                .iter()
                .any(|m| (m["role"] == "user" || m["role"] == "custom") && m.to_string().contains("child_done"))
                && messages.last().is_some_and(|m| m["role"] == "assistant")
        },
    );
    let event = messages
        .iter()
        .filter(|m| m["role"] == "user" || m["role"] == "custom")
        .map(Value::to_string)
        .find(|m| m.contains("child_done"))
        .unwrap_or_else(|| panic!("no child_done event in {messages:#?}"));
    assert!(event.contains(&id) && event.contains(CHILD_RESULT), "{event}");
    poll(
        HANG,
        || client.get_state().map(|s| s["isStreaming"] == false).unwrap_or(false),
        |idle| *idle,
    );
    f.collect(&mut client, &id);
    let (results, list) = f.inspect(&mut client);
    assert!(
        results.to_string().contains(&id) && results.to_string().contains(CHILD_RESULT),
        "{results}"
    );
    let listed = list.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{list}");
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["result"]["value"], CHILD_RESULT);
    assert_eq!(f.child_requests(), 2);
    f.quit_and_expect_clean(client, Duration::from_secs(15));
}
