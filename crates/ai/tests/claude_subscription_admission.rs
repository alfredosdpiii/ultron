//! The admission relay on loopback, ported from the Hermes plugin's `tests/test_admission_restoration.py` (relay
//! cases), `test_admission_breakpoint.py` (relay cases), `test_admission_two_marks.py`, `test_admission_proxy.py`
//! and the relay cases of `test_directsdk_admission.py`: one admitted request, later attempts refused locally,
//! unprovable restorations forwarded unchanged and reported, proxies honoured like native, headers passed through
//! and never kept.

mod claude_subscription_support;

use ai::claude_subscription::admission::{Admission, CONSUMED, ProxyRoute, check_upstream, upstream_proxy};
use ai::claude_subscription::rewrite::MISMATCH;
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{Duration, Instant};

/// Timeouts are inputs no test here exercises; long so a loaded machine never trips them.
fn gate(upstream: &str, queried: Option<Vec<Value>>) -> Admission {
    Admission::new(
        upstream,
        Duration::from_secs(120),
        queried,
        Duration::from_secs(120),
        &BTreeMap::new(),
    )
    .unwrap()
}

fn ok_upstream() -> Upstream {
    Upstream::start(|_, stream| reply_status(stream, 200, ""))
}

fn result() -> Value {
    json!({"type": "tool_result", "tool_use_id": "t1", "content": "output"})
}

fn with(base: Value, extra: Value) -> Value {
    let mut merged = base;
    for (k, v) in extra.as_object().unwrap() {
        merged[k] = v.clone();
    }
    merged
}

fn wire(content: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"model": "fixture", "messages": [
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "probe", "input": {}}]},
        {"role": "user", "content": content},
    ]}))
    .unwrap()
}

fn strip(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| *k != "cache_control")
                .map(|(k, v)| (k.clone(), strip(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip).collect()),
        other => other.clone(),
    }
}

#[test]
fn ai_claude_subscription_unprovable_restoration_forwards_the_native_payload_unchanged() {
    seal();
    let marker = |ttl: &str| json!({"type": "ephemeral", "ttl": ttl});
    let cases = vec![
        (
            vec![result()],
            json!([result(), {"type": "text", "text": "note", "cache_control": marker("1h")}]),
        ),
        (
            vec![result()],
            json!([result(), {"type": "tool_result", "tool_use_id": "extra", "content": "extra"}]),
        ),
        (
            vec![result()],
            json!([result(), {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}]),
        ),
        (
            vec![result()],
            json!([result(), {"type": "text", "text": "note", "citations": []}]),
        ),
        (
            vec![result()],
            json!([result(), {"type": "text", "text": "note", "unknown": true}]),
        ),
        (vec![result()], json!([with(result(), json!({"is_error": true}))])),
        (vec![result()], json!([with(result(), json!({"is_error": 0}))])),
        (vec![result()], json!([with(result(), json!({"tool_use_id": "wrong"}))])),
        (vec![result()], json!([with(result(), json!({"content": "changed"}))])),
        (vec![result()], json!([result(), result()])),
        (vec![result()], json!([{"type": "text", "text": "prepended"}, result()])),
        (
            vec![with(result(), json!({"content": [{"type": "text", "text": "output"}]}))],
            json!([with(
                result(),
                json!({"content": [{"type": "text", "text": "output"}, {"type": "text", "text": "note", "cache_control": {"type": "ephemeral"}}]})
            )]),
        ),
        (
            vec![with(result(), json!({"cache_control": marker("1h")}))],
            json!([with(result(), json!({"cache_control": marker("5m")}))]),
        ),
    ];
    for (queried, native) in cases {
        let upstream = ok_upstream();
        let mut relay = gate(&upstream.url(), Some(queried));
        let (status, _) = post(
            &format!("{}/v1/messages", relay.url),
            &wire(native.clone()),
            &[("Content-Type", "application/json")],
        );
        assert_eq!(status, 200);
        relay.close();
        let outcome = relay.outcome();
        assert_eq!(outcome.unrestored.as_deref(), Some(MISMATCH));
        assert_eq!(outcome.failure, None);
        let received = upstream.requests();
        assert_eq!(received.len(), 1);
        // Nothing is dropped or rewritten: native's own frame goes upstream (only a marker may move).
        assert_eq!(
            strip(&received[0].json()),
            strip(&serde_json::from_slice::<Value>(&wire(native)).unwrap())
        );
    }
}

#[test]
fn ai_claude_subscription_relay_forwards_the_restored_frame_and_an_unmatched_one_unchanged() {
    seal();
    let upstream = Upstream::start(|_, stream| reply_status(stream, 400, ""));
    let real = json!({"type": "tool_result", "tool_use_id": "t1", "content": "real output"});
    let mut relay = gate(&upstream.url(), Some(vec![real.clone()]));
    let native = json!([{"type": "tool_result", "tool_use_id": "t1", "content": "real output\n<system-reminder>userEmail</system-reminder>"}]);
    let (status, _) = post(&format!("{}/v1/messages", relay.url), &wire(native), &[]);
    assert_eq!(status, 400);
    relay.close();
    assert_eq!(upstream.requests()[0].json()["messages"][1]["content"], json!([real]));

    let upstream = ok_upstream();
    let mut relay = gate(&upstream.url(), Some(vec![real.clone()]));
    let payload = wire(json!([with(real, json!({"tool_use_id": "different"}))]));
    post(&format!("{}/v1/messages", relay.url), &payload, &[]);
    relay.close();
    assert_eq!(relay.outcome().unrestored.as_deref(), Some(MISMATCH));
    assert_eq!(relay.outcome().failure, None);
    assert_eq!(upstream.requests()[0].body, payload, "byte for byte");
}

#[test]
fn ai_claude_subscription_wire_continuity_across_parallel_rounds_and_relay_recreation() {
    seal();
    let upstream = ok_upstream();
    let history = json!([
        {"role": "user", "content": "earlier prompt"},
        {"role": "assistant", "content": [{"type": "thinking", "thinking": "signed", "signature": "sig"}, {"type": "text", "text": "earlier answer"}]},
    ]);
    for round in 0..2 {
        let queried = vec![
            json!({"type": "tool_result", "tool_use_id": format!("p{round}a"), "content": "A"}),
            json!({"type": "tool_result", "tool_use_id": format!("p{round}b"), "content": "B"}),
        ];
        let assistant = json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": format!("p{round}a"), "name": "a", "input": {}},
            {"type": "tool_use", "id": format!("p{round}b"), "name": "b", "input": {}},
        ]});
        let mut native = queried.clone();
        native.push(json!({"type": "text", "text": "native date annotation"}));
        let mut messages = history.as_array().unwrap().clone();
        messages.push(assistant);
        messages.push(json!({"role": "user", "content": native}));
        let mut relay = gate(&upstream.url(), Some(queried));
        let (status, _) = post(
            &format!("{}/v1/messages", relay.url),
            &serde_json::to_vec(&json!({"model": "fixture", "messages": messages})).unwrap(),
            &[],
        );
        assert_eq!(status, 200);
        relay.close();
    }
    let received = upstream.requests();
    assert_eq!(received.len(), 2);
    for seen in received {
        let body = seen.json();
        assert_eq!(
            body["messages"].as_array().unwrap()[..2],
            history.as_array().unwrap()[..]
        );
        let last = body["messages"].as_array().unwrap().last().unwrap()["content"].clone();
        assert_eq!(
            last,
            json!([
                {"type": "tool_result", "tool_use_id": last[0]["tool_use_id"], "content": "A"},
                {"type": "tool_result", "tool_use_id": last[1]["tool_use_id"], "content": "B"},
            ])
        );
    }
}

// --------------------------------------------------------------------------------------------- two native marks

fn marker() -> Value {
    json!({"type": "ephemeral"})
}

fn results(n: usize) -> Vec<Value> {
    (0..4).map(|k| json!({"type": "tool_result", "tool_use_id": format!("r{n}c{k}"), "content": format!("output {n}.{k}")})).collect()
}

fn two_mark_assistant(n: usize) -> Value {
    let mut content =
        vec![json!({"type": "thinking", "thinking": format!("plan {n}"), "signature": format!("sig{n}")})];
    content.extend((0..4).map(|k| json!({"type": "tool_use", "id": format!("r{n}c{k}"), "name": "mcp__ultron__rlm", "input": {"code": format!("echo {n}.{k}")}})));
    json!({"role": "assistant", "content": content})
}

fn native_request(rounds: usize, reminder: bool) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [{"type": "text", "text": "run four probes, twice"}]})];
    for n in 0..rounds {
        messages.push(two_mark_assistant(n));
        messages.push(json!({"role": "user", "content": results(n)}));
    }
    let count = messages.len();
    messages[count - 2]["content"][4]["cache_control"] = marker();
    if reminder {
        let text = messages[count - 1]["content"][3]["content"]
            .as_str()
            .unwrap()
            .to_string();
        messages[count - 1]["content"][3]["content"] = json!(format!(
            "{text}\n<system-reminder>userEmail: user@example.com</system-reminder>"
        ));
    }
    messages.push(json!({"role": "system", "content": [{"type": "text", "text": format!("Today's date is 2026-10-04. Request {rounds}."), "cache_control": marker()}]}));
    json!({
        "model": "claude-opus-5-5",
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cc"}, {"type": "text", "text": "You are Claude Code. Ultron system prompt.", "cache_control": marker()}],
        "tools": [{"name": "mcp__ultron__rlm", "description": "run", "input_schema": {"type": "object"}, "cache_control": marker()}],
        "messages": messages,
    })
}

fn message_marks(body: &Value) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (i, m) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
            if b.get("cache_control").is_some() {
                found.push((i, j));
            }
        }
    }
    found
}

fn mark_count(body: &Value) -> usize {
    let own = ["system", "tools"]
        .iter()
        .flat_map(|part| body[*part].as_array().unwrap().iter())
        .filter(|b| b.get("cache_control").is_some())
        .count();
    own + message_marks(body).len()
}

fn prefix(body: &Value, (i, j): (usize, usize)) -> String {
    let mut head: Vec<Value> = body["messages"].as_array().unwrap()[..i].to_vec();
    let mut last = body["messages"][i].clone();
    last["content"] = json!(last["content"].as_array().unwrap()[..=j].to_vec());
    head.push(last);
    strip(&json!({"tools": body["tools"], "system": body["system"], "messages": head})).to_string()
}

fn forward(native: &Value, queried: Vec<Value>) -> Value {
    let upstream = ok_upstream();
    let mut relay = gate(&upstream.url(), Some(queried));
    let (status, _) = post(
        &format!("{}/v1/messages", relay.url),
        &serde_json::to_vec(native).unwrap(),
        &[("Content-Type", "application/json")],
    );
    assert_eq!(status, 200);
    relay.close();
    assert_eq!(relay.outcome().unrestored, None);
    assert_eq!(relay.outcome().failure, None);
    let received = upstream.requests();
    assert_eq!(received.len(), 1);
    received[0].json()
}

#[test]
fn ai_claude_subscription_two_native_marks_keep_one_breakpoint_the_next_request_reads() {
    seal();
    for reminder in [false, true] {
        let native_n = native_request(1, reminder);
        let sent_n = forward(&native_n, results(0));
        let sent_n1 = forward(&native_request(2, reminder), results(1));
        assert_eq!(message_marks(&native_n), [(1, 4), (3, 0)]);
        assert_eq!(message_marks(&sent_n), [(1, 4), (2, 3)]);
        assert_eq!(message_marks(&sent_n1), [(3, 4), (4, 3)]);
        assert_eq!(sent_n["system"], native_n["system"]);
        assert_eq!(sent_n["tools"], native_n["tools"]);
        assert_eq!(strip(&sent_n["messages"][3]), strip(&native_n["messages"][3]));
        assert_eq!(mark_count(&sent_n), 4);
        assert_eq!(mark_count(&native_n), 4);
        let last = *message_marks(&sent_n).last().unwrap();
        assert_eq!(prefix(&sent_n1, last), prefix(&sent_n, last));
    }
}

#[test]
fn ai_claude_subscription_unrestorable_result_falls_back_to_native_tool_use_mark() {
    seal();
    let native = native_request(1, true);
    let raw = serde_json::to_vec(&native).unwrap();
    let sent: Value = serde_json::from_slice(&ai::claude_subscription::rewrite::pin_message_breakpoint(
        &raw,
        Some(&results(0)),
    ))
    .unwrap();
    assert_eq!(message_marks(&sent), [(1, 4)]);
    assert_eq!(mark_count(&sent), mark_count(&native) - 1);
    assert_eq!(strip(&sent), strip(&native));
    assert_eq!(sent["system"], native["system"]);
    let following = native_request(2, true);
    assert_eq!(prefix(&following, (1, 4)), prefix(&sent, (1, 4)));
    let mut recurring = native_request(1, false);
    recurring["messages"].as_array_mut().unwrap().pop();
    recurring["messages"][2]["content"][3]["cache_control"] = marker();
    let raw = serde_json::to_vec(&recurring).unwrap();
    assert_eq!(
        ai::claude_subscription::rewrite::pin_message_breakpoint(&raw, Some(&results(0))),
        raw
    );
}

// ------------------------------------------------------------------------------------------------ admission

#[test]
fn ai_claude_subscription_relay_admits_one_request_and_refuses_the_rest_locally() {
    seal();
    let upstream = Upstream::start(|_, stream| {
        reply_sse(
            stream,
            &reply_events(
                "first",
                &[json!({"type": "text", "text": "FIRST"})],
                "end_turn",
                json!({"input_tokens": 0, "output_tokens": 0}),
            ),
        )
    });
    let mut relay = gate(&upstream.url(), None);
    assert!(relay.url.starts_with("http://127.0.0.1:"));
    assert_eq!(relay.prefix.len(), "/admit/".len() + 64);
    let headers = [
        ("Authorization", "Bearer fake-subscription-token"),
        ("anthropic-version", "2023-06-01"),
        ("Accept-Encoding", "gzip"),
    ];
    // Off-route and browser-origin requests never reach the upstream.
    assert_eq!(
        post(&format!("http://127.0.0.1:{}/v1/messages", relay.port), b"{}", &headers).0,
        404
    );
    assert_eq!(
        post(
            &format!("{}/v1/messages", relay.url),
            b"{}",
            &[("Origin", "http://evil.test")]
        )
        .0,
        404
    );
    assert_eq!(post(&format!("{}/v1/models", relay.url), b"{}", &headers).0, 404);
    let (status, body) = post(
        &format!("{}/v1/messages?beta=true", relay.url),
        b"{\"messages\":[]}",
        &headers,
    );
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("FIRST"));
    for _ in 0..2 {
        let (status, body) = post(&format!("{}/v1/messages", relay.url), b"{}", &headers);
        assert_eq!(status, 400);
        assert_eq!(body, CONSUMED.as_bytes());
    }
    relay.close();
    let outcome = relay.outcome();
    assert!(outcome.used && outcome.capture.complete);
    assert_eq!(outcome.denied, 2);
    assert_eq!(outcome.status, Some(200));
    assert_eq!(outcome.request_id.as_deref(), Some("req_fixture"));
    assert_eq!(
        outcome.capture.message.as_ref().unwrap()["content"],
        json!([{"type": "text", "text": "FIRST"}])
    );
    let seen = upstream.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/messages?beta=true");
    // Native's identity passes through; only the transfer encoding is the relay's.
    assert_eq!(seen[0].header("authorization"), Some("Bearer fake-subscription-token"));
    assert_eq!(seen[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(seen[0].header("accept-encoding"), Some("identity"));
    assert_eq!(
        seen[0].header("host"),
        Some(format!("127.0.0.1:{}", upstream.port).as_str())
    );
}

#[test]
fn ai_claude_subscription_upstream_must_be_https_or_a_loopback_fixture() {
    seal();
    for ok in [
        "https://api.anthropic.com",
        "http://127.0.0.1:9",
        "http://localhost:9/base",
        "http://[::1]:9",
    ] {
        assert!(check_upstream(ok).is_ok(), "{ok}");
    }
    for refused in [
        "http://api.anthropic.com",
        "https://user:pw@api.anthropic.com",
        "https://api.anthropic.com/?q=1",
        "ftp://x",
        "http://10.0.0.1",
    ] {
        assert_eq!(
            check_upstream(refused).unwrap_err(),
            "Native upstream must be HTTPS or a loopback HTTP fixture",
            "{refused}"
        );
    }
}

#[test]
fn ai_claude_subscription_abort_ends_an_exchange_blocked_on_a_silent_upstream() {
    seal();
    let upstream = Upstream::start(|_, stream| {
        let mut sink = [0u8; 1];
        let _ = stream.set_read_timeout(Some(Duration::from_secs(600)));
        let _ = stream.read(&mut sink);
    });
    let mut relay = Admission::new(
        &upstream.url(),
        Duration::from_secs(600),
        None,
        Duration::from_secs(600),
        &BTreeMap::new(),
    )
    .unwrap();
    let url = format!("{}/v1/messages", relay.url);
    let client = std::thread::spawn(move || post(&url, b"{}", &[]));
    let deadline = Instant::now() + Duration::from_secs(120);
    while upstream.requests().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(upstream.requests().len(), 1, "the exchange reached the upstream");
    let started = Instant::now();
    relay.abort();
    let _ = client.join();
    relay.close();
    // The abort ended it, not the 600 s timeouts (a hang guard, loose for load).
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "abort took {:?}",
        started.elapsed()
    );
    assert_eq!(relay.outcome().failure.as_deref(), Some("Cancelled"));
}

// ---------------------------------------------------------------------------------------------------- proxies

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn proxy_for(upstream: &str, pairs: &[(&str, &str)]) -> Option<ProxyRoute> {
    let mut warnings = Vec::new();
    upstream_proxy(&reqwest::Url::parse(upstream).unwrap(), &env(pairs), &mut warnings)
}

#[test]
fn ai_claude_subscription_proxy_selection_by_no_proxy() {
    seal();
    let cases = [
        ("api.example.com", "", true),
        ("api.example.com", ".example.com", false),
        ("example.com", "example.com", false),
        ("api.example.com", "example.com:443", false),
        ("api.example.com", "other.com, .example.com", false),
        ("api.example.com", "other.com .example.com", false),
        ("api.example.com", "*", false),
        ("notexample.com", "example.com", true),
    ];
    for (host, no_proxy, expected) in cases {
        let found = proxy_for(
            &format!("https://{host}"),
            &[("HTTPS_PROXY", "http://proxy.test:8080"), ("NO_PROXY", no_proxy)],
        );
        assert_eq!(found.is_some(), expected, "{host} / {no_proxy}");
    }
    let https = [
        ("HTTPS_PROXY", "http://proxy.test:8080"),
        ("NO_PROXY", "api.example.com:443"),
    ];
    assert!(proxy_for("https://api.example.com:8443", &https).is_some());
    let https = [
        ("HTTPS_PROXY", "http://proxy.test:8080"),
        ("NO_PROXY", "api.example.com:8443"),
    ];
    assert!(proxy_for("https://api.example.com:8443", &https).is_none());
}

#[test]
fn ai_claude_subscription_proxy_variables_follow_native_order() {
    seal();
    let route = |host: &str, port: u16| {
        Some(ProxyRoute {
            host: host.into(),
            port,
            authorization: None,
        })
    };
    assert_eq!(proxy_for("https://api.example.com", &[]), None);
    assert_eq!(
        proxy_for("https://api.example.com", &[("ALL_PROXY", "http://all.test:3128")]),
        None
    );
    assert_eq!(
        proxy_for("https://api.example.com", &[("HTTP_PROXY", "http://plain.test:3128")]),
        route("plain.test", 3128)
    );
    assert_eq!(
        proxy_for(
            "https://api.example.com",
            &[
                ("HTTP_PROXY", "http://plain.test:3128"),
                ("HTTPS_PROXY", "http://upper.test")
            ]
        ),
        route("upper.test", 80)
    );
    assert_eq!(
        proxy_for(
            "https://api.example.com",
            &[
                ("HTTPS_PROXY", "http://upper.test"),
                ("https_proxy", "http://lower.test:3128")
            ]
        )
        .unwrap()
        .host,
        "lower.test"
    );
    assert_eq!(
        proxy_for("https://api.example.com", &[("HTTPS_PROXY", "proxy.test:9")]),
        route("proxy.test", 9)
    );
    // A loopback HTTP fixture never goes through a proxy.
    assert_eq!(
        proxy_for(
            "http://127.0.0.1:9",
            &[
                ("HTTPS_PROXY", "http://proxy.test:8080"),
                ("HTTP_PROXY", "http://proxy.test:8080")
            ]
        ),
        None
    );
}

#[test]
fn ai_claude_subscription_unsupported_proxy_connects_directly_without_leaking_credentials() {
    seal();
    // Proxy URLs are built from parts so the source holds no literal `user:password@` credential.
    let password = "hunter2";
    for url in [
        ["socks5://user:", password, "@proxy.test:1080"].concat(),
        ["https://user:", password, "@proxy.test:8443"].concat(),
        ["http://user:", password, "@proxy.test:notaport"].concat(),
    ] {
        let mut warnings = Vec::new();
        let found = upstream_proxy(
            &reqwest::Url::parse("https://api.example.com").unwrap(),
            &env(&[("HTTPS_PROXY", url.as_str())]),
            &mut warnings,
        );
        assert_eq!(found, None, "{url}");
        let text = warnings.join("\n");
        assert!(text.contains("CONNECT proxy") && !text.contains(password), "{text}");
    }
    let relay = Admission::new(
        "https://api.example.com",
        Duration::from_secs(1),
        None,
        Duration::from_secs(1),
        &env(&[("HTTPS_PROXY", ["socks5://u:", password, "@p:1"].concat().as_str())]),
    )
    .unwrap();
    assert!(relay.proxy().is_none());
    assert_eq!(relay.warnings.len(), 1);
}

#[test]
fn ai_claude_subscription_authenticated_connect_tunnels_to_the_upstream_host() {
    seal();
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = proxy.local_addr().unwrap().port();
    let seen = std::thread::spawn(move || {
        let (mut stream, _) = proxy.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        // No upstream behind it: the TLS handshake that follows fails and the relay records it.
        let _ = std::io::Write::write_all(&mut stream, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        String::from_utf8_lossy(&head).into_owned()
    });
    let password = "p%40ss";
    let pairs = [(
        "HTTPS_PROXY",
        ["http://user:", password, &format!("@127.0.0.1:{port}")].concat(),
    )];
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut relay = Admission::new(
        "https://api.example.invalid/base",
        Duration::from_secs(120),
        None,
        Duration::from_secs(120),
        &env(&pairs),
    )
    .unwrap();
    assert_eq!(
        relay.proxy(),
        Some(&ProxyRoute {
            host: "127.0.0.1".into(),
            port,
            authorization: Some("Basic dXNlcjpwQHNz".into())
        })
    );
    post(
        &format!("{}/v1/messages", relay.url),
        b"{}",
        &[
            ("Content-Type", "application/json"),
            ("Authorization", "Bearer subscription-secret"),
        ],
    );
    relay.close();
    let head = seen.join().unwrap();
    let lower = head.to_ascii_lowercase();
    assert!(head.starts_with("CONNECT api.example.invalid:443 "), "{head}");
    assert!(lower.contains("proxy-authorization: basic dxnlcjpwqhnz"), "{head}");
    // The subscription bearer travels only inside TLS to the upstream host, never to the proxy.
    assert!(
        !lower.contains("\r\nauthorization:") && !lower.contains("subscription-secret"),
        "{head}"
    );
    assert!(relay.outcome().failure.is_some());
}
