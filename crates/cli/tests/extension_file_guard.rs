//! An extension's `before_file_write` as a real file guard in a session (legacy-extension-adapter.ts `fileGuards`,
//! file-hooks.ts `beforeWrite`): a cell's `write()` goes through the host's `files.before_write`, the session's file
//! hooks offer it to the extension process (an example guard written to a temp dir), and its answer decides: a
//! block refuses the write (`[extension] <reason>`), a message lets it through with a note, a failing handler lets
//! it through unchecked with a note. The guard's statistics reach an inspected `rlm.pool` (`guards`, per-turn time
//! reset when the next root turn begins) and every outcome is a line in ULTRON_LOKI_LOG.

mod common;

use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// The example extension: refuses content with "forbidden", fails on "crash", otherwise lets the write through
/// with a note. Each event takes at least 50 ms, so guard times are never zero.
const FILE_GUARD: &str = r#"import json, os, sys, time

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message or "method" not in message:
        continue
    method, params = message["method"], message.get("params") or {}
    if method == "initialize":
        result = {"protocolVersion": "2026-10-10", "extensionInfo": {"name": "file-guard", "version": "1.0.0"},
                  "capabilities": {"events": ["before_file_write"]}}
    elif method == "event":
        with open(os.environ["GUARD_EVENTS"], "a", encoding="utf-8") as log:
            log.write(json.dumps(params) + "\n")
        time.sleep(0.05)
        name = os.path.basename(params["path"])
        if "crash" in params["content"]:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32603, "message": "guard crashed"}})
            continue
        if "forbidden" in params["content"]:
            result = {"block": True, "reason": "file-guard: forbidden text in " + name}
        else:
            result = {"message": "file-guard: checked " + name}
    elif method == "shutdown":
        result = {}
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "Method not found"}})
        continue
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
"#;

struct Actor {
    tx: Sender<Input>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Actor {
    fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    fn close(mut self) {
        let (done, closed) = std::sync::mpsc::channel();
        self.tx.send(Input::Close(done)).unwrap();
        closed.recv_timeout(HANG).unwrap();
        self.handle.take().unwrap().join().unwrap();
    }
}

fn open(sandbox: &common::Sandbox, guard_log: &Path) -> Actor {
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        (
            "ULTRON_SERVER_DIR".into(),
            sandbox.root.join("server").display().to_string(),
        ),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
        // Only the extension guards this session's writes.
        ("ULTRON_LOKI".into(), "off".into()),
        ("ULTRON_LOKI_LOG".into(), guard_log.display().to_string()),
    ];
    let env = cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let repo = session_journal::SessionRepo::new(&sandbox.sessions_dir(), session_journal::SystemClock::shared());
    let metadata = repo
        .create(&sandbox.cwd.display().to_string(), None, None)
        .unwrap()
        .metadata()
        .clone();
    let open = session_core::core::OpenOptions {
        env,
        flags: session_core::ModelFlags {
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..Default::default()
        },
        session_root: sandbox.sessions_dir(),
        metadata,
        mode: "rpc".into(),
        services: cli::compose::Services::default(),
        auto_name: false,
    };
    let (tx, handle, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
    Actor {
        tx,
        handle: Some(handle),
    }
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn guards(actor: &Actor) -> Vec<Value> {
    let pool = actor
        .call(Op::Inspect {
            request: "rlm.pool".into(),
            payload: json!({}),
        })
        .unwrap()
        .unwrap();
    pool["guards"].as_array().cloned().unwrap_or_default()
}

#[test]
fn cli_extension_before_file_write_guards_cell_writes() {
    let cell = "for name, text in [('a.py', 'x = \"forbidden\"\\n'), ('b.py', 'y = 1\\n'), ('c.py', 'crash = 1\\n')]:\n    try:\n        print(await write(name, text))\n    except Exception as e:\n        print('refused:', e)";
    let (port, seen) = common::mock_server(
        vec![
            common::anthropic_tool("t1", cell),
            common::anthropic_text("done"),
            common::anthropic_text("again"),
        ],
        common::Naming::Fail,
    );
    let sandbox = common::Sandbox::new(port);
    let events: PathBuf = sandbox.root.join("guard-events.jsonl");
    let guard_log = sandbox.root.join("logs/loki.jsonl");
    let dir = sandbox.agent_dir.join("extensions").join("file-guard");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("guard.py"), FILE_GUARD).unwrap();
    let manifest = json!({
        "name": "file-guard",
        "command": python(),
        "args": ["-I", dir.join("guard.py").to_string_lossy()],
        "env": {"GUARD_EVENTS": events.to_string_lossy()},
        "startupTimeoutMs": 120_000,
        "eventTimeoutMs": 120_000,
    });
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    let actor = open(&sandbox, &guard_log);

    let ran = actor
        .call(Op::PromptAndWait {
            message: "write three files".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ran["error"], Value::Null, "{ran}");
    let requests = seen.lock().unwrap().requests.clone();
    let result = common::tool_result_text(&requests[1]["body"], "t1").unwrap();
    assert!(
        result.contains("refused: a.py was not written: [extension] file-guard: forbidden text in a.py"),
        "{result}"
    );
    assert!(result.contains("[extension] file-guard: checked b.py"), "{result}");
    assert!(
        result.contains("[extension] extension did not check this write to c.py (")
            && result.contains("); it was written unchecked."),
        "{result}"
    );
    let cwd = &sandbox.cwd;
    assert!(!cwd.join("a.py").exists());
    assert_eq!(std::fs::read_to_string(cwd.join("b.py")).unwrap(), "y = 1\n");
    assert_eq!(std::fs::read_to_string(cwd.join("c.py")).unwrap(), "crash = 1\n");

    // What the extension saw: the absolute path, the content, the session directory and the lane.
    let seen_events = lines(&events);
    assert_eq!(seen_events.len(), 3);
    assert_eq!(
        seen_events[0],
        json!({"type": "before_file_write", "path": cwd.join("a.py").to_string_lossy(),
            "content": "x = \"forbidden\"\n", "cwd": cwd.to_string_lossy(), "lane": "main"})
    );

    // The guard's statistics: the before-write guard counts the three checks; the after-cell one is listed idle.
    let stats = guards(&actor);
    let names: Vec<&Value> = stats.iter().map(|g| &g["name"]).collect();
    assert_eq!(names, vec![&json!("extension"), &json!("extension")]);
    let writes = &stats[0];
    assert_eq!(
        (&writes["checks"], &writes["blocked"], &writes["unchecked"]),
        (&json!(3), &json!(1), &json!(1))
    );
    let total = writes["totalMs"].as_u64().unwrap();
    assert!(total >= 150 && writes["turnMs"].as_u64() == Some(total), "{writes}");
    assert_eq!(stats[1]["checks"], 0);

    // ULTRON_LOKI_LOG: one line per guard outcome.
    let records = lines(&guard_log);
    let outcomes: Vec<(&Value, &Value, &Value, &Value, &Value)> = records
        .iter()
        .map(|r| (&r["guard"], &r["phase"], &r["outcome"], &r["paths"], &r["lane"]))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (
                &json!("extension"),
                &json!("before_write"),
                &json!("blocked"),
                &json!(["a.py"]),
                &json!("main")
            ),
            (
                &json!("extension"),
                &json!("before_write"),
                &json!("allowed"),
                &json!(["b.py"]),
                &json!("main")
            ),
            (
                &json!("extension"),
                &json!("before_write"),
                &json!("unchecked"),
                &json!(["c.py"]),
                &json!("main")
            ),
        ]
    );
    assert_eq!(records[0]["detail"], "[extension] file-guard: forbidden text in a.py");
    assert!(
        records
            .iter()
            .all(|r| r["at"].as_u64().is_some() && r["ms"].as_u64().unwrap() >= 50)
    );

    // The next root turn starts the guard's per-turn time from zero; its total stays.
    let again = actor
        .call(Op::PromptAndWait {
            message: "nothing to write".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(again["error"], Value::Null, "{again}");
    let after = &guards(&actor)[0];
    assert_eq!(
        (after["turnMs"].as_u64(), after["totalMs"].as_u64()),
        (Some(0), Some(total))
    );
    actor.close();
}
