//! Replays `fixtures/radius_auth.json` (recorded from the TypeScript CLI by `fixtures/radius_auth.mjs`): `ultron auth
//! check|print-api-key|print-bearer-token` for a models.json Radius provider (`"oauth": "radius"`), with the stored
//! OAuth token refreshed on the provider's own gateway (a loopback server answering as the recording's did). Each
//! case runs the Rust binary in a private temp HOME / agent dir with only the environment it names.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn gateway(requests: Arc<Mutex<Vec<Value>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
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
            let form: serde_json::Map<String, Value> = String::from_utf8(body)
                .unwrap()
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            requests
                .lock()
                .unwrap()
                .push(json!({"method": method, "path": path, "body": form}));
            let reply =
                json!({"access_token": "refreshed-access", "refresh_token": "refreshed-refresh", "expires_in": 3600})
                    .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    port
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn cli_auth_for_radius_providers_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/radius_auth.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 15);
    for case in cases {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let port = gateway(requests.clone());
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let fill = |v: &Value| v.to_string().replace("<port>", &port.to_string());
        std::fs::write(agent.join("models.json"), fill(&case["models"])).unwrap();
        if !case["auth"].is_null() {
            let mut auth = case["auth"].clone();
            if auth["gw"]["expires"] == "+1h" {
                auth["gw"]["expires"] = json!(now_ms() + 3_600_000);
            }
            std::fs::write(agent.join("auth.json"), auth.to_string()).unwrap();
        }
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .args(&args)
            .current_dir(tmp.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", tmp.path())
            .env("PI_OFFLINE", "1")
            .env("ULTRON_CODING_AGENT_DIR", &agent)
            .env("ULTRON_SERVER_DIR", tmp.path().join("server"));
        for (k, v) in case["env"].as_object().unwrap() {
            command.env(k, v.as_str().unwrap());
        }
        let output = command.output().unwrap();
        let unport = |text: &[u8]| String::from_utf8_lossy(text).replace(&port.to_string(), "<port>");
        let name = args.join(" ");
        assert_eq!(
            unport(&output.stdout),
            case["stdout"].as_str().unwrap(),
            "{name}: stdout"
        );
        assert_eq!(
            unport(&output.stderr),
            case["stderr"].as_str().unwrap(),
            "{name}: stderr"
        );
        assert_eq!(
            output.status.code(),
            case["code"].as_i64().map(|c| c as i32),
            "{name}: exit code"
        );
        assert_eq!(
            Value::Array(requests.lock().unwrap().clone()),
            case["requests"],
            "{name}: gateway requests"
        );
        let stored = std::fs::read_to_string(agent.join("auth.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|auth| auth.get("gw").cloned())
            .map(|gw| json!({"type": gw["type"], "access": gw["access"], "refresh": gw["refresh"]}))
            .unwrap_or(Value::Null);
        assert_eq!(stored, case["stored"], "{name}: auth.json");
    }
}
