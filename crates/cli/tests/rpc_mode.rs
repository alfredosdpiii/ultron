//! Replays `fixtures/rpc_mode.json` (recorded from `node packages/coding-agent/src/cli.ts --mode rpc` by
//! `fixtures/rpc_mode.mjs`): the Rust binary gets the same JSONL command steps against the same scripted Anthropic
//! SSE server, through the Rust `RpcClient`, and must answer with the same responses, emit the same events, send the
//! same provider requests, exit the same way, and leave the same session files: format-4 journals equal under
//! `normalize_journal` (fixtures/journal.mjs). The canonical form is the generator's `canonical`, ported below.

mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A scripted reply: SSE text, or `{status, contentType, body}` (the generator's non-streaming error answer).
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: String,
    body: String,
}

impl Reply {
    fn of(value: &Value) -> Reply {
        match value {
            Value::String(body) => Reply::sse(body),
            reply => Reply {
                status: reply["status"].as_u64().unwrap_or(200) as u16,
                content_type: reply["contentType"].as_str().unwrap_or("text/event-stream").to_string(),
                body: reply["body"].as_str().unwrap_or_default().to_string(),
            },
        }
    }

    fn sse(body: &str) -> Reply {
        Reply {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.to_string(),
        }
    }
}

/// Serve each scripted reply once, recording `{method, url, body}` per request.
fn mock_server(replies: Vec<Reply>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(method), Some(url)) = (parts.next(), parts.next()) else {
                continue;
            };
            let (method, url) = (method.to_string(), url.to_string());
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            seen.lock()
                .unwrap()
                .push(json!({"method": method, "url": url, "body": body}));
            let reply = replies
                .get(i)
                .cloned()
                .unwrap_or_else(|| Reply::sse(&text_reply("(no more replies)")));
            let head = format!(
                "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.status,
                if reply.status == 200 { "OK" } else { "Bad Request" },
                reply.content_type,
                reply.body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.body.as_bytes());
        }
    });
    (port, requests)
}

/// The generator's `textReply`, for requests past the scripted ones.
fn text_reply(text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn root() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
}

/// Replace every match of `digits-shaped` patterns: `127.0.0.1:<port>`, `YYYY-MM-DDTHH-MM-SS-mmmZ_`, the clock line.
fn scrub_text(text: &str, tmp: &str) -> String {
    let tmp_name = tmp.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    let mut text = text
        .replace(tmp, "<TMP>")
        .replace(&root(), "<root>")
        .replace(&tmp_name, "<TMPNAME>");
    let mut out = String::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    let digit = |c: char| c.is_ascii_digit();
    let shape = |at: usize, pattern: &str| -> bool {
        let p: Vec<char> = pattern.chars().collect();
        at + p.len() <= bytes.len()
            && p.iter()
                .zip(&bytes[at..])
                .all(|(p, c)| if *p == '9' { digit(*c) } else { p == c })
    };
    while i < bytes.len() {
        if shape(i, "127.0.0.1:") {
            out.push_str("127.0.0.1:<PORT>");
            i += "127.0.0.1:".len();
            while i < bytes.len() && digit(bytes[i]) {
                i += 1;
            }
            continue;
        }
        if shape(i, "9999-99-99T99-99-99-999Z_") {
            out.push_str("<TIME>_");
            i += 25;
            continue;
        }
        if shape(i, "[clock] 9999-99-99 99:99 UTC+99:99") || shape(i, "[clock] 9999-99-99 99:99 UTC-99:99") {
            out.push_str("[clock] <CLOCK>");
            i += 34;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    text = out;
    text
}

/// The generator's `scrub`.
fn scrub(value: &Value, tmp: &str, key: &str) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| scrub(v, tmp, key)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), scrub(v, tmp, k))).collect()),
        Value::Number(_) if matches!(key, "timestamp" | "startedAt" | "endedAt" | "createdAt" | "modifiedAt") => {
            json!(0)
        }
        Value::String(_) if matches!(key, "timestamp" | "labelTimestamp") => json!("<time>"),
        Value::String(s) => Value::String(scrub_text(s, tmp)),
        other => other.clone(),
    }
}

fn is_uuid_at(chars: &[char], at: usize) -> bool {
    let shape = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
    at + shape.len() <= chars.len()
        && shape.chars().zip(&chars[at..]).all(|(p, c)| {
            if p == '-' {
                *c == '-'
            } else {
                c.is_ascii_digit() || ('a'..='f').contains(c)
            }
        })
}

/// The generator's `numberIds`.
fn number_ids(value: &Value) -> Value {
    fn walk(value: &Value, key: &str, ids: &mut HashMap<String, String>) -> Value {
        let id_for = |raw: &str, ids: &mut HashMap<String, String>| {
            let n = ids.len();
            ids.entry(raw.to_string())
                .or_insert_with(|| format!("<id:{n}>"))
                .clone()
        };
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, key, ids)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), walk(v, k, ids));
                }
                Value::Object(out)
            }
            Value::String(s) => {
                let id_key = matches!(
                    key,
                    "id" | "sessionId"
                        | "parentId"
                        | "leafId"
                        | "entryId"
                        | "firstKeptEntryId"
                        | "parentSession"
                        | "fromId"
                        | "targetId"
                );
                if id_key && s.len() == 8 && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
                    return Value::String(id_for(s, ids));
                }
                let chars: Vec<char> = s.chars().collect();
                let mut out = String::new();
                let mut i = 0;
                while i < chars.len() {
                    if is_uuid_at(&chars, i) {
                        let raw: String = chars[i..i + 36].iter().collect();
                        out.push_str(&id_for(&raw, ids));
                        i += 36;
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                Value::String(out)
            }
            other => other.clone(),
        }
    }
    // Insertion order is first appearance; `ids.len()` numbers them.
    walk(value, "", &mut HashMap::new())
}

struct Canonical {
    responses: Value,
    events: Value,
    updates: Vec<Value>,
}

/// The events Rust streams as Pi's RPC mode does (`modes/rpc/rpc-mode.ts`) and `experimental/rpc-native.ts` does not.
const PI_ONLY_EVENTS: [&str; 3] = ["bash_execution_update", "session_info_changed", "extension_error"];

/// The generator's `canonical`, without [`PI_ONLY_EVENTS`].
fn canonical(lines: &[Value], tmp: &str) -> Canonical {
    let (mut responses, mut events, mut updates) = (Vec::new(), Vec::new(), Vec::new());
    for line in lines.iter().map(|l| scrub(l, tmp, "")) {
        let kind = line["type"].as_str().unwrap_or_default().to_string();
        // Pi's RPC events the native command (the recorded TypeScript) does not emit: rpc_pi_events.rs compares them
        // with Pi's RPC mode.
        if PI_ONLY_EVENTS.contains(&kind.as_str()) {
            continue;
        }
        if kind == "response" {
            responses.push(line);
        } else if kind == "message_update" {
            updates.push(line["assistantMessageEvent"].clone());
        } else if kind == "message_start" && line["message"]["role"] == "assistant" {
            let mut line = line;
            let message = line["message"].as_object_mut().unwrap();
            message.shift_remove("content");
            message.shift_remove("usage");
            message.shift_remove("responseId");
            events.push(line);
        } else if kind == "entry_appended" {
            let mut line = line;
            let entry = line["entry"].as_object_mut().unwrap();
            entry.shift_remove("seq");
            if entry.get("type") == Some(&json!("compaction")) {
                let keep = [
                    "id",
                    "parentId",
                    "type",
                    "summary",
                    "tokensBefore",
                    "details",
                    "timestamp",
                ];
                entry.retain(|k, _| keep.contains(&k.as_str()));
            }
            events.push(line);
        } else {
            events.push(line);
        }
    }
    Canonical {
        responses: number_ids(&Value::Array(responses)),
        events: number_ids(&Value::Array(events)),
        updates,
    }
}

/// `"$<id>.<path>"` values from the earlier response to command `<id>` (the generator's `resolveRefs`).
fn resolve_refs(command: &Value, lines: &[Value]) -> Value {
    let mut out = command.clone();
    for (_, value) in out.as_object_mut().unwrap().iter_mut() {
        let Some(reference) = value.as_str().and_then(|s| s.strip_prefix('$')) else {
            continue;
        };
        let mut parts = reference.split('.');
        let id = parts.next().unwrap();
        let mut found = lines
            .iter()
            .find(|l| l["type"] == "response" && l["id"] == id)
            .cloned()
            .unwrap_or(Value::Null);
        for part in parts {
            found = match part.parse::<usize>() {
                Ok(index) => found[index].clone(),
                Err(_) => found[part].clone(),
            };
        }
        *value = found;
    }
    out
}

/// The first path where two values differ, for readable failures.
fn first_diff(got: &Value, want: &Value, path: &str) -> Option<String> {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            for key in w.keys().chain(g.keys()) {
                let (gv, wv) = (g.get(key).unwrap_or(&Value::Null), w.get(key).unwrap_or(&Value::Null));
                if g.contains_key(key) != w.contains_key(key) {
                    return Some(format!("{path}.{key}: rust {:?} vs ts {:?}", g.get(key), w.get(key)));
                }
                if let Some(diff) = first_diff(gv, wv, &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Array(g), Value::Array(w)) => {
            for (i, (gv, wv)) in g.iter().zip(w).enumerate() {
                if let Some(diff) = first_diff(gv, wv, &format!("{path}[{i}]")) {
                    return Some(diff);
                }
            }
            (g.len() != w.len()).then(|| format!("{path}: rust len {} vs ts len {}", g.len(), w.len()))
        }
        _ => (got != want).then(|| format!("{path}: rust {got} vs ts {want}")),
    }
}

fn assert_same(got: &Value, want: &Value, what: &str) {
    if let Some(diff) = first_diff(got, want, "") {
        panic!("{what}: {diff}");
    }
}

fn assert_subsequence(want: &[Value], got: &[Value], name: &str) {
    let mut rest = got.iter();
    for (i, w) in want.iter().enumerate() {
        assert!(
            rest.any(|g| g == w),
            "{name}: TypeScript update {i} {w} is not in the Rust updates (in order): {got:#?}"
        );
    }
}

fn run_case(case: &Value) {
    run_case_with(case, true);
}

/// [`run_case`]; `journals`: the session files must equal TypeScript's (else they are only returned). Returns the
/// Rust journals, normalized.
fn run_case_with(case: &Value, journals: bool) -> Vec<Value> {
    let name = case["name"].as_str().unwrap();
    let replies = case["replies"].as_array().unwrap().iter().map(Reply::of).collect();
    let (port, requests) = mock_server(replies);
    let tmp = tempfile::Builder::new()
        .prefix("ultron-rpc-fixture-")
        .tempdir()
        .unwrap();
    let root_dir = tmp.path().canonicalize().unwrap();
    let tmp_text = root_dir.display().to_string();
    let (agent_dir, cwd) = (root_dir.join("agent"), root_dir.join("cwd"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    // The case's `provider`: more of the provider's models.json configuration (`modelOverrides`).
    let mut provider = json!({"baseUrl": format!("http://127.0.0.1:{port}")});
    for (key, value) in case["provider"].as_object().into_iter().flatten() {
        provider[key] = value.clone();
    }
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"anthropic": provider}}).to_string(),
    )
    .unwrap();
    let env = [
        // System directories only, as recorded: a user-installed `claude` CLI adds the claude-code provider.
        ("PATH", "/usr/local/bin:/usr/bin:/bin".into()),
        ("HOME", tmp_text.clone()),
        ("ULTRON_CODING_AGENT_DIR", agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR", root_dir.join("server").display().to_string()),
        ("ANTHROPIC_API_KEY", "test-key".into()),
        ("ULTRON_HINDSIGHT_URL", "off".into()),
        ("PI_OFFLINE", "1".into()),
        ("NO_COLOR", "1".into()),
        ("TZ", "UTC".into()),
    ];
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(cwd),
        env_clear: true,
        env: env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        args: case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect(),
        ..Default::default()
    };
    let mut client = RpcClient::start(&options).unwrap();
    for step in case["steps"].as_array().unwrap() {
        client.mark();
        let line = match &step["command"] {
            Value::String(raw) => raw.clone(),
            command => resolve_refs(command, &client.transcript).to_string(),
        };
        client.write_line(&line).unwrap();
        let waits: Vec<String> = match step.get("wait") {
            Some(Value::Array(waits)) => waits.iter().map(|w| w.as_str().unwrap().to_string()).collect(),
            _ => vec!["response".into()],
        };
        let id = step["command"].get("id").cloned();
        // The generator's `touch`: a file a scripted cell waits for, created once the step is done, or once its
        // `touchAfter` line was seen (a state read while the cell still runs).
        let mut touch = step["touch"].as_str();
        for want in waits {
            let touch_now = step["touchAfter"].as_str() == Some(want.as_str());
            let id = id.clone();
            client
                .wait_for(
                    &move |line| {
                        if want == "response" {
                            line["type"] == "response" && line.get("id") == id.as_ref()
                        } else {
                            line["type"] == want.as_str()
                        }
                    },
                    Duration::from_secs(120),
                )
                .unwrap_or_else(|e| panic!("{name}: step {step}: {e}"));
            if touch_now && let Some(file) = touch.take() {
                std::fs::write(options.cwd.as_ref().unwrap().join(file), "").unwrap();
            }
        }
        if let Some(file) = touch {
            std::fs::write(options.cwd.as_ref().unwrap().join(file), "").unwrap();
        }
    }
    let stderr = client.stderr();
    let (lines, code) = client.finish(Duration::from_secs(120));
    assert_eq!(
        code.map(i64::from),
        case["code"].as_i64(),
        "{name} exit code; stderr: {stderr}"
    );
    assert_eq!(stderr, case["stderr"].as_str().unwrap(), "{name} stderr");

    let got = canonical(&lines, &tmp_text);
    let want_responses = case["responses"].as_array().unwrap();
    let got_responses = got.responses.as_array().unwrap();
    assert_eq!(
        got_responses.len(),
        want_responses.len(),
        "{name} response count: {got_responses:#?}"
    );
    for (g, w) in got_responses.iter().zip(want_responses) {
        let id = w["id"].as_str().unwrap_or_default();
        if w["command"] == "parse" {
            // V8's JSON.parse messages are not reproduced; the response shape is.
            assert_eq!(g["success"], false);
            assert!(
                g["error"].as_str().unwrap().starts_with("Failed to parse command: "),
                "{g}"
            );
            assert!(g.get("id").is_none());
        } else {
            assert_same(g, w, &format!("{name} response {id}"));
        }
    }
    let (got_events, want_events) = (got.events.as_array().unwrap(), case["events"].as_array().unwrap());
    for (i, (g, w)) in got_events.iter().zip(want_events).enumerate() {
        assert_same(g, w, &format!("{name} event {i}"));
    }
    assert_eq!(
        got_events.len(),
        want_events.len(),
        "{name} event count: {got_events:#?}"
    );
    assert_subsequence(case["updates"].as_array().unwrap(), &got.updates, name);

    // Generated ids numbered on both sides (a trimmed result names its entry in later requests).
    let got_requests = number_ids(&scrub(&Value::Array(requests.lock().unwrap().clone()), &tmp_text, ""));
    let want_requests = number_ids(&case["requests"]);
    let want_requests = want_requests.as_array().unwrap();
    assert_eq!(
        got_requests.as_array().unwrap().len(),
        want_requests.len(),
        "{name} request count"
    );
    for (i, (g, w)) in got_requests.as_array().unwrap().iter().zip(want_requests).enumerate() {
        for (field, value) in w["body"].as_object().unwrap() {
            // The native harness sizes the summary budget from its own reserve; Rust uses Pi's compaction settings.
            let summary = w["body"]["system"]
                .to_string()
                .contains("context summarization assistant");
            if summary && field == "max_tokens" {
                continue;
            }
            assert_same(&g["body"][field], value, &format!("{name} request {i} body.{field}"));
        }
        assert_eq!(g["url"], w["url"], "{name} request {i} url");
    }

    // The session files: each Rust journal equals one TypeScript journal (files are matched by content, their names
    // carry times and ids).
    let mut wanted: Vec<&Value> = case["sessions"].as_array().unwrap().iter().collect();
    let files = common::session_files(&agent_dir.join("experimental").join("sessions"));
    assert_eq!(files.len(), wanted.len(), "{name} session files");
    let mut normalized = Vec::new();
    for file in &files {
        let journal = scrub(
            &common::normalize_journal(&std::fs::read_to_string(file).unwrap()),
            &tmp_text,
            "",
        );
        normalized.push(journal.clone());
        if !journals {
            continue;
        }
        let Some(position) = wanted.iter().position(|w| **w == journal) else {
            let closest = wanted
                .iter()
                .min_by_key(|w| {
                    let count = |v: &Value| v["transactions"].as_array().map_or(0, Vec::len) as i64;
                    (count(w) - count(&journal)).abs()
                })
                .copied()
                .unwrap_or(&Value::Null);
            let lines = |v: &Value| -> String {
                std::iter::once(v["header"].to_string())
                    .chain(v["transactions"].as_array().into_iter().flatten().map(Value::to_string))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "{name}: no TypeScript journal equals {}: {}\nrust:\n{}\nclosest typescript:\n{}",
                file.display(),
                first_diff(&journal, closest, "").unwrap_or_default(),
                lines(&journal),
                lines(closest)
            );
        };
        wanted.remove(position);
    }
    normalized
}

fn fixture() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/rpc_mode.json")).unwrap();
    fixture.as_array().unwrap().clone()
}

fn case(name: &str) -> Value {
    fixture()
        .into_iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}"))
}

/// A case of `fixtures/rpc_auto_compaction.json` (recorded by `fixtures/rpc_auto_compaction.mjs`).
fn auto_case(name: &str) -> Value {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/rpc_auto_compaction.json")).unwrap();
    fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no case {name}"))
}

/// A response over the threshold compacts before the run ends: `compaction_start`/`compaction_end` (threshold), the
/// compaction entry, the summary request and its journal inside the run's operation, as TypeScript.
#[test]
fn cli_rpc_auto_compaction_threshold_matches_typescript() {
    run_case(&auto_case("auto-compaction-threshold"));
}

/// A tool round whose response crossed the threshold compacts before the next request, which carries the summary.
#[test]
fn cli_rpc_auto_compaction_mid_run_matches_typescript() {
    run_case(&auto_case("auto-compaction-mid-run"));
}

/// A context-overflow error commits, compacts (overflow) and the request is retried once; the run completes.
#[test]
fn cli_rpc_auto_compaction_overflow_retries_and_matches_typescript() {
    run_case(&auto_case("auto-compaction-overflow"));
}

/// A summary request that fails ends the compaction (`compaction_end` with its error) and fails the run
/// (`summarization_failed`); the next run's first checkpoint compacts again.
#[test]
fn cli_rpc_auto_compaction_failed_summary_fails_the_run_and_matches_typescript() {
    run_case(&auto_case("auto-compaction-summary-fails"));
}

/// The same for the compaction before a request (after a tool round): the run fails, nothing more is asked.
#[test]
fn cli_rpc_auto_compaction_failed_mid_run_summary_fails_the_run_and_matches_typescript() {
    run_case(&auto_case("auto-compaction-mid-run-summary-fails"));
}

/// The same for an overflow compaction: the overflowed answer stays, the run fails with the summary's error.
#[test]
fn cli_rpc_auto_compaction_failed_overflow_summary_fails_the_run_and_matches_typescript() {
    run_case(&auto_case("auto-compaction-overflow-summary-fails"));
}

/// A second overflow after the recovery fails the run with it.
#[test]
fn cli_rpc_auto_compaction_second_overflow_fails_and_matches_typescript() {
    run_case(&auto_case("auto-compaction-overflow-twice"));
}

/// A threshold compaction the `before_compaction` hook declines: ContextControl trims the long cell output the
/// model answered after instead (a `context_edit` placed when the run ends), and nothing is summarized.
///
/// Events, responses and requests equal TypeScript's. The journal does not: the Rust lane keeps a context entry
/// written during a run as `ultron.context.pending` and places it when the run ends, where TypeScript's lane keeps
/// it as a pending inbox `write` placed in the run's terminal transaction; the placed entry is the same.
#[test]
fn cli_rpc_auto_compaction_trimmed_instead_matches_typescript() {
    let case = auto_case("auto-compaction-trimmed");
    let journals = run_case_with(&case, false);
    let records = |journal: &Value| -> Vec<Value> {
        journal["transactions"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|t| t.as_array().cloned().unwrap_or_default())
            .collect()
    };
    let edits = |journal: &Value| -> Vec<Value> {
        records(journal)
            .into_iter()
            .filter(|r| r["kind"] == "entry" && r["customType"] == "context_edit")
            .map(|r| r["data"].clone())
            .collect()
    };
    let (rust, ts) = (&journals[0], &case["sessions"][0]);
    let trimmed = edits(rust);
    assert_eq!(trimmed.len(), 1, "one trim edit");
    assert_eq!(trimmed, edits(ts), "the trim edit equals TypeScript's");
    assert_eq!(trimmed[0]["source"], "trim");
    assert!(
        trimmed[0]["edits"][0]["replacement"]["content"]
            .as_str()
            .unwrap()
            .contains("characters trimmed before compaction"),
        "{trimmed:?}"
    );
    assert!(
        !records(rust)
            .iter()
            .any(|r| r["kind"] == "entry" && r["type"] == "compaction"),
        "nothing was summarized"
    );
    let results: Vec<Value> = records(rust)
        .into_iter()
        .filter(|r| r["namespace"] == "pi.result")
        .map(|r| r["value"]["status"].clone())
        .collect();
    assert_eq!(results, [json!("completed")]);
    let preparations: Vec<String> = records(rust)
        .into_iter()
        .filter(|r| r["namespace"] == "pi.op.preparation")
        .map(|r| r["op"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        preparations,
        ["set", "delete"],
        "the declined preparation is removed with the run"
    );
}

/// The same before a request in the middle of a run: the trim waits for the next boundary, the request carries the
/// trimmed result, and the run goes on without a summary.
#[test]
fn cli_rpc_auto_compaction_trimmed_mid_run_matches_typescript() {
    let case = auto_case("auto-compaction-trimmed-mid-run");
    let journals = run_case_with(&case, false);
    let records: Vec<Value> = journals[0]["transactions"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| t.as_array().cloned().unwrap_or_default())
        .collect();
    let edits: Vec<&Value> = records
        .iter()
        .filter(|r| r["kind"] == "entry" && r["customType"] == "context_edit")
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["data"]["source"], "trim");
    assert!(
        !records
            .iter()
            .any(|r| r["kind"] == "entry" && r["type"] == "compaction")
    );
    let results: Vec<&Value> = records
        .iter()
        .filter(|r| r["namespace"] == "pi.result")
        .map(|r| &r["value"]["status"])
        .collect();
    assert_eq!(results, [&json!("completed")]);
}

/// `set_auto_compaction false`: no threshold compaction.
#[test]
fn cli_rpc_auto_compaction_off_matches_typescript() {
    run_case(&auto_case("auto-compaction-off"));
}

/// An Anthropic SSE answer that reports `input` prompt tokens: text, or one `rlm` call.
fn reply_using(input: u64, text: Option<&str>, call: Option<(&str, &str)>) -> Reply {
    let block = match call {
        Some((id, _)) => json!({"type": "tool_use", "id": id, "name": "rlm", "input": {}}),
        None => json!({"type": "text", "text": ""}),
    };
    let delta = match call {
        Some((_, code)) => json!({"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}),
        None => json!({"type": "text_delta", "text": text.unwrap_or_default()}),
    };
    let stop = if call.is_some() { "tool_use" } else { "end_turn" };
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": block}),
        json!({"type": "content_block_delta", "index": 0, "delta": delta}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ];
    Reply::sse(
        &events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect::<String>(),
    )
}

/// The Rust binary in RPC mode against a scripted provider in a private temp home (claude-sonnet-4-5).
fn start_client(replies: Vec<Reply>) -> (RpcClient, Arc<Mutex<Vec<Value>>>, tempfile::TempDir) {
    let (port, requests) = mock_server(replies);
    let tmp = tempfile::tempdir().unwrap();
    let root_dir = tmp.path().canonicalize().unwrap();
    let (agent_dir, cwd) = (root_dir.join("agent"), root_dir.join("cwd"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}).to_string(),
    )
    .unwrap();
    let env = [
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("HOME", root_dir.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR", agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR", root_dir.join("server").display().to_string()),
        ("ANTHROPIC_API_KEY", "test-key".into()),
        ("ULTRON_HINDSIGHT_URL", "off".into()),
        ("PI_OFFLINE", "1".into()),
        ("NO_COLOR", "1".into()),
        ("TZ", "UTC".into()),
    ];
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(cwd),
        env_clear: true,
        env: env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        args: vec!["--model".into(), "anthropic/claude-sonnet-4-5".into()],
        ..Default::default()
    };
    (RpcClient::start(&options).unwrap(), requests, tmp)
}

/// The `compaction_start`/`compaction_end` events of a run, as `type reason`.
fn compaction_events(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| matches!(e["type"].as_str(), Some("compaction_start" | "compaction_end")))
        .map(|e| format!("{} {}", e["type"].as_str().unwrap(), e["reason"].as_str().unwrap()))
        .collect()
}

fn is_summary(request: &Value) -> bool {
    request["body"]["system"]
        .to_string()
        .contains("context summarization assistant")
}

/// A session compacts again after an earlier compaction: each run whose answer crosses the threshold compacts once,
/// at its end. The run after a compaction does not compact again from the kept answer's pre-compaction usage (the
/// TypeScript harness does, see fixtures/rpc_auto_compaction.mjs): the estimate after a compaction ignores usage
/// reported before it (`estimateProjectedContextTokens`, AgentSession's stale-usage guard).
#[test]
fn cli_rpc_auto_compaction_compacts_again_after_an_earlier_compaction() {
    let (mut client, requests, _tmp) = start_client(vec![
        reply_using(990_000, Some("one"), None),
        reply_using(10, Some("## Goal\nFirst summary."), None),
        reply_using(10, Some("small"), None),
        reply_using(990_000, Some("three"), None),
        reply_using(10, Some("## Goal\nSecond summary."), None),
    ]);
    let hang = Duration::from_secs(120);
    let threshold = ["compaction_start threshold", "compaction_end threshold"];
    let first = client.prompt_and_wait("first", hang).unwrap();
    assert_eq!(compaction_events(&first), threshold);
    let second = client.prompt_and_wait("second", hang).unwrap();
    assert!(compaction_events(&second).is_empty(), "{second:#?}");
    let third = client.prompt_and_wait("third", hang).unwrap();
    assert_eq!(compaction_events(&third), threshold);
    let (_, code) = client.finish(hang);
    assert_eq!(code, Some(0));
    let requests = requests.lock().unwrap().clone();
    let summaries: Vec<usize> = (0..requests.len()).filter(|i| is_summary(&requests[*i])).collect();
    assert_eq!(summaries, [1, 4], "{requests:#?}");
    // The second summary updates the first one.
    assert!(requests[4]["body"].to_string().contains("First summary."));
}

/// With automatic compaction off (`set_auto_compaction false`) an overflow is not recovered either (AgentSession's
/// `_checkCompaction`; the TypeScript harness recovers regardless): the run fails with the overflow error.
#[test]
fn cli_rpc_auto_compaction_off_leaves_an_overflow_to_fail_the_run() {
    let overflow = Reply {
        status: 400,
        content_type: "application/json".into(),
        body: json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "prompt is too long: 1213462 tokens > 1000000 maximum"}})
        .to_string(),
    };
    let (mut client, requests, _tmp) = start_client(vec![overflow, reply_using(10, Some("unused"), None)]);
    let hang = Duration::from_secs(120);
    let off = client
        .send(json!({"type": "set_auto_compaction", "enabled": false}))
        .unwrap();
    assert_eq!(off["success"], true);
    let events = client.prompt_and_wait("first", hang).unwrap();
    assert!(compaction_events(&events).is_empty(), "{events:#?}");
    let end = events.iter().rfind(|e| e["type"] == "agent_end").unwrap();
    let last = end["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["stopReason"], "error");
    assert!(last["errorMessage"].as_str().unwrap().contains("prompt is too long"));
    let (_, code) = client.finish(hang);
    assert_eq!(code, Some(0));
    assert_eq!(requests.lock().unwrap().len(), 1, "no summary, no retry");
}

/// A note made in an earlier run survives a compaction in the middle of a later run: the request after it carries
/// the summary, then the notes kept across compaction (ContextControl `kept_across_compaction`, shown after the
/// summary of the compaction the run made), never the note's own entry from before the cut.
#[test]
fn cli_rpc_auto_compaction_mid_run_keeps_notes_across_it() {
    let (mut client, requests, _tmp) = start_client(vec![
        reply_using(
            10,
            None,
            Some(("toolu_note", "await ctx.note('the code word is tangerine')")),
        ),
        reply_using(10, Some("Noted."), None),
        reply_using(990_000, None, Some(("toolu_sum", "1 + 1"))),
        reply_using(10, Some("## Goal\nMid-run summary."), None),
        reply_using(10, Some("Done."), None),
    ]);
    let hang = Duration::from_secs(120);
    client.prompt_and_wait("remember the code word", hang).unwrap();
    let events = client.prompt_and_wait("go", hang).unwrap();
    assert_eq!(
        compaction_events(&events),
        ["compaction_start threshold", "compaction_end threshold"]
    );
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("Done."));
    let (_, code) = client.finish(hang);
    assert_eq!(code, Some(0));
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    assert!(is_summary(&requests[3]));
    let after = &requests[4]["body"]["messages"];
    let texts: Vec<String> = after
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].to_string())
        .collect();
    let summary = texts
        .iter()
        .position(|t| t.contains("Mid-run summary."))
        .expect("the summary");
    let kept = texts
        .iter()
        .position(|t| t.contains("kept across compaction") && t.contains("tangerine"))
        .unwrap_or_else(|| panic!("the kept note: {texts:#?}"));
    assert_eq!(kept, summary + 1, "{texts:#?}");
}

#[test]
fn cli_rpc_auto_compaction_fixture_covers_every_case() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/rpc_auto_compaction.json")).unwrap();
    let names: Vec<&str> = fixture
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "auto-compaction-threshold",
            "auto-compaction-mid-run",
            "auto-compaction-overflow",
            "auto-compaction-summary-fails",
            "auto-compaction-mid-run-summary-fails",
            "auto-compaction-overflow-summary-fails",
            "auto-compaction-overflow-twice",
            "auto-compaction-trimmed",
            "auto-compaction-trimmed-mid-run",
            "auto-compaction-off",
        ]
    );
}

#[test]
fn cli_rpc_prompt_text_matches_typescript() {
    run_case(&case("prompt-text"));
}

#[test]
fn cli_rpc_rlm_cells_and_history_match_typescript() {
    run_case(&case("prompt-rlm-twice"));
}

#[test]
fn cli_rpc_rlm_error_no_session_matches_typescript() {
    run_case(&case("prompt-rlm-error"));
}

#[test]
fn cli_rpc_settings_and_errors_match_typescript() {
    run_case(&case("settings"));
}

#[test]
fn cli_rpc_bash_matches_typescript() {
    run_case(&case("bash"));
}

#[test]
fn cli_rpc_clone_and_new_session_match_typescript() {
    run_case(&case("sessions"));
}

#[test]
fn cli_rpc_names_and_queues_match_typescript() {
    run_case(&case("names-and-queues"));
}

#[test]
fn cli_rpc_steer_and_follow_up_while_running_match_typescript() {
    run_case(&case("steer-follow-up-while-running"));
}

#[test]
fn cli_rpc_abort_running_cell_matches_typescript() {
    run_case(&case("abort-running-cell"));
}

#[test]
fn cli_rpc_compact_matches_typescript() {
    run_case(&case("compact"));
}

#[test]
fn cli_rpc_models_matches_typescript() {
    run_case(&case("models-and-commands"));
}

#[test]
fn cli_rpc_fork_and_switch_match_typescript() {
    run_case(&case("fork-and-switch"));
}

#[test]
fn cli_rpc_fixture_covers_every_case() {
    let names: Vec<String> = fixture()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "prompt-text",
            "prompt-rlm-twice",
            "prompt-rlm-error",
            "settings",
            "bash",
            "sessions",
            "names-and-queues",
            "steer-follow-up-while-running",
            "abort-running-cell",
            "compact",
            "models-and-commands",
            "fork-and-switch",
        ]
    );
}
