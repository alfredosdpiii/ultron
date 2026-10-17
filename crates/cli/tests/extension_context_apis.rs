//! The rest of the extension context in a session (core/extensions/types.ts `ExtensionContext`, bound as the
//! worker's `LegacyExtensionAdapter` binds it): the session actor (session-core `core`) against a scripted Anthropic
//! server, with a real extension process (crates/extensions/tests/probe.py) whose commands ask the host.
//!
//! - `ctx.modelRegistry`: `find`, `getAll`, `getAvailable`, `getApiKeyAndHeaders` (the literal cases of
//!   test/model-registry.test.ts: an `authHeader` key resolved by a command on every request and exactly once per
//!   request, the legacy missing-key error, a failing command), `getApiKeyForProvider`, `hasConfiguredAuth`,
//!   `isUsingOAuth`;
//! - `ctx.scopedModels` and the command context's `getSystemPromptOptions` in `host/context`;
//! - `ctx.sessionManager` getters on the real session;
//! - `getActiveTools`/`setActiveTools` (the REPL pinned, REPL tools filtered, `selectedTools` of
//!   `before_agent_start`) while the model's tool list stays `[rlm]`;
//! - `getAllTools` with `promptGuidelines`, and a REPL tool's `label` in `tools.list()`.

mod common;

use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn probe() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/tests/probe.py")
}

struct World {
    sandbox: common::Sandbox,
    seen: Arc<Mutex<common::Seen>>,
    log: PathBuf,
    counter: PathBuf,
}

fn world(replies: Vec<String>) -> World {
    let (port, seen) = common::mock_server(replies, common::Naming::Fail);
    let sandbox = common::Sandbox::new(port);
    let counter = sandbox.root.join("auth-counter");
    std::fs::write(&counter, "0").unwrap();
    let path = counter.display().to_string();
    let counting = format!(
        "!sh -c 'count=$(/bin/cat \"{path}\"); count=$((count + 1)); echo \"$count\" > \"{path}\"; echo \"token-$count\"'"
    );
    let custom = |api_key: Option<&str>| {
        let mut provider = json!({"baseUrl": "https://example.test/v1", "api": "openai-completions", "authHeader": true,
            "models": [{"id": "test-model"}]});
        if let Some(key) = api_key {
            provider["apiKey"] = json!(key);
        }
        provider
    };
    let models = json!({"providers": {
        "anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")},
        "custom-provider": custom(Some(&counting)),
        "keyless-provider": custom(None),
        "failing-provider": custom(Some("!exit 1")),
    }});
    std::fs::write(sandbox.agent_dir.join("models.json"), models.to_string()).unwrap();
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        json!({"enabledModels": ["anthropic/claude-sonnet-4-5:high"]}).to_string(),
    )
    .unwrap();
    let log = sandbox.root.join("probe.jsonl");
    let dir = sandbox.agent_dir.join("extensions").join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let capabilities = json!({
        "events": ["before_agent_start"],
        "functions": [
            {"name": "host", "description": "Ask the host"},
            {"name": "progress", "label": "Progress", "description": "Reports progress", "executionMode": "sequential",
                "promptGuidelines": ["Use progress for long work."]},
        ],
        "commands": [{"name": "tools"}, {"name": "models"}, {"name": "session"}],
    });
    let model = |provider: &str, id: &str| json!({"provider": provider, "id": id});
    let commands = json!({
        "tools": [
            ["tools/active", {}],
            ["tools/setActive", {"names": ["host", "web", "rlm"]}],
            ["tools/active", {}],
            ["tools/list", {}],
            ["host/context", {}],
        ],
        "models": [
            ["models/find", model("custom-provider", "test-model")],
            ["models/find", model("custom-provider", "missing")],
            ["models/all", {}],
            ["models/available", {}],
            ["models/getApiKeyAndHeaders", model("custom-provider", "test-model")],
            ["models/getApiKeyAndHeaders", model("custom-provider", "test-model")],
            ["models/getApiKeyAndHeaders", model("keyless-provider", "test-model")],
            ["models/getApiKeyAndHeaders", model("failing-provider", "test-model")],
            ["models/getApiKeyAndHeaders", model("anthropic", "claude-sonnet-4-5")],
            ["models/getApiKeyForProvider", {"provider": "anthropic"}],
            ["models/getApiKeyForProvider", {"provider": "keyless-provider"}],
            ["models/hasConfiguredAuth", {"provider": "anthropic"}],
            ["models/hasConfiguredAuth", {"provider": "keyless-provider"}],
            ["models/isUsingOAuth", {"provider": "anthropic"}],
            ["models/find", {"provider": "anthropic"}],
        ],
        "session": [
            ["session/file", {}],
            ["session/id", {}],
            ["session/dir", {}],
            ["session/cwd", {}],
            ["session/header", {}],
            ["session/leaf", {}],
            ["session/leafEntry", {}],
            ["session/entry", {"id": {"$result": "session/leaf"}}],
            ["session/entry", {"id": "no-such-entry"}],
            ["session/label", {"id": {"$result": "session/leaf"}}],
            ["session/setLabel", {"entryId": {"$result": "session/leaf"}, "label": "mark"}],
            ["agent/waitForIdle", {}],
            ["session/label", {"id": {"$result": "session/leaf"}}],
            ["session/tree", {}],
            ["session/branch", {}],
            ["session/contextEntries", {}],
            ["session/entry", {}],
        ],
    });
    let manifest = json!({
        "name": "probe",
        "command": python(),
        "args": ["-I", probe().to_string_lossy()],
        "env": {
            "PROBE_LOG": log.to_string_lossy(),
            "PROBE_CAPABILITIES": capabilities.to_string(),
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
        counter,
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

impl World {
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
        let open = session_core::core::OpenOptions {
            env,
            flags: session_core::ModelFlags {
                model: Some("anthropic/claude-sonnet-4-5".into()),
                ..Default::default()
            },
            session_root: sandbox.sessions_dir(),
            metadata,
            mode: "print".into(),
            services: cli::compose::Services::default(),
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

    /// The host's answers to command `name`'s requests, in order.
    fn outcomes(&self, name: &str) -> Vec<(String, Value)> {
        let log = self.log();
        let start = log
            .iter()
            .position(|e| e["command"]["name"] == name)
            .unwrap_or_else(|| panic!("no command {name}: {log:?}"));
        log[start + 1..]
            .iter()
            .take_while(|e| e.get("command").is_none())
            .filter(|e| e.get("host").is_some())
            .map(|e| (e["host"].as_str().unwrap().to_string(), e["outcome"].clone()))
            .collect()
    }

    fn command(&self, actor: &Actor, name: &str) -> Vec<(String, Value)> {
        actor
            .call(Op::ExtensionCommand {
                name: name.into(),
                args: String::new(),
            })
            .unwrap();
        // The command's answers are logged before it returns.
        self.outcomes(name)
    }
}

fn get(outcomes: &[(String, Value)], method: &str, n: usize) -> Value {
    outcomes
        .iter()
        .filter(|(m, _)| m == method)
        .nth(n)
        .map(|(_, o)| o.clone())
        .unwrap_or_else(|| panic!("no answer {n} to {method}: {outcomes:?}"))
}

fn result(outcomes: &[(String, Value)], method: &str, n: usize) -> Value {
    let outcome = get(outcomes, method, n);
    assert!(outcome.get("error").is_none(), "{method}: {outcome}");
    outcome["result"].clone()
}

fn ids(models: &Value) -> Vec<String> {
    models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| format!("{}/{}", m["provider"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect()
}

#[test]
fn cli_extension_context_model_registry_session_manager_and_active_tools() {
    let world = world(vec![
        common::anthropic_tool(
            "t1",
            "import json\nprint(json.dumps(await tools.list()))\nprint(await tools.progress(updates=['half']))",
        ),
        common::anthropic_text("done"),
    ]);
    let metadata = world.create();
    let actor = world.open(metadata.clone());

    // getActiveTools / setActiveTools: the REPL stays, the extension's REPL tool `host` stays out.
    let tools = world.command(&actor, "tools");
    assert_eq!(result(&tools, "tools/active", 0), json!(["rlm"]));
    assert_eq!(result(&tools, "tools/setActive", 0), Value::Null);
    assert_eq!(result(&tools, "tools/active", 1), json!(["web", "rlm"]));
    let all_tools = result(&tools, "tools/list", 0);
    assert_eq!(
        (&all_tools[1]["name"], &all_tools[1]["promptGuidelines"]),
        (&json!("progress"), &json!(["Use progress for long work."]))
    );
    assert!(all_tools[0].get("promptGuidelines").is_none());
    let context = result(&tools, "host/context", 0);
    assert_eq!(
        context["systemPromptOptions"],
        json!({"cwd": world.sandbox.cwd.display().to_string()})
    );
    let scoped = context["scopedModels"].as_array().unwrap();
    assert_eq!(scoped.len(), 1, "{scoped:?}");
    assert_eq!(
        (
            &scoped[0]["model"]["provider"],
            &scoped[0]["model"]["id"],
            &scoped[0]["thinkingLevel"]
        ),
        (&json!("anthropic"), &json!("claude-sonnet-4-5"), &json!("high"))
    );

    // A run: before_agent_start sees the active tools; the model still has exactly [rlm]; the cell sees the REPL
    // tool's label.
    let ran = actor
        .call(Op::PromptAndWait {
            message: "hello".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ran["error"], Value::Null, "{ran}");
    let start = world
        .log()
        .into_iter()
        .find(|e| e["event"]["type"] == "before_agent_start")
        .unwrap();
    assert_eq!(
        start["event"]["systemPromptOptions"]["selectedTools"],
        json!(["web", "rlm"])
    );
    let requests = world.seen.lock().unwrap().requests.clone();
    for request in &requests {
        let names: Vec<&str> = request["body"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["rlm"]);
    }
    let output = common::tool_result_text(&requests[1]["body"], "t1").unwrap();
    assert!(
        output.contains(r#"{"name": "progress", "label": "Progress", "description": "Reports progress"}"#),
        "{output}"
    );
    assert!(
        output.contains(r#"{"name": "host", "description": "Ask the host"}"#),
        "{output}"
    );
    assert!(output.contains("done"), "{output}");

    // ctx.modelRegistry.
    let models = world.command(&actor, "models");
    let found = result(&models, "models/find", 0);
    assert_eq!(
        (&found["provider"], &found["id"], &found["api"], &found["baseUrl"]),
        (
            &json!("custom-provider"),
            &json!("test-model"),
            &json!("openai-completions"),
            &json!("https://example.test/v1")
        )
    );
    assert_eq!(result(&models, "models/find", 1), Value::Null);
    let all = ids(&result(&models, "models/all", 0));
    for id in [
        "anthropic/claude-sonnet-4-5",
        "custom-provider/test-model",
        "keyless-provider/test-model",
    ] {
        assert!(all.contains(&id.to_string()), "{id} in {all:?}");
    }
    let available = ids(&result(&models, "models/available", 0));
    assert!(available.contains(&"anthropic/claude-sonnet-4-5".to_string()));
    assert!(available.contains(&"custom-provider/test-model".to_string()));
    assert!(!available.contains(&"keyless-provider/test-model".to_string()));
    // "getApiKeyAndHeaders resolves authHeader on every request" and "... resolves configured auth exactly once".
    assert_eq!(
        result(&models, "models/getApiKeyAndHeaders", 0),
        json!({"ok": true, "apiKey": "token-1", "headers": {"Authorization": "Bearer token-1"}})
    );
    assert_eq!(
        result(&models, "models/getApiKeyAndHeaders", 1),
        json!({"ok": true, "apiKey": "token-2", "headers": {"Authorization": "Bearer token-2"}})
    );
    assert_eq!(std::fs::read_to_string(&world.counter).unwrap().trim(), "2");
    // "getApiKeyAndHeaders preserves the legacy missing-key authHeader error".
    assert_eq!(
        result(&models, "models/getApiKeyAndHeaders", 2),
        json!({"ok": false, "error": "No API key found for \"keyless-provider\""})
    );
    // "getApiKeyAndHeaders returns an error for failed authHeader resolution".
    let failed = result(&models, "models/getApiKeyAndHeaders", 3);
    assert_eq!(failed["ok"], false);
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .starts_with("Failed to resolve API key for provider \"failing-provider\""),
        "{failed}"
    );
    assert_eq!(
        result(&models, "models/getApiKeyAndHeaders", 4),
        json!({"ok": true, "apiKey": "test-key"})
    );
    assert_eq!(result(&models, "models/getApiKeyForProvider", 0), json!("test-key"));
    assert_eq!(result(&models, "models/getApiKeyForProvider", 1), Value::Null);
    assert_eq!(result(&models, "models/hasConfiguredAuth", 0), json!(true));
    assert_eq!(result(&models, "models/hasConfiguredAuth", 1), json!(false));
    assert_eq!(result(&models, "models/isUsingOAuth", 0), json!(false));
    assert_eq!(
        get(&models, "models/find", 2),
        json!({"error": "\"id\" must be a string"})
    );

    // ctx.sessionManager on the real session.
    let session = world.command(&actor, "session");
    let path = metadata.path.display().to_string();
    assert_eq!(result(&session, "session/file", 0), json!(path));
    assert_eq!(result(&session, "session/id", 0), json!(metadata.id));
    assert_eq!(
        result(&session, "session/dir", 0),
        json!(metadata.path.parent().unwrap().display().to_string())
    );
    assert_eq!(
        result(&session, "session/cwd", 0),
        json!(world.sandbox.cwd.display().to_string())
    );
    assert_eq!(
        result(&session, "session/header", 0),
        json!({"type": "session", "version": 3, "id": metadata.id,
            "timestamp": session_file::js::iso_string(metadata.created_at),
            "cwd": world.sandbox.cwd.display().to_string()})
    );
    let read = actor.call(Op::Entries { since: None }).unwrap().unwrap();
    let (entries, leaf) = (read["entries"].as_array().unwrap().clone(), read["leafId"].clone());
    assert_eq!(result(&session, "session/leaf", 0), leaf);
    let tip = entries.iter().find(|e| e["id"] == leaf).unwrap().clone();
    assert_eq!(result(&session, "session/leafEntry", 0), tip);
    assert_eq!(result(&session, "session/entry", 0), tip);
    assert_eq!(result(&session, "session/entry", 1), Value::Null);
    assert_eq!(result(&session, "session/label", 0), Value::Null);
    assert_eq!(result(&session, "session/label", 1), json!("mark"));
    // The tree: the labelled tip is the deepest node of the only path.
    let tree = result(&session, "session/tree", 0);
    let mut node = &tree[0];
    let mut path_ids = vec![node["entry"]["id"].clone()];
    while let Some(child) = node["children"].as_array().and_then(|c| c.first()) {
        node = child;
        path_ids.push(node["entry"]["id"].clone());
    }
    assert_eq!(node["entry"], tip);
    assert_eq!(node["label"], "mark");
    let branch = result(&session, "session/branch", 0);
    assert_eq!(
        branch
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].clone())
            .collect::<Vec<_>>(),
        path_ids
    );
    assert_eq!(branch, json!(entries));
    // Without a compaction the context entries are the branch.
    assert_eq!(result(&session, "session/contextEntries", 0), branch);
    assert_eq!(
        get(&session, "session/entry", 2),
        json!({"error": "\"id\" must be a string"})
    );
    actor.close();
}
