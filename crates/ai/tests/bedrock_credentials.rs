//! The Bedrock provider's AWS default credential chain against `fixtures/bedrock-credentials.json`
//! (`fixtures/generate-bedrock-credentials.mjs`: the TypeScript provider and the AWS SDK it runs on). Each scenario
//! writes its files into a temp HOME, serves the recorded canned responses from one loopback HTTP server (SSO, SSO
//! OIDC, STS, container and instance metadata endpoints are pointed at it, and Bedrock itself is the model's base
//! URL), and compares the credential requests, the credentials the Bedrock request was signed with, files after the
//! run, and the final message with what the SDK did. `aws login` scenarios also check each DPoP proof: decoded as the
//! generator records it, and its ES256 signature valid for the header's key. Requests the SDK's credential cache sends
//! after the result (`lateRequests`: a background refresh of credentials about to expire) are not part of a call and
//! are not ported.

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

const RECORDED_HEADERS: [&str; 6] = [
    "authorization",
    "content-type",
    "dpop",
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
            if name == "dpop" {
                recorded_headers.insert(name.into(), decode_dpop(value));
                continue;
            }
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

/// The DPoP key's SEC1 PEM, built from the recorded scalar and point (`DER: version 1, the scalar, prime256v1, the
/// point`), as Node exports it.
fn dpop_pem() -> String {
    let dpop = &FIXTURE["dpop"];
    let hex = dpop["d"].as_str().unwrap();
    let d: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let point = |key: &str| ai::jsutil::atob(&dpop[key].as_str().unwrap().replace('-', "+").replace('_', "/")).unwrap();
    let mut der = vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20];
    der.extend(&d);
    der.extend([0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]);
    der.extend([0xa1, 0x44, 0x03, 0x42, 0x00, 0x04]);
    der.extend(point("x"));
    der.extend(point("y"));
    let body = ai::jsutil::btoa(&der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
        lines.join("\n")
    )
}

/// The PEM as it sits inside a JSON string.
fn dpop_pem_json() -> String {
    let quoted = serde_json::to_string(&dpop_pem()).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

fn base64url_decode(text: &str) -> Vec<u8> {
    ai::jsutil::atob(&text.replace('-', "+").replace('_', "/")).unwrap()
}

/// A DPoP proof as the generator records it (jti and iat as placeholders), after checking its signature with the
/// header's key.
fn decode_dpop(value: &str) -> Value {
    let parts: Vec<&str> = value.split('.').collect();
    assert_eq!(parts.len(), 3, "a DPoP JWT");
    let header: Value = serde_json::from_slice(&base64url_decode(parts[0])).unwrap();
    let mut payload: Value = serde_json::from_slice(&base64url_decode(parts[1])).unwrap();
    let signature = base64url_decode(parts[2]);
    let mut point = vec![0x04];
    point.extend(base64url_decode(header["jwk"]["x"].as_str().unwrap()));
    point.extend(base64url_decode(header["jwk"]["y"].as_str().unwrap()));
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("the DPoP proof is signed by its key");
    let jti = payload["jti"].as_str().unwrap_or_default();
    let v4 = jti.len() == 36
        && jti.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            14 => c == '4',
            19 => "89ab".contains(c),
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        });
    if v4 {
        payload["jti"] = json!("<uuid>");
    }
    if payload["iat"]
        .as_i64()
        .is_some_and(|iat| (iat * 1000 - now_ms()).abs() < 600_000)
    {
        payload["iat"] = json!("<iat>");
    }
    json!({"header": header, "payload": payload, "signatureBytes": signature.len()})
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
    let mut out = text
        .replace("<dpop-pem>", &dpop_pem_json())
        .replace("<home>", home)
        .replace("<port>", &port.to_string());
    while let Some(start) = out.find("<now+") {
        let end = start + out[start..].find('>').unwrap();
        let seconds: i64 = out[start + 5..end].parse().unwrap();
        out.replace_range(start..=end, &iso(now_ms() + seconds * 1000));
    }
    out
}

/// The generator's normalization: paths, the port, timestamped session names and run-time ISO dates.
fn normalize(text: &str, home: &str, port: u16) -> String {
    let mut out = text
        .replace(&dpop_pem_json(), "<dpop-pem>")
        .replace(home, "<home>")
        .replace(&port.to_string(), "<port>");
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
            if let Some(htu) = r["headers"]["dpop"]["payload"]["htu"].as_str() {
                r["headers"]["dpop"]["payload"]["htu"] = json!(normalize(htu, &home, port));
            }
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
    for (path, minutes) in scenario
        .get("afterLifetime")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let text = std::fs::read_to_string(home_dir.path().join(path)).unwrap();
        let token: Value = serde_json::from_str(&text).unwrap();
        let expires = ai::model_refresh::parse_iso_ms(token["accessToken"]["expiresAt"].as_str().unwrap()).unwrap();
        let lifetime = ((expires - now_ms()) as f64 / 60_000.0).round() as i64;
        assert_eq!(json!(lifetime), *minutes, "{name}: {path} lifetime in minutes");
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
    assert_eq!(names.len(), 55);
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
    (ai_bedrock_credentials_login_valid_token, "login-valid-token"),
    (ai_bedrock_credentials_login_refresh_writes_the_cache, "login-refresh"),
    (ai_bedrock_credentials_login_refresh_within_five_minutes, "login-refresh-within-five-minutes"),
    (ai_bedrock_credentials_login_refresh_retries_a_server_error, "login-refresh-retries-server-error"),
    (ai_bedrock_credentials_login_refresh_default_lifetime, "login-refresh-default-lifetime"),
    (ai_bedrock_credentials_login_refresh_token_expired, "login-refresh-token-expired"),
    (ai_bedrock_credentials_login_refresh_credentials_changed, "login-refresh-credentials-changed"),
    (ai_bedrock_credentials_login_refresh_insufficient_permissions, "login-refresh-insufficient-permissions"),
    (ai_bedrock_credentials_login_refresh_denied_other, "login-refresh-denied-other"),
    (ai_bedrock_credentials_login_refresh_failure_keeps_a_valid_token, "login-refresh-fails-keeps-valid-token"),
    (ai_bedrock_credentials_login_refresh_failure_expired_token_moves_on, "login-refresh-fails-expired-token"),
    (ai_bedrock_credentials_login_refresh_response_missing_fields, "login-refresh-response-missing-fields"),
    (ai_bedrock_credentials_login_refresh_response_missing_fields_valid_token, "login-refresh-response-missing-fields-valid-token"),
    (ai_bedrock_credentials_login_missing_token_file, "login-missing-token-file"),
    (ai_bedrock_credentials_login_token_missing_fields, "login-token-missing-fields"),
    (ai_bedrock_credentials_login_cache_directory_env, "login-cache-directory-env"),
    (ai_bedrock_credentials_login_static_keys_win, "login-static-keys-win"),
    (ai_bedrock_credentials_login_default_profile, "login-default-profile"),
}

/// The Signin endpoint of each partition, as the SDK client's endpoint rules resolve it (recorded).
#[test]
fn ai_bedrock_credentials_signin_endpoints_match_the_sdk_rules() {
    for (region, url) in FIXTURE["signinEndpoints"].as_object().unwrap() {
        assert_eq!(
            format!("{}/", ai::aws_credentials::signin_endpoint(region)),
            url.as_str().unwrap(),
            "{region}"
        );
    }
}

/// The PEM the replay writes is the one Node exported for the generator's key, and it parses to that key.
#[test]
fn ai_bedrock_credentials_dpop_key_is_nodes_sec1_pem() {
    use sha2::Digest;
    let pem = dpop_pem();
    let digest: String = sha2::Sha256::digest(pem.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, FIXTURE["dpop"]["pemSha256"].as_str().unwrap());
    let (scalar, point) = ai::aws_credentials::parse_sec1_pem(&pem).unwrap();
    assert_eq!(scalar, (1u8..=32).collect::<Vec<_>>());
    assert_eq!(point.len(), 65);
    assert_eq!(point[0], 4);
    assert!(
        ai::aws_credentials::parse_sec1_pem("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n").is_err()
    );
    // Without the public key, or on another curve.
    let mut no_public = vec![0x30, 0x31, 0x02, 0x01, 0x01, 0x04, 0x20];
    no_public.extend(1u8..=32);
    no_public.extend([0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]);
    let wrap = |der: &[u8]| {
        format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
            ai::jsutil::btoa(der)
        )
    };
    assert_eq!(
        ai::aws_credentials::parse_sec1_pem(&wrap(&no_public)).unwrap_err(),
        "The DPoP key has no public key"
    );
    let mut other_curve = no_public.clone();
    let last = other_curve.len() - 1;
    other_curve[last] = 0x08;
    assert!(
        ai::aws_credentials::parse_sec1_pem(&wrap(&other_curve))
            .unwrap_err()
            .contains("P-256")
    );
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
