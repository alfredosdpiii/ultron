//! `ultron setup` in a real terminal (tmux, private socket): each test replays a scenario of
//! `fixtures/setup_tui.json`, recorded from the TypeScript wizard by `fixtures/setup_tui.mjs`, with the same fakes:
//! PATH holds only a fake python3 and a no-op browser opener (no docker, no analyzers), memory starts off (nothing
//! probes Hindsight), and the custom endpoint is a local stand-in on an ephemeral 127.0.0.1 port. Every screen, the
//! files left and the requests the stand-in saw are compared. The Rust build has no Node.js, so the recorded
//! environment step's `Node.js` row is left out of the expected screens (see decisions.tsv).

mod tui_harness;

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tui_harness::{TempDir, normalize};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/setup_tui.json")).unwrap())
}

/// The OpenAI-compatible stand-in of the generator: GET /v1/models lists one model, chat completions stream "ok".
fn endpoint() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = header.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                seen.lock().unwrap().push(format!("{method} {path}"));
                let (status, kind, body) = match (method, path) {
                    ("GET", "/v1/models") => (
                        200,
                        "application/json",
                        r#"{"object":"list","data":[{"id":"model-one","object":"model"}]}"#.to_string(),
                    ),
                    ("POST", "/v1/chat/completions") => {
                        let chunk = |delta: &str, finish: &str| {
                            format!(
                                "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"model-one\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
                            )
                        };
                        (
                            200,
                            "text/event-stream",
                            format!(
                                "{}{}data: [DONE]\n\n",
                                chunk(r#"{"role":"assistant","content":"ok"}"#, "null"),
                                chunk("{}", "\"stop\"")
                            ),
                        )
                    }
                    _ => (404, "text/plain", String::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            });
        }
    });
    (port, requests)
}

struct Tmux {
    socket: String,
}

impl Tmux {
    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("tmux")
            .args(["-S", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        // Only this test's own tmux server, by its private socket.
        self.run(&["kill-server"]);
    }
}

/// The recorded screen as the Rust build draws it: without the `Node.js` row, one blank line added at the bottom.
fn without_node(screen: &str) -> String {
    let mut lines: Vec<&str> = screen.split('\n').collect();
    if let Some(at) = lines.iter().position(|l| l.contains("✓ Node.js: ")) {
        lines.remove(at);
        let end = if lines.last() == Some(&"") {
            lines.len() - 1
        } else {
            lines.len()
        };
        lines.insert(end, "");
    }
    lines.join("\n")
}

fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap();
    let (port, requests) = endpoint();
    let tmp = TempDir::new();
    let root: &Path = &tmp.0;
    let bin = root.join("bin");
    for dir in ["agent", "cwd", "bin", "server"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    for (file, text) in [
        ("python3", "#!/bin/sh\necho 'Python 3.12.0'\n"),
        ("xdg-open", "#!/bin/sh\nexit 0\n"),
    ] {
        std::fs::write(bin.join(file), text).unwrap();
        std::fs::set_permissions(bin.join(file), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    for (rel, text) in scenario["files"].as_object().into_iter().flatten() {
        std::fs::write(root.join(rel), text.as_str().unwrap()).unwrap();
    }
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let r = |p: &str| root.join(p).display().to_string();
    let vars = [
        ("PATH", bin.display().to_string()),
        ("HOME", root.display().to_string()),
        ("XDG_CONFIG_HOME", r(".config")),
        ("XDG_DATA_HOME", r(".local/share")),
        ("ULTRON_CODING_AGENT_DIR", r("agent")),
        ("ULTRON_SERVER_DIR", r("server")),
        ("ULTRON_PYTHON", "python3".into()),
        ("TZ", "UTC".into()),
        ("TERM", "xterm-256color".into()),
        ("COLORTERM", "truecolor".into()),
        ("LANG", "C.UTF-8".into()),
        ("PI_OFFLINE", "1".into()),
    ];
    let script = root.join("run.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncd {}\nexec /usr/bin/env -i {} {} {}\n",
            quote(&r("cwd")),
            vars.iter()
                .map(|(k, v)| quote(&format!("{k}={v}")))
                .collect::<Vec<_>>()
                .join(" "),
            quote(env!("CARGO_BIN_EXE_ultron")),
            match scenario["args"].as_array() {
                Some(args) => args
                    .iter()
                    .map(|a| quote(a.as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(" "),
                None => "setup".into(),
            },
        ),
    )
    .unwrap();
    let tmux = Tmux { socket: r("tmux.sock") };
    tmux.run(&[
        "new-session",
        "-d",
        "-s",
        "t",
        "-x",
        &fixture["cols"].to_string(),
        "-y",
        &fixture["rows"].to_string(),
        &format!(
            "sh {}; echo '<exited>'; sleep 600",
            quote(&script.display().to_string())
        ),
    ]);
    let root_text = root.display().to_string();
    let port_text = format!(":{port}");
    let seconds = regex::Regex::new(r"in \d+\.\ds").unwrap();
    let norm = |s: &str| {
        seconds
            .replace_all(&normalize(s, &root_text).replace(&port_text, ":<PORT>"), "in <S>s")
            .into_owned()
    };
    let capture = || norm(&tmux.run(&["capture-pane", "-p", "-t", "t"]));
    for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        let send = |keys: &Value| {
            if let Some(keys) = keys.as_array() {
                let mut args = vec!["send-keys", "-t", "t"];
                args.extend(keys.iter().map(|k| k.as_str().unwrap()));
                tmux.run(&args);
            }
        };
        send(&step["keys"]);
        if let Some(text) = step["type"].as_str() {
            tmux.run(&["send-keys", "-t", "t", "-l", &text.replace("<PORT>", &port.to_string())]);
        }
        send(&step["then"]);
        if step["partial"] == true {
            // Only the step's own text: what follows (the start going on without a model) is not compared.
            let wait = step["wait"].as_str().unwrap();
            let deadline = Instant::now() + Duration::from_secs(120);
            while !capture().contains(wait) {
                assert!(
                    Instant::now() < deadline,
                    "{name} step {}: {wait:?} never shown",
                    index + 1
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        let want = without_node(scenario["captures"][index].as_str().unwrap());
        // Wait for the recorded screen (hang guard 120 s); a real difference fails after the guard.
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut screen = capture();
        while screen != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            let next = capture();
            if !next.trim().is_empty() {
                screen = next;
            }
        }
        assert_eq!(screen, want, "{name} step {}", index + 1);
    }
    for (rel, want) in scenario["artifacts"].as_object().into_iter().flatten() {
        let got = std::fs::read_to_string(root.join(rel))
            .ok()
            .map(|t| t.replace(&root_text, "<TMP>").replace(&port_text, ":<PORT>"));
        assert_eq!(got.as_deref(), want.as_str(), "{name}: {rel}");
    }
    let want: Vec<String> = scenario["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    assert_eq!(*requests.lock().unwrap(), want, "{name}: endpoint requests");
}

#[test]
fn cli_setup_tui_skipping_every_step_like_ts() {
    replay("skip every step");
}

#[test]
fn cli_setup_tui_custom_endpoint_with_live_test_like_ts() {
    replay("custom endpoint");
}

#[test]
fn cli_setup_tui_ctrl_c_stops_the_wizard_like_ts() {
    replay("quit with ctrl+c");
}

#[test]
fn cli_setup_tui_first_run_offer_runs_setup_like_ts() {
    replay("first run offer, run setup, quit");
}

#[test]
fn cli_setup_tui_first_run_offer_dont_ask_again_is_saved_like_ts() {
    replay("first run offer, never");
}
