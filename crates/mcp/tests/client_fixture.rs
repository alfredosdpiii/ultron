//! Replays crates/mcp/fixtures/client.json: the Rust client runs the same steps against the same fake server as the
//! TypeScript client did, and every result, error, event and raw JSON-RPC frame (both directions) must match.

use mcp::{CancelToken, Client, ClientOptions, Error, RequestOptions, StdioOptions, StdioTransport, Transport};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| {
        if Path::new("/usr/bin/python3").exists() {
            "/usr/bin/python3".into()
        } else {
            "python3".into()
        }
    })
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn error_value(error: &Error) -> Value {
    let mut out = json!({ "name": error.name(), "message": error.to_string() });
    if let Error::Mcp { code, data, .. } = error {
        out["code"] = json!(code);
        if let Some(data) = data {
            out["data"] = data.clone();
        }
    }
    out
}

fn unit(result: Result<(), Error>) -> Result<Value, Error> {
    result.map(|()| Value::Null)
}

/// The complete frames the fake server has logged so far (a line still being written is left out).
fn logged(log: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let complete = text.rfind('\n').map_or("", |end| &text[..end]);
    complete
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// How many `tools/call` requests for `name` the fake server has read.
fn calls_read(log: &Path, name: &str) -> usize {
    logged(log)
        .iter()
        .filter(|frame| frame["dir"] == "in")
        .filter_map(|frame| serde_json::from_str::<Value>(frame["line"].as_str()?).ok())
        .filter(|message| message["method"] == "tools/call" && message["params"]["name"] == name)
        .count()
}

fn run_step(client: &Client, step: &Value, events: &Arc<Mutex<Vec<Value>>>, log: &Path) -> Result<Value, Error> {
    let s = |k: &str| step.get(k).and_then(Value::as_str);
    match s("op").unwrap_or_default() {
        "listTools" => client.list_tools(RequestOptions::default()).map(Value::from),
        "listResources" => client.list_resources(RequestOptions::default()).map(Value::from),
        "listResourceTemplates" => client
            .list_resource_templates(RequestOptions::default())
            .map(Value::from),
        "listResourcesPage" => client.list_resources_page(s("cursor"), RequestOptions::default()),
        "readResource" => client.read_resource(s("uri").unwrap_or_default(), RequestOptions::default()),
        "ping" => unit(client.ping(RequestOptions::default())),
        "request" => client.request(
            s("method").unwrap_or_default(),
            step.get("params").cloned(),
            RequestOptions::default(),
        ),
        "notify" => unit(client.notify(s("method").unwrap_or_default(), step.get("params").cloned())),
        "close" => {
            client.close();
            Ok(Value::Null)
        }
        "callTool" => {
            let mut options = RequestOptions::default();
            if let Some(ms) = step.get("timeoutMs").and_then(Value::as_u64) {
                options.timeout = Some(Duration::from_millis(ms));
            }
            if step.get("progress") == Some(&json!(true)) {
                let events = events.clone();
                options.on_progress = Some(Arc::new(move |p: &Value| {
                    events.lock().unwrap().push(json!({ "progress": p }))
                }));
            }
            let cancel = s("abort").map(|reason| (CancelToken::new(), reason.to_string()));
            options.cancel = cancel.as_ref().map(|(token, _)| token.clone());
            let name = s("name").unwrap_or_default().to_string();
            let before = calls_read(log, &name);
            let call = {
                let client = client.clone();
                let name = name.clone();
                let args = step.get("args").cloned();
                std::thread::spawn(move || client.call_tool(&name, args, options))
            };
            if let Some((token, reason)) = cancel {
                // Cancel once the request is on the wire (the server has read it), as the TypeScript abort follows
                // the synchronous send. Hang guard 120 s.
                let deadline = Instant::now() + Duration::from_secs(120);
                while calls_read(log, &name) == before && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                token.cancel(Some(&reason));
            }
            call.join().unwrap()
        }
        other => panic!("unknown step {other}"),
    }
}

fn read_frames(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Compares serialized JSON, so key order counts too.
fn assert_json(label: &str, actual: &Value, expected: &Value) {
    assert_eq!(
        serde_json::to_string_pretty(actual).unwrap(),
        serde_json::to_string_pretty(expected).unwrap(),
        "{label}"
    );
}

#[test]
fn mcp_client_replays_typescript_stdio_session_frames_and_results() {
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(fixtures().join("client.json")).unwrap()).unwrap();
    let roots = fixture["roots"].as_array().unwrap().clone();
    for session in fixture["sessions"].as_array().unwrap() {
        let config = &session["client"];
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let client = Client::new(ClientOptions {
            name: config["name"].as_str().unwrap().into(),
            version: config["version"].as_str().unwrap().into(),
            title: config.get("title").and_then(Value::as_str).map(Into::into),
            protocol_version: config.get("protocolVersion").and_then(Value::as_str).map(Into::into),
            roots: (config.get("roots") == Some(&json!(true))).then(|| roots.clone()),
            ..ClientOptions::default()
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        client.on_error(move |e| sink.lock().unwrap().push(json!({ "error": e })));
        let sink = events.clone();
        client.on_notification("notifications/tools/list_changed", move |p| {
            sink.lock()
                .unwrap()
                .push(json!({ "notification": p.cloned().unwrap_or(Value::Null) }))
        });
        let sink = events.clone();
        client.on_close(move || sink.lock().unwrap().push(json!({ "closed": true })));
        let transport = Arc::new(StdioTransport::new(StdioOptions {
            command: python(),
            args: vec!["-I".into(), fixtures().join("fake_server.py").to_string_lossy().into()],
            env: vec![("MCP_FAKE_LOG".into(), log.to_string_lossy().into())],
            ..StdioOptions::default()
        }));
        let mut results = Vec::new();
        let connect = client.connect(transport.clone() as Arc<dyn Transport>);
        let mut first = json!({ "op": "connect", "state": client.connection_state().as_str() });
        match connect {
            Ok(v) => first["ok"] = v,
            Err(e) => first["error"] = error_value(&e),
        }
        results.push(first);
        let expected = session["results"].as_array().unwrap();
        for recorded in &expected[1..] {
            let mut record: Map<String, Value> = recorded.as_object().unwrap().clone();
            record.shift_remove("ok");
            record.shift_remove("error");
            let outcome = run_step(&client, &Value::Object(record.clone()), &events, &log);
            match outcome {
                Ok(v) => record.insert("ok".into(), v),
                Err(e) => record.insert("error".into(), error_value(&e)),
            };
            results.push(Value::Object(record));
        }
        client.close();
        let label = config["name"].as_str().unwrap();
        for (actual, expected) in results.iter().zip(expected) {
            assert_json(label, actual, expected);
        }
        assert_eq!(results.len(), expected.len(), "{label}");
        let state = json!({
            "connectionState": client.connection_state().as_str(),
            "protocolVersion": client.protocol_version(),
            "serverInfo": client.server_info(),
            "serverCapabilities": client.server_capabilities(),
            "instructions": client.instructions(),
        });
        assert_json(label, &state, &session["state"]);
        assert_json(label, &Value::Array(events.lock().unwrap().clone()), &session["events"]);
        assert_json(label, &Value::Array(read_frames(&log)), &session["frames"]);
        assert_eq!(transport.stderr(), session["stderr"].as_str().unwrap(), "{label}");
    }
}
