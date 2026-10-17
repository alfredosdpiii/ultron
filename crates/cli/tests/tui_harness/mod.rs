//! The tmux harness of the interactive TUI tests (a copy of the one in tests/interactive.rs, shared by the
//! command parity tests): the scripted Anthropic SSE server, screen normalization, private temp dirs, one TUI
//! process per tmux server.
#![allow(dead_code)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A scripted reply: SSE chunks, optionally held after `hold_after` chunks until released.
#[derive(Clone)]
pub struct Reply {
    pub chunks: Vec<String>,
    pub hold_after: Option<usize>,
}

pub fn sse(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// An Anthropic text reply (`text()` of the generator).
pub fn text_reply(text: &str) -> Reply {
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
pub struct Gate {
    released: Mutex<usize>,
    cv: Condvar,
}

/// The scripted SSE server. Requests whose body asks for a session title (the naming call) get `title` (after
/// `title_delay`) and do not use up a scripted reply.
pub struct Mock {
    pub port: u16,
    pub gate: Arc<Gate>,
    pub title_gate: Arc<Gate>,
    pub title_requests: Arc<AtomicUsize>,
}

/// Wait until `gate` is released once more than `ticket` times.
pub fn wait_gate(gate: &Gate, ticket: usize) {
    let mut released = gate.released.lock().unwrap();
    while *released <= ticket {
        released = gate.cv.wait(released).unwrap();
    }
}

impl Mock {
    /// `title`: the naming call's reply, and whether it waits for [`Mock::release_title`].
    pub fn start(replies: Vec<Reply>, title: Option<(String, bool)>) -> Mock {
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

    pub fn release_title(&self) {
        *self.title_gate.released.lock().unwrap() += 1;
        self.title_gate.cv.notify_all();
    }

    /// Wait until the naming call reached the server.
    pub fn wait_title_request(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while self.title_requests.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no naming request");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn release(&self) {
        *self.gate.released.lock().unwrap() += 1;
        self.gate.cv.notify_all();
    }
}

/// `normalize()` of the generator: temp dir, its session-directory name, session-file times, UUIDs, spinner frames.
pub fn normalize(screen: &str, tmp: &str) -> String {
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
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
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
pub struct Tui {
    pub socket: String,
    pub root: PathBuf,
    pub _tmp: Option<TempDir>,
    pub started: Instant,
}

impl Tui {
    pub fn start(port: u16, cols: u64, rows: u64, args: &[&str], env: &[(&str, &str)]) -> Tui {
        let tmp = TempDir::new();
        let mut tui = Self::start_at(&tmp.0, port, cols, rows, args, env);
        tui._tmp = Some(tmp);
        tui
    }

    /// Start in an existing temp root (kept by the caller), e.g. to run twice over the same sessions.
    pub fn start_at(root: &Path, port: u16, cols: u64, rows: u64, args: &[&str], env: &[(&str, &str)]) -> Tui {
        let models = format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#);
        Self::start_with_models(root, &models, cols, rows, args, env)
    }

    /// [`Tui::start_at`] with the agent dir's `models.json` given whole.
    pub fn start_with_models(
        root: &Path,
        models: &str,
        cols: u64,
        rows: u64,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Tui {
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
        std::fs::write(agent.join("models.json"), models).unwrap();
        // `ensureTool("fd")` finds `<agentDir>/bin/fd` first: a run never downloads fd from GitHub.
        let bin = agent.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        if bin.join("fd").symlink_metadata().is_ok() {
            // A restart over the same agent directory.
        } else if std::path::Path::new("/usr/bin/fd").exists() {
            std::os::unix::fs::symlink("/usr/bin/fd", bin.join("fd")).unwrap();
        } else {
            std::fs::write(bin.join("fd"), "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(bin.join("fd"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        // A login's browser opener resolves here first and does nothing: a replay never opens the developer's browser.
        let no_browser = root.join("no-browser");
        std::fs::create_dir_all(&no_browser).unwrap();
        for name in ["xdg-open", "open"] {
            let opener = no_browser.join(name);
            std::fs::write(&opener, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let mut vars: Vec<(String, String)> = vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", no_browser.display())),
            ("HOME".into(), root.display().to_string()),
            ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
            ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
            ("TZ".into(), "UTC".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("LANG".into(), "C.UTF-8".into()),
            // The startup update check never reaches the npm registry from a test.
            ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
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

    pub fn tmux(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-S", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn capture(&self) -> String {
        normalize(
            &self.tmux(&["capture-pane", "-p", "-t", "t"]),
            &self.root().display().to_string(),
        )
    }

    pub fn send_text(&self, text: &str) {
        self.tmux(&["send-keys", "-t", "t", "-l", text]);
    }

    pub fn send_keys(&self, keys: &[&str]) {
        let mut args = vec!["send-keys", "-t", "t"];
        args.extend(keys);
        self.tmux(&args);
    }

    /// From `screen`, wait until the screen has been unchanged for 400 ms; returns it. (The recorded captures are
    /// defined the same way by the generator, so this is the comparison's own rule, not a guess at "done".)
    fn settle(&self, mut screen: String) -> String {
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

    /// Wait until the screen differs from `before`, then until it is unchanged for 400 ms; returns it.
    pub fn wait_change(&self, before: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut screen = self.capture();
        while screen == before {
            assert!(Instant::now() < deadline, "timed out waiting for a change:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        self.settle(screen)
    }

    /// From `screen`, wait until the screen equals `want` (hang guard 120 s); returns the last screen seen. A screen
    /// that `settle` saw quiet for 400 ms can still be mid-redraw on a loaded machine, so a comparison against a
    /// recorded capture waits for the capture itself; a real difference still fails, after the guard.
    pub fn wait_screen(&self, mut screen: String, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        while screen != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        screen
    }

    /// Wait until the screen shows `text`, then until it is unchanged for 400 ms; returns it.
    pub fn wait(&self, text: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut screen = self.capture();
        while !screen.contains(text) {
            assert!(Instant::now() < deadline, "timed out waiting for {text:?}:\n{screen}");
            std::thread::sleep(Duration::from_millis(20));
            screen = self.capture();
        }
        self.settle(screen)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
        // The binary flushes its session file and stops its kernel when its terminal closes: wait until every
        // process working in this root is gone, so a second run over the same root sees the flushed sessions
        // (hang guard 120 s).
        let deadline = Instant::now() + Duration::from_secs(120);
        while processes_in(&self.root) {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Whatever outlived the guard (a session daemon that never saw its last client leave) is killed, so no test
        // leaves processes behind.
        for pid in pids_in(&self.root) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

/// Whether a live (non-zombie) process other than this one has its working directory under `root`.
fn processes_in(root: &Path) -> bool {
    !pids_in(root).is_empty()
}

/// Live (non-zombie) processes, other than this one, whose working directory is under `root`.
fn pids_in(root: &Path) -> Vec<u32> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<u32>().ok()?;
            if pid == std::process::id() {
                return None;
            }
            let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            if stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                == Some("Z")
            {
                return None;
            }
            std::fs::read_link(entry.path().join("cwd"))
                .is_ok_and(|cwd| cwd.starts_with(&root))
                .then_some(pid)
        })
        .collect()
}

pub fn replies(scenario: &Value) -> Vec<Reply> {
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
