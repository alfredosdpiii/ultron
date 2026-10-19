//! Replays crates/mcp/fixtures/gateway.json, recorded from the TypeScript `mcp` gateway (extensions/mcp/index.ts) by
//! generate-gateway.mjs: the same mcp.json files, fake servers and steps run through the Rust gateway, and every
//! result (content and details), notice, server log and lifecycle trace must match.

use mcp::CancelToken;
use mcp::config::ServerEntry;
use mcp::connection::{ConnectionEnv, SecretSink};
use mcp::gateway::{Gateway, GatewayOptions, server_names_of};
use mcp::log::ServerLog;
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Hang guard for every wait on a condition.
const GUARD: Duration = Duration::from_secs(120);

fn fixture() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gateway.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    })
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn server_script() -> String {
    fixtures_dir().join("fake_gateway_server.py").display().to_string()
}

/// Waits for `ready` (a condition on files or state), failing after the hang guard.
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct HttpServer(Child, String);

impl HttpServer {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(python())
            .arg(fixtures_dir().join("fake_http_server.py"))
            .env("MCP_FAKE_LOG", dir.join("http.log"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        HttpServer(child, line.trim().to_string())
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The TypeScript generator's placeholders, filled.
fn fill(config: &Value, port: &str, log_alpha: &Path) -> Value {
    let text = config
        .to_string()
        .replace("<PY>", &python())
        .replace("<SERVER>", &server_script())
        .replace("<LOG_ALPHA>", &log_alpha.display().to_string())
        .replace("<PORT>", port);
    serde_json::from_str(&text).unwrap()
}

/// Paths of saved binary resources (`<dir>/ultron-mcp-<16 hex><ext>`) as `<SAVED><ext>`.
fn replace_saved(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("ultron-mcp-") {
        let hex: String = rest[at + 11..].chars().take(16).collect();
        if hex.len() == 16 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let before = &rest[..at];
            let start = before
                .rfind(|c: char| c == '"' || c.is_whitespace())
                .map_or(0, |i| i + 1);
            if before[start..].starts_with('/') {
                out.push_str(&before[..start]);
                out.push_str("<SAVED>");
                rest = &rest[at + 27..];
                continue;
            }
        }
        out.push_str(&rest[..at + 11]);
        rest = &rest[at + 11..];
    }
    out.push_str(rest);
    out
}

struct Scenario {
    base: PathBuf,
    agent: PathBuf,
    cwd: PathBuf,
    port: String,
}

impl Scenario {
    fn normalize(&self, value: &Value) -> Value {
        let text = value
            .to_string()
            .replace(&server_script(), "<SERVER>")
            .replace(&python(), "<PY>")
            .replace(&self.cwd.display().to_string(), "<CWD>")
            .replace(&self.agent.display().to_string(), "<AGENT>")
            .replace(&self.base.display().to_string(), "<BASE>")
            .replace(&format!("127.0.0.1:{}", self.port), "127.0.0.1:<PORT>");
        serde_json::from_str(&replace_saved(&text)).unwrap()
    }
}

/// `<TIME>` for the ISO timestamp that starts each log line.
fn mask_times(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let stamp = line.get(..24).unwrap_or_default();
            if stamp.len() == 24 && stamp.as_bytes()[10] == b'T' && stamp.ends_with('Z') {
                format!("<TIME>{}", &line[24..])
            } else {
                line.to_string()
            }
        })
        .collect()
}

fn read_lines(path: &Path) -> Vec<String> {
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    lines.sort();
    lines
}

fn run(recorded: &Value) {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let agent = base.join("agent");
    let cwd = base.join("project");
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    let name = recorded["name"].as_str().unwrap();
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let log_alpha = dir.join("alpha.log");
    let http = HttpServer::start(&dir);
    let scenario = Scenario {
        base: base.clone(),
        agent: agent.clone(),
        cwd: cwd.clone(),
        port: http.1.clone(),
    };
    let config = if name == "empty" {
        json!({})
    } else {
        fixture()["config"].clone()
    };
    if let Some(global) = config.get("global") {
        std::fs::write(agent.join("mcp.json"), fill(global, &http.1, &log_alpha).to_string()).unwrap();
    }
    if let Some(project) = config.get("project") {
        std::fs::write(cwd.join(".pi/mcp.json"), fill(project, &http.1, &log_alpha).to_string()).unwrap();
    }
    let secrets: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink_secrets = secrets.clone();
    let sink: SecretSink = Arc::new(move |values: &[String]| {
        let mut known = sink_secrets.lock().unwrap();
        for value in values.iter().map(|v| v.trim()) {
            if value.chars().count() >= 8 && !known.iter().any(|k| k == value) {
                known.push(value.to_string());
            }
        }
    });
    let env_overlay = fixture()["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: agent.clone(),
        project_trusted: recorded["trusted"] == true,
        servers: None,
        env: ConnectionEnv {
            cwd: cwd.clone(),
            home: base.clone(),
            env_overlay,
            client_version: mcp::gateway::client_version(),
            log: Some(Arc::new(ServerLog::new(agent.join("mcp.log")))),
            secrets: Some(sink),
            ..ConnectionEnv::default()
        },
        open_url: None,
    });
    let execute = |params: &Value| -> Value {
        match gateway.execute(params.as_object().unwrap(), None) {
            Ok(result) => json!({ "result": result }),
            Err(error) => json!({ "threw": error }),
        }
    };
    for (index, expected) in recorded["events"].as_array().unwrap().iter().enumerate() {
        let mut got: Map<String, Value> = Map::new();
        for key in ["op", "params", "server", "tool", "text"] {
            if let Some(value) = expected.get(key)
                && !(key == "tool" && expected["op"] == "tool")
            {
                got.insert(key.into(), value.clone());
            }
        }
        match expected["op"].as_str().unwrap() {
            "tool" => {
                let tool = gateway.tool().map_or(Value::Null, |t| {
                    json!({ "name": t.name, "label": t.label, "description": t.description, "parameters": t.parameters })
                });
                got.insert("tool".into(), tool);
            }
            "call" => {
                got.extend(execute(&expected["params"]).as_object().unwrap().clone());
            }
            "start" => {
                let notices: Vec<Value> = gateway
                    .start(None)
                    .into_iter()
                    .map(|(message, level)| json!({ "message": message, "level": level }))
                    .collect();
                got.insert("notices".into(), json!(notices));
            }
            "waitConnected" => {
                gateway.settle();
                let status = gateway.status();
                let server = status["details"]["servers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|s| s["name"] == expected["server"])
                    .unwrap()["status"]
                    .clone();
                got.insert("status".into(), server);
            }
            "waitTool" => {
                let mut found = false;
                wait_until("the changed tool list", || {
                    let listed = execute(&json!({ "server": expected["server"] }));
                    found = listed["result"]["details"]["tools"]
                        .as_array()
                        .is_some_and(|tools| tools.contains(&expected["tool"]));
                    found
                });
                got.insert("found".into(), json!(found));
            }
            "log" => {
                let text = std::fs::read_to_string(agent.join("mcp.log")).unwrap_or_default();
                got.insert("text".into(), json!(mask_times(&text)));
            }
            "mask" => {
                let mut text = expected["text"].as_str().unwrap().to_string();
                for secret in secrets.lock().unwrap().iter() {
                    text = text.replace(secret.as_str(), "[REDACTED:known_secret]");
                }
                got.insert("masked".into(), json!(text));
            }
            "shutdown" => gateway.close(),
            "serverLog" => {
                got.insert("lines".into(), json!(read_lines(&log_alpha)));
            }
            "cancel" => {
                let calls = || {
                    std::fs::read_to_string(&log_alpha)
                        .unwrap_or_default()
                        .matches("tools/call hang")
                        .count()
                };
                let before = calls();
                let cancel = CancelToken::new();
                let params = expected["params"].as_object().unwrap().clone();
                let outcome = std::thread::scope(|scope| {
                    let running = scope.spawn(|| gateway.execute(&params, Some(&cancel)));
                    wait_until("the server to receive the call", || calls() > before);
                    cancel.cancel(None);
                    running.join().unwrap()
                });
                match outcome {
                    Ok(result) => got.insert("result".into(), result),
                    Err(error) => got.insert("threw".into(), json!(error)),
                };
            }
            other => panic!("unknown op {other}"),
        }
        let got = scenario.normalize(&Value::Object(got));
        assert_eq!(&got, expected, "{name} event {index}");
    }
    gateway.close();
}

fn scenario(name: &str) -> &'static Value {
    fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
}

#[test]
fn mcp_gateway_matches_typescript_over_stdio_and_http() {
    run(scenario("main"));
}

#[test]
fn mcp_gateway_reads_the_project_config_only_when_trusted() {
    run(scenario("untrusted"));
}

#[test]
fn mcp_gateway_is_not_registered_without_servers() {
    run(scenario("empty"));
}

#[test]
fn mcp_gateway_description_names_the_enabled_servers_for_the_runtime_guide() {
    let tool = &scenario("main")["events"][0]["tool"];
    let names = server_names_of(tool["description"].as_str().unwrap());
    assert_eq!(names, ["alpha", "beta-srv", "plain", "dies", "missing", "web"]);
}

#[test]
fn mcp_gateway_given_servers_skip_the_config_files() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("mcp.json"),
        json!({ "mcpServers": { "fromfile": { "command": "x" } } }).to_string(),
    )
    .unwrap();
    let entry = ServerEntry {
        name: "given".into(),
        config: json!({ "command": "x" }).as_object().unwrap().clone(),
        source: "test".into(),
        scope: mcp::config::Scope::Global,
    };
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: tmp.path().to_path_buf(),
        servers: Some(vec![entry]),
        ..GatewayOptions::default()
    });
    // The gateway tool's description names the enabled servers (what the runtime guide shows).
    let tool = gateway.tool().expect("a gateway with a server registers its tool");
    assert_eq!(server_names_of(&tool.description), ["given"]);
}
