//! `/login` for a models.json Radius provider (`"oauth": "radius"`) in a real terminal (tmux): each test replays a
//! scenario of `fixtures/radius_login.json`, recorded from the TypeScript client TUI by `fixtures/radius_login.mjs`
//! against the same loopback Radius gateway (device authorization, pending token polls until released, then the
//! token), offline (PI_OFFLINE=1). Every stable screen, the saved `auth.json` and the gateway's distinct requests
//! must equal the TypeScript ones after the same normalization.

mod tui_harness;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tui_harness::{TempDir, Tui, normalize};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/radius_login.json")).unwrap())
}

/// The loopback Radius gateway of the recording.
struct Gateway {
    port: u16,
    released: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Gateway {
    fn start() -> Gateway {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let released = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (gate, seen) = (released.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                );
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let form: serde_json::Map<String, Value> = url_decode_form(&String::from_utf8(body).unwrap());
                seen.lock()
                    .unwrap()
                    .push(json!({"method": method, "path": path, "body": form}));
                let (status, reply) = match path.as_str() {
                    "/v1/oauth/device" => (
                        200,
                        json!({"device_code": "dev-1", "user_code": "WXYZ-1234",
                            "verification_uri": format!("http://127.0.0.1:{port}/device"), "expires_in": 600, "interval": 1}),
                    ),
                    "/v1/oauth/token" if !gate.load(Ordering::SeqCst) => {
                        (400, json!({"error": "authorization_pending"}))
                    }
                    "/v1/oauth/token" => (
                        200,
                        json!({"access_token": "radius-access", "refresh_token": "radius-refresh", "expires_in": 3600}),
                    ),
                    _ => (404, json!({})),
                };
                let text = reply.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
            }
        });
        Gateway {
            port,
            released,
            requests,
        }
    }
}

fn url_decode_form(body: &str) -> serde_json::Map<String, Value> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| {
            let decode = |s: &str| {
                let bytes = s.replace('+', " ").into_bytes();
                let mut out = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == b'%' && i + 2 < bytes.len() {
                        out.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap(), 16).unwrap());
                        i += 3;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                String::from_utf8(out).unwrap()
            };
            (decode(k), json!(decode(v)))
        })
        .collect()
}

fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap();
    let gateway = Gateway::start();
    let tmp = TempDir::new();
    // Beside the harness's own `agent` (it writes an Anthropic-only models.json there), same name length so the
    // status line wraps where the recording's does.
    let agent = tmp.0.join("gwdir");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": {
            "anthropic": {"baseUrl": "http://127.0.0.1:9"},
            "gw": {"oauth": "radius", "baseUrl": format!("http://127.0.0.1:{}/v1", gateway.port), "name": "My Radius"},
        }})
        .to_string(),
    )
    .unwrap();
    let agent_dir = agent.display().to_string();
    let tui = Tui::start_at(
        &tmp.0,
        9,
        fixture["cols"].as_u64().unwrap(),
        fixture["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[
            ("ULTRON_CODING_AGENT_DIR", &agent_dir),
            ("ULTRON_AUTO_NAME", "off"),
            // The TypeScript layout (no Rust-only quality-of-life rows), as the other TUI replays run.
            ("ULTRON_TUI_PARITY", "1"),
            ("ULTRON_SPLASH", "off"),
            ("PI_OFFLINE", "1"),
        ],
    );
    let root = tmp.0.display().to_string();
    let port = gateway.port.to_string();
    // The recording's agent dir is `<TMP>/agent`; this one is `<TMP>/gwdir`.
    let screen = |text: &str| {
        normalize(text, &root)
            .replace(&port, "<port>")
            .replace("<TMP>/gwdir", "<TMP>/agent")
    };
    let mut screens = Vec::new();
    for step in scenario["steps"].as_array().unwrap() {
        if step["release"] == true {
            gateway.released.store(true, Ordering::SeqCst);
        }
        if let Some(text) = step["type"].as_str() {
            tui.send_text(text);
        }
        if let Some(keys) = step["keys"].as_array() {
            let keys: Vec<&str> = keys.iter().map(|k| k.as_str().unwrap()).collect();
            tui.send_keys(&keys);
        }
        let settled = tui.wait(step["wait"].as_str().unwrap());
        screens.push(screen(&settled));
    }
    let expected: Vec<String> = scenario["captures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    for (index, (got, want)) in screens.iter().zip(&expected).enumerate() {
        assert_eq!(got, want, "{name} step {}", index + 1);
    }
    let auth = std::fs::read_to_string(agent.join("auth.json")).ok().map(|text| {
        let mut value: Value = serde_json::from_str(&text).unwrap();
        if let Some(gw) = value.get_mut("gw") {
            gw["expires"] = json!(0);
        }
        value
    });
    let want_auth: Option<Value> = scenario["artifacts"]["agent/auth.json"]
        .as_str()
        .map(|t| serde_json::from_str(t).unwrap());
    // TypeScript's store writes `{}` once the login starts; an untouched Rust profile has no file yet.
    let empty = Some(json!({}));
    assert!(
        auth == want_auth || (auth.is_none() && want_auth == empty),
        "{name}: auth.json {auth:?} vs {want_auth:?}"
    );
    let mut distinct: Vec<Value> = Vec::new();
    for request in gateway.requests.lock().unwrap().iter() {
        let request = serde_json::from_str::<Value>(&request.to_string().replace(&port, "<port>")).unwrap();
        if !distinct.contains(&request) {
            distinct.push(request);
        }
    }
    assert_eq!(Value::Array(distinct), scenario["requests"], "{name}: gateway requests");
}

#[test]
fn cli_radius_device_code_login_like_ts() {
    replay("device-code-login");
}

#[test]
fn cli_radius_device_code_login_cancel_like_ts() {
    replay("device-code-cancel");
}
