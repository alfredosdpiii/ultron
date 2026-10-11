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

/// Serve each scripted reply once, recording `{method, url, body}` per request.
fn mock_server(replies: Vec<String>) -> (u16, Arc<Mutex<Vec<Value>>>) {
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
                .unwrap_or_else(|| text_reply("(no more replies)"));
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
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

/// The generator's `canonical`.
fn canonical(lines: &[Value], tmp: &str) -> Canonical {
    let (mut responses, mut events, mut updates) = (Vec::new(), Vec::new(), Vec::new());
    for line in lines.iter().map(|l| scrub(l, tmp, "")) {
        let kind = line["type"].as_str().unwrap_or_default().to_string();
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

/// Responses Rust answers with the TS error shape because the subsystem behind them is not ported.
const UNPORTED: [(&str, &str); 2] = [("models-and-commands", "gc"), ("models-and-commands", "ex")];

fn run_case(case: &Value) {
    let name = case["name"].as_str().unwrap();
    let replies = case["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
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
    std::fs::write(
        agent_dir.join("models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
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
        for want in waits {
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
                    Duration::from_secs(60),
                )
                .unwrap_or_else(|e| panic!("{name}: step {step}: {e}"));
        }
    }
    let stderr = client.stderr();
    let (lines, code) = client.finish(Duration::from_secs(60));
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
        if UNPORTED.contains(&(name, id)) {
            assert_eq!(
                (&g["id"], &g["command"], &g["success"]),
                (&w["id"], &w["command"], &json!(false)),
                "{name} {id}"
            );
            assert!(
                g["error"]
                    .as_str()
                    .unwrap()
                    .contains("not implemented in the Rust build"),
                "{name} {id}: {g}"
            );
        } else if w["command"] == "parse" {
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

    let got_requests = scrub(&Value::Array(requests.lock().unwrap().clone()), &tmp_text, "");
    let want_requests = case["requests"].as_array().unwrap();
    assert_eq!(
        got_requests.as_array().unwrap().len(),
        want_requests.len(),
        "{name} request count"
    );
    for (i, (g, w)) in got_requests.as_array().unwrap().iter().zip(want_requests).enumerate() {
        for (field, value) in w["body"].as_object().unwrap() {
            // The native harness sizes the summary budget from its own reserve; Rust uses Pi's compaction settings.
            if name == "compact" && i == 2 && field == "max_tokens" {
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
    for file in &files {
        let journal = scrub(
            &common::normalize_journal(&std::fs::read_to_string(file).unwrap()),
            &tmp_text,
            "",
        );
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
