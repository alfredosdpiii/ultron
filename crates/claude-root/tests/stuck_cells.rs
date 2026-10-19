//! A stuck cell never locks the root's `rlm` tool (2026-10-19: a hung `git push` in cell-102 held lane main, and
//! every later call of the Claude root was refused "REPL lane main is still running cell-102", with no way out).
//! A fake `git` on the injected PATH blocks reading a FIFO this test controls; it records its pid in a file the test
//! controls, and the test only ever looks at that pid (nothing here sends a signal: the product's cancel does).
//!
//! - `ultron mcp`: `notifications/cancelled` for the call cancels the cell as Esc does (the git child dies, the shell
//!   jobs of the turn stop), the call answers with the abort, and the next call runs in lane main;
//! - `ultron mcp`: without a cancel, the next call runs in a side lane with the note, the stuck call answers as
//!   detached, `await rlm.cancel(id)` from the side lane stops the stuck cell and its git child, and calls go back to
//!   main; a stuck cell left to end is delivered as a `cell_done` runtime event at the top of a later result;
//! - the native root lane (the `rlm` tool a native session runs) routes past a busy lane the same way.

use claude_root::control::ControlClient;
use claude_root::mcp_protocol::serve;
use claude_root::mcp_server::{Running, start};
use serde_json::{Value, json};
use session_core::RunEnv;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A private root: HOME, agent and server dirs, the work dir, and `bin/git`, which blocks reading `w/git.fifo`.
struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new().prefix("ucs-").tempdir_in("/tmp").unwrap();
        for sub in ["h", "a", "s", "r", "w", "tmp", "bin"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let root = Root { dir };
        // Pids are compared with /proc: the kernel runs unsandboxed here.
        std::fs::write(root.path("a/settings.json"), r#"{"sandbox": {"mode": "off"}}"#).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(root.path("w/git.fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        let git = root.path("bin/git");
        std::fs::write(
            &git,
            format!(
                "#!/bin/sh\necho $$ > {pid}.tmp && /bin/mv {pid}.tmp {pid}\nread line < {fifo}\necho \"pushed $line\"\n",
                pid = root.path("w/git.pid").display(),
                fifo = root.path("w/git.fifo").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&git, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        root
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    fn env(&self) -> RunEnv {
        let p = |s: &str| self.path(s).display().to_string();
        let vars: Vec<(String, String)> = vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", p("bin"))),
            ("HOME".into(), p("h")),
            ("ULTRON_CODING_AGENT_DIR".into(), p("a")),
            ("ULTRON_SERVER_DIR".into(), p("s")),
            ("XDG_RUNTIME_DIR".into(), p("r")),
            ("TMPDIR".into(), p("tmp")),
            ("ULTRON_SELF_COMMAND".into(), r#"["/opt/ultron/bin/ultron"]"#.into()),
            ("ULTRON_LOKI".into(), "off".into()),
            ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
            ("ULTRON_PYTHON".into(), python()),
            ("PI_OFFLINE".into(), "1".into()),
        ];
        RunEnv {
            cwd: self.path("w"),
            home: self.path("h"),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            docs: repo().join("packages/coding-agent/docs"),
        }
    }

    /// The PATH a cell runs `git` with: only the fake one's directory (the kernel and the host's shell take the
    /// server process's PATH, not the injected one, so the cells set it; nothing can reach a real git).
    fn git_path(&self) -> String {
        self.path("bin").display().to_string()
    }

    /// A cell that runs `git push` in the kernel (a subprocess of the kernel).
    fn push(&self) -> String {
        format!(
            "import os, subprocess\nr = subprocess.run(['git', 'push'], capture_output=True, text=True, env={{**os.environ, 'PATH': {:?}}})\nprint(r.stdout)",
            self.git_path()
        )
    }

    /// The fake git's pid, once it started (and the pid file is removed for the next one).
    fn git_pid(&self) -> i32 {
        let path = self.path("w/git.pid");
        wait_until("git starts", || path.exists());
        let pid = std::fs::read_to_string(&path).unwrap().trim().parse().unwrap();
        std::fs::remove_file(&path).unwrap();
        pid
    }

    /// Lets a blocked git read its line and end (nothing happens when no git is reading).
    fn release_git(&self) {
        // O_NONBLOCK: opening the FIFO for writing fails at once when no reader has it open.
        if let Ok(mut fifo) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(0o4000)
            .open(self.path("w/git.fifo"))
        {
            let _ = fifo.write_all(b"origin\n");
        }
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        // A git a failed test left blocked ends with its FIFO.
        self.release_git();
    }
}

/// Whether `pid` has ended (gone, or a zombie nobody reaped yet).
fn ended(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.rsplit_once(") ").is_some_and(|(_, rest)| rest.starts_with('Z')),
    }
}

/// Replies by id, read off the server's stdout as they come.
type Replies = Arc<(Mutex<HashMap<u64, Value>>, Condvar)>;

/// A server started through its product entry, its MCP connection on pipes; requests may overlap.
struct Server {
    running: Arc<Running>,
    stdin: Option<std::io::PipeWriter>,
    replies: Replies,
    serving: Option<std::thread::JoinHandle<()>>,
    next: u64,
}

impl Server {
    fn start(env: RunEnv) -> Server {
        let (running, handlers) = start(&[], &claude_root::Ctx { env }).unwrap();
        let (input, stdin) = std::io::pipe().unwrap();
        let (stdout, output) = std::io::pipe().unwrap();
        let serving = std::thread::spawn(move || {
            serve(handlers, &mut BufReader::new(input), Box::new(output));
        });
        let replies: Replies = Arc::default();
        let sink = replies.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let message: Value = serde_json::from_str(&line).unwrap();
                if let Some(id) = message["id"].as_u64() {
                    sink.0.lock().unwrap().insert(id, message);
                    sink.1.notify_all();
                }
            }
        });
        let mut server = Server {
            running,
            stdin: Some(stdin),
            replies,
            serving: Some(serving),
            next: 0,
        };
        let id = server.send("initialize", json!({"protocolVersion": "2025-06-18"}));
        server.reply(id);
        server
    }

    fn line(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    /// Sends a request; its id.
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.line(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn send_cell(&mut self, code: &str) -> u64 {
        self.send("tools/call", json!({"name": "rlm", "arguments": {"code": code}}))
    }

    fn reply(&self, id: u64) -> Value {
        let deadline = Instant::now() + HANG_GUARD;
        let mut replies = self.replies.0.lock().unwrap();
        loop {
            if let Some(reply) = replies.remove(&id) {
                return reply;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no reply to request {id}");
            replies = self.replies.1.wait_timeout(replies, left).unwrap().0;
        }
    }

    /// A call's result text, and whether it is an error.
    fn result(&self, id: u64) -> (String, bool) {
        let reply = self.reply(id);
        let result = &reply["result"];
        (
            result["content"][0]["text"].as_str().unwrap_or_default().to_string(),
            result["isError"] == true,
        )
    }

    fn cell(&mut self, code: &str) -> String {
        let id = self.send_cell(code);
        let (text, error) = self.result(id);
        assert!(!error, "{text}");
        text
    }

    fn pending_events(&self) -> u64 {
        let mut client = ControlClient::connect(Path::new(&self.running.socket()), HANG_GUARD).unwrap();
        client.request("status", Default::default(), HANG_GUARD).unwrap()["pendingEvents"]
            .as_u64()
            .unwrap_or(0)
    }

    fn close(mut self) {
        drop(self.stdin.take());
        self.serving.take().unwrap().join().unwrap();
        self.running.shutdown();
    }
}

/// `[lane main is still running cell-N (started Ns ago); this ran in lane main-side-1; ...]`: the cell id.
fn noted_cell(text: &str) -> String {
    let rest = text
        .strip_prefix("[lane main is still running ")
        .unwrap_or_else(|| panic!("no side-lane note: {text}"));
    let (cell, rest) = rest.split_once(" (started ").unwrap();
    let (secs, rest) = rest
        .split_once("s ago); this ran in lane main-side-1; await rlm.cancel(")
        .unwrap();
    assert!(secs.parse::<u64>().is_ok(), "{text}");
    assert!(rest.starts_with(&format!("\"{cell}\") stops it]\n")), "{text}");
    cell.to_string()
}

#[test]
fn claude_root_mcp_cancelled_call_cancels_its_stuck_cell_and_the_next_call_runs_in_main() {
    let root = Root::new();
    let mut server = Server::start(root.env());
    assert_eq!(server.cell("x = 1\nprint('warm')"), "warm");
    // A shell job of this turn: Esc stops it with the turn.
    let job = server.cell("job = await bash('sleep 3600', yield_after=0)\nprint(job.id)");
    let stuck = server.send_cell(&root.push());
    let git = root.git_pid();
    server.line(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": stuck}}));
    // The call answers with the abort, as an aborted rlm call does.
    let (text, error) = server.result(stuck);
    assert!(error);
    assert_eq!(text, "Abort requested");
    // The cell's kernel was killed with its git child.
    wait_until("git ends", || ended(git));
    // The next call runs in lane main, with no note and no side lane: a new kernel there, which restored the
    // lane's checkpoint.
    let next = server.cell("print('x' in globals(), [l['lane'] for l in await rlm.lanes()])");
    assert!(
        next.starts_with("True ['main']\n[RLM kernel restarted after: Abort requested]\n"),
        "{next}"
    );
    assert_eq!(
        server.cell("print([(j.id, j.running, j.cancelled) for j in await rlm.jobs()])"),
        format!("[('{job}', False, True)]")
    );
    server.close();
}

#[test]
fn claude_root_mcp_busy_lane_runs_the_next_call_in_a_side_lane_and_rlm_cancel_stops_the_stuck_cell() {
    let root = Root::new();
    let mut server = Server::start(root.env());
    assert_eq!(server.cell("x = 1\nprint('warm')"), "warm");
    // Through the host's bash this time (the host op is stopped, not the kernel).
    let stuck = server.send_cell(&format!(
        "print(await bash('PATH={} git push', yield_after=None))",
        root.git_path()
    ));
    let git = root.git_pid();
    let side = server.cell("print('side', 'x' in globals())");
    let cell = noted_cell(&side);
    assert!(side.ends_with("\nside False"), "{side}");
    // The stuck call is answered: its cell keeps running in the background.
    let (text, error) = server.result(stuck);
    assert!(!error);
    assert_eq!(
        text,
        format!(
            "Cell {cell} is still running in the background (it waits on the host); its end arrives as a <runtime_event kind=\"cell_done\" id=\"{cell}\">. Do other work or end your turn."
        )
    );
    // From the side lane, rlm.cancel stops it and its git child.
    let cancelled = server.cell(&format!("print(await rlm.cancel({cell:?}))"));
    assert_eq!(noted_cell(&cancelled), cell);
    assert!(cancelled.contains("\n{'cancelled': True, 'status': "), "{cancelled}");
    wait_until("git ends", || ended(git));
    // Main is free: the next call runs there (its kernel was killed past the cancel grace and restarted from the
    // lane's checkpoint).
    let next = server.cell("print(x, [l['lane'] for l in await rlm.lanes()])");
    assert!(
        next.starts_with("1 ['main']\n[RLM kernel restarted after: cancelled from another cell]\n"),
        "{next}"
    );
    server.close();
}

#[test]
fn claude_root_mcp_stuck_cell_left_to_end_arrives_as_a_cell_done_event() {
    let root = Root::new();
    let mut server = Server::start(root.env());
    assert_eq!(server.cell("print('warm')"), "warm");
    let stuck = server.send_cell(&root.push());
    root.git_pid();
    let side = server.cell("print('side')");
    let cell = noted_cell(&side);
    server.result(stuck);
    root.release_git();
    wait_until("the cell's end is waiting", || server.pending_events() > 0);
    let next = server.cell("print('next')");
    assert!(
        next.starts_with(&format!(
            "<runtime_event kind=\"cell_done\" id=\"{cell}\" status=\"completed\""
        )),
        "{next}"
    );
    assert!(next.contains("pushed origin"), "{next}");
    // Back in lane main: no note.
    assert!(next.ends_with("\nnext"), "{next}");
    assert!(!next.contains("[lane main"), "{next}");
    server.close();
}

#[test]
fn claude_root_native_root_lane_routes_past_a_busy_lane_and_rlm_cancel_stops_the_cell() {
    let root = Root::new();
    let env = root.env();
    let runtime = session_core::compose::Runtime::new(
        &env,
        "print",
        root.path("sessions"),
        session_core::compose::Services::default(),
    )
    .unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    let lane = Arc::new(session_core::lane::Lane::open(&runtime, &model, None, "native").unwrap());
    let run = |lane: &session_core::lane::Lane, code: &str| -> Result<String, String> {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let signal = ai::AbortSignal::new();
        let update = |_| {};
        let result = tokio.block_on(agent::AgentTool::execute(
            &*lane.tool,
            "call",
            json!({"code": code}),
            &signal,
            &update,
        ))?;
        Ok(result
            .content
            .iter()
            .find_map(|part| match part {
                ai::UserContent::Text(text) => Some(text.text.clone()),
                ai::UserContent::Image(_) => None,
            })
            .unwrap_or_default())
    };
    assert_eq!(run(&lane, "x = 1\nprint('warm')").unwrap(), "warm");
    let stuck_lane = lane.clone();
    let stuck = {
        let push = root.push();
        std::thread::spawn(move || run(&stuck_lane, &push))
    };
    let git = root.git_pid();
    let side = run(&lane, "print('side')").unwrap();
    let cell = noted_cell(&side);
    assert!(side.ends_with("\nside"), "{side}");
    assert!(
        stuck
            .join()
            .unwrap()
            .unwrap()
            .starts_with(&format!("Cell {cell} is still running in the background"))
    );
    let cancelled = run(&lane, &format!("print(await rlm.cancel({cell:?}))")).unwrap();
    assert_eq!(noted_cell(&cancelled), cell);
    wait_until("git ends", || ended(git));
    let next = run(&lane, "print(x, [l['lane'] for l in await rlm.lanes()])").unwrap();
    assert!(
        next.starts_with("1 ['main']\n[RLM kernel restarted after: cancelled from another cell]\n"),
        "{next}"
    );
    lane.shutdown();
}
