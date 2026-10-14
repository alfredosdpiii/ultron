//! A47-A50 at the host level, ported from packages/coding-agent/test/ultron-a47-a50-bounded-inference.test.ts:
//! the session's frames (`inference::FrameSession`) over the Rust typed-agent host (`delegation::agents::Agents`
//! with its task journal and usage ledger) through the production adapter `session_core::frames` (frame tasks,
//! their runner, usage reservations), with `rlm.infer` / `rlm.map` / `rlm.frames` sent to `inference::Frames` as a
//! kernel's requests would be. Where TS fakes a harness lane per frame, a scripted chat-completions transport
//! answers each frame request (by its first user message), records it, and can hold it open, fail it with HTTP 500,
//! or report usage with cache reads. Frames are `rlm-frame@1` tasks: journal, admission, cancellation and usage all
//! apply. The end-to-end cases through the binary are in a47_a50_bounded_inference.rs.

use ai::{Body, HttpRequest, HttpResponse, Model, Transport};
use delegation::agents::{Agents, AgentsOptions};
use delegation::task_store::MemoryStore;
use delegation::usage::UsageLedger;
use inference::{FrameSession, Frames, Inference, MemoryTraceStore, ModelSettings};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use session_core::frames::{StackFrameTasks, frame_runner};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The scripted frame model (TS `MODEL`).
fn model() -> Model {
    serde_json::from_value(json!({
        "id": "frame-model", "name": "Frame model", "api": "openai-completions", "provider": "fake",
        "baseUrl": "http://fake.invalid/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 32000, "maxTokens": 8192,
    }))
    .unwrap()
}

#[derive(Default)]
struct Reply {
    text: String,
    /// (input, output, cache read); default input = request bytes / 4, output = 8.
    usage: Option<(u64, u64, u64)>,
    /// HTTP 500 with no usage.
    fail: bool,
    /// Held until the flag is set (or the request is dropped).
    wait: Option<Arc<AtomicBool>>,
}

fn text(text: &str) -> Reply {
    Reply {
        text: text.into(),
        ..Reply::default()
    }
}

#[derive(Clone, Debug)]
struct Call {
    body: Value,
}

impl Call {
    fn messages(&self, role: &str) -> Vec<String> {
        self.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == role)
            .map(|m| match &m["content"] {
                Value::String(text) => text.clone(),
                other => other
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
            })
            .collect()
    }
    fn system(&self) -> String {
        self.messages("system").join("\n")
    }
    fn message(&self) -> String {
        self.messages("user").last().cloned().unwrap_or_default()
    }
    fn first(&self) -> String {
        self.messages("user").first().cloned().unwrap_or_default()
    }
    fn max_tokens(&self) -> u64 {
        self.body["max_completion_tokens"]
            .as_u64()
            .or(self.body["max_tokens"].as_u64())
            .unwrap()
    }
}

type Script = dyn Fn(&Call) -> Reply + Send + Sync;

/// The provider frames call: answers each request by `script`, records it, and records held requests dropped
/// unanswered (their frame was cancelled).
struct Scripted {
    script: Box<Script>,
    calls: Mutex<Vec<Call>>,
    dropped: Arc<Mutex<Vec<String>>>,
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// Records its request as dropped unless disarmed (the reply was sent).
struct Held {
    task: String,
    dropped: Arc<Mutex<Vec<String>>>,
    armed: bool,
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.armed {
            self.dropped.lock().unwrap().push(self.task.clone());
        }
    }
}

impl Transport for Scripted {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let call = Call {
            body: serde_json::from_str(&request.body).unwrap(),
        };
        let reply = (self.script)(&call);
        let bytes = request.body.len() as u64;
        let held = Held {
            task: call.first(),
            dropped: self.dropped.clone(),
            armed: reply.wait.is_some(),
        };
        self.calls.lock().unwrap().push(call);
        Box::pin(async move {
            // The whole guard moves into the request (not only its flag): dropping the request drops it.
            let mut held = held;
            if let Some(flag) = &reply.wait {
                // Pending until released; the frame's request loop polls it again every few milliseconds.
                std::future::poll_fn(|_| match flag.load(Ordering::SeqCst) {
                    true => std::task::Poll::Ready(()),
                    false => std::task::Poll::Pending,
                })
                .await;
            }
            held.armed = false;
            let headers: ai::http::HeaderMap = Default::default();
            if reply.fail {
                let body = br#"{"error":{"message":"HTTP 500"}}"#.to_vec();
                return Ok(HttpResponse {
                    status: 500,
                    headers,
                    body: Box::new(Chunks(vec![body].into_iter())),
                });
            }
            let (input, output, cached) = reply.usage.unwrap_or((bytes.div_ceil(4), 8, 0));
            let events = [
                json!({"id": "c", "object": "chat.completion.chunk", "model": "frame-model",
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply.text}, "finish_reason": null}]}),
                json!({"id": "c", "object": "chat.completion.chunk", "model": "frame-model",
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": input + cached, "completion_tokens": output,
                        "prompt_tokens_details": {"cached_tokens": cached}}}),
            ];
            let mut chunks: Vec<Vec<u8>> = events.iter().map(|e| format!("data: {e}\n\n").into_bytes()).collect();
            chunks.push(b"data: [DONE]\n\n".to_vec());
            let mut headers = headers;
            headers.insert("content-type".into(), "text/event-stream".into());
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

/// TS `setup(script)`: the host, its journal and usage ledger, and the frames' traces.
struct Setup {
    agents: Agents,
    usage: UsageLedger,
    frames: Arc<FrameSession>,
    traces: Arc<MemoryTraceStore>,
    transport: Arc<Scripted>,
    context: tempfile::TempDir,
}

fn setup(script: impl Fn(&Call) -> Reply + Send + Sync + 'static) -> Setup {
    let usage = UsageLedger::in_memory(Default::default());
    let traces = Arc::new(MemoryTraceStore::default());
    let frames = FrameSession::new(traces.clone());
    let mut options = AgentsOptions::new(Box::new(MemoryStore::default()));
    options.usage = Some(usage.clone());
    options.frames = Some(frame_runner(frames.clone()));
    let (agents, _ends) = Agents::new(options);
    frames.set_tasks(Arc::new(StackFrameTasks {
        agents: agents.clone(),
        usage: usage.clone(),
        stack: Weak::new(),
    }));
    Setup {
        agents,
        usage,
        frames,
        traces,
        transport: Arc::new(Scripted {
            script: Box::new(script),
            calls: Mutex::default(),
            dropped: Arc::default(),
        }),
        context: tempfile::tempdir().unwrap(),
    }
}

impl Setup {
    /// The request handler of `lane`, as the session builds it for each cell.
    fn handler(&self, lane: &str) -> Frames {
        let refuse: Arc<dyn HostHandler> =
            Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| Err(format!("not wired: {kind}")));
        let inference = Inference::new(
            refuse,
            model(),
            Some("test-key".into()),
            self.transport.clone(),
            self.context.path(),
        )
        .with_session(self.frames.clone(), lane);
        Frames::new(inference, Default::default(), ModelSettings::default)
    }

    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.call_with(kind, payload, "main", &Signal::new())
    }

    fn call_with(&self, kind: &str, payload: Value, lane: &str, signal: &Signal) -> Result<Value, String> {
        self.handler(lane)
            .handle(kind, payload.as_object().unwrap().clone(), signal)
    }

    fn calls(&self) -> Vec<Call> {
        self.transport.calls.lock().unwrap().clone()
    }

    /// The journal's `rlm-frame@1` tasks: `(state, lane)`.
    fn frame_tasks(&self) -> Vec<Value> {
        let status = self.agents.handle("agents.status", json!({}), "main", None).unwrap();
        status["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|task| task["definition"] == "rlm-frame@1")
            .cloned()
            .collect()
    }

    fn frame_states(&self) -> Vec<String> {
        self.frame_tasks()
            .iter()
            .map(|task| task["state"].as_str().unwrap().to_string())
            .collect()
    }
}

/// Wait until `ready` holds (a hang guard, never a timing assumption).
fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let since = std::time::Instant::now();
    while !ready() {
        assert!(
            since.elapsed() < Duration::from_secs(120),
            "timed out waiting for {what}"
        );
        std::thread::yield_now();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn view_of(message: &str) -> &str {
    let Some(start) = message.find("--- view 1: ") else {
        return "";
    };
    let body = &message[start..];
    let body = &body[body.find("---\n").map_or(0, |i| i + 4)..];
    &body[..body.find("\n--- end of view 1 ---").unwrap_or(body.len())]
}

fn literal(text: &str) -> Value {
    json!({"kind": "text", "label": "literal", "text": text})
}

#[test]
fn acceptance_a47_host_the_frame_is_a_journaled_task_seeing_only_its_task_and_views_with_no_tools() {
    let s = setup(|call| {
        let errors = view_of(&call.message())
            .split('\n')
            .filter(|l| l.contains("ERROR"))
            .count();
        text(&errors.to_string())
    });
    let body = "ok\nERROR one\nok\nERROR two\n";
    let reply = s
        .call(
            "rlm.infer",
            json!({"task": "Count the ERROR lines.", "context": [{"kind": "text", "label": "app.log[0:26]", "text": body}],
                "contract": {"type": "integer"}}),
        )
        .unwrap();
    assert_eq!(reply["status"], "complete");
    assert_eq!(reply["value"], 2);
    let calls = s.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].system().contains("inference frame"));
    assert!(calls[0].system().contains("You have no tools."));
    assert!(calls[0].message().contains("Count the ERROR lines."));
    assert!(calls[0].message().contains(body));
    assert!(calls[0].body.get("tools").is_none());
    // The frame is an ordinary journaled task, and its observation names it.
    let tasks = s.frame_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["state"], "completed");
    assert_eq!(reply["task_id"], tasks[0]["id"]);
    assert_eq!(reply["spent"]["calls"], 1);
    assert!(reply["spent"]["tokens"].as_u64().unwrap() > 0);
}

#[test]
fn acceptance_a49_host_a_map_of_8_frames_under_calls_6_runs_6_and_marks_2_incomplete_in_order() {
    let s = setup(|call| {
        let message = call.message();
        let at = message.find("item ").unwrap() + 5;
        text(
            &message[at..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>(),
        )
    });
    let frames: Vec<Value> = (0..8)
        .map(|i| json!({"task": format!("Echo item {i}."), "context": []}))
        .collect();
    let reply = s
        .call(
            "rlm.map",
            json!({"frames": frames, "contract": {"type": "string"}, "budget": {"calls": 6}}),
        )
        .unwrap();
    assert_eq!(s.calls().len(), 6);
    let results = reply["results"].as_array().unwrap();
    let statuses: Vec<&str> = results.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, [["complete"; 6].as_slice(), &["incomplete"; 2]].concat());
    let values: Vec<Value> = results[..6].iter().map(|r| r["value"].clone()).collect();
    assert_eq!(values, ["0", "1", "2", "3", "4", "5"].map(|v| json!(v)));
    assert_eq!(results[6]["reason"], "budget_exhausted");
    assert_eq!(results[6]["remaining"]["calls"], 0);
    assert_eq!(reply["budget"]["spent"]["calls"], 6);
    // Frames that never got a call are refused before admission: only the six that ran are tasks.
    assert_eq!(s.frame_states(), ["completed"; 6]);
}

#[test]
fn acceptance_a49_host_concurrent_siblings_hold_tranches_that_never_oversubscribe_the_token_pool() {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let s = setup(move |_| Reply {
        usage: Some((100, 8, 0)),
        wait: Some(gate.clone()),
        ..text("1")
    });
    let pool = 40_000u64;
    let frames: Vec<Value> = (0..8)
        .map(|i| json!({"task": format!("Frame {i}."), "context": [literal(&"x".repeat(200))]}))
        .collect();
    let payload =
        json!({"frames": frames, "contract": {"type": "integer"}, "budget": {"tokens": pool}, "concurrency": 8});
    let reply = std::thread::scope(|scope| {
        let pending = scope.spawn(|| s.call("rlm.map", payload));
        wait_until("eight frames in flight", || s.calls().len() == 8);
        // Every in-flight request is capped at its tranche: at most a quarter of what remained, at most 16k.
        let caps: Vec<u64> = s.calls().iter().map(Call::max_tokens).collect();
        release.store(true, Ordering::SeqCst);
        (caps, pending.join().unwrap().unwrap())
    });
    let (caps, reply) = reply;
    for cap in &caps {
        assert!(*cap <= 16_384u64.min(pool / 4), "{caps:?}");
    }
    assert!(caps.iter().sum::<u64>() < pool, "{caps:?}");
    let results = reply["results"].as_array().unwrap();
    assert!(results.iter().all(|r| r["status"] == "complete"));
    assert_eq!(reply["budget"]["spent"]["tokens"], 8 * 108);
}

#[test]
fn acceptance_a49_host_when_the_pool_cannot_cover_a_request_the_frame_is_incomplete_and_nothing_is_sent() {
    let s = setup(|_| Reply {
        usage: Some((50, 8, 0)),
        ..text("1")
    });
    let reply = s
        .call(
            "rlm.map",
            json!({"frames": [{"task": "Small.", "context": []}, {"task": "Huge.", "context": [literal(&"y".repeat(4000))]}],
                "contract": {"type": "integer"}, "budget": {"tokens": 700}, "concurrency": 1}),
        )
        .unwrap();
    let results = reply["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "complete");
    assert_eq!(
        (&results[1]["status"], &results[1]["reason"]),
        (&json!("incomplete"), &json!("budget_exhausted"))
    );
    assert_eq!(s.calls().len(), 1);
    assert!(reply["budget"]["spent"]["tokens"].as_u64().unwrap() <= 700);
    // The second frame ran as a task that could not afford its first request: it failed, unsent.
    assert_eq!(s.frame_states(), ["completed", "failed"]);
}

#[test]
fn acceptance_a49_host_a_provider_failure_refunds_its_tranche_cache_reads_are_discounted_and_the_ledger_settles() {
    let s = setup(|call| {
        if call.message().contains("fails") {
            Reply {
                fail: true,
                ..Reply::default()
            }
        } else {
            Reply {
                usage: Some((100, 10, 1_000)),
                ..text("7")
            }
        }
    });
    let reply = s
        .call(
            "rlm.map",
            json!({"frames": [{"task": "This one fails.", "context": []}, {"task": "This one works.", "context": []}],
                "contract": {"type": "integer"}, "budget": {"tokens": 50000}}),
        )
        .unwrap();
    let results = reply["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "error");
    assert_eq!(results[0]["spent"], json!({"calls": 1, "tokens": 0}));
    assert_eq!(results[1]["status"], "complete");
    assert_eq!(results[1]["value"], 7);
    assert_eq!(results[1]["spent"], json!({"calls": 1, "tokens": 210}));
    // Nothing is left held: the whole pool minus what was charged remains.
    assert_eq!(reply["budget"]["spent"], json!({"calls": 2, "tokens": 210}));
    let status = s.usage.status(None).unwrap();
    assert_eq!(status["activeReservations"], 0);
    assert_eq!(status["usage"]["modelCalls"], 2);
    assert_eq!(s.frame_states(), ["failed", "completed"]);
}

#[test]
fn acceptance_a49_host_a_frame_without_depth_cannot_start_nested_frames() {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    // Only the outer frame's request is held: a nested request that wrongly went out would answer at once.
    let s = setup(move |call| Reply {
        wait: call.first().contains("outer").then(|| gate.clone()),
        ..text("done")
    });
    std::thread::scope(|scope| {
        let outer = scope.spawn(|| s.call("rlm.infer", json!({"task": "outer", "context": []})));
        wait_until("the outer frame's request", || s.calls().len() == 1);
        // A request from the outer frame's lane (its task's lane) is a nested frame with no depth left.
        let lane = s.frame_tasks()[0]["id"]
            .as_str()
            .map(|id| format!("ultron.rlm-frame.{id}"))
            .unwrap();
        let nested = s.call_with(
            "rlm.infer",
            json!({"task": "nested", "context": []}),
            &lane,
            &Signal::new(),
        );
        // Release the outer frame before any assertion, so a failure never leaves it waiting.
        let calls = s.calls().len();
        release.store(true, Ordering::SeqCst);
        let nested = nested.unwrap();
        assert_eq!(
            (&nested["status"], &nested["reason"]),
            (&json!("incomplete"), &json!("depth_exhausted"))
        );
        assert_eq!(calls, 1);
        assert_eq!(outer.join().unwrap().unwrap()["status"], "complete");
    });
    // The nested frame never became a task; its trace names the frame it was asked from.
    assert_eq!(s.frame_states(), ["completed"]);
    let listed = s.call("rlm.frames", json!({})).unwrap();
    let frames = listed["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    let (nested, outer) = match frames[0]["reason"] == "depth_exhausted" {
        true => (&frames[0], &frames[1]),
        false => (&frames[1], &frames[0]),
    };
    assert_eq!(nested["status"], "incomplete");
    assert_eq!(nested["reason"], "depth_exhausted");
    assert_eq!(nested["parentFrame"], outer["id"]);
    assert_eq!(nested["callerTaskId"], s.frame_tasks()[0]["id"]);
    assert_eq!(outer["taskId"], s.frame_tasks()[0]["id"]);
}

#[test]
fn acceptance_a50_host_all_in_flight_frames_are_cancelled_and_the_map_returns_per_item_errors() {
    let never = Arc::new(AtomicBool::new(false));
    let gate = never.clone();
    let s = setup(move |_| Reply {
        wait: Some(gate.clone()),
        ..text("1")
    });
    let cell = Signal::new();
    let frames: Vec<Value> = (0..4)
        .map(|i| json!({"task": format!("Wait {i}."), "context": []}))
        .collect();
    // A frame's own timeout is only a hang guard here: only the abort may end them.
    let payload = json!({"frames": frames, "contract": {"type": "integer"}, "timeout_ms": 120_000});
    let reply = std::thread::scope(|scope| {
        let pending = scope.spawn(|| s.call_with("rlm.map", payload, "main", &cell));
        wait_until("four frames in flight", || s.calls().len() == 4);
        cell.abort("user pressed Esc");
        pending.join().unwrap().unwrap()
    });
    let statuses: Vec<&str> = reply["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["error"; 4]);
    // Every frame's request was dropped unanswered (its lane aborted), and every frame task is cancelled.
    wait_until("every request dropped", || {
        s.transport.dropped.lock().unwrap().len() == 4
    });
    let mut dropped = s.transport.dropped.lock().unwrap().clone();
    dropped.sort();
    let mut sent: Vec<String> = s.calls().iter().map(Call::first).collect();
    sent.sort();
    assert_eq!(dropped, sent);
    assert_eq!(s.frame_states(), ["cancelled"; 4]);
    // Cancelled by the cell's abort (not by their own timeout).
    for task in s.frame_tasks() {
        assert_eq!(
            task["result"]["error"], "Inference cancelled: the calling cell was aborted",
            "{task}"
        );
    }
    // The per-item errors stay observable after the abort: each frame's trace records its error.
    let listed = s.call("rlm.frames", json!({})).unwrap();
    let traced: Vec<&Value> = listed["frames"].as_array().unwrap().iter().collect();
    assert_eq!(traced.len(), 4);
    assert!(traced.iter().all(|frame| frame["status"] == "error"), "{listed}");
    for result in reply["results"].as_array().unwrap() {
        let trace = s.call("rlm.frames", json!({"id": result["trace_id"]})).unwrap();
        assert_eq!(trace["status"], "error");
        assert_eq!(trace["taskId"], result["task_id"]);
    }
}

#[test]
fn acceptance_a47_host_traces_are_stored_as_session_values_and_listed_newest_first() {
    let s = setup(|_| text("42"));
    let first = s
        .call(
            "rlm.infer",
            json!({"task": "first", "context": [literal("a")], "contract": {"type": "integer"}}),
        )
        .unwrap();
    let second = s
        .call("rlm.infer", json!({"task": "second", "context": [literal("b")]}))
        .unwrap();
    let listed = s.call("rlm.frames", json!({})).unwrap();
    let ids: Vec<&Value> = listed["frames"].as_array().unwrap().iter().map(|f| &f["id"]).collect();
    assert_eq!(ids, [&second["trace_id"], &first["trace_id"]]);
    let trace = s.call("rlm.frames", json!({"id": first["trace_id"]})).unwrap();
    assert_eq!(
        (&trace["id"], &trace["status"], &trace["task"], &trace["value"]),
        (&first["trace_id"], &json!("complete"), &json!("first"), &json!("42"))
    );
    let documents = s.traces.documents.lock().unwrap();
    assert!(documents.contains_key("index"));
    // The trace records view metadata, never the materialized text.
    let stored = documents[first["trace_id"].as_str().unwrap()].to_string();
    assert!(!stored.contains("\"text\""));
    drop(documents);
    // Another handler of the session (a later cell) reads the same traces.
    let later = s
        .call_with("rlm.frames", json!({"id": second["trace_id"]}), "main", &Signal::new())
        .unwrap();
    assert_eq!(later["task"], "second");
}
