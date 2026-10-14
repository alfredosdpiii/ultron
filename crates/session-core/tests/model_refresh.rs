//! Replays `crates/ai/fixtures/model-refresh.json` (recorded from the TypeScript `ModelRuntime` by
//! `crates/ai/fixtures/generate-model-refresh.mjs`): stored catalogs restored on start (pi.dev overlays newer than the
//! bundled data, Radius catalogs, a legacy Radius credential's `gatewayConfig` imported), and `ultron update
//! --models` refreshing every provider with a credential against loopback stand-ins for pi.dev and a Radius gateway
//! that answer as the recording's did. Private temp profiles and an injected environment.

use serde_json::{Map, Value, json};
use session_core::{RunEnv, lane, model_refresh};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../ai/fixtures/model-refresh.json")).unwrap()
}

type Requests = Arc<Mutex<Vec<Value>>>;

/// One parsed request: method, path, lowercased headers, body.
fn read_request(stream: &TcpStream) -> (String, String, HashMap<String, String>, String) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_string());
        }
    }
    let length = headers.get("content-length").map_or(0, |v| v.parse().unwrap());
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (method, path, headers, String::from_utf8(body).unwrap())
}

fn respond(mut stream: TcpStream, status: u16, headers: &[(&str, String)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let _ = write!(stream, "{head}\r\n{body}");
}

/// A loopback server answering with `answer` and recording each request as the generator does.
fn serve(
    name: &'static str,
    requests: Requests,
    answer: impl Fn(&str, &str, &HashMap<String, String>, u16) -> (u16, Vec<(&'static str, String)>, String)
    + Send
    + 'static,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (method, path, headers, body) = read_request(&stream);
            let form: Option<Map<String, Value>> = (!body.is_empty()).then(|| {
                body.split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect()
            });
            let agent = headers.get("user-agent").map(|ua| {
                if ua.starts_with("pi/") {
                    "pi/<version>".to_string()
                } else {
                    ua.clone()
                }
            });
            requests.lock().unwrap().push(json!({
                "server": name, "method": method, "path": path,
                "accept": headers.get("accept"), "authorization": headers.get("authorization"),
                "ifNoneMatch": headers.get("if-none-match"), "userAgent": agent, "body": form,
            }));
            let (status, extra, text) = answer(&method, &path, &headers, port);
            respond(stream, status, &extra, &text);
        }
    });
    port
}

fn remote(provider: &str, id: &str) -> Value {
    json!({"id": id, "name": format!("Remote {id}"), "api": "openai-completions", "provider": provider,
        "baseUrl": format!("https://{provider}.invalid/v1"), "reasoning": false, "input": ["text"],
        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 64000, "maxTokens": 4000})
}

/// The recording's loopback servers: pi.dev's catalog and a Radius gateway.
fn servers(generated_at: i64, requests: Requests) -> (u16, u16) {
    let date = |ms: i64| http_date(ms);
    let (newer, older) = (date(generated_at + 86_400_000), date(generated_at - 86_400_000));
    let json_type = || ("content-type", "application/json".to_string());
    let catalog = serve("catalog", requests.clone(), move |_, path, headers, _| {
        let provider = path.rsplit('/').next().unwrap_or("");
        match provider {
            "deepseek" => (
                200,
                vec![json_type(), ("last-modified", newer.clone()), ("etag", "\"d1\"".into())],
                json!({"models": [remote("ignored", "deepseek-remote"), {"name": "no id"}]}).to_string(),
            ),
            "xai" if headers.get("if-none-match").map(String::as_str) == Some("\"x1\"") => (304, vec![], String::new()),
            "xai" => (
                200,
                vec![json_type(), ("last-modified", newer.clone()), ("etag", "\"x2\"".into())],
                json!([remote("xai", "grok-fresh")]).to_string(),
            ),
            "cerebras" => (404, vec![json_type()], json!({"error": "not found"}).to_string()),
            "mistral" => (
                200,
                vec![json_type(), ("last-modified", older.clone())],
                json!({"mistral-remote": remote("mistral", "mistral-remote")}).to_string(),
            ),
            _ => (500, vec![json_type()], json!({"error": "boom"}).to_string()),
        }
    });
    let gateway = serve("gateway", requests, move |_, path, headers, port| {
        if path == "/v1/oauth/token" {
            return (
                200,
                vec![json_type()],
                json!({"access_token": "gw2-new", "refresh_token": "gw2-refresh-new", "expires_in": 3600}).to_string(),
            );
        }
        let token = headers
            .get("authorization")
            .map(|a| a.trim_start_matches("Bearer ").to_string())
            .unwrap_or_default();
        if token == "fail-key" {
            return (
                503,
                vec![("content-type", "text/plain".into())],
                "  gateway down  ".into(),
            );
        }
        let model = json!({"id": format!("m-{token}"), "name": format!("Gateway m-{token}"), "reasoning": true,
            "input": ["text"], "cost": {"input": 3, "output": 4, "cacheRead": 0, "cacheWrite": 0},
            "contextWindow": 200000, "maxTokens": 9000});
        (
            200,
            vec![json_type()],
            json!({"baseUrl": format!("http://127.0.0.1:{port}/served/{token}"), "models": [model, {"id": "broken"}]})
                .to_string(),
        )
    });
    (catalog, gateway)
}

/// `Date#toUTCString`.
fn http_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][month as usize - 1];
    format!(
        "{weekday}, {day:02} {name} {year} {:02}:{:02}:{:02} GMT",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

struct Profile {
    env: RunEnv,
    agent: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn profile(spec: &Value, ports: (u16, u16)) -> Profile {
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(tmp.path().join("cwd")).unwrap();
    let fill = |v: &Value| {
        v.to_string()
            .replace("<catalog-port>", &ports.0.to_string())
            .replace("<gateway-port>", &ports.1.to_string())
    };
    std::fs::write(agent.join("models.json"), fill(&spec["models"])).unwrap();
    std::fs::write(agent.join("auth.json"), fill(&spec["auth"])).unwrap();
    if !spec["store"].is_null() {
        std::fs::write(agent.join("models-store.json"), fill(&spec["store"])).unwrap();
    }
    let mut map: HashMap<String, String> = spec["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    Profile {
        env: RunEnv {
            cwd: tmp.path().join("cwd"),
            home: tmp.path().to_path_buf(),
            var: Arc::new(move |k| map.get(k).cloned()),
            docs: tmp.path().join("docs"),
        },
        agent,
        _tmp: tmp,
    }
}

/// Ports as placeholders, times of this run as `<now>`, numbers normalized.
fn normalize(value: &Value, ports: (u16, u16), started: i64) -> Value {
    let text = value
        .to_string()
        .replace(&ports.0.to_string(), "<catalog-port>")
        .replace(&ports.1.to_string(), "<gateway-port>");
    fn walk(value: Value, started: i64) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let recent = v
                            .as_f64()
                            .is_some_and(|n| n >= started as f64 && n < (started + 86_400_000) as f64);
                        if (k == "checkedAt" || k == "expires") && recent {
                            (k, json!("<now>"))
                        } else {
                            (k, walk(v, started))
                        }
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| walk(v, started)).collect()),
            other => other,
        }
    }
    ai::json::normalize_numbers(walk(serde_json::from_str(&text).unwrap(), started))
}

fn read(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// Requests compared as a multiset: providers refresh in parallel in TypeScript. The gateway's user agent is the
/// HTTP client's own (undici's `node`), not part of the protocol.
fn request_set(requests: &Value) -> Vec<String> {
    let mut out: Vec<String> = requests
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if r["server"] == "gateway" {
                r["userAgent"] = Value::Null;
            }
            r.to_string()
        })
        .collect();
    out.sort();
    out
}

#[test]
fn session_core_model_refresh_matches_typescript() {
    let fixture = fixture();
    let generated_at = ai::model_refresh::parse_iso_ms(fixture["generatedAt"].as_str().unwrap()).unwrap();
    assert_eq!(ai::model_refresh::builtin_generated_at(), Some(generated_at));
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for spec in cases {
        let name = spec["name"].as_str().unwrap();
        let requests: Requests = Arc::default();
        let ports = servers(generated_at, requests.clone());
        let profile = profile(spec, ports);
        let started = lane::now_ms();
        let errors = if spec["network"] == true {
            let errors = model_refresh::refresh_catalogs(
                &profile.env,
                &format!("http://127.0.0.1:{}", ports.0),
                &ai::model_refresh::management_user_agent("0.0.0"),
                true,
            )
            .unwrap();
            let map: Map<String, Value> = errors.into_iter().map(|(p, e)| (p, json!(e))).collect();
            Value::Object(map)
        } else {
            Value::Null
        };
        assert_eq!(normalize(&errors, ports, started), spec["errors"], "{name}: errors");
        let registry = lane::registry(&profile.env);
        for (provider, expected) in spec["catalogs"].as_object().unwrap() {
            let actual = serde_json::to_value(registry.models(provider)).unwrap();
            assert_eq!(
                normalize(&actual, ports, started),
                ai::json::normalize_numbers(expected.clone()),
                "{name}: {provider}"
            );
        }
        let recorded = Value::Array(requests.lock().unwrap().clone());
        assert_eq!(
            request_set(&normalize(&recorded, ports, started)),
            request_set(&spec["requests"]),
            "{name}: requests"
        );
        assert_eq!(
            normalize(&read(&profile.agent.join("models-store.json")), ports, started),
            ai::json::normalize_numbers(spec["storeAfter"].clone()),
            "{name}: models-store.json"
        );
        assert_eq!(
            normalize(&read(&profile.agent.join("auth.json")), ports, started),
            ai::json::normalize_numbers(spec["authAfter"].clone()),
            "{name}: auth.json"
        );
    }
}

/// A stored catalog newer than the bundled data restores its models on every start without the network; an older
/// one is ignored (the bundled data already supersedes it).
#[test]
fn session_core_stored_catalog_overlay_needs_a_newer_stamp() {
    let generated = ai::model_refresh::builtin_generated_at().unwrap();
    for (stamp, listed) in [(generated + 1, true), (generated, false)] {
        let spec = json!({"models": {"providers": {}}, "auth": {}, "env": {},
            "store": {"deepseek": {"models": [remote("deepseek", "x-new")], "lastModified": stamp, "checkedAt": 1}}});
        let profile = profile(&spec, (1, 2));
        let ids: Vec<String> = lane::registry(&profile.env)
            .models("deepseek")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids.contains(&"x-new".to_string()), listed, "{stamp}");
    }
}
