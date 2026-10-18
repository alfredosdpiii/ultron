//! An extension that registers its own `mcp` tool replaces the built-in MCP gateway (extensions/mcp/coexistence.ts
//! `preferNativeMcp`, ported from ultron-native-mcp.test.ts "another mcp extension replaces the built-in one": the
//! built-in extension is left out, and pi-mcp-adapter is then not skipped). The Rust form of "an extension
//! registering `mcp`" is an extension process registering the function `mcp` (crates/extensions/tests/probe.py):
//! `tools.mcp` is the extension's, the gateway's `mcp.json` servers never start (not even eager ones), its runtime
//! guide section and its `/mcp` are gone, and an installed adapter gets no "left out" notice (it is reported as the
//! JavaScript extension it is). Without such an extension the gateway stays (the control case).

use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::lane::Lane;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Session {
    runtime: Runtime,
    lane: Lane,
    log: PathBuf,
    notices: Arc<Mutex<Vec<(String, String)>>>,
    tokio: tokio::runtime::Runtime,
}

/// A session whose `mcp.json` has one eager server (it would connect as the session starts) and an installed
/// pi-mcp-adapter; with `custom`, an extension process registers the function `mcp`.
fn session(root: &Path, custom: bool) -> Session {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let log = root.join("alpha.log");
    let servers = json!({"alpha": {
        "command": python(),
        "args": [repo().join("crates/mcp/fixtures/fake_gateway_server.py"), "alpha", log],
        "lifecycle": "eager",
    }});
    std::fs::write(agent.join("mcp.json"), json!({ "mcpServers": servers }).to_string()).unwrap();
    let adapter = agent.join("extensions/pi-mcp-adapter");
    std::fs::create_dir_all(&adapter).unwrap();
    std::fs::write(adapter.join("index.ts"), "export default () => {};\n").unwrap();
    if custom {
        let dir = agent.join("extensions/my-mcp");
        std::fs::create_dir_all(&dir).unwrap();
        let capabilities =
            json!({"functions": [{"name": "mcp", "label": "My MCP", "description": "My own MCP gateway"}]});
        let manifest = json!({
            "name": "my-mcp",
            "command": python(),
            "args": ["-I", repo().join("crates/extensions/tests/probe.py")],
            "env": {"PROBE_CAPABILITIES": capabilities.to_string()},
            "startupTimeoutMs": 120_000,
            "eventTimeoutMs": 120_000,
        });
        std::fs::write(dir.join("extension.json"), manifest.to_string()).unwrap();
    }
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("ULTRON_LOKI".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&repo().join("packages/coding-agent/docs")),
    };
    let notices: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let sink = notices.clone();
    let services = Services {
        mcp_notices: Some(Arc::new(move |message: &str, level: &str| {
            sink.lock().unwrap().push((message.into(), level.into()));
        })),
        ..Services::default()
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), services).unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    let lane = Lane::open(&runtime, &model, None, "mcp-replaced").unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    Session {
        runtime,
        lane,
        log,
        notices,
        tokio,
    }
}

impl Session {
    fn rlm(&self, code: &str) -> String {
        let signal = ai::http::AbortSignal::default();
        let update = |_| {};
        let result = self
            .tokio
            .block_on(agent::AgentTool::execute(
                &*self.lane.tool,
                "call-1",
                json!({ "code": code }),
                &signal,
                &update,
            ))
            .unwrap();
        match &result.content[0] {
            ai::UserContent::Text(text) => text.text.clone(),
            other => panic!("not text: {other:?}"),
        }
    }

    fn mcp_commands(&self) -> Vec<Value> {
        self.lane
            .stack
            .commands()
            .into_iter()
            .filter(|c| c["name"] == "mcp")
            .collect()
    }
}

const NOTICE: &str = "pi-mcp-adapter is installed but no longer needed: Ultron connects MCP servers itself and reads the same mcp.json. The adapter was not loaded; remove it with `ultron remove npm:pi-mcp-adapter`.";

#[test]
fn session_core_an_extensions_mcp_tool_replaces_the_builtin_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), true);
    // `tools.mcp` is the extension's (its label), and calls reach its process (the probe knows no `mcp` body).
    assert_eq!(
        session.rlm("print([(t['name'], t['label']) for t in await tools.list()])"),
        "[('mcp', 'My MCP')]"
    );
    let called = session.rlm("try:\n    await tools.call('mcp', {})\nexcept Exception as e:\n    print(e)");
    assert!(called.contains("no function"), "{called}");
    // The gateway's servers never start (the control case waits for the eager one), its guide section and `/mcp`
    // are gone.
    assert!(!session.log.exists(), "the eager server was started");
    let status = session.lane.stack.mcp().command("").0;
    assert!(!status.contains("alpha"), "{status}");
    let system = session.runtime.system.current();
    assert!(!system.contains("MCP servers (alpha)"), "{system}");
    assert!(session.mcp_commands().is_empty(), "{:?}", session.mcp_commands());
    // The adapter is not left out with a notice: it is reported as a JavaScript extension.
    assert!(session.notices.lock().unwrap().iter().all(|(m, _)| m != NOTICE));
    assert!(
        session.runtime.diagnostics.iter().any(|d| d.contains("pi-mcp-adapter")),
        "{:?}",
        session.runtime.diagnostics
    );
    session.lane.shutdown();
}

#[test]
fn session_core_without_an_mcp_extension_the_builtin_gateway_stays() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), false);
    assert_eq!(
        session.rlm("print([(t['name'], t['label']) for t in await tools.list()])"),
        "[('mcp', 'MCP')]"
    );
    assert!(session.runtime.system.current().contains("MCP servers (alpha)"));
    assert_eq!(session.mcp_commands().len(), 1);
    assert!(session.lane.stack.mcp().command("").0.contains("alpha"));
    // The eager server starts with the session (a hang guard, never a pass condition).
    let start = std::time::Instant::now();
    while !session.log.exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(120),
            "the eager server never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        *session.notices.lock().unwrap(),
        [(NOTICE.to_string(), "info".to_string())]
    );
    assert!(
        session
            .runtime
            .diagnostics
            .iter()
            .all(|d| !d.contains("pi-mcp-adapter")),
        "{:?}",
        session.runtime.diagnostics
    );
    session.lane.shutdown();
}
