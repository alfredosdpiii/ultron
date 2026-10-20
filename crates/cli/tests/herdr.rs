//! Ultron in herdr's agents panel (crates/herdr, crate::herdr_report, claude_root::herdr_hook), through the product
//! entries: the TUI in tmux, `-p`, `ultron mcp` + `ultron hook` (what `ultron claude` runs) and `ultron claude
//! --print-config`. herdr is a FAKE unix socket in a temp dir (HERDR_* injected into the spawned ultron); the real
//! herdr socket and the real `claude` are never used, no provider is reached (a scripted 127.0.0.1 server).

mod common;
mod executable_support;
mod tui_harness;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const HANG_GUARD: Duration = Duration::from_secs(120);
const BIN: &str = env!("CARGO_BIN_EXE_ultron");

/// Connections accepted and the request lines read.
type Seen = Mutex<(usize, Vec<Value>)>;

/// A fake herdr: every request line it got (and how many connections), each answered `{"id", "result": {}}`.
#[derive(Clone, Default)]
struct FakeHerdr {
    seen: Arc<(Seen, Condvar)>,
}

impl FakeHerdr {
    fn start(path: &Path) -> FakeHerdr {
        let listener = UnixListener::bind(path).unwrap();
        let fake = FakeHerdr::default();
        let seen = fake.seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                seen.0.lock().unwrap().0 += 1;
                let mut line = String::new();
                if BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .is_err()
                    || line.is_empty()
                {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
                let _ = writeln!(stream, "{}", json!({"id": request["id"], "result": {}}));
                seen.0.lock().unwrap().1.push(request);
                seen.1.notify_all();
            }
        });
        fake
    }

    fn lines(&self) -> Vec<Value> {
        self.seen.0.lock().unwrap().1.clone()
    }

    fn connections(&self) -> usize {
        self.seen.0.lock().unwrap().0
    }

    /// Wait for the first request after `from` that matches; returns its index.
    fn wait(&self, from: usize, what: &str, matches: impl Fn(&Value) -> bool) -> usize {
        let deadline = Instant::now() + HANG_GUARD;
        let mut seen = self.seen.0.lock().unwrap();
        loop {
            if let Some(i) = seen.1.iter().skip(from).position(&matches) {
                return from + i;
            }
            assert!(Instant::now() < deadline, "no {what} after #{from}: {:#?}", seen.1);
            seen = self.seen.1.wait_timeout(seen, Duration::from_millis(200)).unwrap().0;
        }
    }

    fn wait_state(&self, from: usize, state: &str) -> usize {
        self.wait(from, state, |l| {
            l["method"] == "pane.report_agent" && l["params"]["state"] == state
        })
    }

    fn wait_method(&self, from: usize, method: &str) -> usize {
        self.wait(from, method, |l| l["method"] == method)
    }
}

fn herdr_env(socket: &Path) -> Vec<(String, String)> {
    vec![
        ("HERDR_ENV".into(), "1".into()),
        ("HERDR_SOCKET_PATH".into(), socket.display().to_string()),
        ("HERDR_PANE_ID".into(), "w1:t2:p3".into()),
        ("HERDR_TAB_ID".into(), "w1:t2".into()),
        ("HERDR_WORKSPACE_ID".into(), "w1".into()),
    ]
}

fn borrowed(env: &[(String, String)]) -> Vec<(&str, &str)> {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// The TUI in `root` inside a herdr pane on `socket`. The TUI's notifications run `herdr notification show`: a FAKE
/// `herdr` comes first on PATH, so the real herdr CLI never runs.
fn herdr_tui(root: &Path, port: u16, socket: &Path) -> Tui {
    let bin = root.join("fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    executable_support::install(&bin.join("herdr"), "#!/bin/sh\nexit 0\n");
    let mut env = herdr_env(socket);
    env.push((
        "PATH".into(),
        format!("{}:{}:/usr/bin:/bin", bin.display(), root.join("no-browser").display()),
    ));
    Tui::start_at(
        root,
        port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &borrowed(&env),
    )
}

/// Every request is Ultron's, for the pane, with a strictly increasing `seq` and an `ultron:<ms>:<6>` id.
fn check_common(lines: &[Value]) {
    let mut last = 0;
    for line in lines {
        let params = &line["params"];
        assert_eq!(params["pane_id"], "w1:t2:p3", "{line}");
        assert_eq!(params["source"], "ultron", "{line}");
        assert_eq!(params["agent"], "ultron", "{line}");
        let seq = params["seq"].as_u64().unwrap();
        assert!(seq > last, "seq not increasing: {lines:#?}");
        last = seq;
        let id = line["id"].as_str().unwrap();
        let parts: Vec<&str> = id.split(':').collect();
        assert!(parts.len() == 3 && parts[0] == "ultron" && parts[2].len() == 6, "{id}");
    }
}

fn states(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["method"] == "pane.report_agent")
        .map(|l| l["params"]["state"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn cli_herdr_tui_session_lifecycle_reports_session_states_metadata_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let herdr = FakeHerdr::start(&socket);
    let mock = Mock::start(
        vec![
            text_reply("Done."),
            text_reply("The tests pass.\n\nShould I also update the docs?"),
        ],
        Some(("Herdr greeting chat".into(), false)),
    );
    let tmp = TempDir::new();
    let tui = herdr_tui(&tmp.0, mock.port, &socket);
    let root = tui.root().display().to_string();
    let session = herdr.wait_method(0, "pane.report_agent_session");
    let line = herdr.lines()[session].clone();
    let id = line["params"]["agent_session_id"].as_str().unwrap().to_string();
    let path = line["params"]["agent_session_path"].as_str().unwrap().to_string();
    assert_eq!(line["params"]["session_start_source"], "startup");
    assert_eq!(
        line["params"]["resume_argv"],
        json!([
            "env",
            format!("ULTRON_SERVER_DIR={root}/server"),
            format!("ULTRON_CODING_AGENT_DIR={root}/agent"),
            "ultron",
            "--session",
            id
        ])
    );
    assert!(path.ends_with(".jsonl") && path.starts_with(&root), "{path}");
    let idle = herdr.wait_state(session, "idle");
    // The state report carries the session too.
    assert_eq!(herdr.lines()[idle]["params"]["agent_session_id"], json!(id));
    herdr.wait(0, "first metadata", |l| {
        l["method"] == "pane.report_metadata"
            && l["params"]["display_agent"] == "Ultron"
            && l["params"]["tokens"]["model"] == "claude-sonnet-4-5"
            && l["params"]["tokens"]["cost"] == "$0.000"
    });

    tui.wait("ultron v");
    tui.send_text("hello there");
    tui.send_keys(&["Enter"]);
    let working = herdr.wait_state(idle + 1, "working");
    let idle = herdr.wait_state(working + 1, "idle");
    assert_eq!(herdr.lines()[idle]["params"].get("message"), None);
    tui.wait("Done.");
    // The session's name (the naming call), its context use and cost arrive as metadata.
    herdr.wait(working, "metadata after the run", |l| {
        l["method"] == "pane.report_metadata"
            && l["params"]["title"] == "Herdr greeting chat"
            && l["params"]["tokens"]["ctx"].as_str().is_some_and(|c| c.ends_with('%'))
            && l["params"]["tokens"]["cost"]
                .as_str()
                .is_some_and(|c| c.starts_with('$'))
    });

    tui.send_text("and next");
    tui.send_keys(&["Enter"]);
    let working = herdr.wait_state(idle + 1, "working");
    let blocked = herdr.wait_state(working + 1, "blocked");
    assert_eq!(
        herdr.lines()[blocked]["params"]["message"],
        "Should I also update the docs?"
    );
    tui.wait("update the docs?");

    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    let release = herdr.wait_method(blocked + 1, "pane.release_agent");
    tui.wait("<exited>");
    let lines = herdr.lines();
    assert_eq!(release, lines.len() - 1, "the release is the last report: {lines:#?}");
    check_common(&lines);
    assert_eq!(states(&lines), ["idle", "working", "idle", "working", "blocked"]);
    let sessions = lines
        .iter()
        .filter(|l| l["method"] == "pane.report_agent_session")
        .count();
    assert_eq!(sessions, 1);
    // Metadata is coalesced: a handful of reports, not one per event.
    let metadata = lines.iter().filter(|l| l["method"] == "pane.report_metadata").count();
    assert!((1..=8).contains(&metadata), "{metadata} metadata reports: {lines:#?}");
}

#[test]
fn cli_herdr_socket_that_never_accepts_does_not_hold_the_tui() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    // A stuck herdr: listening with a backlog of 0, never accepting, its one queued connection taken, so every
    // connect to it waits (until the reporter's connect timeout, on the reporter's thread). A report sent from the
    // main or render thread without that timeout would hold the TUI here forever.
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let listener =
        rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC, None).unwrap();
    rustix::net::bind(&listener, &SocketAddrUnix::new(&socket).unwrap()).unwrap();
    rustix::net::listen(&listener, 0).unwrap();
    let _queued = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mock = Mock::start(vec![text_reply("Done.")], None);
    let tmp = TempDir::new();
    let tui = herdr_tui(&tmp.0, mock.port, &socket);
    tui.wait("ultron v");
    tui.send_text("hello");
    tui.send_keys(&["Enter"]);
    tui.wait("Done.");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// `ultron -p <prompt>` in a sandbox against a scripted reply, with `env` added.
fn print_run(reply: &str, env: &[(String, String)], settings: Option<&str>) -> (std::process::Output, PathBuf) {
    let (port, _seen) = common::mock_server(
        vec![common::anthropic_text(reply)],
        common::Naming::Title("Print run".into()),
    );
    let sandbox = common::Sandbox::new(port);
    if let Some(settings) = settings {
        std::fs::write(sandbox.agent_dir.join("settings.json"), settings).unwrap();
    }
    let root = sandbox.root.clone();
    let output = sandbox
        .ultron()
        .env("PATH", "/usr/bin:/bin")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .args(["--model", "anthropic/claude-sonnet-4-5", "-p", "hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // The sandbox is removed here; the root path is only reported.
    drop(sandbox);
    (output, root)
}

#[test]
fn cli_herdr_print_mode_reports_working_then_the_end_then_release() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let herdr = FakeHerdr::start(&socket);
    let (output, _) = print_run("Done.", &herdr_env(&socket), None);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Done.\n");
    let lines = herdr.lines();
    check_common(&lines);
    let methods: Vec<&str> = lines.iter().map(|l| l["method"].as_str().unwrap()).collect();
    assert_eq!(
        methods,
        ["pane.report_agent", "pane.report_agent", "pane.release_agent"],
        "{lines:#?}"
    );
    assert_eq!(states(&lines), ["working", "idle"]);

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let herdr = FakeHerdr::start(&socket);
    let (output, _) = print_run("Which branch should I use?", &herdr_env(&socket), None);
    assert!(output.status.success());
    let lines = herdr.lines();
    assert_eq!(states(&lines), ["working", "blocked"]);
    assert_eq!(lines[1]["params"]["message"], "Which branch should I use?");
    assert_eq!(lines.last().unwrap()["method"], "pane.release_agent");
    // No session or metadata reports from a prompted run.
    assert_eq!(lines.len(), 3, "{lines:#?}");
}

#[test]
fn cli_herdr_is_off_by_setting_or_env_and_silent_without_a_socket() {
    for (extra, settings) in [
        (vec![("ULTRON_HERDR".to_string(), "0".to_string())], None),
        (Vec::new(), Some(r#"{"herdr": false}"#)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let herdr = FakeHerdr::start(&socket);
        let mut env = herdr_env(&socket);
        env.extend(extra.clone());
        let (output, _) = print_run("Done.", &env, settings);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "Done.\n");
        assert_eq!(herdr.connections(), 0, "{extra:?} {settings:?}: {:#?}", herdr.lines());
    }
    // No socket where herdr says it is: the run is the same, nothing on stderr.
    let dir = tempfile::tempdir().unwrap();
    let (output, _) = print_run("Done.", &herdr_env(&dir.path().join("absent.sock")), None);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Done.\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// A private root like the claude-root tests' (crates/cli/tests/claude_root.rs) with the FAKE `claude` first on PATH.
fn claude_root() -> (tempfile::TempDir, Vec<(String, String)>) {
    let dir = tempfile::Builder::new().prefix("uch-").tempdir_in("/tmp").unwrap();
    for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
        std::fs::create_dir(dir.path().join(sub)).unwrap();
    }
    let fake = std::fs::read_to_string(repo().join("crates/claude-root/fixtures/fake_claude.py")).unwrap();
    let claude = dir.path().join("bin/claude");
    executable_support::install(&claude, &format!("#!{} -I\n{fake}", python()));
    let p = |s: &str| dir.path().join(s).display().to_string();
    let env = vec![
        ("PATH".into(), format!("{}:/usr/bin:/bin", p("bin"))),
        ("HOME".into(), p("h")),
        ("ULTRON_CODING_AGENT_DIR".into(), p("a")),
        ("ULTRON_SERVER_DIR".into(), p("s")),
        ("XDG_RUNTIME_DIR".into(), p("r")),
        ("TMPDIR".into(), p("tmp")),
        ("ULTRON_SELF_COMMAND".into(), r#"["/opt/ultron/bin/ultron"]"#.into()),
        ("ULTRON_CLAUDE_BIN".into(), claude.display().to_string()),
        ("ULTRON_LOKI".into(), "off".into()),
        ("ULTRON_PYTHON".into(), python()),
        ("FAKE_CLAUDE_OUT".into(), p("launch.json")),
        ("LANG".into(), "C.UTF-8".into()),
    ];
    (dir, env)
}

fn command(root: &Path, args: &[&str], env: &[(String, String)]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .current_dir(root.join("w"))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    command
}

fn run_hook(root: &Path, env: &[(String, String)], socket: &Path, event: &str, input: Value) -> String {
    let mut hook = command(root, &["hook", event, "--socket", &socket.display().to_string()], env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = hook.wait_with_output().unwrap();
    assert!(output.status.success(), "ultron hook {event}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_herdr_claude_hooks_report_session_states_and_release() {
    let (dir, mut env) = claude_root();
    let root = dir.path();
    let herdr_socket = root.join("tmp/herdr.sock");
    let herdr = FakeHerdr::start(&herdr_socket);
    env.extend(herdr_env(&herdr_socket));
    let control = root.join("r/ctl.sock");
    let mut server = command(
        root,
        &["mcp", "--no-instructions", "--socket", &control.display().to_string()],
        &env,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let transcript = root.join("w/transcript.jsonl");
    let base = json!({"session_id": "c-123", "transcript_path": transcript.display().to_string(), "cwd": root.join("w").display().to_string()});
    let input = |extra: Value| {
        let mut input = base.clone();
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        input
    };
    // The SessionStart hook waits for the server's control socket.
    run_hook(
        root,
        &env,
        &control,
        "session-start",
        input(json!({"hook_event_name": "SessionStart", "source": "startup"})),
    );
    let session = herdr.wait_method(0, "pane.report_agent_session");
    assert_eq!(
        herdr.lines()[session]["params"],
        json!({"pane_id": "w1:t2:p3", "source": "ultron", "agent": "ultron", "agent_session_id": "c-123",
            "agent_session_path": transcript.display().to_string(),
            "resume_argv": ["ultron", "claude", "--resume", "c-123"], "session_start_source": "startup",
            "seq": herdr.lines()[session]["params"]["seq"]})
    );
    let idle = herdr.wait_state(session, "idle");
    herdr.wait(0, "display agent", |l| {
        l["method"] == "pane.report_metadata" && l["params"]["display_agent"] == "Ultron (Claude)"
    });
    run_hook(
        root,
        &env,
        &control,
        "user-prompt",
        input(json!({"hook_event_name": "UserPromptSubmit", "prompt": "go"})),
    );
    let working = herdr.wait_state(idle + 1, "working");
    let turn = |text: &str| {
        let lines = [
            json!({"type": "user", "message": {"role": "user", "content": "go"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}),
        ];
        std::fs::write(&transcript, lines.map(|l| format!("{l}\n")).concat()).unwrap();
    };
    turn("Two options.\n\nWhich branch should I use?");
    // The persona is off: the Stop hook's answer is empty, and the turn ends.
    assert_eq!(
        run_hook(root, &env, &control, "stop", input(json!({"hook_event_name": "Stop"}))),
        ""
    );
    let blocked = herdr.wait_state(working + 1, "blocked");
    assert_eq!(
        herdr.lines()[blocked]["params"]["message"],
        "Which branch should I use?"
    );
    run_hook(
        root,
        &env,
        &control,
        "user-prompt",
        input(json!({"hook_event_name": "UserPromptSubmit", "prompt": "main"})),
    );
    let working = herdr.wait_state(blocked + 1, "working");
    run_hook(
        root,
        &env,
        &control,
        "notification",
        input(
            json!({"hook_event_name": "Notification", "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use rlm"}),
        ),
    );
    let blocked = herdr.wait_state(working + 1, "blocked");
    assert_eq!(
        herdr.lines()[blocked]["params"]["message"],
        "Claude needs your permission to use rlm"
    );
    turn("Done: merged into main.");
    run_hook(root, &env, &control, "stop", input(json!({"hook_event_name": "Stop"})));
    let idle = herdr.wait_state(blocked + 1, "idle");
    run_hook(
        root,
        &env,
        &control,
        "session-end",
        input(json!({"hook_event_name": "SessionEnd", "reason": "clear"})),
    );
    let released = herdr.wait_method(idle + 1, "pane.release_agent");
    // `/clear`: a new session starts reporting again; the server's end releases it.
    run_hook(
        root,
        &env,
        &control,
        "session-start",
        input(json!({"hook_event_name": "SessionStart", "source": "clear", "session_id": "c-456"})),
    );
    let again = herdr.wait_method(released + 1, "pane.report_agent_session");
    assert_eq!(herdr.lines()[again]["params"]["agent_session_id"], "c-456");
    assert_eq!(herdr.lines()[again]["params"]["session_start_source"], "clear");
    herdr.wait_state(again, "idle");
    drop(server.stdin.take());
    assert!(server.wait().unwrap().success());
    let lines = herdr.lines();
    assert_eq!(lines.last().unwrap()["method"], "pane.release_agent", "{lines:#?}");
    check_common(&lines);
    assert_eq!(
        states(&lines),
        ["idle", "working", "blocked", "working", "blocked", "idle", "idle"]
    );
}

/// The `--settings` JSON of `ultron claude --print-config`.
fn launch_settings(env: &[(String, String)], root: &Path) -> Value {
    let output = command(root, &["claude", "--print-config"], env).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let args = printed["args"].as_array().unwrap();
    let at = args.iter().position(|a| a == "--settings").unwrap();
    serde_json::from_str(args[at + 1].as_str().unwrap()).unwrap()
}

#[test]
fn cli_herdr_claude_launch_adds_the_herdr_hooks_only_inside_herdr() {
    let (dir, mut env) = claude_root();
    let root = dir.path();
    let outside = launch_settings(&env, root);
    let events = |settings: &Value| -> Vec<String> { settings["hooks"].as_object().unwrap().keys().cloned().collect() };
    assert_eq!(
        events(&outside),
        ["SessionStart", "UserPromptSubmit", "Stop", "PostToolUse"]
    );
    env.extend(herdr_env(&root.join("tmp/never-used.sock")));
    let inside = launch_settings(&env, root);
    assert_eq!(
        events(&inside),
        [
            "SessionStart",
            "UserPromptSubmit",
            "Stop",
            "PostToolUse",
            "Notification",
            "SessionEnd"
        ]
    );
    let command = |event: &str| {
        inside["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        command("SessionEnd"),
        command("Stop").replacen("stop", "session-end", 1)
    );
    assert_eq!(
        command("Notification"),
        command("Stop").replacen("stop", "notification", 1)
    );
    assert_eq!(inside["hooks"]["Notification"][0]["hooks"][0]["timeout"], 10);
}

/// The printed launch of `ultron claude --print-config [args]`.
fn launch(env: &[(String, String)], root: &Path, extra: &[&str]) -> Value {
    let mut args = vec!["claude", "--print-config"];
    args.extend(extra);
    let output = command(root, &args, env).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_claude_launch_wakes_the_idle_root_through_an_async_rewake_hook_except_in_print_mode() {
    let (dir, env) = claude_root();
    let root = dir.path();
    let arg_after = |printed: &Value, flag: &str| -> String {
        let args = printed["args"].as_array().unwrap();
        let at = args.iter().position(|a| a == flag).unwrap();
        args[at + 1].as_str().unwrap().to_string()
    };
    let interactive = launch(&env, root, &[]);
    let settings: Value = serde_json::from_str(&arg_after(&interactive, "--settings")).unwrap();
    let waiter = &settings["hooks"]["PostToolUse"][0];
    assert_eq!(waiter["matcher"], "mcp__ultron__rlm");
    let hook = &waiter["hooks"][0];
    assert_eq!(
        (hook["async"].clone(), hook["asyncRewake"].clone()),
        (json!(true), json!(true))
    );
    assert!(
        hook["command"].as_str().unwrap().contains("hook wait-events --socket"),
        "{hook}"
    );
    assert!(arg_after(&interactive, "--mcp-config").contains("--wake"));
    let prompt = arg_after(&interactive, "--system-prompt");
    assert!(prompt.contains("starts a new turn if yours ended"), "{prompt}");
    // A print run ends its async hooks with it: no wake, and the guide says nothing wakes the model.
    let print = launch(&env, root, &["-p", "hi"]);
    assert!(!arg_after(&print, "--mcp-config").contains("--wake"));
    assert!(arg_after(&print, "--system-prompt").contains("Nothing wakes you after you reply"));
}
