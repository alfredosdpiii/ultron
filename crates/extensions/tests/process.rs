//! Process tests: the host drives the example extension (examples/hello/hello.py) as a real child process.

use extensions::rpc::CancelToken;
use extensions::ui::{NoticeLevel, Placement, UiModel, UiUpdate};
use extensions::{ExtensionHost, HostApi, Manifest, RpcError, SessionInfo, parse_manifest};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn example_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello/hello.py")
}

/// The example's manifest with the kernel's interpreter, written into `dir` under `name`.
fn manifest(dir: &Path, name: &str, extra: Value) -> Manifest {
    let ext_dir = dir.join(name);
    std::fs::create_dir_all(&ext_dir).unwrap();
    let mut value = json!({
        "name": name,
        "command": python(),
        "args": [example_script().to_string_lossy()],
        // Inputs no test here exercises; long so a loaded machine never trips them.
        "eventTimeoutMs": 120_000,
        "startupTimeoutMs": 120_000,
        "maxRestarts": 2,
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        value[k] = v;
    }
    let path = ext_dir.join("extension.json");
    let text = serde_json::to_string_pretty(&value).unwrap();
    std::fs::write(&path, &text).unwrap();
    parse_manifest(&path, &text).unwrap()
}

#[derive(Default)]
struct Recorder {
    ui: Mutex<UiModel>,
    logs: Mutex<Vec<(String, String, String)>>,
    exits: Mutex<Vec<(String, String)>>,
    requests: Mutex<Vec<(String, String, Value)>>,
}

impl HostApi for Recorder {
    fn request(&self, extension: &str, method: &str, params: &Value, _cancel: &CancelToken) -> Result<Value, RpcError> {
        self.requests
            .lock()
            .unwrap()
            .push((extension.to_string(), method.to_string(), params.clone()));
        match method {
            // The user picks the last option.
            "ui/select" => Ok(params["options"]
                .as_array()
                .and_then(|o| o.last())
                .cloned()
                .unwrap_or_default()),
            _ => Err(RpcError::remote(-32601, format!("Method not found: {method}"))),
        }
    }
    fn ui(&self, extension: &str, update: UiUpdate) {
        self.ui.lock().unwrap().apply(extension, update);
    }
    fn log(&self, extension: &str, level: &str, message: &str) {
        self.logs
            .lock()
            .unwrap()
            .push((extension.to_string(), level.to_string(), message.to_string()));
    }
    fn exited(&self, extension: &str, reason: &str) {
        self.exits
            .lock()
            .unwrap()
            .push((extension.to_string(), reason.to_string()));
    }
}

fn session(has_ui: bool) -> SessionInfo {
    SessionInfo {
        cwd: "/work".into(),
        mode: if has_ui { "interactive" } else { "print" }.into(),
        has_ui,
        flags: Map::new(),
    }
}

fn load(manifests: Vec<Manifest>, has_ui: bool) -> (ExtensionHost, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let host = ExtensionHost::load(manifests, session(has_ui), recorder.clone());
    (host, recorder)
}

/// A call timeout no test here exercises (a hang guard).
const CALL: Option<Duration> = Some(Duration::from_secs(120));

fn info(host: &ExtensionHost, ext: &str) -> Value {
    host.call_function(ext, "info", json!({}), CALL, None).unwrap()["details"].clone()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(120);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_default()
}

#[test]
fn extensions_process_handshake_registers_functions_commands_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(vec![manifest(dir.path(), "hello", json!({}))], true);
    assert!(host.diagnostics.is_empty(), "{:?}", host.diagnostics);
    let names: Vec<String> = host
        .functions()
        .into_iter()
        .map(|(e, f)| format!("{e}.{}", f.name))
        .collect();
    assert_eq!(
        names,
        [
            "hello.greet",
            "hello.slow",
            "hello.cancelled",
            "hello.crash",
            "hello.info",
            "hello.shout"
        ]
    );
    let greet = &host.functions()[0].1;
    assert_eq!(greet.description, "Greet someone");
    assert_eq!(greet.parameters["properties"]["name"]["type"], "string");
    let commands = host.router().registered_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].invocation_name, "hello");
    assert!(host.router().has_handlers("tool_call"));
    assert!(!host.router().has_handlers("agent_end"));
    let facts = info(&host, "hello");
    assert_eq!(facts["restart"], 0);
    assert_eq!(
        facts["session"],
        json!({ "cwd": "/work", "mode": "interactive", "hasUI": true })
    );
    assert_eq!(host.processes()[0].info()["name"], "hello");
    host.shutdown();
}

#[test]
fn extensions_process_kernel_function_call_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(vec![manifest(dir.path(), "hello", json!({}))], false);
    let result = host
        .call_function("hello", "greet", json!({ "name": "Ultron" }), CALL, None)
        .unwrap();
    assert_eq!(text(&result), "Hello, Ultron!");
    let missing = host.call_function("hello", "nope", json!({}), None, None).unwrap_err();
    assert_eq!(missing.to_string(), "Extension hello has no function nope");
    let unknown = host.call_function("other", "greet", json!({}), None, None).unwrap_err();
    assert_eq!(unknown.to_string(), "No extension named other");
    host.shutdown();
}

#[test]
fn extensions_process_events_veto_block_and_transform() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(vec![manifest(dir.path(), "hello", json!({}))], false);
    let router = host.router();
    assert_eq!(
        router.emit(&json!({ "type": "session_before_switch", "reason": "resume" })),
        Some(json!({ "cancel": true }))
    );
    assert_eq!(
        router.emit(&json!({ "type": "session_before_switch", "reason": "new" })),
        None
    );
    let blocked = router
        .emit_tool_call(&json!({ "type": "tool_call", "toolCallId": "t", "toolName": "rlm", "input": { "code": "bash('rm -rf /')" } }))
        .unwrap();
    assert_eq!(
        blocked.result,
        Some(json!({ "block": true, "reason": "hello refuses rm -rf" }))
    );
    let allowed = router
        .emit_tool_call(&json!({ "type": "tool_call", "toolCallId": "t", "toolName": "rlm", "input": { "code": "1" } }))
        .unwrap();
    assert_eq!(allowed.result, None);
    assert_eq!(
        router.emit_input("hi", None, "interactive", None),
        json!({ "action": "transform", "text": "hello" })
    );
    assert_eq!(
        router.emit_input("yo", None, "interactive", None),
        json!({ "action": "continue" })
    );
    host.shutdown();
}

#[test]
fn extensions_process_command_prompts_and_contributes_declarative_ui() {
    let dir = tempfile::tempdir().unwrap();
    let (host, recorder) = load(vec![manifest(dir.path(), "hello", json!({}))], true);
    host.router()
        .emit(&json!({ "type": "session_start", "reason": "startup" }));
    wait_for("startup status", || {
        recorder.ui.lock().unwrap().status_line() == ["hello: startup"]
    });
    host.run_command("hello", "Bryan", None).unwrap();
    {
        let ui = recorder.ui.lock().unwrap();
        // The status line was replaced in place, not added.
        assert_eq!(ui.status_line(), ["greeted Bryan"]);
        assert_eq!(ui.panels.len(), 1);
        assert_eq!(ui.panels[0].lines, ["Hello,", "Bryan"]);
        assert_eq!(ui.panels[0].placement, Placement::BelowEditor);
    }
    let requests = recorder.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1, "ui/select");
    assert_eq!(
        requests[0].2,
        json!({ "title": "Greet whom?", "options": ["world", "Bryan"] })
    );
    // ui_prompt_start / ui_prompt_end reached the subscriber around the prompt.
    wait_for("prompt events", || {
        info(&host, "hello")["prompts"].as_array().is_some_and(|p| p.len() == 2)
    });
    assert_eq!(
        info(&host, "hello")["prompts"],
        json!([
            ["ui_prompt_start", "select", "Greet whom?"],
            ["ui_prompt_end", "select", "Greet whom?"]
        ])
    );
    let unknown = host.run_command("nope", "", None).unwrap_err();
    assert_eq!(unknown.to_string(), "Unknown command /nope");
    host.shutdown();
}

#[test]
fn extensions_process_without_ui_prompts_get_default_answers() {
    let dir = tempfile::tempdir().unwrap();
    let (host, recorder) = load(vec![manifest(dir.path(), "hello", json!({}))], false);
    host.run_command("hello", "", None).unwrap();
    // No UI: the select was answered `null` without reaching the host, and UI updates are dropped.
    assert!(recorder.requests.lock().unwrap().is_empty());
    // Settle, then check nothing arrived (a short settle can only miss a late update, never fail).
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(*recorder.ui.lock().unwrap(), UiModel::default());
    host.shutdown();
}

#[test]
fn extensions_process_timeout_and_cancel_notify_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(vec![manifest(dir.path(), "hello", json!({}))], false);
    let started = Instant::now();
    let error = host
        .call_function(
            "hello",
            "slow",
            json!({ "seconds": 600 }),
            Some(Duration::from_millis(200)),
            None,
        )
        .unwrap_err();
    assert_eq!(error, RpcError::Timeout(200));
    // The 200 ms timeout ended it, not the 600 s call (a hang guard, loose for load).
    assert!(started.elapsed() < Duration::from_secs(120));

    // Cancel once the extension is running the second slow call; info() reports its request id. Request ids: 1 is
    // initialize, 2 is the timed-out call; the second slow call and the info() polls share the ids after that.
    let token = CancelToken::new();
    let (error, running) = std::thread::scope(|scope| {
        let canceller = scope.spawn(|| {
            let mut running = Value::Null;
            wait_for("the second slow call to run", || {
                running = info(&host, "hello")["running"].clone();
                running.as_array().is_some_and(|ids| ids.iter().any(|id| id != 2))
            });
            token.cancel(Some("user pressed Escape"));
            running
        });
        let error = host
            .call_function("hello", "slow", json!({ "seconds": 600 }), None, Some(&token))
            .unwrap_err();
        (error, canceller.join().unwrap())
    });
    assert_eq!(error, RpcError::Cancelled("user pressed Escape".into()));
    let second = running.as_array().unwrap().iter().find(|id| *id != 2).unwrap().clone();
    // Both requests were cancelled on the extension's side too (`notifications/cancelled`).
    wait_for("cancellations", || {
        host.call_function("hello", "cancelled", json!({}), CALL, None).unwrap()["details"] == json!([2, second])
    });
    // An already-cancelled token never sends the request.
    assert_eq!(
        host.call_function("hello", "greet", json!({}), None, Some(&token))
            .unwrap_err(),
        RpcError::Cancelled("user pressed Escape".into())
    );
    host.shutdown();
}

#[test]
fn extensions_process_crash_is_isolated_restarted_then_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let (host, recorder) = load(
        vec![
            manifest(dir.path(), "hello", json!({})),
            manifest(dir.path(), "other", json!({})),
        ],
        false,
    );
    let first_pid = info(&host, "hello")["pid"].clone();
    let crash = host.call_function("hello", "crash", json!({}), CALL, None).unwrap_err();
    assert!(matches!(crash, RpcError::Closed(_)), "{crash:?}");
    // The other extension is unaffected.
    assert_eq!(
        text(&host.call_function("other", "greet", json!({}), None, None).unwrap()),
        "Hello, world!"
    );
    // The next use restarts the crashed one.
    let facts = info(&host, "hello");
    assert_eq!(facts["restart"], 1);
    assert_ne!(facts["pid"], first_pid);
    assert_eq!(host.process("hello").unwrap().restarts(), 1);
    // Second crash: the last restart allowed (maxRestarts 2).
    host.call_function("hello", "crash", json!({}), None, None).unwrap_err();
    assert_eq!(info(&host, "hello")["restart"], 2);
    host.call_function("hello", "crash", json!({}), None, None).unwrap_err();
    let disabled = host.call_function("hello", "greet", json!({}), None, None).unwrap_err();
    assert_eq!(
        disabled.to_string(),
        "Extension hello is disabled: it exited after 2 restart(s)"
    );
    // A disabled extension no longer receives events; the other one still vetoes.
    assert_eq!(
        host.router()
            .emit(&json!({ "type": "session_before_switch", "reason": "resume" })),
        Some(json!({ "cancel": true }))
    );
    assert!(!host.router().endpoints()[0].subscribes("session_before_switch"));
    assert!(
        recorder
            .exits
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e == "hello")
            .count()
            >= 3
    );
    host.shutdown();
}

#[test]
fn extensions_process_crash_during_event_dispatch_reports_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(
        vec![
            manifest(dir.path(), "crashy", json!({ "env": { "HELLO_CRASH_ON_TURN": "7" } })),
            manifest(dir.path(), "steady", json!({})),
        ],
        false,
    );
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    host.router().on_error(move |e| sink.lock().unwrap().push(e.clone()));
    host.router()
        .emit(&json!({ "type": "turn_start", "turnIndex": 7, "timestamp": 0 }));
    let errors = errors.lock().unwrap().clone();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].event, "turn_start");
    assert!(errors[0].extension_path.ends_with("crashy/extension.json"));
    assert_eq!(errors[0].error, "Extension crashy exited: extension process exited");
    // The steady extension was still called after it, and the crashed one restarts for the next event.
    assert_eq!(info(&host, "steady")["restart"], 0);
    host.router()
        .emit(&json!({ "type": "turn_start", "turnIndex": 8, "timestamp": 0 }));
    assert_eq!(info(&host, "crashy")["restart"], 1);
    host.shutdown();
}

#[test]
fn extensions_process_event_timeout_is_a_handler_error() {
    let dir = tempfile::tempdir().unwrap();
    // A process that answers initialize and then never answers anything.
    let script = dir.path().join("mute.py");
    std::fs::write(
        &script,
        r#"import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": "2026-10-10",
            "extensionInfo": {"name": "mute"}, "capabilities": {"events": ["agent_start"]}}}), flush=True)
"#,
    )
    .unwrap();
    let m = manifest(
        dir.path(),
        "mute",
        json!({ "args": [script.to_string_lossy()], "eventTimeoutMs": 150 }),
    );
    let (host, _) = load(vec![m], false);
    assert!(host.diagnostics.is_empty(), "{:?}", host.diagnostics);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    host.router()
        .on_error(move |e| sink.lock().unwrap().push(e.error.clone()));
    host.router().emit(&json!({ "type": "agent_start" }));
    assert_eq!(
        *errors.lock().unwrap(),
        ["Extension mute did not answer agent_start within 150ms"]
    );
    host.shutdown();
}

#[test]
fn extensions_process_failed_handshakes_become_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let wrong_version = dir.path().join("old.py");
    std::fs::write(
        &wrong_version,
        r#"import json, sys
m = json.loads(sys.stdin.readline())
print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": "1999-01-01",
    "extensionInfo": {"name": "old"}, "capabilities": {}}}), flush=True)
"#,
    )
    .unwrap();
    let dies = dir.path().join("dies.py");
    std::fs::write(
        &dies,
        "import sys\nsys.stderr.write('missing module\\n')\nsys.exit(1)\n",
    )
    .unwrap();
    let bad_event = dir.path().join("bad_event.py");
    std::fs::write(
        &bad_event,
        r#"import json, sys
m = json.loads(sys.stdin.readline())
print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": "2026-10-10",
    "extensionInfo": {"name": "x"}, "capabilities": {"events": ["on_everything"]}}}), flush=True)
"#,
    )
    .unwrap();
    let manifests = vec![
        manifest(dir.path(), "old", json!({ "args": [wrong_version.to_string_lossy()] })),
        manifest(dir.path(), "dies", json!({ "args": [dies.to_string_lossy()] })),
        manifest(
            dir.path(),
            "missing",
            json!({ "command": "/nonexistent/ultron-ext", "args": [] }),
        ),
        manifest(
            dir.path(),
            "bad_event",
            json!({ "args": [bad_event.to_string_lossy()] }),
        ),
        manifest(dir.path(), "hello", json!({})),
    ];
    let (host, _) = load(manifests, false);
    let messages: Vec<String> = host.diagnostics.iter().map(|d| d.message.clone()).collect();
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(
        messages[0],
        "Extension old failed to start: unsupported protocol version \"1999-01-01\" (host speaks 2026-10-10)"
    );
    assert_eq!(
        messages[1],
        "Extension dies failed to start: process exited during initialize: missing module"
    );
    assert!(messages[2].starts_with("Extension missing failed to start: spawn /nonexistent/ultron-ext failed"));
    assert_eq!(
        messages[3],
        "Extension bad_event failed to start: unknown event \"on_everything\""
    );
    // The good one still loaded.
    assert_eq!(host.processes().len(), 1);
    assert_eq!(host.processes()[0].name(), "hello");
    host.shutdown();
}

#[test]
fn extensions_process_bus_messages_reach_every_subscriber() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(
        vec![
            manifest(dir.path(), "a", json!({})),
            manifest(dir.path(), "b", json!({})),
        ],
        false,
    );
    host.call_function(
        "a",
        "shout",
        json!({ "channel": "greetings", "data": { "n": 1 } }),
        None,
        None,
    )
    .unwrap();
    host.call_function("a", "shout", json!({ "channel": "elsewhere", "data": 2 }), None, None)
        .unwrap();
    wait_for("bus delivery", || {
        info(&host, "b")["bus"] == json!([["greetings", { "n": 1 }]])
    });
    // Like EventEmitter, the sender hears its own channel too.
    assert_eq!(info(&host, "a")["bus"], json!([["greetings", { "n": 1 }]]));
    host.shutdown();
}

#[test]
fn extensions_process_shutdown_stops_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _) = load(vec![manifest(dir.path(), "hello", json!({}))], false);
    let pid = info(&host, "hello")["pid"].as_u64().unwrap();
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    host.shutdown();
    wait_for("process exit", || {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| s.split_whitespace().nth(2) == Some("Z"))
            .unwrap_or(true)
    });
    let error = host.call_function("hello", "greet", json!({}), None, None).unwrap_err();
    assert_eq!(error.to_string(), "Extension hello was shut down");
}

#[test]
fn extensions_ui_model_keys_status_and_panels_per_extension() {
    let mut ui = UiModel::default();
    let status = |key: &str, text: Option<&str>| UiUpdate::Status {
        key: key.into(),
        text: text.map(str::to_string),
    };
    ui.apply("a", status("k", Some("a1")));
    ui.apply("b", status("k", Some("b1")));
    ui.apply("a", status("k", Some("a2")));
    assert_eq!(ui.status_line(), ["a2", "b1"]);
    ui.apply("a", status("k", None));
    ui.apply("a", status("gone", None));
    assert_eq!(ui.status_line(), ["b1"]);
    let update = extensions::ui::parse_update("ui/setPanel", &json!({ "key": "p", "lines": ["x"] }))
        .unwrap()
        .unwrap();
    ui.apply("a", update);
    assert_eq!(ui.panels[0].placement, Placement::AboveEditor);
    let removal = extensions::ui::parse_update("ui/setPanel", &json!({ "key": "p", "lines": null }))
        .unwrap()
        .unwrap();
    ui.apply("a", removal);
    assert!(ui.panels.is_empty());
    let notice = extensions::ui::parse_update("ui/notify", &json!({ "message": "m", "type": "error" }))
        .unwrap()
        .unwrap();
    ui.apply("a", notice);
    assert_eq!(ui.notices, [("a".to_string(), NoticeLevel::Error, "m".to_string())]);
    assert_eq!(
        extensions::ui::parse_update("ui/setPanel", &json!({ "key": "p", "lines": [1] })).unwrap_err(),
        "\"lines\" must be strings"
    );
    assert_eq!(
        extensions::ui::parse_update("ui/notify", &json!({ "message": "m", "type": "loud" })).unwrap_err(),
        "unknown notice type \"loud\""
    );
    assert_eq!(extensions::ui::parse_update("ui/unknown", &json!({})), Ok(None));
}
