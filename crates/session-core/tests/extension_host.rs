//! The session's extension UI as the worker's `ExtensionUIBridge.createContext` sends it (Pi's RPC
//! `extension_ui_request` fields): notifications, status, widgets, title and editor text reach the client; the rest of
//! the UI context is headless; dialogs answer Pi's defaults without a client, and `hasUI` follows the client.

use extensions::HostApi;
use extensions::rpc::CancelToken;
use extensions::ui::{NoticeLevel, Placement, UiUpdate};
use serde_json::{Map, Value, json};
use session_core::extension_host::{ExtensionUi, SessionHost};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Recorder {
    serving: AtomicBool,
    emitted: Mutex<Vec<Value>>,
    dialogs: Mutex<Vec<(Value, Option<Duration>)>>,
}

impl ExtensionUi for Recorder {
    fn serving(&self) -> bool {
        self.serving.load(Ordering::SeqCst)
    }
    fn dialog(&self, request: Map<String, Value>, timeout: Option<Duration>, _: &dyn Fn() -> bool) -> Option<Value> {
        self.dialogs.lock().unwrap().push((Value::Object(request), timeout));
        Some(json!({"value": 7}))
    }
    fn emit(&self, request: Map<String, Value>) {
        self.emitted.lock().unwrap().push(Value::Object(request));
    }
}

fn host(ui: Option<Arc<Recorder>>, mode: &str) -> SessionHost {
    SessionHost::new(
        PathBuf::from("/w"),
        PathBuf::from("/agent"),
        PathBuf::from("/home"),
        mode,
        true,
        ui.map(|u| u as Arc<dyn ExtensionUi>),
        None,
    )
}

#[test]
fn session_core_extension_ui_reaches_the_client_as_pi_rpc_requests() {
    let ui = Arc::new(Recorder::default());
    let host = host(Some(ui.clone()), "interactive");
    assert!(host.ui_capable() && !host.has_ui());
    ui.serving.store(true, Ordering::SeqCst);
    assert!(host.has_ui());
    for update in [
        UiUpdate::Notify {
            message: "hi".into(),
            level: NoticeLevel::Warning,
        },
        UiUpdate::Status {
            key: "k".into(),
            text: Some("on".into()),
        },
        UiUpdate::Status {
            key: "k".into(),
            text: None,
        },
        UiUpdate::Panel {
            key: "p".into(),
            lines: Some(vec!["a".into()]),
            placement: Placement::BelowEditor,
        },
        UiUpdate::Title("T".into()),
        UiUpdate::EditorText("e".into()),
        UiUpdate::PasteToEditor("p".into()),
        // Headless in the worker: dropped.
        UiUpdate::WorkingMessage(Some("w".into())),
        UiUpdate::Theme("dark".into()),
        UiUpdate::ToolsExpanded(true),
    ] {
        host.ui("ext", update);
    }
    assert_eq!(
        *ui.emitted.lock().unwrap(),
        [
            json!({"method": "notify", "message": "hi", "notifyType": "warning"}),
            json!({"method": "setStatus", "statusKey": "k", "statusText": "on"}),
            json!({"method": "setStatus", "statusKey": "k"}),
            json!({"method": "setWidget", "widgetKey": "p", "widgetLines": ["a"], "widgetPlacement": "belowEditor"}),
            json!({"method": "setTitle", "title": "T"}),
            json!({"method": "set_editor_text", "text": "e"}),
            json!({"method": "set_editor_text", "text": "p"}),
        ]
    );
    // A dialog answer of the wrong type is the default; the editor has no timeout.
    let cancel = CancelToken::new();
    let select = host
        .request(
            "ext",
            "ui/select",
            &json!({"title": "t", "options": ["x"], "timeoutMs": 250}),
            &cancel,
        )
        .unwrap();
    assert_eq!(select, Value::Null);
    let editor = host
        .request(
            "ext",
            "ui/editor",
            &json!({"title": "t", "prefill": "x", "timeoutMs": 9}),
            &cancel,
        )
        .unwrap();
    assert_eq!(editor, Value::Null);
    assert_eq!(
        *ui.dialogs.lock().unwrap(),
        [
            (
                json!({"method": "select", "title": "t", "options": ["x"], "timeout": 250}),
                Some(Duration::from_millis(250))
            ),
            (json!({"method": "editor", "title": "t", "prefill": "x"}), None),
        ]
    );
    // The headless rest of the UI context.
    assert_eq!(
        host.request("ext", "ui/getEditorText", &json!({}), &cancel).unwrap(),
        json!("")
    );
    assert_eq!(
        host.request("ext", "ui/getAllThemes", &json!({}), &cancel).unwrap(),
        json!([])
    );
}

#[test]
fn session_core_extension_requests_without_a_session_or_client() {
    let host = host(None, "print");
    assert!(!host.ui_capable() && !host.has_ui());
    let cancel = CancelToken::new();
    assert_eq!(
        host.request("ext", "ui/confirm", &json!({"title": "t", "message": "m"}), &cancel)
            .unwrap(),
        json!(false)
    );
    assert_eq!(
        host.request("ext", "ui/select", &json!({"title": "t", "options": []}), &cancel)
            .unwrap(),
        Value::Null
    );
    // The worker's command context answers session changes without acting; sendMessage is a no-op.
    for method in ["session/new", "session/fork", "session/switch"] {
        assert_eq!(
            host.request("ext", method, &json!({}), &cancel).unwrap(),
            json!({"cancelled": false})
        );
    }
    assert_eq!(
        host.request("ext", "session/reload", &json!({}), &cancel).unwrap(),
        Value::Null
    );
    assert_eq!(
        host.request("ext", "session/sendMessage", &json!({}), &cancel).unwrap(),
        Value::Null
    );
    assert_eq!(
        host.request("ext", "tools/list", &json!({}), &cancel).unwrap(),
        json!([])
    );
    assert_eq!(
        host.request("ext", "commands/list", &json!({}), &cancel).unwrap(),
        json!([])
    );
    // Requests that need the session fail until it is open.
    assert_eq!(
        host.request("ext", "agent/abort", &json!({}), &cancel)
            .unwrap_err()
            .to_string(),
        "The session is not open"
    );
    assert_eq!(
        host.request("ext", "session/getName", &json!({}), &cancel)
            .unwrap_err()
            .to_string(),
        "The session is not open"
    );
    let context = host.request("ext", "host/context", &json!({}), &cancel).unwrap();
    assert_eq!(
        context,
        json!({"cwd": "/w", "mode": "print", "hasUI": false, "model": null, "thinkingLevel": "medium", "isIdle": true,
            "projectTrusted": true, "hasPendingMessages": false, "contextUsage": null, "systemPrompt": null,
            "scopedModels": [], "systemPromptOptions": {"cwd": "/w"}})
    );
    assert_eq!(
        session_core::extension_host::content_to_text(
            &json!([{"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}])
        ),
        "a\nb"
    );
    let api = session_core::extension_apis::ExtensionCommandsApi::new(None, "/w".into(), "/a".into(), "/h".into());
    assert_eq!(api.list(), json!([]));
}

/// `extensions.list` (worker-services.ts): every registered command by invocation name with its Pi `SourceInfo`
/// (`auto` for the profile's extensions directory).
#[test]
fn session_core_extension_commands_are_listed_for_cells() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (agent, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&cwd).unwrap();
    let dir = agent.join("extensions").join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py");
    let capabilities = json!({"commands": [{"name": "go", "description": "Go"}, {"name": "bare"}]});
    let manifest = json!({"command": python, "args": ["-I", probe], "env": {"PROBE_CAPABILITIES": capabilities.to_string()},
        "startupTimeoutMs": 120_000});
    let path = dir.join("extension.json");
    std::fs::write(&path, manifest.to_string()).unwrap();
    let manifests = vec![extensions::parse_manifest(&path, &manifest.to_string()).unwrap()];
    let session = extensions::SessionInfo {
        cwd: cwd.display().to_string(),
        mode: "print".into(),
        has_ui: false,
        flags: Map::new(),
    };
    let host = Arc::new(extensions::ExtensionHost::load(
        manifests,
        session,
        Arc::new(extensions::NoHost),
    ));
    let api =
        session_core::extension_apis::ExtensionCommandsApi::new(Some(host.clone()), cwd, agent.clone(), root.clone());
    let source = json!({"path": path.display().to_string(), "source": "auto", "scope": "user", "origin": "top-level",
        "baseDir": agent.display().to_string()});
    assert_eq!(
        api.list(),
        json!([{"name": "go", "description": "Go", "sourceInfo": source}, {"name": "bare", "sourceInfo": source}])
    );
    host.shutdown();
}
