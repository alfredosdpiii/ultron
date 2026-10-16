//! The interactive TUI in a real terminal (tmux). Parity tests replay `fixtures/interactive.json`, recorded from the
//! TypeScript client TUI (`node packages/coding-agent/src/cli.ts`, which opens experimental/client-tui.ts) by
//! `fixtures/interactive.mjs`: the Rust binary starts in tmux against the same scripted Anthropic SSE server with
//! private HOME / ULTRON_CODING_AGENT_DIR / ULTRON_SERVER_DIR temp dirs, gets the same keystrokes, and every stable
//! screen must equal the TypeScript one after the same normalization (temp paths, ids, spinner frames).
//! Rust-only tests cover the user requirements that differ from TypeScript (session names, the resume picker).

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/interactive.json")).unwrap())
}

/// A scripted reply: SSE chunks, optionally held after `hold_after` chunks until released.
#[derive(Clone)]
struct Reply {
    chunks: Vec<String>,
    hold_after: Option<usize>,
}

fn sse(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// An Anthropic text reply (`text()` of the generator).
fn text_reply(text: &str) -> Reply {
    let start = serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}});
    Reply {
        chunks: sse(&[
            start,
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            serde_json::json!({"type": "message_stop"}),
        ]),
        hold_after: None,
    }
}

#[derive(Default)]
struct Gate {
    released: Mutex<usize>,
    cv: Condvar,
}

/// The scripted SSE server. Requests whose body asks for a session title (the naming call) get `title` (after
/// `title_delay`) and do not use up a scripted reply.
struct Mock {
    port: u16,
    gate: Arc<Gate>,
    title_gate: Arc<Gate>,
    title_requests: Arc<AtomicUsize>,
}

/// Wait until `gate` is released once more than `ticket` times.
fn wait_gate(gate: &Gate, ticket: usize) {
    let mut released = gate.released.lock().unwrap();
    while *released <= ticket {
        released = gate.cv.wait(released).unwrap();
    }
}

impl Mock {
    /// `title`: the naming call's reply, and whether it waits for [`Mock::release_title`].
    fn start(replies: Vec<Reply>, title: Option<(String, bool)>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let gate = Arc::new(Gate::default());
        let title_gate = Arc::new(Gate::default());
        let title_requests = Arc::new(AtomicUsize::new(0));
        let next = Arc::new(AtomicUsize::new(0));
        let (g, t, tg) = (gate.clone(), title_requests.clone(), title_gate.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (replies, gate, next, titles, title, title_gate) = (
                    replies.clone(),
                    g.clone(),
                    next.clone(),
                    t.clone(),
                    title.clone(),
                    tg.clone(),
                );
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() {
                        return;
                    }
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = header.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            length = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body = String::from_utf8_lossy(&body);
                    let reply = if body.contains(cli::naming::SYSTEM) {
                        titles.fetch_add(1, Ordering::SeqCst);
                        let (text, hold) = title.unwrap_or(("Untitled".into(), false));
                        if hold {
                            wait_gate(&title_gate, 0);
                        }
                        text_reply(&text)
                    } else {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        replies
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| text_reply("(no more replies)"))
                    };
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let cut = reply.hold_after.unwrap_or(reply.chunks.len());
                    for c in &reply.chunks[..cut] {
                        let _ = stream.write_all(c.as_bytes());
                    }
                    let _ = stream.flush();
                    if reply.hold_after.is_some() {
                        let mut released = gate.released.lock().unwrap();
                        let ticket = *released;
                        while *released == ticket {
                            released = gate.cv.wait(released).unwrap();
                        }
                    }
                    for c in &reply.chunks[cut..] {
                        let _ = stream.write_all(c.as_bytes());
                    }
                });
            }
        });
        Mock {
            port,
            gate,
            title_gate,
            title_requests,
        }
    }

    fn release_title(&self) {
        *self.title_gate.released.lock().unwrap() += 1;
        self.title_gate.cv.notify_all();
    }

    /// Wait until the naming call reached the server.
    fn wait_title_request(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.title_requests.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no naming request");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn release(&self) {
        *self.gate.released.lock().unwrap() += 1;
        self.gate.cv.notify_all();
    }
}

/// `normalize()` of the generator: temp dir, its session-directory name, session-file times, UUIDs, spinner frames.
fn normalize(screen: &str, tmp: &str) -> String {
    let tmp_name = tmp.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    let text = screen.replace(tmp, "<TMP>").replace(&tmp_name, "<TMPNAME>");
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let digit = |c: Option<&char>| c.is_some_and(char::is_ascii_digit);
    let hex = |c: Option<&char>| c.is_some_and(|c| c.is_ascii_digit() || ('a'..='f').contains(c));
    while i < chars.len() {
        // \d{4}-\d\d-\d\dT[0-9\-\n ]{12,16}Z
        let date = (0..10).all(|k| {
            if k == 4 || k == 7 {
                chars.get(i + k) == Some(&'-')
            } else {
                digit(chars.get(i + k))
            }
        }) && chars.get(i + 10) == Some(&'T');
        if date {
            let start = i + 11;
            let mut end = start;
            while end < chars.len()
                && end - start < 16
                && (chars[end].is_ascii_digit() || matches!(chars[end], '-' | '\n' | ' '))
            {
                end += 1;
            }
            if let Some(z) = (start + 12..=end).rev().find(|&z| chars.get(z) == Some(&'Z')) {
                out.push_str("<TIME>");
                i = z + 1;
                continue;
            }
        }
        // 8-4-4-4-12 lowercase hex
        let groups = [8, 4, 4, 4, 12];
        let mut j = i;
        let uuid = groups.iter().enumerate().all(|(g, &n)| {
            let ok = (0..n).all(|k| hex(chars.get(j + k)));
            j += n;
            if ok && g < 4 {
                if chars.get(j) != Some(&'-') {
                    return false;
                }
                j += 1;
            }
            ok
        });
        if uuid {
            out.push_str("<ID>");
            i = j;
            continue;
        }
        let c = chars[i];
        out.push(if "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c) {
            '*'
        } else {
            c
        });
        i += 1;
    }
    out
}

/// A private temp dir of the same length as the generator's (`/tmp/ultron-tui-XXXXXX`), so wrapped lines match.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(0);
        loop {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let seed = std::process::id() as u64 * 7919 + n * 104_729 + Instant::now().elapsed().as_nanos() as u64;
            let suffix: String = (0..6)
                .map(|k| {
                    // Digits only: the session selector searches the session's cwd, so random letters could make
                    // every session match a typed query ("alp" matched all three when the suffix held a, l, p).
                    let alphabet = b"0123456789";
                    alphabet[((seed >> (k * 5)) as usize + k * 13) % alphabet.len()] as char
                })
                .collect();
            let path = PathBuf::from(format!("/tmp/ultron-tui-{suffix}"));
            if std::fs::create_dir(&path).is_ok() {
                return TempDir(path);
            }
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One TUI process in its own tmux server.
struct Tui {
    socket: String,
    root: PathBuf,
    _tmp: Option<TempDir>,
    started: Instant,
}

impl Tui {
    fn start(port: u16, cols: u64, rows: u64, args: &[&str], env: &[(&str, &str)]) -> Tui {
        let tmp = TempDir::new();
        let mut tui = Self::start_at(&tmp.0, port, cols, rows, args, env);
        tui._tmp = Some(tmp);
        tui
    }

    /// Start in an existing temp root (kept by the caller), e.g. to run twice over the same sessions.
    fn start_at(root: &Path, port: u16, cols: u64, rows: u64, args: &[&str], env: &[(&str, &str)]) -> Tui {
        assert!(
            Command::new("tmux")
                .arg("-V")
                .output()
                .is_ok_and(|o| o.status.success()),
            "these tests drive the TUI in tmux; install tmux"
        );
        let root = root.to_path_buf();
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(root.join("cwd")).unwrap();
        std::fs::create_dir_all(root.join("server")).unwrap();
        std::fs::write(
            agent.join("models.json"),
            format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
        )
        .unwrap();
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let mut vars: Vec<(String, String)> = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), root.display().to_string()),
            ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
            ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
            ("TZ".into(), "UTC".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ];
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let script = root.join("run.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncd {}\nexec env -i {} {} {}\n",
                quote(&root.join("cwd").display().to_string()),
                vars.iter()
                    .map(|(k, v)| quote(&format!("{k}={v}")))
                    .collect::<Vec<_>>()
                    .join(" "),
                quote(env!("CARGO_BIN_EXE_ultron")),
                args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
            ),
        )
        .unwrap();
        // The tmux server's socket lives in the temp dir, so nothing is left behind.
        let socket = root.join("tmux.sock").display().to_string();
        let started = Instant::now();
        let tui = Tui {
            socket,
            root,
            _tmp: None,
            started,
        };
        tui.tmux(&[
            "new-session",
            "-d",
            "-s",
            "t",
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
            // Read by `sh`, never executed by path: an exec of a file this process just wrote can fail with
            // ETXTBSY while another test thread's fork still holds the write descriptor.
            &format!("sh {}; echo '<exited>'; sleep 60", quote(&script.display().to_string())),
        ]);
        tui
    }

    fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-S", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn capture(&self) -> String {
        normalize(
            &self.tmux(&["capture-pane", "-p", "-t", "t"]),
            &self.root().display().to_string(),
        )
    }

    fn send_text(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "t", "-l", text]);
    }

    fn send_keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "t"];
        args.extend(keys);
        self.tmux(&args);
    }

    /// Wait until the screen shows `text`, then until it is unchanged for 400 ms; returns it.
    /// The first screen that satisfies `done` (a hang guard of 120 s). Unlike [`Tui::wait`] (a screen that stopped
    /// changing for 400 ms), it does not stop at a frame drawn before a later redraw, however slow the machine.
    fn wait_for(&self, what: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut screen = self.capture();
        while !done(&screen) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        screen
    }

    fn wait(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut screen = self.capture();
        while !screen.contains(text) {
            assert!(Instant::now() < deadline, "timed out waiting for {text:?}:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        let mut since = Instant::now();
        while since.elapsed() < Duration::from_millis(400) {
            std::thread::sleep(Duration::from_millis(50));
            let next = self.capture();
            if next != screen {
                screen = next;
                since = Instant::now();
            }
        }
        screen
    }

    /// From `screen`, wait until the screen equals `want` (hang guard 120 s); returns the last screen seen. A screen
    /// quiet for 400 ms can still be mid-redraw on a loaded machine, so a comparison against a recorded capture
    /// waits for the capture itself; a real difference still fails, after the guard.
    fn wait_screen(&self, mut screen: String, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        while screen != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        screen
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        // The binary flushes its session file and stops its kernel when its terminal closes: wait for every
        // process of this root to be gone (a hang guard of 120 s).
        let deadline = Instant::now() + Duration::from_secs(120);
        let pattern = self.root.display().to_string();
        while Instant::now() < deadline
            && Command::new("pgrep")
                .args(["-f", &pattern])
                .output()
                .is_ok_and(|o| o.status.success())
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Whatever outlived the guard (a session daemon that never saw its last client leave) is killed, so no test
        // leaves processes behind: only server generations recorded in this test's own server dir, and only while
        // their command line still names this root (never `pkill`).
        for pid in recorded_server_pids(&self.root.join("server")) {
            let ours = std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(&pattern));
            if let (true, Some(pid)) = (ours, rustix::process::Pid::from_raw(pid)) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

/// The pids of the server generations recorded under `dir` (`generation-*.json`, any depth).
fn recorded_server_pids(dir: &Path) -> Vec<i32> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            pids.extend(recorded_server_pids(&path));
        } else if name.starts_with("generation-") && name.ends_with(".json") {
            let record = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok());
            if let Some(pid) = record.and_then(|r| r["pid"].as_i64()).filter(|&p| p > 0) {
                pids.push(pid as i32);
            }
        }
    }
    pids
}

fn replies(scenario: &Value) -> Vec<Reply> {
    scenario["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Reply {
            chunks: r["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect(),
            hold_after: r["holdAfter"].as_u64().map(|n| n as usize),
        })
        .collect()
}

/// Replay one recorded scenario and compare every screen.
fn replay(name: &str) {
    let scenario = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("scenario {name} is not in the fixture"));
    let mock = Mock::start(replies(scenario), None);
    // The TypeScript client TUI names nothing and has none of the Rust-only additions.
    let mut env = vec![("ULTRON_AUTO_NAME", "off"), ("ULTRON_TUI_PARITY", "1")];
    if scenario["splash"] != true {
        env.push(("ULTRON_SPLASH", "off"));
    }
    let tui = Tui::start(
        mock.port,
        scenario["cols"].as_u64().unwrap(),
        scenario["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &env,
    );
    let captures = scenario["captures"].as_array().unwrap();
    for (i, (step, want)) in scenario["steps"].as_array().unwrap().iter().zip(captures).enumerate() {
        if step["release"] == true {
            mock.release();
        }
        if let Some(text) = step["type"].as_str() {
            tui.send_text(text);
        }
        if let Some(keys) = step["keys"].as_array() {
            tui.send_keys(&keys.iter().map(|k| k.as_str().unwrap()).collect::<Vec<_>>());
        }
        let want = want.as_str().unwrap();
        let got = tui.wait_screen(tui.wait(step["wait"].as_str().unwrap()), want);
        if got != want {
            let diff: Vec<String> = got
                .lines()
                .zip(want.lines())
                .enumerate()
                .filter(|(_, (g, w))| g != w)
                .map(|(row, (g, w))| format!("row {row}:\n  got  {g:?}\n  want {w:?}"))
                .collect();
            panic!(
                "{name} step {i} differs from TypeScript:\n{}\n--- got\n{got}\n--- want\n{want}",
                diff.join("\n")
            );
        }
    }
}

macro_rules! parity {
    ($($test:ident => $name:literal),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                replay($name);
            }
        )*

        #[test]
        fn cli_interactive_fixture_has_a_test_per_scenario() {
            let tested = [$($name),*];
            for scenario in fixture()["scenarios"].as_array().unwrap() {
                let name = scenario["name"].as_str().unwrap();
                assert!(tested.contains(&name), "scenario {name} has no parity test");
            }
        }
    };
}

parity! {
    cli_interactive_startup_screen_matches_ts => "startup",
    cli_interactive_rlm_cell_turn_matches_ts => "prompt-rlm-cell",
    cli_interactive_streaming_and_steering_match_ts => "streaming-and-steer",
    cli_interactive_escape_aborts_turn_like_ts => "escape-abort",
    cli_interactive_ctrl_c_clears_editor_like_ts => "ctrl-c-clear",
    cli_interactive_slash_autocomplete_matches_ts => "slash-list",
    cli_interactive_unknown_command_matches_ts => "unknown-command",
    cli_interactive_session_info_matches_ts => "session-info",
    cli_interactive_hotkeys_match_ts => "hotkeys",
    cli_interactive_new_session_matches_ts => "new-session",
    cli_interactive_copy_without_messages_matches_ts => "copy-empty",
    cli_interactive_model_selector_matches_ts => "model-selector",
    cli_interactive_quit_matches_ts => "quit",
    cli_interactive_quit_after_turn_matches_ts => "quit-after-turn",
    cli_interactive_ctrl_d_exits_like_ts => "ctrl-d-exit",
    cli_interactive_splash_matches_ts => "splash",
}

/// Time to the first frame showing the header: under the TypeScript product's best time
/// (`fixtures/interactive-latency.json`, `interactive.mjs --latency`). A wall-clock benchmark against a time
/// recorded on an idle machine, so it runs alone, never inside the shared suite (it failed there under load).
#[test]
#[ignore = "benchmark: run alone with `cargo test -p cli --test interactive -- --ignored first_frame`"]
fn cli_interactive_first_frame_beats_typescript_startup() {
    let latency: Value = serde_json::from_str(include_str!("../fixtures/interactive-latency.json")).unwrap();
    let budget = latency["tsFirstFrameMs"].as_u64().unwrap();
    let mock = Mock::start(Vec::new(), None);
    let mut best = u128::MAX;
    for _ in 0..3 {
        let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
        let deadline = Instant::now() + Duration::from_secs(120);
        while !tui.capture().contains("ultron v") {
            assert!(Instant::now() < deadline, "no first frame");
            std::thread::sleep(Duration::from_millis(5));
        }
        best = best.min(tui.started.elapsed().as_millis());
    }
    assert!(
        best < u128::from(budget),
        "first frame after {best} ms; TypeScript shows it after {budget} ms"
    );
}

fn footer_cwd_line(screen: &str) -> String {
    screen
        .lines()
        .find(|l| l.starts_with("~/cwd"))
        .unwrap_or("")
        .to_string()
}

/// The first prompt names the session at once (the first line, cleaned); the model's title replaces it when it
/// arrives; the name is stored in the session file.
#[test]
fn cli_interactive_first_prompt_names_session_then_model_title() {
    let mock = Mock::start(vec![text_reply("Sure.")], Some(("Parser fix plan".into(), true)));
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("please   fix the parser\nthen run tests");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Sure.");
    // The fallback is up while the title call waits.
    assert_eq!(footer_cwd_line(&screen), "~/cwd • please fix the parser");
    mock.wait_title_request();
    mock.release_title();
    let screen = tui.wait("Parser fix plan");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • Parser fix plan");
    // Pi's terminal title carries the name too.
    assert_eq!(
        tui.tmux(&["display-message", "-p", "-t", "t", "#{pane_title}"]).trim(),
        "ultron - Parser fix plan - cwd"
    );
    assert_eq!(mock.title_requests.load(Ordering::SeqCst), 1);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    let names = stored_names(tui.root());
    assert_eq!(names, ["please fix the parser", "Parser fix plan"]);
}

/// `/name` before the title arrives wins: the late title is dropped, and `/name` with no argument shows the name.
#[test]
fn cli_interactive_slash_name_wins_over_late_title() {
    let mock = Mock::start(vec![text_reply("Ok.")], Some(("Model title".into(), true)));
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("first prompt");
    tui.send_keys(&["Enter"]);
    tui.wait("Ok.");
    tui.send_text("/name My session");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Session name set: My session");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • My session");
    // The title arrives after the user's name and is dropped.
    mock.wait_title_request();
    mock.release_title();
    std::thread::sleep(Duration::from_millis(500));
    tui.send_text("/name");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Session name: My session");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • My session");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    assert_eq!(stored_names(tui.root()), ["first prompt", "My session"]);
}

/// A title that arrives after `/new` belongs to the old session: the new one stays unnamed.
#[test]
fn cli_interactive_late_title_does_not_name_a_new_session() {
    let mock = Mock::start(vec![text_reply("Ok.")], Some(("Old title".into(), true)));
    let tui = Tui::start(mock.port, 100, 30, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    tui.send_text("old work");
    tui.send_keys(&["Enter"]);
    tui.wait("Ok.");
    mock.wait_title_request();
    tui.send_text("/new");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("New session started");
    assert_eq!(footer_cwd_line(&screen), "~/cwd");
    mock.release_title();
    std::thread::sleep(Duration::from_millis(500));
    tui.send_text("/name");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Usage: /name <name>");
    assert_eq!(footer_cwd_line(&screen), "~/cwd");
}

/// `ultron -n <name>` names the session from the start; the first prompt does not rename it.
#[test]
fn cli_interactive_name_flag_names_session() {
    let mock = Mock::start(vec![text_reply("Hi.")], Some(("Ignored".into(), false)));
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5", "-n", "Release prep"],
        &[],
    );
    let screen = tui.wait("ultron v");
    assert_eq!(
        footer_cwd_line(&tui.wait("Session:")),
        "~/cwd • Release prep",
        "{screen}"
    );
    tui.send_text("hello");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Hi.");
    assert_eq!(footer_cwd_line(&screen), "~/cwd • Release prep");
    assert_eq!(mock.title_requests.load(Ordering::SeqCst), 0);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    assert_eq!(stored_names(tui.root()), ["Release prep"]);
}

/// Names stored in the session files under the agent dir (each `pi.session.name` value the format-4 journals set),
/// oldest first.
fn stored_names(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for file in session_files(&root.join("agent/experimental/sessions")) {
        for line in std::fs::read_to_string(file).unwrap().lines().skip(1) {
            let records = match serde_json::from_str::<Value>(line).unwrap() {
                Value::Array(records) => records,
                record => vec![record],
            };
            for record in records {
                if record["namespace"] == "pi.session.name" && record["op"] == "set" {
                    names.push(record["value"].as_str().unwrap().to_string());
                }
            }
        }
    }
    names
}

fn session_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(session_files(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    found
}

/// Two sessions in this directory (one named) and one elsewhere: `/resume` lists this directory's only, by name or
/// first message, and switching shows the resumed transcript; `ultron -r` lists them with names and opens the pick.
#[test]
fn cli_interactive_resume_lists_this_directory_with_names() {
    let mock = Mock::start(vec![text_reply("One."), text_reply("Two.")], None);
    let tmp = TempDir::new();
    let root = tmp.0.clone();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let quiet = [("ULTRON_AUTO_NAME", "off")];
    {
        let tui = Tui::start_at(
            &root,
            mock.port,
            100,
            30,
            &[&args[..], &["-n", "Named one"]].concat(),
            &quiet,
        );
        tui.wait("ultron v");
        tui.send_text("alpha prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("One.");
        tui.send_text("/new");
        tui.send_keys(&["Enter"]);
        tui.wait("New session started");
        tui.send_text("beta prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("Two.");
        // A session of another directory, in this directory's session folder and in its own.
        let here = session_files(&root.join("agent/experimental/sessions"))[0]
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::write(
            here.join("2026-01-02T00-00-00-000Z_0199bbbb-0000-7000-8000-000000000000.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"0199bbbb-0000-7000-8000-000000000000\",\"timestamp\":\"2026-01-02T00:00:00.000Z\",\"cwd\":\"/elsewhere\"}\n{\"type\":\"session_info\",\"id\":\"a1\",\"parentId\":null,\"timestamp\":\"2026-01-02T00:00:01.000Z\",\"name\":\"Foreign\"}\n",
        )
        .unwrap();
        let other = root.join("agent/experimental/sessions/--elsewhere--");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("2026-01-01T00-00-00-000Z_0199aaaa-0000-7000-8000-000000000000.jsonl"),
            "{\"type\":\"session\",\"version\":3,\"id\":\"0199aaaa-0000-7000-8000-000000000000\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/elsewhere\"}\n",
        )
        .unwrap();
        tui.send_text("/resume");
        tui.send_keys(&["Enter"]);
        // Each check waits for the frame that completes it: a list, transcript or footer can be drawn after the
        // frame that first shows the heading or status (a fixed settle time loses that race under load).
        let screen = tui.wait_for("the session list", |s| {
            s.contains("Resume Session") && s.contains("› beta prompt") && s.contains("  Named one")
        });
        assert!(!screen.contains("elsewhere") && !screen.contains("Foreign"), "{screen}");
        tui.send_keys(&["Down", "Enter"]);
        let screen = tui.wait_for("the resumed transcript", |s| {
            s.contains("Resumed session")
                && s.contains("alpha prompt")
                && s.contains("One.")
                && !s.contains("beta prompt")
                && footer_cwd_line(s) == "~/cwd • Named one"
        });
        assert!(!screen.contains("beta prompt"), "{screen}");
        tui.send_text("/quit");
        tui.send_keys(&["Enter"]);
        tui.wait("<exited>");
    }
    let tui = Tui::start_at(&root, mock.port, 100, 30, &[&args[..], &["-r"]].concat(), &quiet);
    let screen = tui.wait_for("both sessions listed", |s| {
        s.contains("Select a session number:") && s.lines().filter(|l| l.starts_with("  ")).count() >= 2
    });
    let listed: Vec<&str> = screen.lines().filter(|l| l.starts_with("  ")).collect();
    assert_eq!(listed.len(), 2, "{screen}");
    assert!(
        listed[0].starts_with("  1. <ID>") && !listed[0].contains("Named"),
        "{screen}"
    );
    assert_eq!(listed[1], "  2. <ID>  Named one", "{screen}");
    tui.send_text("2");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait_for("the picked session", |s| {
        s.contains("One.") && s.contains("alpha prompt") && footer_cwd_line(s) == "~/cwd • Named one"
    });
    assert!(screen.contains("alpha prompt"), "{screen}");
}

/// `/resume` search covers every message (`allMessagesText`), through the worker's session listing: a word only a
/// later reply holds finds that session; its row still shows the first prompt.
#[test]
fn cli_interactive_resume_search_finds_a_word_from_a_later_message() {
    let mock = Mock::start(
        vec![text_reply("One."), text_reply("Quokka habitat."), text_reply("Three.")],
        None,
    );
    let tmp = TempDir::new();
    let root = tmp.0.clone();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let tui = Tui::start_at(&root, mock.port, 100, 30, &args, &[("ULTRON_AUTO_NAME", "off")]);
    tui.wait("ultron v");
    for (prompt, reply) in [("alpha prompt", "One."), ("second ask", "Quokka habitat.")] {
        tui.send_text(prompt);
        tui.send_keys(&["Enter"]);
        tui.wait(reply);
    }
    tui.send_text("/new");
    tui.send_keys(&["Enter"]);
    tui.wait("New session started");
    tui.send_text("beta prompt");
    tui.send_keys(&["Enter"]);
    tui.wait("Three.");
    tui.send_text("/resume");
    tui.send_keys(&["Enter"]);
    tui.wait_for("the session list", |s| {
        s.contains("Resume Session") && s.contains("beta prompt") && s.contains("alpha prompt")
    });
    // A quoted phrase matches as a substring, so nothing in the temp paths can match by accident.
    tui.send_text("\"quokka\"");
    // The selector's rows: the lines between its search line and the rule under it (the transcript above still
    // shows the current session).
    let rows = |s: &str| -> Vec<String> {
        s.lines()
            .skip_while(|l| !l.starts_with("> \"quokka\""))
            .skip(1)
            .take_while(|l| !l.starts_with('─'))
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    };
    let screen = tui.wait_for("the search result", |s| {
        rows(s)
            .first()
            .is_some_and(|r| r.starts_with("› alpha prompt") || r.contains("No sessions"))
    });
    let rows = rows(&screen);
    assert_eq!(rows.len(), 1, "{screen}");
    assert!(rows[0].starts_with("› alpha prompt"), "{screen}");
    tui.send_keys(&["Enter"]);
    tui.wait_for("the resumed transcript", |s| {
        s.contains("Resumed session") && s.contains("Quokka habitat.") && !s.contains("beta prompt")
    });
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// Up in the editor recalls prompts from earlier runs in this directory (prompt-history.jsonl).
#[test]
fn cli_interactive_prompt_history_survives_restart() {
    let mock = Mock::start(vec![text_reply("A."), text_reply("B.")], None);
    let tmp = TempDir::new();
    let args = ["--model", "anthropic/claude-sonnet-4-5"];
    let quiet = [("ULTRON_AUTO_NAME", "off")];
    {
        let tui = Tui::start_at(&tmp.0, mock.port, 100, 30, &args, &quiet);
        tui.wait("ultron v");
        tui.send_text("remember this prompt");
        tui.send_keys(&["Enter"]);
        tui.wait("A.");
        tui.send_keys(&["C-d"]);
        tui.wait("<exited>");
    }
    let history = std::fs::read_to_string(tmp.0.join("agent/prompt-history.jsonl")).unwrap();
    let entry: Value = serde_json::from_str(history.lines().next().unwrap()).unwrap();
    assert_eq!(entry["text"], "remember this prompt");
    assert_eq!(entry["cwd"], tmp.0.join("cwd").display().to_string());
    let tui = Tui::start_at(&tmp.0, mock.port, 100, 30, &args, &quiet);
    tui.wait("Session:");
    tui.send_keys(&["Up"]);
    let screen = tui.wait("remember this prompt");
    assert!(screen.lines().any(|l| l == " remember this prompt"), "{screen}");
}

/// `/copy` puts the last assistant text on the clipboard through the terminal (OSC 52; tmux keeps it as a buffer).
#[test]
fn cli_interactive_copy_sends_last_assistant_text_to_the_terminal_clipboard() {
    let mock = Mock::start(vec![text_reply("Copy **me** please.")], None);
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        // The TypeScript `/copy` (with the overlays on, `/copy` opens the picker: interactive_overlays.rs).
        &[("ULTRON_AUTO_NAME", "off"), ("ULTRON_TUI_PARITY", "1")],
    );
    tui.wait("Session:");
    tui.tmux(&["set-option", "-s", "set-clipboard", "on"]);
    tui.send_text("hi");
    tui.send_keys(&["Enter"]);
    tui.wait("Copy me please.");
    tui.send_text("/copy");
    tui.send_keys(&["Enter"]);
    tui.wait("Copied last agent message to clipboard");
    assert_eq!(tui.tmux(&["show-buffer"]), "Copy **me** please.");
}
