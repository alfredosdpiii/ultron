//! `session_before_compact` with the compaction's `signal` (`Router::emit_cancellable`): AgentSession hands the hook
//! `event.signal` and ends the compaction aborted whatever the hook answers after an abort. Over the protocol the
//! signal is `notifications/cancelled` for the event request: the host tells the extension, stops waiting for it
//! at once (a hook that ignores the signal does not hold the compaction), and calls no further handler.

use extensions::rpc::CancelToken;
use extensions::{Cancelled, ExtensionHost, Manifest, NoHost, SessionInfo, parse_manifest};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// An extension subscribed to `session_before_compact` that logs `start <requestId>` when the event arrives and
/// `cancelled <requestId> <reason>` when the host cancels it. Mode `hold` never answers the event (and ignores the
/// cancellation); `answer` answers at once with a supplied compaction.
const SCRIPT: &str = r#"import json, os, sys, threading
log_path, mode = os.environ["HOOK_LOG"], os.environ["HOOK_MODE"]
lock = threading.Lock()
def log(line):
    with lock, open(log_path, "a") as out:
        out.write(line + "\n")
def send(message):
    with lock:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"protocolVersion": "2026-10-10",
            "extensionInfo": {"name": mode}, "capabilities": {"events": ["session_before_compact"]}}})
    elif method == "event":
        log("start %s" % m["id"])
        if mode == "answer":
            send({"jsonrpc": "2.0", "id": m["id"], "result": {"compaction": {"summary": "from " + mode}}})
    elif method == "notifications/cancelled":
        log("cancelled %s %s" % (m["params"]["requestId"], m["params"].get("reason")))
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {}})
"#;

fn manifest(dir: &Path, mode: &str) -> Manifest {
    let ext = dir.join(mode);
    std::fs::create_dir_all(&ext).unwrap();
    let script = ext.join("hook.py");
    std::fs::write(&script, SCRIPT).unwrap();
    let value = json!({
        "name": mode,
        "command": python(),
        "args": ["-I", script.to_string_lossy()],
        "env": {"HOOK_LOG": ext.join("log").to_string_lossy(), "HOOK_MODE": mode},
        "eventTimeoutMs": 120_000,
        "startupTimeoutMs": 120_000,
    });
    let path = ext.join("extension.json");
    let text = value.to_string();
    std::fs::write(&path, &text).unwrap();
    parse_manifest(&path, &text).unwrap()
}

fn load(manifests: Vec<Manifest>) -> ExtensionHost {
    let session = SessionInfo {
        cwd: "/work".into(),
        mode: "rpc".into(),
        has_ui: false,
        flags: Map::new(),
    };
    let host = ExtensionHost::load(manifests, session, Arc::new(NoHost));
    assert!(host.diagnostics.is_empty(), "{:?}", host.diagnostics);
    host
}

fn log(dir: &Path, mode: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(mode).join("log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Waits for a condition on the extension's log (a hang guard, never a pass condition).
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(120);
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn event() -> Value {
    json!({"type": "session_before_compact", "reason": "manual", "willRetry": false})
}

/// An abort while a hook runs ends the dispatch at once with the signal's reason, though the hook never answers;
/// the hook is told (`notifications/cancelled` for its event request, reason "Aborted"), and the extension after it
/// is never called; nothing is reported as a handler error.
#[test]
fn extensions_cancellable_abort_gives_up_a_hook_that_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let host = load(vec![manifest(dir.path(), "hold"), manifest(dir.path(), "answer")]);
    let errors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = errors.clone();
    host.router()
        .on_error(move |e| sink.lock().unwrap().push(e.error.clone()));
    let token = CancelToken::new();
    let outcome = std::thread::scope(|scope| {
        let dispatch = scope.spawn(|| host.router().emit_cancellable(&event(), &token));
        wait_for("the hook to start", || !log(dir.path(), "hold").is_empty());
        token.cancel(None);
        dispatch.join().unwrap()
    });
    assert_eq!(outcome, Err(Cancelled("Aborted".into())));
    let started = log(dir.path(), "hold")[0].clone();
    let id = started.strip_prefix("start ").unwrap().to_string();
    wait_for("the cancellation to reach the hook", || {
        log(dir.path(), "hold").len() == 2
    });
    assert_eq!(log(dir.path(), "hold"), [started, format!("cancelled {id} Aborted")]);
    assert!(log(dir.path(), "answer").is_empty(), "no handler runs after the abort");
    // A given-up hook is not a handler error (TypeScript reports only what a handler throws).
    assert!(errors.lock().unwrap().is_empty(), "{:?}", errors.lock().unwrap());
    // A signal that already fired dispatches nothing.
    assert_eq!(
        host.router().emit_cancellable(&event(), &token),
        Err(Cancelled("Aborted".into()))
    );
    assert_eq!(log(dir.path(), "hold").len(), 2);
    host.shutdown();
}

/// Without an abort the dispatch is `emit`'s: every handler runs and the last truthy result is returned.
#[test]
fn extensions_cancellable_without_an_abort_is_emit() {
    let dir = tempfile::tempdir().unwrap();
    let host = load(vec![manifest(dir.path(), "answer")]);
    let token = CancelToken::new();
    let expected = json!({"compaction": {"summary": "from answer"}});
    assert_eq!(
        host.router().emit_cancellable(&event(), &token),
        Ok(Some(expected.clone()))
    );
    assert_eq!(host.router().emit(&event()), Some(expected));
    assert_eq!(log(dir.path(), "answer").len(), 2);
    host.shutdown();
}
