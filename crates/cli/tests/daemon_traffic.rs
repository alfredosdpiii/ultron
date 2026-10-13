//! Replays `crates/daemon/fixtures/traffic.json` (recorded by `generate-traffic.mjs` from the TypeScript client
//! runtime, server generation, coordinator and session worker): the Rust client runtime runs the same scripted
//! exchanges against the Rust server process (coordinator and worker processes included) through a recording proxy.
//!
//! Every frame is decoded and normalized the way the generator normalizes it; frames are then compared in order and,
//! when equal, re-encoded with the protocol encoder so the bytes match too. Two kinds of frames are compared by
//! projection, because what fills them is not ported: the `pi.models` snapshot (the TypeScript model registry's
//! catalog) and the `pi.transcript` updates (the format-4 lane runtime's ops, ids and `seq` numbers); for those the
//! decoded lane-watch event sequence and the final transcript are compared instead.

#[path = "daemon_common/mod.rs"]
mod common;

use chord_delta::Decoder;
use common::{HANG_GUARD, Home, assert_no_processes, processes_mentioning, provider, start_server, text_reply};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn cbor_json(value: &protocol::Value) -> Value {
    client::runtime::to_json(value)
}

/// The generator's `normalize`.
fn normalize(frames: &[(String, Value)], tmp: &str) -> Vec<(String, Value)> {
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let file_time = Regex::new(r"\d{4}-\d\d-\d\dT\d\d-\d\d-\d\d-\d{3}Z").unwrap();
    let port = Regex::new(r"127\.0\.0\.1:\d+").unwrap();
    let tmp_name = tmp.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    let mut ids: HashMap<String, String> = HashMap::new();
    fn walk(value: &Value, key: Option<&str>, text: &mut dyn FnMut(&str) -> String) -> Value {
        if key == Some("availableModels")
            && let Value::Array(items) = value
        {
            return json!({"count": items.len()});
        }
        match value {
            Value::String(s) => Value::String(text(s)),
            Value::Number(n) if n.as_f64().is_some_and(|f| f > 1e12) => json!("<time>"),
            Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, None, text)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), walk(v, Some(k), text)))
                    .collect::<Map<_, _>>(),
            ),
            other => other.clone(),
        }
    }
    let mut text = |s: &str| -> String {
        let s = s.replace(tmp, "<TMP>").replace(&tmp_name, "<TMPNAME>");
        let s = file_time.replace_all(&s, "<FILETIME>").into_owned();
        let s = port.replace_all(&s, "127.0.0.1:<PORT>").into_owned();
        uuid.replace_all(&s, |c: &regex::Captures<'_>| {
            let id = c[0].to_string();
            let next = format!("<id:{}>", ids.len());
            ids.entry(id).or_insert(next).clone()
        })
        .into_owned()
    };
    frames
        .iter()
        .map(|(dir, frame)| (dir.clone(), walk(frame, None, &mut text)))
        .collect()
}

/// Counts of proxy pump threads started and finished.
type Pumps = Arc<(Mutex<(usize, usize)>, Condvar)>;

/// Waits until every pump the proxy started has finished: each connection's frames are then all recorded, both
/// ways (hang guard [`HANG_GUARD`]).
fn wait_drained(pumps: &Pumps) {
    let (counts, changed) = &**pumps;
    let counts = counts.lock().unwrap();
    let (counts, _) = changed
        .wait_timeout_while(counts, HANG_GUARD, |(started, finished)| {
            *started == 0 || finished < started
        })
        .unwrap();
    assert!(
        counts.0 > 0 && counts.1 == counts.0,
        "proxy connections never ended: {counts:?}"
    );
}

/// Waits until no session worker of this test is running: the idle worker retired, as between two separate
/// `ultron` runs (the generator waits 1.5 s for the same; here the condition itself, hang guard [`HANG_GUARD`]).
fn wait_workers_retired(root: &Path) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let workers: Vec<u32> = processes_mentioning(root)
            .into_iter()
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains("\0session-worker\0"))
            })
            .collect();
        if workers.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "session workers never retired: {workers:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Decoded frames in arrival order, each with the direction it travelled.
type Frames = Arc<Mutex<Vec<(String, Value)>>>;

/// A recording proxy in front of `upstream`: decoded frames in arrival order.
fn proxy(path: &Path, upstream: &Path) -> (Frames, Pumps) {
    let frames = Arc::new(Mutex::new(Vec::new()));
    let pumps: Pumps = Arc::default();
    let listener = UnixListener::bind(path).unwrap();
    let (seen, upstream, counts) = (frames.clone(), upstream.to_path_buf(), pumps.clone());
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let server = UnixStream::connect(&upstream).unwrap();
            counts.0.lock().unwrap().0 += 2;
            for (dir, mut from, mut to) in [
                ("c2s", client.try_clone().unwrap(), server.try_clone().unwrap()),
                ("s2c", server.try_clone().unwrap(), client.try_clone().unwrap()),
            ] {
                let (seen, counts) = (seen.clone(), counts.clone());
                std::thread::spawn(move || {
                    let mut decoder = protocol::FrameDecoder::default();
                    let mut buffer = vec![0u8; 65536];
                    loop {
                        let n = match from.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        // Record before forwarding, so a frame is recorded before anything it causes.
                        let mut seen = seen.lock().unwrap();
                        for frame in decoder.push(&buffer[..n]).unwrap() {
                            let value = protocol::decode_cbor(&frame, &Default::default()).unwrap();
                            seen.push((dir.to_string(), cbor_json(&value)));
                        }
                        if to.write_all(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(std::net::Shutdown::Both);
                    counts.0.lock().unwrap().1 += 1;
                    counts.1.notify_all();
                });
            }
        }
    });
    (frames, pumps)
}

/// `got` reordered into `want`'s interleaving of directions, keeping each direction's own order. Frames of one
/// direction are ordered by the protocol; how the two directions interleave is timing (a pipelined request may reach
/// the proxy after the response to the one before it), so the comparison does not depend on it. Frames beyond
/// `want`'s pattern keep their order at the end, so a count mismatch still shows.
fn interleave_like(got: Vec<(String, Value)>, want: &[(String, Value)]) -> Vec<(String, Value)> {
    let mut queues: HashMap<String, std::collections::VecDeque<(String, Value)>> = HashMap::new();
    let mut order = Vec::new();
    for frame in got {
        if !queues.contains_key(&frame.0) {
            order.push(frame.0.clone());
        }
        queues.entry(frame.0.clone()).or_default().push_back(frame);
    }
    let mut out = Vec::new();
    for (dir, _) in want {
        if let Some(frame) = queues.get_mut(dir).and_then(|q| q.pop_front()) {
            out.push(frame);
        }
    }
    for dir in order {
        out.extend(queues.remove(&dir).into_iter().flatten());
    }
    out
}

fn is_transcript_update(frame: &Value, transcript: Option<&str>) -> bool {
    frame["type"] == "service_update" && transcript.is_some_and(|t| frame["subscriptionId"] == t)
}

/// The subscription ID the client chose for `service` in these frames.
fn subscription_for(frames: &[(String, Value)], service: &str) -> Option<String> {
    frames.iter().find_map(|(_, f)| {
        let args = &f["call"]["args"];
        (f["call"]["member"] == "subscribe" && args[1] == service).then(|| args[0].as_str().unwrap().to_string())
    })
}

/// The lane-watch events the transcript updates carry, and the final snapshot's comparable part.
fn transcript_projection(frames: &[(String, Value)], transcript: &str) -> (Vec<Value>, Value) {
    let mut decoder = Decoder::default();
    let mut state = Value::Null;
    let mut events = Vec::new();
    for (_, frame) in frames {
        let ops = if frame["type"] == "response" && frame["result"]["serviceId"] == "pi.transcript" {
            frame["result"]["instances"][0]["members"][0]["ops"].clone()
        } else if is_transcript_update(frame, Some(transcript)) {
            frame["update"]["ops"].clone()
        } else {
            continue;
        };
        let ops = decoder.decode(ops.as_array().unwrap()).unwrap();
        state = chord_delta::apply(state, ops).unwrap();
        if !state["event"].is_null() {
            let event = &state["event"];
            let mut summary = json!({"type": event["type"]});
            if let Some(role) = event["message"]["role"].as_str() {
                summary["role"] = json!(role);
            }
            if let Some(status) = event["status"].as_str() {
                summary["status"] = json!(status);
            }
            events.push(summary);
        }
    }
    let snapshot = &state["snapshot"];
    let messages: Vec<Value> = snapshot["transcript"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| json!({"role": e["message"]["role"], "content": e["message"]["content"]}))
        .collect();
    let final_part = json!({
        "messages": messages, "configuration": snapshot["configuration"],
        "messageCount": snapshot["stats"]["messageCount"], "operation": snapshot["operation"],
        "lastResult": snapshot["lastResult"]["status"], "usage": snapshot["stats"]["usage"],
    });
    (events, final_part)
}

/// Frames compared one to one: everything but the transcript updates, with the models snapshot reduced to its
/// configuration and the transcript snapshot to its member shape.
fn comparable(frames: &[(String, Value)]) -> Vec<(String, Value)> {
    let transcript = subscription_for(frames, "pi.transcript");
    let kept = frames
        .iter()
        .filter(|(_, f)| !is_transcript_update(f, transcript.as_deref()))
        .map(|(dir, f)| {
            let mut f = f.clone();
            if f["result"]["serviceId"] == "pi.transcript"
                && let Some(members) = f["result"]["instances"][0]["members"].as_array_mut()
            {
                // The snapshot's entries come from the session store, compared by `transcript_projection`.
                for member in members {
                    member["ops"] = json!("<transcript>");
                }
            }
            if f["result"]["serviceId"] == "pi.models"
                && let Some(members) = f["result"]["instances"][0]["members"].as_array_mut()
            {
                for member in members {
                    if member["kind"] == "state" {
                        let value = &member["ops"][0][1];
                        member["ops"] = json!({"configuration": value["configuration"], "refresh": value["refresh"]});
                    }
                }
            }
            (dir.clone(), f)
        })
        .collect();
    renumber_all(kept)
}

/// Number `<id:N>` placeholders again by first appearance among the compared frames only, so IDs that appear only
/// in projected frames do not shift the others.
fn renumber_all(frames: Vec<(String, Value)>) -> Vec<(String, Value)> {
    let mut ids = HashMap::new();
    frames
        .into_iter()
        .map(|(dir, f)| (dir, renumber(&f, &mut ids)))
        .collect()
}

fn renumber(value: &Value, ids: &mut HashMap<String, String>) -> Value {
    let placeholder = Regex::new(r"<id:\d+>").unwrap();
    match value {
        Value::String(s) => Value::String(
            placeholder
                .replace_all(s, |c: &regex::Captures<'_>| {
                    let next = format!("<n:{}>", ids.len());
                    ids.entry(c[0].to_string()).or_insert(next).clone()
                })
                .into_owned(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| renumber(v, ids)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), renumber(v, ids))).collect()),
        other => other.clone(),
    }
}

fn command_for(case: &Value, session_id: Option<String>) -> client::runtime::ClientCommand {
    client::runtime::ClientCommand {
        session_id: if case["resume"] == true { session_id } else { None },
        prompt: case["command"]["prompt"].as_str().map(str::to_string),
        no_session: case["noSession"] == true,
        ..Default::default()
    }
}

#[test]
fn cli_daemon_traffic_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../daemon/fixtures/traffic.json")).unwrap();
    for recorded in fixture.as_array().unwrap() {
        let name = recorded["name"].as_str().unwrap();
        let case = recorded;
        let replies = case["replies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| text_reply(r.as_str().unwrap()))
            .collect();
        let provider = provider(replies, Duration::ZERO);
        let home = Home::new(provider.port);
        let tmp = home.root.display().to_string();
        let mut server = start_server(&home, common::SERVER_ID);
        let mut session_id = None;
        if case.get("setup").is_some() {
            let setup = client::runtime::ClientCommand {
                prompt: case["setup"]["prompt"].as_str().map(str::to_string),
                ..Default::default()
            };
            match client::runtime::run_client(&setup, &server.route()).unwrap() {
                client::runtime::ClientResult::Prompted { session_id: id, .. } => session_id = Some(id),
                other => panic!("{name} setup: {other:?}"),
            }
            wait_workers_retired(&home.root);
        }
        let proxy_path = home.server_dir().join("proxy.sock");
        let (frames, pumps) = proxy(&proxy_path, &server.socket);
        let route = client::runtime::Route {
            server_id: server.server_id.clone(),
            path: proxy_path,
        };
        let result = client::runtime::run_client(&command_for(case, session_id), &route).unwrap();
        wait_drained(&pumps);
        let mut got = frames.lock().unwrap().clone();
        got.push(("result".into(), result.to_json()));
        server.stop();
        let got = normalize(&got, &tmp);
        let want: Vec<(String, Value)> = recorded["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["dir"].as_str().unwrap().to_string(), f["frame"].clone()))
            .collect();
        // Transcript updates are compared by projection, so their count may differ: leave them out before
        // aligning the interleaving.
        let strip = |frames: &[(String, Value)]| -> Vec<(String, Value)> {
            let transcript = subscription_for(frames, "pi.transcript");
            frames
                .iter()
                .filter(|(_, f)| !is_transcript_update(f, transcript.as_deref()))
                .cloned()
                .collect()
        };
        let want_core = strip(&want);
        let got_core = interleave_like(strip(&got), &want_core);
        let (got_cmp, want_cmp) = (comparable(&got_core), comparable(&want_core));
        for (i, (g, w)) in got_cmp.iter().zip(&want_cmp).enumerate() {
            assert_eq!(g, w, "{name} frame {i}");
            let encode = |v: &Value| protocol::encode_cbor(&client::runtime::to_cbor(v), &Default::default()).unwrap();
            assert_eq!(encode(&g.1), encode(&w.1), "{name} frame {i} bytes");
        }
        assert_eq!(got_cmp.len(), want_cmp.len(), "{name} frame count");
        if let (Some(gt), Some(wt)) = (
            subscription_for(&got, "pi.transcript"),
            subscription_for(&want, "pi.transcript"),
        ) {
            let (g_events, g_final) = transcript_projection(&got, &gt);
            let (w_events, w_final) = transcript_projection(&want, &wt);
            assert_eq!(g_events, w_events, "{name} transcript events");
            assert_eq!(g_final, w_final, "{name} final transcript");
        }
        drop(server);
        assert_no_processes(&home.root, Duration::from_secs(10));
    }
}
