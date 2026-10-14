//! Replays `fixtures/provider_auth.json` (recorded from the TypeScript CLI by `fixtures/provider_auth.mjs`): `ultron
//! auth` for built-in and models.json providers, with every built-in OAuth provider's expired token refreshed
//! through its token endpoint. The endpoints answer from the same stubs the recording used (an in-memory transport;
//! nothing leaves the process), so the output, exit code, the provider requests and the credential left in auth.json
//! must match.

use ai::http::{BoxFuture, HttpRequest, HttpResponse, Transport};
use serde_json::{Map, Value, json};
use session_core::stored_auth::OAuthOptions;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct OneBody(Option<Vec<u8>>);

impl ai::http::Body for OneBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// The recording's stubs: `"<METHOD> <url prefix>"` to responses taken in order, the last one repeating.
struct Stubs {
    stubs: Map<String, Value>,
    used: Mutex<HashMap<String, usize>>,
    requests: Mutex<Vec<Value>>,
}

impl Transport for Stubs {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let key = format!("{} {}", request.method.to_uppercase(), request.url);
        let authorization = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone());
        self.requests.lock().unwrap().push(json!({
            "method": request.method.to_uppercase(), "url": request.url,
            "body": if request.body.is_empty() { Value::Null } else { json!(request.body) },
            "authorization": authorization,
        }));
        let stub = self
            .stubs
            .iter()
            .find(|(k, _)| key.starts_with(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()));
        let response = stub.map(|(k, responses)| {
            let mut used = self.used.lock().unwrap();
            let index = used.entry(k).or_insert(0);
            let list = responses.as_array().unwrap();
            let response = list[(*index).min(list.len() - 1)].clone();
            *index += 1;
            response
        });
        Box::pin(async move {
            let response = response.ok_or_else(|| format!("unstubbed {key}"))?;
            let body = match &response["body"] {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            Ok(HttpResponse {
                status: response["status"].as_u64().unwrap_or(200) as u16,
                headers: [("content-type".to_string(), "application/json".to_string())]
                    .into_iter()
                    .collect(),
                body: Box::new(OneBody(Some(body.into_bytes()))),
            })
        })
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Expiries within the next week as `<now+>`, as the generator records them.
fn times(value: Value, started: i64) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let recent = v
                        .as_f64()
                        .is_some_and(|n| n >= started as f64 && n < (started + 7 * 86_400_000) as f64);
                    if k == "expires" && recent {
                        (k, json!("<now+>"))
                    } else {
                        (k, times(v, started))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

#[test]
fn cli_auth_for_builtin_and_models_json_providers_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/provider_auth.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 43);
    for case in cases {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        if !case["models"].is_null() {
            std::fs::write(agent.join("models.json"), case["models"].to_string()).unwrap();
        }
        if !case["auth"].is_null() {
            std::fs::write(agent.join("auth.json"), case["auth"].to_string()).unwrap();
        }
        let mut vars: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        vars.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
        let env = cli::Env {
            cwd: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: tmp.path().join("docs"),
        };
        let stubs = Arc::new(Stubs {
            stubs: fixture[case["stubs"]
                .as_str()
                .map(|s| if s == "failing" { "failing" } else { "stubs" })
                .unwrap()]
            .as_object()
            .unwrap()
            .clone(),
            used: Mutex::default(),
            requests: Mutex::default(),
        });
        let options = OAuthOptions {
            transport: stubs.clone(),
            callback_port: None,
        };
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let started = now_ms();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::auth::run_with(&args, &env, &mut out, &mut err, &options).unwrap();
        let name = args.join(" ");
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert_eq!(
            out,
            case["stdout"].as_str().unwrap(),
            "{name}: stdout (stderr {err:?}, requests {:?})",
            stubs.requests.lock().unwrap()
        );
        assert_eq!(err, case["stderr"].as_str().unwrap(), "{name}: stderr");
        assert_eq!(i64::from(code), case["code"].as_i64().unwrap(), "{name}: exit code");
        let expected: Vec<Value> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                json!({"method": r["method"], "url": r["url"], "body": r["body"],
                    "authorization": r["headers"]["authorization"]})
            })
            .collect();
        assert_eq!(*stubs.requests.lock().unwrap(), expected, "{name}: requests");
        let stored = std::fs::read_to_string(agent.join("auth.json"))
            .ok()
            .map(|t| serde_json::from_str::<Value>(&t).unwrap())
            .unwrap_or(Value::Null);
        assert_eq!(
            ai::json::normalize_numbers(times(stored, started)),
            ai::json::normalize_numbers(case["stored"].clone()),
            "{name}: auth.json"
        );
    }
}
