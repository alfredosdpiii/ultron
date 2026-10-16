//! The Bedrock provider's AWS default credential chain against `fixtures/bedrock-credentials.json`
//! (`fixtures/generate-bedrock-credentials.mjs`: the TypeScript provider and the AWS SDK it runs on). Each scenario
//! writes its files into a temp HOME, serves the recorded canned responses from one loopback HTTP server (SSO, SSO
//! OIDC, STS, container and instance metadata endpoints are pointed at it, and Bedrock itself is the model's base
//! URL), and compares the credential requests, the credentials the Bedrock request was signed with, files after the
//! run, and the final message with what the SDK did.

use ai::aws::encode_message;
use ai::aws_credentials::{parse_ini, sha1_hex};
use ai::bedrock::{self, BedrockOptions};
use ai::{AssistantMessageEvent, Model, ReqwestTransport, TranscriptContext};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, LazyLock, Mutex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/bedrock-credentials.json")).unwrap());

const RECORDED_HEADERS: [&str; 5] = [
    "authorization",
    "content-type",
    "x-amz-sso_bearer_token",
    "x-aws-ec2-metadata-token",
    "x-aws-ec2-metadata-token-ttl-seconds",
];

#[derive(Default)]
struct Recorded {
    requests: Vec<Value>,
    bedrock: Option<Value>,
    used: BTreeMap<String, usize>,
}

struct FakeAws {
    port: u16,
    recorded: Arc<Mutex<Recorded>>,
}

fn converse_stream() -> Vec<u8> {
    let event = |kind: &str, body: Value| {
        encode_message(
            &[
                (":event-type", kind),
                (":message-type", "event"),
                (":content-type", "application/json"),
            ],
            body.to_string().as_bytes(),
        )
    };
    [
        event("messageStart", json!({"role": "assistant"})),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "ok"}}),
        ),
        event("contentBlockStop", json!({"contentBlockIndex": 0})),
        event("messageStop", json!({"stopReason": "end_turn"})),
        event(
            "metadata",
            json!({"usage": {"inputTokens": 5, "outputTokens": 1, "totalTokens": 6}, "metrics": {"latencyMs": 3}}),
        ),
    ]
    .concat()
}

/// A request's method, target, lower-case headers and body.
type Request = (String, String, BTreeMap<String, String>, Vec<u8>);

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some((method, target, headers, body))
}

impl FakeAws {
    fn start(routes: Value) -> FakeAws {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Recorded::default()));
        let shared = recorded.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let recorded = shared.clone();
                let routes = routes.clone();
                std::thread::spawn(move || serve(&mut stream, &recorded, &routes));
            }
        });
        FakeAws { port, recorded }
    }
}

fn serve(stream: &mut TcpStream, recorded: &Mutex<Recorded>, routes: &Value) {
    let Some((method, target, headers, body)) = read_request(stream) else {
        return;
    };
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let reply = |stream: &mut TcpStream, status: u16, content_type: &str, extra: &[(String, String)], body: &[u8]| {
        let mut head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    };
    if method == "POST" && path.ends_with("/converse-stream") {
        let authorization = headers.get("authorization").cloned().unwrap_or_default();
        let credential = authorization
            .split("Credential=")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(|c| json!(c))
            .unwrap_or(Value::Null);
        recorded.lock().unwrap().bedrock = Some(json!({
            "credential": credential,
            "sessionToken": headers.get("x-amz-security-token").map_or(Value::Null, |t| json!(t)),
        }));
        reply(
            stream,
            200,
            "application/vnd.amazon.eventstream",
            &[("x-amzn-requestid".into(), "req-1".into())],
            &converse_stream(),
        );
        return;
    }
    let mut recorded_headers = Map::new();
    for name in RECORDED_HEADERS {
        if let Some(value) = headers.get(name) {
            let value = if name == "authorization" && value.starts_with("AWS4-HMAC-SHA256") {
                let credential = value.split("Credential=").nth(1).unwrap_or_default();
                let credential = credential.split(',').next().unwrap_or_default();
                let mut parts: Vec<&str> = credential.split('/').collect();
                if parts.len() > 1 {
                    parts[1] = "<date>";
                }
                format!("AWS4-HMAC-SHA256 Credential={}", parts.join("/"))
            } else {
                value.clone()
            };
            recorded_headers.insert(name.into(), json!(value));
        }
    }
    let mut pairs: Vec<(String, String)> = reqwest::Url::parse(&format!("http://x/?{query}"))
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    let key = format!("{method} {path}");
    let mut guard = recorded.lock().unwrap();
    guard.requests.push(json!({
        "method": method,
        "path": path,
        "query": pairs.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "headers": recorded_headers,
        "body": String::from_utf8_lossy(&body),
    }));
    let index = {
        let used = guard.used.entry(key.clone()).or_insert(0);
        *used += 1;
        *used - 1
    };
    drop(guard);
    let Some(list) = routes.get(&key).and_then(Value::as_array) else {
        reply(stream, 404, "text/plain", &[], b"no route");
        return;
    };
    let canned = &list[index.min(list.len() - 1)];
    let mut content_type = "application/json".to_string();
    let mut extra = Vec::new();
    for (name, value) in canned.get("headers").and_then(Value::as_object).into_iter().flatten() {
        if name == "content-type" {
            content_type = value.as_str().unwrap().to_string();
        } else {
            extra.push((name.clone(), value.as_str().unwrap().to_string()));
        }
    }
    let body = match &canned["body"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let status = canned.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
    reply(stream, status, &content_type, &extra, body.as_bytes());
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// `new Date(ms).toISOString()`.
fn iso(ms: i64) -> String {
    ai::claude_code_cli::iso_string(ms as f64)
}

/// `<home>`, `<port>` and `<now+N>` in a scenario value.
fn expand(text: &str, home: &str, port: u16) -> String {
    let mut out = text.replace("<home>", home).replace("<port>", &port.to_string());
    while let Some(start) = out.find("<now+") {
        let end = start + out[start..].find('>').unwrap();
        let seconds: i64 = out[start + 5..end].parse().unwrap();
        out.replace_range(start..=end, &iso(now_ms() + seconds * 1000));
    }
    out
}

/// The generator's normalization: paths, the port, timestamped session names and run-time ISO dates.
fn normalize(text: &str, home: &str, port: u16) -> String {
    let mut out = text.replace(home, "<home>").replace(&port.to_string(), "<port>");
    for prefix in ["aws-sdk-js-session-", "aws-sdk-js-"] {
        let mut result = String::new();
        let mut rest = out.as_str();
        while let Some(at) = rest.find(prefix) {
            let after = &rest[at + prefix.len()..];
            let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            result.push_str(&rest[..at + prefix.len()]);
            if digits > 0 {
                result.push_str("<now>");
            }
            rest = &after[digits..];
        }
        result.push_str(rest);
        out = result;
    }
    let bytes = out.as_bytes();
    let mut result = String::new();
    let mut i = 0;
    while i < out.len() {
        let digit = |j: usize| bytes.get(j).is_some_and(u8::is_ascii_digit);
        let shape = (0..4).all(|k| digit(i + k))
            && bytes.get(i + 4) == Some(&b'-')
            && bytes.get(i + 10) == Some(&b'T')
            && bytes.get(i + 13) == Some(&b':');
        if shape && (i == 0 || !digit(i - 1)) {
            let mut end = i + 19;
            if bytes.get(end) == Some(&b'.') {
                end += 1;
                while digit(end) {
                    end += 1;
                }
            }
            if bytes.get(end) == Some(&b'Z') {
                let iso = &out[i..=end];
                if iso.starts_with("20") && iso < "2090" && iso > "2010" {
                    result.push_str("<iso>");
                    i = end + 1;
                    continue;
                }
            }
        }
        let c = out[i..].chars().next().unwrap();
        result.push(c);
        i += c.len_utf8();
    }
    result
}

/// Settings fall back to the process env; these could make a replay read a real token file or reach a real AWS
/// endpoint. Refuse to run with them set.
fn assert_isolated_process_env() {
    for (name, _) in std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))) {
        let risky = name.starts_with("AWS_ENDPOINT_URL")
            || name.starts_with("AWS_CONTAINER_")
            || name.starts_with("AWS_EC2_METADATA")
            || ["AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_ROLE_ARN", "AWS_SDK_LOAD_CONFIG"].contains(&name.as_str());
        assert!(
            !risky,
            "{name} is set in the test process; unset it so the replay stays on local fakes"
        );
    }
}

fn replay(scenario: &Value) {
    assert_isolated_process_env();
    let name = scenario["name"].as_str().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path().to_str().unwrap().to_string();
    let server = FakeAws::start(scenario["routes"].clone());
    let port = server.port;
    for (path, content) in scenario["files"].as_object().unwrap() {
        let full = home_dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, expand(content.as_str().unwrap(), &home, port)).unwrap();
        if path.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let mut env = Map::new();
    for (key, value) in scenario["processEnv"].as_object().unwrap() {
        if let Some(value) = value.as_str() {
            env.insert(key.clone(), json!(expand(value, &home, port)));
        }
    }
    let mut options = scenario["options"].clone();
    options["env"] = Value::Object(env);
    let options: BedrockOptions = serde_json::from_value(options).unwrap();
    let model: Model = serde_json::from_value(json!({
        "id": "amazon.nova-pro-v1:0",
        "name": "Nova Pro",
        "api": "bedrock-converse-stream",
        "provider": "amazon-bedrock",
        "baseUrl": format!("http://127.0.0.1:{port}"),
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200000,
        "maxTokens": 8192,
    }))
    .unwrap();
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 2}]})).unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let transport = ReqwestTransport::with_client(client, None);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &AssistantMessageEvent| {};
    if scenario["warm"] == true {
        // Upstream's SSO refresh throttle is process-wide: a first call, then only the second is compared.
        runtime.block_on(bedrock::stream(&model, &context, &options, &transport, &mut sink));
        let mut recorded = server.recorded.lock().unwrap();
        recorded.requests.clear();
        recorded.bedrock = None;
        recorded.used.clear();
    }
    let message = runtime.block_on(bedrock::stream(&model, &context, &options, &transport, &mut sink));
    let message = serde_json::to_value(&message).unwrap();

    let recorded = server.recorded.lock().unwrap();
    let requests: Vec<Value> = recorded
        .requests
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r["body"] = json!(normalize(r["body"].as_str().unwrap(), &home, port));
            r
        })
        .collect();
    assert_eq!(
        Value::Array(requests),
        scenario["requests"],
        "{name}: credential requests"
    );
    assert_eq!(
        recorded.bedrock.clone().unwrap_or(Value::Null),
        scenario["bedrock"],
        "{name}: signing credentials"
    );
    for (path, expected) in scenario["after"].as_object().unwrap() {
        let actual = std::fs::read_to_string(home_dir.path().join(path))
            .ok()
            .map_or(Value::Null, |text| json!(normalize(&text, &home, port)));
        assert_eq!(&actual, expected, "{name}: {path} after the run");
    }
    let error = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .map_or(Value::Null, |m| json!(normalize(m, &home, port)));
    let stop = match message["stopReason"].as_str() {
        Some("error") if scenario["stopReason"].is_null() => Value::Null,
        other => json!(other),
    };
    assert_eq!(
        (stop, error),
        (scenario["stopReason"].clone(), scenario["errorMessage"].clone()),
        "{name}: outcome"
    );
    let diagnostics = message.get("diagnostics").cloned().map(|d| {
        Value::Array(
            d.as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let mut entry = entry.clone();
                    entry.as_object_mut().unwrap().shift_remove("timestamp");
                    entry
                })
                .collect(),
        )
    });
    assert_eq!(
        diagnostics.unwrap_or(Value::Null),
        scenario["diagnostics"],
        "{name}: diagnostics"
    );
}

fn scenario(name: &str) -> &'static Value {
    FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"))
}

#[test]
fn ai_bedrock_credentials_fixture_covers_every_scenario() {
    let names: Vec<&str> = FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 37);
}

macro_rules! replays {
    ($(($test:ident, $name:literal),)*) => {
        $(
            #[test]
            fn $test() {
                replay(scenario($name));
            }
        )*
    };
}

replays! {
    (ai_bedrock_credentials_sso_session, "sso-session"),
    (ai_bedrock_credentials_sso_session_refresh_writes_the_cache, "sso-session-refresh"),
    (ai_bedrock_credentials_sso_refresh_failure_keeps_a_valid_token, "sso-session-refresh-fails-uses-valid-token"),
    (ai_bedrock_credentials_sso_refresh_without_new_refresh_token_drops_it, "sso-session-refresh-drops-refresh-token"),
    (ai_bedrock_credentials_sso_refresh_is_throttled_for_thirty_seconds, "sso-session-refresh-throttled"),
    (ai_bedrock_credentials_sso_expiring_token_without_refresh_fields, "sso-session-expired-no-refresh-token"),
    (ai_bedrock_credentials_sso_missing_cache, "sso-session-missing-cache"),
    (ai_bedrock_credentials_sso_unknown_session, "sso-session-unknown-session"),
    (ai_bedrock_credentials_sso_legacy, "sso-legacy"),
    (ai_bedrock_credentials_sso_legacy_expired, "sso-legacy-expired"),
    (ai_bedrock_credentials_sso_legacy_missing_fields, "sso-legacy-missing-fields"),
    (ai_bedrock_credentials_sso_get_role_credentials_denied, "sso-get-role-credentials-denied"),
    (ai_bedrock_credentials_sso_invalid_role_credentials, "sso-invalid-role-credentials"),
    (ai_bedrock_credentials_credential_process, "credential-process"),
    (ai_bedrock_credentials_credential_process_default_profile, "credential-process-default-profile-no-token"),
    (ai_bedrock_credentials_credential_process_bad_version_runs_twice, "credential-process-bad-version-falls-through"),
    (ai_bedrock_credentials_credential_process_expired, "credential-process-expired"),
    (ai_bedrock_credentials_credential_process_failure, "credential-process-fails"),
    (ai_bedrock_credentials_static_keys_win_over_credential_process, "credential-process-static-keys-win"),
    (ai_bedrock_credentials_web_identity_env, "web-identity-env"),
    (ai_bedrock_credentials_web_identity_default_session_name, "web-identity-env-default-session-name"),
    (ai_bedrock_credentials_web_identity_profile, "web-identity-profile"),
    (ai_bedrock_credentials_web_identity_sts_error, "web-identity-sts-error"),
    (ai_bedrock_credentials_web_identity_missing_token_file, "web-identity-missing-token-file"),
    (ai_bedrock_credentials_assume_role_source_profile, "assume-role-source-profile"),
    (ai_bedrock_credentials_assume_role_cycle, "assume-role-cycle"),
    (ai_bedrock_credentials_assume_role_credential_source_imds, "assume-role-credential-source-imds"),
    (ai_bedrock_credentials_imds_v2, "imds-v2"),
    (ai_bedrock_credentials_imds_v1_fallback, "imds-v1-fallback"),
    (ai_bedrock_credentials_imds_v1_fallback_blocked, "imds-v1-fallback-blocked"),
    (ai_bedrock_credentials_imds_token_400, "imds-token-400"),
    (ai_bedrock_credentials_imds_invalid_credentials, "imds-invalid-credentials"),
    (ai_bedrock_credentials_container_full_uri, "container-full-uri"),
    (ai_bedrock_credentials_container_token_file, "container-token-file"),
    (ai_bedrock_credentials_container_host_not_allowed, "container-host-not-allowed"),
    (ai_bedrock_credentials_profile_skips_env_keys, "profile-skips-env-keys-then-nothing"),
    (ai_bedrock_credentials_ini_subsections_and_comments, "ini-subsections-and-comments"),
}

// ------------------------------------------------------------------------------------- ported literal expectations

/// `@smithy/core/config` parseIni tests: comments, quoted and prefixed section names, sub-sections.
#[test]
fn ai_bedrock_credentials_parse_ini_matches_smithy() {
    let parsed = parse_ini(
        "[default]\naws_access_key_id = A ; comment\nkey#notcomment = v#1\n[profile 'quoted']\nregion = r\n[sso-session s]\nsso_region = us-east-1\n[unknown thing]\nx = 1\n[profile two  spaces]\ny = 2\n[services dev]\ns3 =\n  endpoint_url = http://s3\nafter = top\n",
    );
    let flat: BTreeMap<String, Vec<(String, String)>> = parsed
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|(a, b)| (a.clone(), b.clone())).collect()))
        .collect();
    let pairs = |items: &[(&str, &str)]| {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect::<Vec<_>>()
    };
    let mut expected = BTreeMap::new();
    expected.insert(
        "default".to_string(),
        pairs(&[("aws_access_key_id", "A"), ("key#notcomment", "v#1")]),
    );
    expected.insert("profile.quoted".to_string(), pairs(&[("region", "r")]));
    expected.insert("sso-session.s".to_string(), pairs(&[("sso_region", "us-east-1")]));
    expected.insert("profile two  spaces".to_string(), pairs(&[("y", "2")]));
    expected.insert(
        "services.dev".to_string(),
        pairs(&[("s3.endpoint_url", "http://s3"), ("after", "top")]),
    );
    assert_eq!(flat, expected);
    assert!(parse_ini("[profile __proto__]\nx = 1\n").is_empty());
}

#[test]
fn ai_bedrock_credentials_sha1_names_the_sso_cache() {
    // The cache names recorded by the fixture generator (node:crypto sha1).
    assert_eq!(sha1_hex(b"corp"), "ee0bfd2552fbd840c02cc48b6e823320543c450f");
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(&[b'a'; 1000]), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
}
