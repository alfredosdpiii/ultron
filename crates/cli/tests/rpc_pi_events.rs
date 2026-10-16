//! Replays `fixtures/rpc_pi_events.json` (recorded from Pi's RPC mode, `modes/rpc/rpc-mode.ts`, over the test-suite
//! harness by `fixtures/rpc_pi_events.mjs`): the Rust `ultron --mode rpc`, with the example extension
//! crates/extensions/examples/hello installed (the generator's inline extension does the same for these events), gets
//! the same command steps and must answer them the same way and stream the same Pi events:
//!
//! - `user_bash` reaches the extension before a command runs: `hello ...` is answered by it (recorded as if it ran,
//!   never run), `rm -rf` and an invalid result fail the command closed (`extension_error`, the response's error);
//! - `bash_execution_update`: a command's output as it streams, under the command's id (none without one);
//! - `session_info_changed` after `set_session_name`, and the extension's failure on it as `extension_error`.
//!
//! Only these events are compared: the Rust stream also carries the native RPC events (rpc-native.ts: a recorded
//! command's `message_*` and `entry_appended`, extension UI requests) that `rpc_mode.rs` compares with the native
//! command, which Pi's RPC mode does not emit.

use cli::rpc::client::{RpcClient, RpcClientOptions};
use serde_json::{Map, Value, json};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);
/// The Pi events this fixture covers (everything else in the Rust stream is native RPC, see the module docs).
const COVERED: [&str; 2] = ["extension_error", "session_info_changed"];

fn fixture() -> Value {
    let text =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/rpc_pi_events.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The generator's `canonical`: timestamps 0, extension paths `<ext>`, `cwd` `<CWD>`; deltas joined per id.
fn canonical(lines: &[Value], cwd: &str) -> (Vec<Value>, Vec<Value>, Map<String, Value>) {
    fn scrub(value: &Value, key: &str, cwd: &str) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|v| scrub(v, key, cwd)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), scrub(v, k, cwd))).collect()),
            Value::Number(_) if key == "timestamp" => json!(0),
            Value::String(_) if key == "extensionPath" => json!("<ext>"),
            Value::String(s) => Value::String(s.replace(cwd, "<CWD>")),
            other => other.clone(),
        }
    }
    let (mut responses, mut events, mut output) = (Vec::new(), Vec::new(), Map::new());
    for line in lines.iter().map(|l| scrub(l, "", cwd)) {
        match line["type"].as_str().unwrap_or_default() {
            "response" => responses.push(line),
            "bash_execution_update" => {
                let id = line["id"].as_str().unwrap_or("<none>").to_string();
                let joined = output.entry(id).or_insert_with(|| json!(""));
                *joined = json!(format!(
                    "{}{}",
                    joined.as_str().unwrap(),
                    line["delta"].as_str().unwrap()
                ));
            }
            kind if COVERED.contains(&kind) => events.push(line),
            _ => {}
        }
    }
    (responses, events, output)
}

/// A provider endpoint that only counts connections: nothing in these steps may call the model.
fn silent_provider() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            seen.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, count)
}

#[test]
fn cli_rpc_pi_events_user_bash_bash_updates_session_info_and_extension_errors() {
    let case = fixture();
    let (port, connections) = silent_provider();
    let tmp = tempfile::Builder::new()
        .prefix("ultron-rpc-pi-events-")
        .tempdir()
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&cwd).unwrap();
    let installed = agent_dir.join("extensions").join("hello");
    std::fs::create_dir_all(&installed).unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    for file in ["extension.json", "hello.py"] {
        std::fs::copy(example.join(file), installed.join(file)).unwrap();
    }
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}).to_string(),
    )
    .unwrap();
    let env = [
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("HOME", root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR", root.join("server").display().to_string()),
        ("ANTHROPIC_API_KEY", "test-key".into()),
        ("ULTRON_HINDSIGHT_URL", "off".into()),
        ("PI_OFFLINE", "1".into()),
        ("NO_COLOR", "1".into()),
        ("TZ", "UTC".into()),
    ];
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(cwd.clone()),
        env_clear: true,
        env: env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        args: ["--model", "anthropic/claude-sonnet-4-5", "--no-session"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    let mut client = RpcClient::start(&options).unwrap();
    for step in case["steps"].as_array().unwrap() {
        client.mark();
        client.write_line(&step["command"].to_string()).unwrap();
        let waits: Vec<String> = match step.get("wait") {
            Some(Value::Array(waits)) => waits.iter().map(|w| w.as_str().unwrap().to_string()).collect(),
            _ => vec!["response".into()],
        };
        let id = step["command"].get("id").cloned();
        for want in waits {
            let id = id.clone();
            client
                .wait_for(
                    &move |line| {
                        if want == "response" {
                            line["type"] == "response" && line.get("id") == id.as_ref()
                        } else {
                            line["type"] == want.as_str()
                        }
                    },
                    HANG,
                )
                .unwrap_or_else(|e| panic!("step {step}: {e}\n{}", client.stderr()));
        }
    }
    let stderr = client.stderr();
    let (lines, code) = client.finish(HANG);
    assert_eq!(
        code.map(i64::from),
        case["code"].as_i64(),
        "exit code; stderr: {stderr}"
    );
    let (responses, events, output) = canonical(&lines, &cwd.display().to_string());
    let want_events = case["events"].as_array().unwrap();
    assert!(
        want_events
            .iter()
            .all(|e| COVERED.contains(&e["type"].as_str().unwrap()))
    );
    let want_responses = case["responses"].as_array().unwrap();
    assert_eq!(responses.len(), want_responses.len(), "{responses:#?}");
    for (got, want) in responses.iter().zip(want_responses) {
        assert_eq!(got, want);
    }
    assert_eq!(&events, want_events);
    assert_eq!(Value::Object(output), case["output"]);
    assert_eq!(connections.load(Ordering::SeqCst), 0, "no model call");
}
