//! The worker adapter's extension API pieces in the session (legacy-extension-adapter.ts): REPL tools keep their
//! metadata and progress (`replTools`, `executeTool` with `onUpdate`), the rest of the UI context is the worker's
//! headless one even with a client attached (`createHeadlessExtensionUI`), and `setActiveTools` keeps the REPL
//! pinned. Real probe extension processes (crates/extensions/tests/probe.py) in temp directories.

use extensions::HostApi;
use extensions::rpc::CancelToken;
use extensions::ui::UiUpdate;
use kernel::Signal;
use kernel_apis::{ToolInfo, ToolRunner};
use serde_json::{Map, Value, json};
use session_core::extension_host::{ExtensionUi, SET_THEME_ERROR, SessionHost};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn load(dir: &Path, capabilities: Value) -> Arc<extensions::ExtensionHost> {
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py");
    let manifest = json!({"name": "probe", "command": python(), "args": ["-I", probe],
        "env": {"PROBE_CAPABILITIES": capabilities.to_string()}, "startupTimeoutMs": 120_000});
    let path = dir.join("extension.json");
    std::fs::write(&path, manifest.to_string()).unwrap();
    let session = extensions::SessionInfo {
        cwd: dir.display().to_string(),
        mode: "print".into(),
        has_ui: false,
        flags: Map::new(),
    };
    Arc::new(extensions::ExtensionHost::load(
        vec![extensions::parse_manifest(&path, &manifest.to_string()).unwrap()],
        session,
        Arc::new(extensions::NoHost),
    ))
}

/// `replTools`: name, label, description, parameters and executionMode as registered; `executeTool` passes the
/// call's id and reports the extension's progress to `onUpdate`.
#[test]
fn session_core_extension_tools_keep_metadata_and_progress() {
    let dir = tempfile::tempdir().unwrap();
    let host = load(
        dir.path(),
        json!({"functions": [
            {"name": "progress", "label": "Progress", "description": "Reports progress", "executionMode": "sequential",
                "parameters": {"type": "object", "properties": {"updates": {"type": "array"}}}},
            {"name": "host", "description": "Ask the host"},
        ]}),
    );
    let tools = session_core::extension_tools::ExtensionTools::new(host.clone());
    assert_eq!(
        tools.tools(),
        [
            ToolInfo {
                name: "progress".into(),
                label: Some("Progress".into()),
                description: "Reports progress".into(),
                parameters: json!({"type": "object", "properties": {"updates": {"type": "array"}}}),
                execution_mode: Some("sequential".into()),
            },
            ToolInfo {
                name: "host".into(),
                label: None,
                description: "Ask the host".into(),
                parameters: json!({"type": "object"}),
                execution_mode: None,
            },
        ]
    );
    let seen = Mutex::new(Vec::new());
    let mut params = Map::new();
    params.insert("updates".into(), json!(["working", "almost"]));
    let result = tools
        .execute_with_updates(
            "progress",
            "repl-call-7",
            &params,
            &Signal::new(),
            &|partial: &Value| seen.lock().unwrap().push(partial.clone()),
        )
        .unwrap();
    assert_eq!(result["details"], json!({"toolCallId": "repl-call-7"}));
    assert_eq!(
        *seen.lock().unwrap(),
        [
            json!({"content": [{"type": "text", "text": "working"}]}),
            json!({"content": [{"type": "text", "text": "almost"}]}),
        ]
    );
    host.shutdown();
}

#[derive(Default)]
struct Serving {
    emitted: Mutex<Vec<Value>>,
}

impl ExtensionUi for Serving {
    fn serving(&self) -> bool {
        true
    }
    fn dialog(&self, _: Map<String, Value>, _: Option<Duration>, _: &dyn Fn() -> bool) -> Option<Value> {
        None
    }
    fn emit(&self, request: Map<String, Value>) {
        self.emitted.lock().unwrap().push(Value::Object(request));
    }
}

fn session_host(ui: Arc<Serving>) -> SessionHost {
    SessionHost::new(
        PathBuf::from("/w"),
        PathBuf::from("/agent"),
        PathBuf::from("/home"),
        "interactive",
        true,
        Some(ui as Arc<dyn ExtensionUi>),
        None,
    )
}

/// With a client attached the worker forwards only dialogs, notifications, status, widgets, title and editor text;
/// the working line, hidden-thinking label, tool expansion and theme are its headless context: dropped, or answered
/// with its constants (`getEditorText` "", `getToolsExpanded` false, `getAllThemes` [], `setTheme` refused).
#[test]
fn session_core_extension_ui_rest_is_the_worker_headless_context() {
    let ui = Arc::new(Serving::default());
    let host = session_host(ui.clone());
    assert!(host.has_ui() && host.ui_capable());
    for update in [
        UiUpdate::WorkingMessage(Some("Thinking hard".into())),
        UiUpdate::WorkingMessage(None),
        UiUpdate::WorkingVisible(false),
        UiUpdate::HiddenThinkingLabel(Some("hidden".into())),
        UiUpdate::ToolsExpanded(true),
        UiUpdate::Theme("light".into()),
    ] {
        host.ui("ext", update);
    }
    assert_eq!(*ui.emitted.lock().unwrap(), Vec::<Value>::new());
    let cancel = CancelToken::new();
    let ask = |method: &str, params: Value| host.request("ext", method, &params, &cancel).unwrap();
    assert_eq!(ask("ui/getEditorText", json!({})), json!(""));
    assert_eq!(ask("ui/getToolsExpanded", json!({})), json!(false));
    assert_eq!(ask("ui/getAllThemes", json!({})), json!([]));
    assert_eq!(
        ask("ui/setTheme", json!({"name": "light"})),
        json!({"success": false, "error": "Native worker UI adapter does not switch themes"})
    );
    assert_eq!(SET_THEME_ERROR, "Native worker UI adapter does not switch themes");
    // A forwarded update still reaches the client.
    host.ui("ext", UiUpdate::Title("T".into()));
    assert_eq!(
        *ui.emitted.lock().unwrap(),
        [json!({"method": "setTitle", "title": "T"})]
    );
}

/// `getActiveTools` starts as the REPL; `setActiveTools` keeps it pinned in front of what an extension sets (or
/// where the extension put it); `getSystemPromptOptions` is the worker's `{ cwd }`; the registry is unreachable
/// before the session opens.
#[test]
fn session_core_extension_active_tools_keep_the_repl() {
    let host = session_host(Arc::new(Serving::default()));
    let cancel = CancelToken::new();
    let ask = |method: &str, params: Value| host.request("ext", method, &params, &cancel);
    assert_eq!(ask("tools/active", json!({})).unwrap(), json!(["rlm"]));
    // legacy-extension-adapter.test.ts "an extension that extends or clears the active tools never takes the REPL
    // away": `[...getActiveTools(), "mcp_search"]`, then `[]`.
    let mut grown: Vec<Value> = ask("tools/active", json!({})).unwrap().as_array().unwrap().clone();
    grown.push(json!("mcp_search"));
    ask("tools/setActive", json!({"names": grown})).unwrap();
    assert_eq!(host.active_tools(), ["rlm", "mcp_search"]);
    ask("tools/setActive", json!({"names": []})).unwrap();
    assert_eq!(host.active_tools(), ["rlm"]);
    assert_eq!(
        ask("tools/setActive", json!({"names": ["web", "grep"]})).unwrap(),
        Value::Null
    );
    assert_eq!(ask("tools/active", json!({})).unwrap(), json!(["rlm", "web", "grep"]));
    ask("tools/setActive", json!({"names": ["web", "rlm"]})).unwrap();
    assert_eq!(host.active_tools(), ["web", "rlm"]);
    ask("tools/setActive", json!({"names": []})).unwrap();
    assert_eq!(host.active_tools(), ["rlm"]);
    assert_eq!(
        ask("tools/setActive", json!({"names": "web"})).unwrap_err().to_string(),
        "\"names\" must be an array of strings"
    );
    let context = ask("host/context", json!({})).unwrap();
    assert_eq!(context["systemPromptOptions"], json!({"cwd": "/w"}));
    assert_eq!(context["scopedModels"], json!([]));
    assert_eq!(
        ask("models/find", json!({"provider": "anthropic", "id": "x"}))
            .unwrap_err()
            .to_string(),
        "The session is not open"
    );
    assert_eq!(
        ask("session/file", json!({})).unwrap_err().to_string(),
        "The session is not open"
    );
}
