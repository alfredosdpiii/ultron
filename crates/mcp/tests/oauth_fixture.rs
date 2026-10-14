//! Replays crates/mcp/fixtures/oauth.json, recorded from the TypeScript MCP extension by generate-oauth.mjs against
//! fixtures/fake_oauth_server.py: a 401 needs sign-in; `/mcp login` runs discovery, dynamic client registration and the
//! PKCE flow against a loopback callback on a free port (the "browser" follows the authorization server's redirect);
//! the credentials land in mcp-auth.json; an expired token is refreshed once after a 401 with a rotating refresh token;
//! `/mcp logout` removes them; an eager server that needs sign-in says so at session start; a server with
//! `auth.provider` sends the provider's current key on every request; a rejected Authorization header of the config
//! fails the connect. Every notice, prompt, opened URL, result, the credential file and the server's log must match.

use mcp::CancelToken;
use mcp::connection::ConnectionEnv;
use mcp::gateway::{CommandUi, Gateway, GatewayOptions};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Server(Child, String);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server() -> Server {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_oauth_server.py");
    let mut child = Command::new(python())
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    Server(child, line.trim().to_string())
}

/// The client of the test: no dialogs; a redirect-URL prompt stays open until the browser's callback ends it.
#[derive(Default)]
struct Ui {
    notices: Mutex<Vec<Value>>,
    prompts: Mutex<Vec<Value>>,
}

impl CommandUi for Ui {
    fn has_ui(&self) -> bool {
        false
    }

    fn notify(&self, message: &str, level: &str) {
        self.notices
            .lock()
            .unwrap()
            .push(json!({ "message": message, "level": level }));
    }

    fn select(&self, _: &str, _: &[String]) -> Option<String> {
        None
    }

    fn input(&self, title: &str, placeholder: &str, cancel: &CancelToken) -> Option<String> {
        self.prompts
            .lock()
            .unwrap()
            .push(json!({ "title": title, "placeholder": placeholder }));
        wait_until("the sign-in prompt to close", || cancel.is_cancelled());
        None
    }
}

fn take(list: &Mutex<Vec<Value>>) -> Value {
    Value::Array(std::mem::take(&mut *list.lock().unwrap()))
}

/// Replaces `<prefix><digits>` with `<prefix><PORT>` (the server's) or `<prefix><CB>` (a callback's).
fn ports(text: &str, prefix: &str, port: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at + prefix.len()]);
        rest = &rest[at + prefix.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue;
        }
        out.push_str(if digits == port { "<PORT>" } else { "<CB>" });
        rest = &rest[digits.len()..];
    }
    out.push_str(rest);
    out
}

/// Replaces the value after `prefix` (characters of `class`) with `placeholder`.
fn value_after(text: &str, prefix: &str, class: fn(char) -> bool, placeholder: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at + prefix.len()]);
        rest = &rest[at + prefix.len()..];
        let len: usize = rest.chars().take_while(|c| class(*c)).map(char::len_utf8).sum();
        if len > 0 {
            out.push_str(placeholder);
            rest = &rest[len..];
        }
    }
    out.push_str(rest);
    out
}

fn normalize(value: &Value, agent: &Path, port: &str) -> Value {
    let text = value.to_string().replace(&agent.display().to_string(), "<AGENT>");
    let text = ports(&ports(&text, "127.0.0.1:", port), "127.0.0.1%3A", port);
    let b64 = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let text = value_after(&text, "code_challenge=", b64, "<CHALLENGE>");
    let text = value_after(&text, "state=", |c| c.is_ascii_hexdigit(), "<STATE>");
    let text = value_after(&text, "\"codeVerifier\":\"", b64, "<VERIFIER>");
    let text = value_after(&text, "\"oauthState\":\"", |c| c.is_ascii_hexdigit(), "<STATE>");
    let text = value_after(&text, "\"tokensExpireAt\":", |c| c.is_ascii_digit(), "\"<EXPIRES>\"");
    serde_json::from_str(&text).unwrap()
}

#[test]
fn mcp_oauth_sign_in_refresh_and_sign_out_match_typescript() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/oauth.json")).unwrap(),
    )
    .unwrap();
    let server = start_server();
    let origin = format!("http://127.0.0.1:{}", server.1);
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (agent, cwd): (PathBuf, PathBuf) = (base.join("agent"), base.join("project"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let config = fixture["config"].to_string().replace("<ORIGIN>", &origin);
    std::fs::write(agent.join("mcp.json"), config).unwrap();
    let provider_token = Arc::new(Mutex::new("prov-1".to_string()));
    let token = provider_token.clone();
    let opened: Arc<Mutex<Vec<Value>>> = Arc::default();
    let opened_by = opened.clone();
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: agent.clone(),
        project_trusted: false,
        servers: None,
        env: ConnectionEnv {
            cwd: cwd.clone(),
            home: base.clone(),
            client_version: mcp::gateway::client_version(),
            provider_token: Some(Arc::new(move |provider: &str| {
                (provider == "fakeprov").then(|| token.lock().unwrap().clone())
            })),
            ..ConnectionEnv::default()
        },
        open_url: Some(Arc::new(move |url: &str| {
            opened_by.lock().unwrap().push(json!(url));
            // The browser: follows the authorization server's redirect to the loopback callback.
            let url = url.to_string();
            std::thread::spawn(move || {
                let _ = reqwest::blocking::get(url);
            });
        })),
    });
    let ui = Ui::default();
    let async_notices: Arc<Mutex<Vec<Value>>> = Arc::default();
    let http = reqwest::blocking::Client::new();
    for (index, expected) in fixture["events"].as_array().unwrap().iter().enumerate() {
        let mut got = json!({ "op": expected["op"] });
        for key in ["params", "args", "token"] {
            if let Some(value) = expected.get(key) {
                got[key] = value.clone();
            }
        }
        match expected["op"].as_str().unwrap() {
            "start" => {
                let sink = async_notices.clone();
                let notices: Vec<Value> = gateway
                    .start(Some(Arc::new(move |message: &str, level: &str| {
                        sink.lock().unwrap().push(json!({ "message": message, "level": level }));
                    })))
                    .into_iter()
                    .map(|(message, level)| json!({ "message": message, "level": level }))
                    .collect();
                wait_until("the eager server's sign-in notice", || {
                    async_notices
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|n| n["message"].as_str().is_some_and(|m| m.contains("eager-issues")))
                });
                let mut all = notices;
                all.extend(take(&async_notices).as_array().unwrap().clone());
                got["notices"] = json!(all);
            }
            "call" => {
                let result = gateway.execute(expected["params"].as_object().unwrap(), None).unwrap();
                got["text"] = json!(Gateway::text_of(&result));
                got["details"] = result["details"].clone();
            }
            "command" => {
                gateway.run_command(expected["args"].as_str().unwrap(), &ui);
                got["notices"] = take(&ui.notices);
                got["prompts"] = take(&ui.prompts);
                got["opened"] = take(&opened);
            }
            "authFile" => {
                let path = agent.join("mcp-auth.json");
                got["content"] =
                    std::fs::read_to_string(&path).map_or(Value::Null, |text| serde_json::from_str(&text).unwrap());
            }
            "expire" => {
                http.post(format!("{origin}/__expire")).body("{}").send().unwrap();
            }
            "providerToken" => *provider_token.lock().unwrap() = expected["token"].as_str().unwrap().into(),
            "serverLog" => {
                got["log"] =
                    serde_json::from_str(&http.get(format!("{origin}/__log")).send().unwrap().text().unwrap()).unwrap()
            }
            "shutdown" => gateway.close(),
            other => panic!("unknown op {other}"),
        }
        let got = normalize(&got, &agent, &server.1);
        assert_eq!(&got, expected, "event {index}");
        // The credential file's key order is TypeScript's too (each field where its update put it).
        assert_eq!(got.to_string(), expected.to_string(), "event {index}");
    }
    // The credential file is the user's alone.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(agent.join("mcp-auth.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
