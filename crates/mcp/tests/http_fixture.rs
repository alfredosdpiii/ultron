//! Replays crates/mcp/fixtures/http.json: the Rust client over HttpTransport runs the same steps against the same
//! fake HTTP server as the TypeScript StreamableHttpTransport did; results, errors and received requests must match.

use mcp::http::{HttpOptions, HttpTransport, http_error};
use mcp::{Client, ClientOptions, Error, RequestOptions, Transport};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn error_value(error: &Error) -> Value {
    if let Some((kind, status)) = http_error(error) {
        return json!({ "name": kind, "message": error.to_string(), "status": status });
    }
    let mut out = json!({ "name": error.name(), "message": error.to_string() });
    if let Error::Mcp { code, .. } = error {
        out["code"] = json!(code);
    }
    out
}

#[test]
fn mcp_http_transport_matches_typescript_frames() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("http.json")).unwrap()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("log.jsonl");
    let mut server = Command::new(python())
        .arg(dir.join("fake_http_server.py"))
        .env("MCP_FAKE_LOG", &log)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    let transport = Arc::new(HttpTransport::new(HttpOptions {
        url: format!("http://127.0.0.1:{}/mcp", port.trim()),
        headers: vec![("X-Custom".into(), "c1".into())],
        ..HttpOptions::default()
    }));
    let client = Client::new(ClientOptions {
        name: "fixture-client".into(),
        version: "1.0.0".into(),
        ..ClientOptions::default()
    });
    client.connect(Arc::clone(&transport) as Arc<dyn Transport>).unwrap();
    let mut steps = vec![
        json!({ "op": "connect", "ok": { "protocolVersion": client.protocol_version(), "sessionId": transport.session_id() } }),
    ];
    for step in fixture["steps"].as_array().unwrap().iter().skip(1) {
        let op = step["op"].as_str().unwrap();
        let result = match op {
            "listTools" => client.list_tools(RequestOptions::default()).map(Value::from),
            "close" => {
                client.close();
                Ok(Value::Null)
            }
            _ => client.call_tool(
                step["name"].as_str().unwrap(),
                Some(step.get("args").cloned().unwrap_or(json!({}))),
                RequestOptions::default(),
            ),
        };
        let mut out = step.clone();
        let obj = out.as_object_mut().unwrap();
        obj.remove("ok");
        obj.remove("err");
        match result {
            Ok(v) => obj.insert("ok".into(), v),
            Err(e) => obj.insert("err".into(), error_value(&e)),
        };
        steps.push(out);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let _ = server.kill();
    let _ = server.wait();
    assert_eq!(Value::Array(steps), fixture["steps"]);
    let requests: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The GET stream opens on its own thread in both implementations, so only its presence is ordered loosely.
    let split =
        |list: &[Value]| -> (Vec<Value>, Vec<Value>) { list.iter().cloned().partition(|r| r["method"] != "GET") };
    let (got, got_get) = split(&requests);
    let (want, want_get) = split(fixture["requests"].as_array().unwrap());
    assert_eq!(got, want);
    assert_eq!(got_get, want_get);
}
