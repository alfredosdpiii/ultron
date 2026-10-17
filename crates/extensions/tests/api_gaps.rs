//! The handshake's event list, `registerTool` metadata and `onUpdate` over the protocol, and the no-UI `setTheme`
//! answer, against real probe extension processes (tests/probe.py) in temp directories.

use extensions::rpc::CancelToken;
use extensions::{
    DELIVERED_EVENTS, ExtensionHost, FunctionSpec, HostApi, NoHost, RpcError, SessionInfo, parse_manifest,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/probe.py")
}

struct Probe {
    dir: tempfile::TempDir,
    host: ExtensionHost,
}

impl Probe {
    fn load(capabilities: Value, api: Arc<dyn HostApi>) -> Probe {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("probe.jsonl");
        let manifest = json!({
            "name": "probe",
            "command": python(),
            "args": ["-I", probe().to_string_lossy()],
            "env": {"PROBE_LOG": log.to_string_lossy(), "PROBE_CAPABILITIES": capabilities.to_string()},
            "startupTimeoutMs": 120_000,
            "eventTimeoutMs": 120_000,
        });
        let path = dir.path().join("extension.json");
        std::fs::write(&path, manifest.to_string()).unwrap();
        let session = SessionInfo {
            cwd: dir.path().display().to_string(),
            mode: "print".into(),
            has_ui: false,
            flags: Map::new(),
        };
        let host = ExtensionHost::load(
            vec![parse_manifest(&path, &manifest.to_string()).unwrap()],
            session,
            api,
        );
        Probe { dir, host }
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("probe.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.host.shutdown();
    }
}

/// The host advertises the events it emits; a subscription to any other known event (`pi.on("turn_start")`) still
/// loads and simply never fires; an unknown name is refused.
#[test]
fn extensions_handshake_advertises_only_delivered_events() {
    let probe = Probe::load(
        json!({"events": ["turn_start", "session_start", "tool_call"]}),
        Arc::new(NoHost),
    );
    assert!(probe.host.diagnostics.is_empty(), "{:?}", probe.host.diagnostics);
    let process = probe.host.process("probe").unwrap();
    assert_eq!(
        process.registrations().events,
        ["turn_start", "session_start", "tool_call"]
    );
    let initialize = probe
        .log()
        .into_iter()
        .find_map(|e| e.get("initialize").cloned())
        .unwrap();
    assert_eq!(
        initialize["capabilities"]["events"],
        json!([
            "project_trust",
            "session_start",
            "session_info_changed",
            "session_before_fork",
            "session_before_compact",
            "session_compact",
            "session_compact_failed",
            "session_shutdown",
            "session_before_tree",
            "session_tree",
            "context",
            "context_with_system",
            "before_agent_start",
            "agent_end",
            "ui_prompt_start",
            "ui_prompt_end",
            "user_bash",
            "before_file_write",
            "after_cell_changes"
        ])
    );
    assert_eq!(initialize["capabilities"]["events"], json!(DELIVERED_EVENTS));
    let unknown = Probe::load(json!({"events": ["no_such_event"]}), Arc::new(NoHost));
    assert!(unknown.host.processes().is_empty());
    assert!(
        unknown.host.diagnostics[0]
            .message
            .contains("unknown event \"no_such_event\""),
        "{:?}",
        unknown.host.diagnostics
    );
}

/// `registerTool`'s `label`, `executionMode` and `promptGuidelines` cross the handshake; an execution mode other
/// than TypeScript's two fails the load.
#[test]
fn extensions_function_metadata_is_registered() {
    let probe = Probe::load(
        json!({"functions": [
            {"name": "progress", "label": "Progress", "description": "Reports progress", "executionMode": "sequential",
                "promptGuidelines": ["Use progress for long work."]},
            {"name": "host"},
        ]}),
        Arc::new(NoHost),
    );
    let functions: Vec<FunctionSpec> = probe.host.functions().into_iter().map(|(_, f)| f).collect();
    assert_eq!(
        functions,
        [
            FunctionSpec {
                name: "progress".into(),
                description: "Reports progress".into(),
                parameters: json!({"type": "object"}),
                label: Some("Progress".into()),
                execution_mode: Some("sequential".into()),
                prompt_guidelines: Some(vec!["Use progress for long work.".into()]),
            },
            FunctionSpec {
                name: "host".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                label: None,
                execution_mode: None,
                prompt_guidelines: None,
            },
        ]
    );
    let bad = Probe::load(
        json!({"functions": [{"name": "f", "executionMode": "eager"}]}),
        Arc::new(NoHost),
    );
    assert!(bad.host.processes().is_empty());
    assert!(
        bad.host.diagnostics[0]
            .message
            .contains("function \"f\": executionMode must be \"sequential\" or \"parallel\""),
        "{:?}",
        bad.host.diagnostics
    );
}

/// `execute(toolCallId, params, signal, onUpdate)`: the call carries its id, and the extension's `functions/update`
/// notifications for that id reach the caller's `onUpdate`, in order, before the result.
#[test]
fn extensions_function_updates_reach_the_call_in_flight() {
    let probe = Probe::load(json!({"functions": [{"name": "progress"}]}), Arc::new(NoHost));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let result = probe
        .host
        .call_function_with_updates(
            "probe",
            "progress",
            json!({"updates": ["one", "two"]}),
            "repl-call-1",
            Arc::new(move |partial| sink.lock().unwrap().push(partial)),
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        result,
        json!({"content": [{"type": "text", "text": "done"}], "details": {"toolCallId": "repl-call-1"}})
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [
            json!({"content": [{"type": "text", "text": "one"}]}),
            json!({"content": [{"type": "text", "text": "two"}]}),
        ]
    );
    // A plain call has no id; its updates go nowhere.
    let plain = probe
        .host
        .call_function("probe", "progress", json!({"updates": ["x"]}), None, None)
        .unwrap();
    assert_eq!(plain["details"], json!({"toolCallId": null}));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// Without a UI, `setTheme(name)` answers `noOpUIContext`'s result.
#[test]
fn extensions_set_theme_without_ui_answers_ui_not_available() {
    let probe = Probe::load(json!({"functions": [{"name": "host"}]}), Arc::new(NoHost));
    let answer = probe
        .host
        .call_function(
            "probe",
            "host",
            json!({"method": "ui/setTheme", "params": {"name": "dark"}}),
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        answer["details"],
        json!({"success": false, "error": "UI not available"})
    );
    let cancel = CancelToken::new();
    assert!(matches!(
        NoHost.request("probe", "ui/nothing", &json!({}), &cancel),
        Err(RpcError::Remote { code: -32601, .. })
    ));
}
