//! The MCP gateway in a session (session-worker.ts with extensions/mcp built in): servers of the profile's
//! `mcp.json` are reached from a cell through the `mcp` namespace of the real runtime.py, as `tools.call("mcp", ...)`
//! host requests served by the session's `tools.*` module; the model's tool list stays exactly [rlm]; values a
//! server's env resolves to are masked in the cell's output; the servers close with the session. The expected values
//! are the TypeScript gateway's, recorded in crates/mcp/fixtures/gateway.json (generate-gateway.mjs) against the same
//! fake server.

use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::lane::Lane;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: &str = "fixture-not-a-secret-0001";

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mcp/fixtures/fake_gateway_server.py")
}

/// Waits for `ready`, failing after a 120 s hang guard.
fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Session {
    runtime: Runtime,
    lane: Lane,
    log: PathBuf,
    tokio: tokio::runtime::Runtime,
}

fn session(root: &Path, servers: Option<Value>) -> Session {
    session_with(root, servers, Services::default())
}

fn session_with(root: &Path, servers: Option<Value>, services: Services) -> Session {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let log = root.join("alpha.log");
    let servers = servers.unwrap_or_else(|| {
        json!({
            "alpha": {
                "command": python(),
                "args": [server_script(), "alpha", log],
                "env": { "GW_TOKEN": SECRET },
                "toolExposure": { "hidden_tool": "hidden" },
                "timeout": 3600,
            },
        })
    });
    std::fs::write(agent.join("mcp.json"), json!({ "mcpServers": servers }).to_string()).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), services).unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    let lane = Lane::open(&runtime, &model, None, "mcp-gateway").unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    Session {
        runtime,
        lane,
        log,
        tokio,
    }
}

impl Session {
    /// One `rlm` call as the model makes it: the cell's output text, masked and bounded by the tool.
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

    fn served(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

#[test]
fn session_core_mcp_servers_are_kernel_apis_and_the_model_keeps_one_tool() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), None);
    let tools: Vec<String> = session.lane.tools().iter().map(|t| t.tool().name.clone()).collect();
    assert_eq!(tools, ["rlm"]);
    // Lazy: the server starts on first use.
    assert_eq!(
        session.rlm("print([(s['name'], s['status']) for s in await mcp.servers()])"),
        "[('alpha', 'not connected')]"
    );
    assert!(session.served().is_empty());
    assert_eq!(
        session.rlm("r = await mcp.call('alpha_echo', text='hi')\nprint(r, r.ok, r.details)"),
        "hi True {'mode': 'call', 'server': 'alpha', 'tool': 'echo'}"
    );
    assert_eq!(
        session.rlm("print((await mcp.alpha.add(a=2, b=3)).json())"),
        "{'sum': 5}"
    );
    assert_eq!(
        session.rlm("print(len(await mcp.tools('alpha')), 'alpha_hidden_tool' in await mcp.tools('alpha'))"),
        "16 False"
    );
    assert_eq!(
        session.rlm("try:\n    await mcp.call('nope')\nexcept McpError as e:\n    print(e.status, e)"),
        "tool_not_found mcp nope: Tool \"nope\" not found. List tools with await mcp.tools(\"server\") or await mcp.search(\"words\")."
    );
    assert_eq!(
        session.rlm("print((await mcp.describe('alpha_echo'))['parameters'])"),
        "{'type': 'object', 'properties': {'text': {'type': 'string'}}, 'required': ['text']}"
    );
    // The gateway is an extension tool in the REPL, labelled by its mode in the call records.
    assert_eq!(
        session.rlm("print([(t['name'], t['label']) for t in await tools.list()])"),
        "[('mcp', 'MCP')]"
    );
    let calls = session.lane.stack.tool_calls().list();
    assert!(calls.iter().any(|c| c["label"] == "mcp alpha_echo"), "{calls:?}");
    // One connection serves every cell.
    assert_eq!(session.served().iter().filter(|l| *l == "initialize").count(), 1);
    // The server's env value is a known secret: masked in what the model sees.
    assert_eq!(
        session.rlm("print(await mcp.call('alpha_env'))"),
        "token=[REDACTED:known_secret]"
    );
    let (masked, count) = session.runtime.config.masker.mask_counted(&format!("x {SECRET}"), None);
    assert_eq!((masked.as_str(), count), ("x [REDACTED:known_secret]", 1));
    // The servers close with the session.
    session.lane.shutdown();
    wait_until("the server saw its stdin close", || {
        session.served().contains(&"eof".to_string())
    });
}

#[test]
fn session_core_mcp_servers_are_listed_in_the_runtime_guide() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), None);
    let description = mcp::gateway::gateway_description(&["alpha".into()]);
    let section = resources::runtime_guide::extension_tools_prompt(
        &[resources::runtime_guide::ExtensionToolSummary {
            name: "mcp".into(),
            description,
        }],
        &["alpha".into()],
        &[],
        resources::runtime_guide::EventDelivery::Wake,
    )
    .unwrap();
    assert!(session.runtime.system.contains(&section), "{}", session.runtime.system);
    assert!(
        session
            .runtime
            .system
            .contains("MCP servers (alpha) are reached through the `mcp` namespace")
    );
    session.lane.shutdown();
}

#[test]
fn session_core_without_mcp_servers_there_is_no_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), Some(json!({})));
    assert!(!session.runtime.system.contains("## Extension tools"));
    assert_eq!(
        session.rlm("try:\n    await mcp.servers()\nexcept Exception as e:\n    print(e)"),
        "Extension tools are not available in this session"
    );
    session.lane.shutdown();
}

#[test]
fn session_core_mcp_configuration_errors_reach_the_clients_ui_when_the_session_starts() {
    let dir = tempfile::tempdir().unwrap();
    let notices: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let sink = notices.clone();
    let gateways: Arc<std::sync::Mutex<usize>> = Arc::default();
    let seen = gateways.clone();
    let services = Services {
        mcp_notices: Some(Arc::new(move |message: &str, level: &str| {
            sink.lock().unwrap().push((message.into(), level.into()));
        })),
        on_mcp: Some(Arc::new(move |_| *seen.lock().unwrap() += 1)),
        ..Services::default()
    };
    let session = session_with(dir.path(), Some(json!({ "bad name!": { "command": "x" } })), services);
    let agent = dir.path().join("home/.ultron/agent/mcp.json");
    assert_eq!(
        *notices.lock().unwrap(),
        [(
            format!(
                "MCP configuration:\n{}: invalid server name \"bad name!\" (use letters, digits, \"_\" and \"-\")",
                agent.display()
            ),
            "warning".to_string()
        )]
    );
    assert_eq!(*gateways.lock().unwrap(), 1);
    session.lane.shutdown();
}

#[test]
fn session_core_cancelling_a_detached_mcp_call_cancels_the_request_on_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), None);
    let started = session.rlm("c = await mcp.call('alpha_hang', yield_after=0)\nprint(c.status, c.label, c.id)");
    let id = started
        .strip_prefix("running mcp alpha_hang ")
        .expect(&started)
        .to_string();
    wait_until("the server received the call", || {
        session.served().contains(&"tools/call hang".to_string())
    });
    // `call.cancel()` from another cell, as `tools.cancel` does; the server's 3600 s timeout cannot end it.
    let calls = session.lane.stack.tool_calls().clone();
    std::thread::spawn(move || calls.cancel(&id, "cancelled"));
    wait_until("the server saw the cancellation", || {
        session.served().iter().any(|l| l.starts_with("cancelled "))
    });
    wait_until("the call ended", || {
        session
            .lane
            .stack
            .tool_calls()
            .list()
            .iter()
            .any(|c| c["status"] == "cancelled")
    });
    assert_eq!(session.rlm("print((await c.refresh()).status)"), "cancelled");
    // The connection survives a cancelled request.
    assert_eq!(
        session.rlm("print(await mcp.call('alpha_echo', text='after'))"),
        "after"
    );
    session.lane.shutdown();
}

#[test]
fn session_core_tool_arguments_are_validated_and_coerced_as_typescript_prepares_them() {
    let dir = tempfile::tempdir().unwrap();
    let session = session(dir.path(), None);
    // `limit` arrives as a string and is coerced to a number (prepareToolArguments).
    assert_eq!(
        session.rlm("print(await tools.call('mcp', {'search': 'echo', 'limit': '1'}))"),
        "Found 1 tools\nalpha_echo: Echo the text back"
    );
    assert_eq!(
        session.rlm("try:\n    await tools.call('mcp', {'tool': ['x']})\nexcept ToolError as e:\n    print(e)"),
        "mcp status failed: Validation failed for tool \"mcp\":\n  - tool: must be string\n\nReceived arguments:\n{\n  \"tool\": [\n    \"x\"\n  ]\n}"
    );
    // An `args` string that is not JSON: V8's JSON.parse message, as the TypeScript gateway reports it.
    assert_eq!(
        session
            .rlm("try:\n    await mcp.call('alpha_echo', 'x')\nexcept Exception as e:\n    print(type(e).__name__, e)"),
        "JSONDecodeError Expecting value: line 1 column 1 (char 0)"
    );
    assert_eq!(
        session.rlm("try:\n    await tools.call('mcp', {'tool': 'alpha_echo', 'args': 'x'})\nexcept McpError as e:\n    print(e)"),
        "mcp alpha_echo: Error: Unexpected token 'x', \"x\" is not valid JSON"
    );
    session.lane.shutdown();
}
