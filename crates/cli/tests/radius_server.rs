//! `ultron server` (PI_EXPERIMENTAL=1) replayed against the TypeScript recording (fixtures/radius_server.mjs →
//! radius_server.json): for each scenario the Rust command runs against the same scripted gateway on 127.0.0.1
//! (`radius_bridge` with the scenario's host mode, never the real gateway), is stopped with SIGTERM once the lines
//! the generator waited for are out, and must print what the TypeScript `runServerCommand` printed (stdout, stderr,
//! exit code) and show the gateway the same upgrades and close frames. The `Socket:` line differs only in its
//! directory (the Rust server's instance directory, decision wave29), so both sides compare it by file name.

mod radius_bridge;
mod tui_harness;

use radius_bridge::{Bridge, HANG_GUARD, HostMode, Upgrade};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use tui_harness::{Mock, TempDir, text_reply};

const HOST: &str = "pi-session-relay.host.v1";

fn fixture() -> Vec<Value> {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/radius_server.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// `Socket: <dir>/<name>` → `Socket: <DIR>/<name>`.
fn normalize_socket(stdout: &str) -> String {
    stdout
        .lines()
        .map(|line| match line.strip_prefix("Socket: ") {
            Some(path) => format!(
                "Socket: <DIR>/{}",
                Path::new(path).file_name().unwrap().to_string_lossy()
            ),
            None => line.to_string(),
        })
        .map(|line| line + "\n")
        .collect()
}

/// The generator's wait: after `Server:` and `Socket:`, each wanted prefix in order.
fn has_lines(lines: &[String], until: &[&str]) -> bool {
    let mut at = 2;
    for want in until {
        while at < lines.len() && !lines[at].starts_with(want) {
            at += 1;
        }
        if at >= lines.len() {
            return false;
        }
        at += 1;
    }
    true
}

fn upgrade_json(upgrade: &Upgrade) -> Value {
    serde_json::json!({
        "path": upgrade.path,
        "protocol": upgrade.protocol,
        "authorization": upgrade.authorization,
        "closed": upgrade.closed,
    })
}

fn until_of(name: &str) -> Vec<&'static str> {
    match name {
        "connected" | "stored-key" => vec!["Radius: connected"],
        "reconnects-after-close" => vec![
            "Radius: connected",
            "Radius: reconnecting: Radius relay host closed (4000: gateway restarting)",
            "Radius: connected",
        ],
        "refused" => vec!["Radius: reconnecting:"],
        "no-credential" | "offline" => vec!["Radius: not connected; local only"],
        other => panic!("unknown scenario {other}"),
    }
}

#[test]
fn cli_radius_server_command_prints_what_typescript_prints() {
    let mock = Mock::start(vec![text_reply("ok")], None);
    let cases = fixture();
    assert_eq!(cases.len(), 6);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let mode = match case["gateway"].as_str().unwrap() {
            "accept" => HostMode::Accept,
            "close-first" => HostMode::CloseFirst(4000, "gateway restarting"),
            "refuse" => HostMode::Refuse,
            other => panic!("unknown gateway {other}"),
        };
        let bridge = Bridge::start_with(mode);
        let tmp = TempDir::new();
        for dir in ["agent", "cwd", "server"] {
            std::fs::create_dir_all(tmp.0.join(dir)).unwrap();
        }
        std::fs::write(
            tmp.0.join("agent/settings.json"),
            r#"{"sessionNaming":{"enabled":false}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.0.join("agent/models.json"),
            format!(
                r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{}"}}}}}}"#,
                mock.port
            ),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .args(["server", "--server-id", "00000000-0000-4000-8000-0000000000aa"])
            .args(case["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()))
            .current_dir(tmp.0.join("cwd"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &tmp.0)
            .env("ULTRON_CODING_AGENT_DIR", tmp.0.join("agent"))
            .env("ULTRON_SERVER_DIR", tmp.0.join("server"))
            .env("PI_EXPERIMENTAL", "1")
            .env("PI_RADIUS_GATEWAY", bridge.origin())
            .env("NO_COLOR", "1")
            .env("TZ", "UTC")
            .env("ANTHROPIC_API_KEY", "test-key");
        for (key, value) in case["env"].as_object().unwrap() {
            command.env(key, value.as_str().unwrap());
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let until = until_of(name);
        let mut seen = Vec::new();
        while !has_lines(&seen, &until) {
            let line = lines
                .recv_timeout(HANG_GUARD)
                .unwrap_or_else(|_| panic!("{name}: the server command stopped printing after {seen:?}"));
            seen.push(line);
        }
        // SIGTERM to this test's own child only.
        let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        let status = child.wait().unwrap();
        while let Ok(line) = lines.recv_timeout(HANG_GUARD) {
            seen.push(line);
        }
        let stdout: String = seen.iter().map(|l| format!("{l}\n")).collect();
        let socket = seen[1].trim_start_matches("Socket: ").to_string();
        assert!(
            socket.ends_with("00000000-0000-4000-8000-0000000000aa.sock"),
            "{name}: {socket}"
        );
        let mut upgrades: Vec<Value> = bridge.upgrades().iter().map(upgrade_json).collect();
        if mode == HostMode::Refuse {
            upgrades.truncate(1);
        } else if !upgrades.is_empty() {
            // The last host connection's close frame (sent before the process exits).
            bridge.wait_host_closed_count(upgrades.len());
            upgrades = bridge.upgrades().iter().map(upgrade_json).collect();
        }
        assert!(bridge.upgrades().iter().all(|u| u.protocol == HOST));
        assert_eq!(
            normalize_socket(&stdout),
            normalize_socket(case["stdout"].as_str().unwrap()),
            "{name}: stdout"
        );
        assert_eq!(
            errors.join().unwrap(),
            case["stderr"].as_str().unwrap(),
            "{name}: stderr"
        );
        assert_eq!(
            status.code(),
            case["code"].as_i64().map(|c| c as i32),
            "{name}: exit code"
        );
        assert_eq!(Value::Array(upgrades), case["upgrades"], "{name}: what the gateway saw");
    }
}
