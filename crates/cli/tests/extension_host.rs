//! Third-party extensions in a session as the TypeScript worker binds them (legacy-extension-adapter.ts): the
//! session actor (session-core `core`) runs against a scripted Anthropic server with a real extension process
//! (crates/extensions/tests/probe.py) that logs what it receives and asks the host what its script says.
//!
//! - events: `session_start` ("startup", and "fork" with `previousSessionFile` in a fork), `before_agent_start`
//!   (its message reaches the model after the prompt), `context`, `agent_end` (`messages: []`), `before_file_write`
//!   (a block reaches the cell as `[extension] <reason>`), `session_before_fork` (cancels), `session_shutdown`
//!   ("quit", or "fork" with `targetSessionFile`);
//! - host requests from a handler and from a command: `host/context`, `session/setName`/`getName`/`appendEntry`/
//!   `leaf`, `host/exec`, `thinking/get`, `tools/list`, `flags/get`, `model/set`, `agent/waitForIdle`, UI dialogs over
//!   the client UI, `host/shutdown` (an injected action), and "Method not found" for anything else.

mod common;

use serde_json::{Map, Value, json};
use session_core::core::{Input, Op};
use session_core::extension_host::ExtensionUi;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py")
}

/// A client UI that serves: it picks the second option, confirms, and records notifications.
#[derive(Default)]
struct FakeUi {
    dialogs: Mutex<Vec<Value>>,
    emitted: Mutex<Vec<Value>>,
}

impl ExtensionUi for FakeUi {
    fn serving(&self) -> bool {
        true
    }
    fn dialog(&self, request: Map<String, Value>, _: Option<Duration>, _: &dyn Fn() -> bool) -> Option<Value> {
        let request = Value::Object(request);
        self.dialogs.lock().unwrap().push(request.clone());
        match request["method"].as_str() {
            Some("select") => Some(json!({"value": request["options"][1]})),
            Some("confirm") => Some(json!({"confirmed": true})),
            _ => None,
        }
    }
    fn emit(&self, request: Map<String, Value>) {
        self.emitted.lock().unwrap().push(Value::Object(request));
    }
}

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

struct World {
    sandbox: common::Sandbox,
    seen: Arc<Mutex<common::Seen>>,
    log: PathBuf,
    ui: Arc<FakeUi>,
    shutdowns: Arc<AtomicUsize>,
}

fn world(replies: Vec<String>) -> World {
    let (port, seen) = common::mock_server(replies, common::Naming::Fail);
    let sandbox = common::Sandbox::new(port);
    let log = sandbox.root.join("probe.jsonl");
    let dir = sandbox.agent_dir.join("extensions").join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let capabilities = json!({
        "events": ["session_start", "before_agent_start", "context", "agent_end", "session_before_fork",
            "before_file_write", "session_shutdown"],
        "functions": [{"name": "host"}],
        "commands": [{"name": "probe", "description": "Ask the host"}],
        "flags": [{"name": "plan", "type": "boolean", "default": true}],
    });
    let answers = json!({
        "session_start": {"$host": ["host/context", {}]},
        "before_agent_start": {"message": {"customType": "probe", "content": "Injected context", "display": false}},
        "session_before_fork": {"cancel": true},
        "before_file_write": {"block": true, "reason": "probe says no"},
    });
    let commands = json!({"probe": [
        ["session/setName", {"name": "Probe named"}],
        ["session/appendEntry", {"customType": "probe-note", "data": {"n": 1}}],
        ["agent/waitForIdle", {}],
        ["session/getName", {}],
        ["host/exec", {"command": "/bin/echo", "args": ["hi there"]}],
        ["thinking/get", {}],
        ["tools/list", {}],
        ["flags/get", {"name": "plan"}],
        ["ui/select", {"title": "Pick", "options": ["a", "b"]}],
        ["ui/confirm", {"title": "Sure?", "message": "Really"}],
        ["ui/input", {"title": "Name"}],
        ["session/leaf", {}],
        ["model/set", {"provider": "anthropic", "id": "claude-sonnet-4-5"}],
        ["model/set", {"provider": "nobody", "id": "nothing"}],
        ["host/shutdown", {}],
        ["unknown/method", {}],
    ]});
    let manifest = json!({
        "name": "probe",
        "command": python(),
        "args": ["-I", probe().to_string_lossy()],
        "env": {
            "PROBE_LOG": log.to_string_lossy(),
            "PROBE_CAPABILITIES": capabilities.to_string(),
            "PROBE_ANSWERS": answers.to_string(),
            "PROBE_COMMANDS": commands.to_string(),
        },
        "startupTimeoutMs": 120_000,
        "eventTimeoutMs": 120_000,
    });
    std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    World {
        sandbox,
        seen,
        log,
        ui: Arc::new(FakeUi::default()),
        shutdowns: Arc::new(AtomicUsize::new(0)),
    }
}

impl World {
    /// The session actor over `metadata` in RPC mode (the client UI serving).
    fn open(&self, metadata: session_journal::repo::Metadata) -> Actor {
        let sandbox = &self.sandbox;
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
        ];
        let env = cli::Env {
            cwd: sandbox.cwd.clone(),
            home: sandbox.root.clone(),
            var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            docs: config::paths::normalize(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
            ),
        };
        let shutdowns = self.shutdowns.clone();
        let open = session_core::core::OpenOptions {
            env,
            flags: session_core::ModelFlags {
                model: Some("anthropic/claude-sonnet-4-5".into()),
                ..Default::default()
            },
            session_root: sandbox.sessions_dir(),
            metadata,
            mode: "rpc".into(),
            services: cli::compose::Services {
                extension_ui: Some(self.ui.clone() as Arc<dyn ExtensionUi>),
                on_extension_shutdown: Some(Arc::new(move || {
                    shutdowns.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            },
            auto_name: false,
        };
        let (tx, handle, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
        Actor {
            tx,
            handle: Some(handle),
        }
    }

    fn create(&self) -> session_journal::repo::Metadata {
        let repo =
            session_journal::SessionRepo::new(&self.sandbox.sessions_dir(), session_journal::SystemClock::shared());
        let created = repo
            .create(&self.sandbox.cwd.display().to_string(), None, None)
            .unwrap();
        created.metadata().clone()
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Waits until the log holds an entry `ready` accepts (a hang guard, never a pass condition).
    fn wait_for(&self, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(found) = self.log().into_iter().find(|e| ready(e)) {
                return found;
            }
            assert!(start.elapsed() < HANG, "timed out waiting for {what}: {:?}", self.log());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["event"]["type"] == kind)
            .map(|e| e["event"].clone())
            .collect()
    }

    /// The host's answers to the command's requests, in order.
    fn outcomes(&self) -> Vec<(String, Value)> {
        let log = self.log();
        let start = log.iter().position(|e| e.get("command").is_some()).unwrap();
        log[start..]
            .iter()
            .filter(|e| e.get("host").is_some())
            .map(|e| (e["host"].as_str().unwrap().to_string(), e["outcome"].clone()))
            .collect()
    }
}

fn texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| match &m["content"] {
            Value::String(text) => vec![text.clone()],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn cli_extension_host_events_requests_and_ui_in_a_session() {
    let world = world(vec![
        common::anthropic_tool(
            "t1",
            "try:\n    await write('blocked.txt', 'hi')\nexcept Exception as e:\n    print('refused:', e)",
        ),
        common::anthropic_text("done"),
    ]);
    let actor = world.open(world.create());

    // session_start without waiting; its handler reads host/context while the actor runs.
    let start = world.wait_for("session_start", |e| e["event"]["type"] == "session_start");
    assert_eq!(start["event"], json!({"type": "session_start", "reason": "startup"}));
    let context = world.wait_for("host/context", |e| e["host"] == "host/context")["outcome"]["result"].clone();
    assert_eq!(context["cwd"], world.sandbox.cwd.display().to_string());
    assert_eq!((&context["mode"], &context["hasUI"]), (&json!("rpc"), &json!(true)));
    assert_eq!(context["model"]["provider"], "anthropic");
    assert_eq!(context["model"]["id"], "claude-sonnet-4-5");
    assert_eq!(
        (&context["isIdle"], &context["hasPendingMessages"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(context["contextUsage"], Value::Null);
    assert!(context["systemPrompt"].as_str().unwrap().contains("rlm"));

    // A run: before_agent_start's message follows the prompt to the model; context sees the request; the cell's
    // write is refused by before_file_write; agent_end follows.
    let ran = actor
        .call(Op::PromptAndWait {
            message: "hello".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ran["error"], Value::Null, "{ran}");
    let requests = world.seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2);
    let first = texts(&requests[0]["body"]);
    let at = first.iter().position(|t| t.contains("hello")).unwrap();
    assert_eq!(first[at + 1], "Injected context", "{first:?}");
    let result = common::tool_result_text(&requests[1]["body"], "t1").unwrap();
    assert!(
        result.contains("refused:") && result.contains("[extension] probe says no"),
        "{result}"
    );
    let agent_start = &world.events("before_agent_start")[0];
    assert_eq!(agent_start["prompt"], "hello");
    assert_eq!(agent_start["systemPromptOptions"]["selectedTools"], json!(["rlm"]));
    assert!(agent_start["systemPrompt"].is_string());
    let write = &world.events("before_file_write")[0];
    assert_eq!(
        write["path"],
        world.sandbox.cwd.join("blocked.txt").display().to_string()
    );
    assert_eq!((&write["content"], &write["lane"]), (&json!("hi"), &json!("main")));
    let contexts = world.events("context");
    assert_eq!(contexts.len(), 2);
    assert!(
        contexts[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["role"] != "system")
    );
    assert!(!world.sandbox.cwd.join("blocked.txt").exists());
    world.wait_for("agent_end", |e| e["event"]["type"] == "agent_end");
    assert_eq!(
        world.events("agent_end")[0],
        json!({"type": "agent_end", "messages": []})
    );

    // A command asking the host.
    actor
        .call(Op::ExtensionCommand {
            name: "probe".into(),
            args: String::new(),
        })
        .unwrap();
    let outcomes = world.outcomes();
    let get = |method: &str, n: usize| -> Value {
        outcomes
            .iter()
            .filter(|(m, _)| m == method)
            .nth(n)
            .map(|(_, o)| o.clone())
            .unwrap()
    };
    assert_eq!(get("session/setName", 0), json!({"result": null}));
    assert_eq!(get("session/getName", 0), json!({"result": "Probe named"}));
    assert_eq!(
        get("host/exec", 0),
        json!({"result": {"stdout": "hi there\n", "stderr": "", "code": 0, "killed": false}})
    );
    assert_eq!(get("thinking/get", 0)["result"], context["thinkingLevel"]);
    let tools = get("tools/list", 0)["result"].clone();
    assert_eq!(tools[0]["name"], "host");
    assert_eq!(tools[0]["sourceInfo"]["source"], "auto");
    assert_eq!(get("flags/get", 0), json!({"result": true}));
    assert_eq!(get("ui/select", 0), json!({"result": "b"}));
    assert_eq!(get("ui/confirm", 0), json!({"result": true}));
    assert_eq!(get("ui/input", 0), json!({"result": null}));
    assert_eq!(get("session/appendEntry", 0), json!({"result": null}));
    assert_eq!(get("agent/waitForIdle", 0), json!({"result": null}));
    // Fire-and-forget requests reach the actor in order: the wait for idle comes after them, so the name and the
    // custom entry (now the tip) are in place for the reads after it.
    let leaf = get("session/leaf", 0)["result"].clone();
    let (entries, leaf_id) = {
        let read = actor.call(Op::Entries { since: None }).unwrap().unwrap();
        (read["entries"].as_array().unwrap().clone(), read["leafId"].clone())
    };
    assert_eq!(leaf, leaf_id);
    let tip = entries.iter().find(|e| e["id"] == leaf_id).unwrap();
    assert_eq!(
        (&tip["type"], &tip["customType"], &tip["data"]),
        (&json!("custom"), &json!("probe-note"), &json!({"n": 1}))
    );
    assert_eq!(get("model/set", 0), json!({"result": true}));
    assert_eq!(get("model/set", 1), json!({"result": false}));
    assert_eq!(get("host/shutdown", 0), json!({"result": null}));
    assert_eq!(world.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        get("unknown/method", 0),
        json!({"error": "Method not found: unknown/method"})
    );
    let dialogs = world.ui.dialogs.lock().unwrap().clone();
    assert_eq!(
        dialogs,
        [
            json!({"method": "select", "title": "Pick", "options": ["a", "b"]}),
            json!({"method": "confirm", "title": "Sure?", "message": "Really"}),
            json!({"method": "input", "title": "Name"}),
        ]
    );

    // session_before_fork cancels a fork.
    let fork = actor
        .call(Op::BeforeFork {
            entry_id: leaf_id.as_str().unwrap().into(),
            position: "before".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(fork, json!({"cancelled": true}));
    assert_eq!(
        world.events("session_before_fork")[0],
        json!({"type": "session_before_fork", "entryId": leaf_id, "position": "before"})
    );

    // A client moved on to a fork: this session's shutdown says so, and the fork's start says where it came from.
    let target = world.create();
    let source_file = actor.call(Op::State).unwrap().unwrap()["sessionFile"].clone();
    actor
        .call(Op::Forked {
            target: target.path.display().to_string(),
        })
        .unwrap();
    actor.close();
    let shutdown = world.events("session_shutdown");
    assert_eq!(
        shutdown,
        [json!({"type": "session_shutdown", "reason": "fork", "targetSessionFile": target.path.display().to_string()})]
    );
    let forked = world.open(target);
    world.wait_for("the fork's session_start", |e| e["event"]["reason"] == "fork");
    assert_eq!(
        world.events("session_start")[1],
        json!({"type": "session_start", "reason": "fork", "previousSessionFile": source_file})
    );
    forked.close();
    assert_eq!(
        world.events("session_shutdown")[1],
        json!({"type": "session_shutdown", "reason": "quit"})
    );
}
