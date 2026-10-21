//! What each model request of a session carries, resolved when it is sent (TypeScript resolves auth per request,
//! core/model-runtime.ts `prepareRequest` -> `getAuth`):
//! - `--api-key` is the starting model's provider's key only (session-worker.ts `setRuntimeApiKey(resolved.model
//!   .provider, apiKey)`); another provider's requests carry that provider's own key;
//! - a root generation's key follows the model its checkpoint pinned, so a model switch during the turn changes
//!   neither (drive/generation.ts `prepareGeneration` -> `lane.models.getModel(identity...)`);
//! - a durable request carries its own options and provider session id (generation.ts `streamSimple(..., {...options,
//!   sessionId})`, compaction's `maxTokens`/`cacheRetention`);
//! - frames are lanes of the session: the root lane's model now, its provider's key, the harness retry policy;
//! - a stored OAuth credential written after the model was chosen (`/login`) applies to its next request;
//! - a Bedrock AWS-profile or Vertex project/location login (credential env only) passes the key check, and the
//!   ambient-credentials marker is never sent as a key.
//!
//! Sessions run against a scripted Anthropic server on loopback in a private home; nothing reads the real environment.

mod p_session_support;

use agent::StreamFn;
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{Message, TranscriptContext, UserMessage, UserMessageContent};
use ai::{AssistantMessage, Body, HttpRequest, HttpResponse, SimpleStreamOptions, Transport};
use harness::chat::{Models, StreamRequest};
use harness::scheduler::Cancel;
use p_session_support::{overloaded, text, tool};
use serde_json::{Map, Value, json};
use session_core::core::{Input, Op};
use session_core::lane::{self, Provider};
use session_core::request_keys::{Keyed, Keys, Retrying};
use session_core::turns::{Bridge, GenerationStart, Request, TurnModels};
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);
/// The fake `--api-key` of the starting provider (built from parts, so no secret scanner sees a key).
fn flag_key() -> String {
    ["flag", "key", "a"].join("-")
}
fn other_key() -> String {
    ["other", "env", "key"].join("-")
}

/// One request the scripted server saw: its headers (lowercase names) and body.
#[derive(Clone, Debug)]
struct Seen {
    headers: HashMap<String, String>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

type Script = Box<dyn FnMut(usize) -> String + Send>;

fn in_order(replies: Vec<String>) -> Script {
    Box::new(move |index| replies.get(index).cloned().unwrap_or_else(|| text("(no more replies)")))
}

fn server(mut script: Script) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let (mut length, mut headers) = (0, HashMap::new());
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        length = v.parse().unwrap();
                    }
                    headers.insert(k, v);
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let index = {
                let mut kept = kept.lock().unwrap();
                kept.push(Seen { headers, body });
                kept.len() - 1
            };
            let reply = script(index);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, seen)
}

struct Session {
    tx: Sender<Input>,
    actor: Option<std::thread::JoinHandle<()>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    cwd: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

/// models.json: Anthropic and an `other` Anthropic-API provider (key `$OTHER_KEY`) on the scripted server.
fn two_providers(url: &str) -> Value {
    json!({"providers": {
        "anthropic": {"baseUrl": url},
        "other": {"baseUrl": url, "api": "anthropic-messages", "apiKey": "$OTHER_KEY",
            "models": [{"id": "m1", "contextWindow": 200000, "maxTokens": 8000}]},
    }})
}

fn session(
    script: Script,
    models: impl Fn(&str) -> Value,
    flags: ModelFlags,
    vars: Vec<(&str, String)>,
    settings: Option<Value>,
) -> Session {
    let (port, seen) = server(script);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let url = format!("http://127.0.0.1:{port}");
    std::fs::write(agent_dir.join("models.json"), models(&url).to_string()).unwrap();
    if let Some(settings) = settings {
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    }
    let mut all: Vec<(String, String)> = vec![
        ("HOME".into(), root.display().to_string()),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        (
            "ULTRON_PYTHON".into(),
            std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into()),
        ),
        ("PI_OFFLINE".into(), "1".into()),
        ("TZ".into(), "UTC".into()),
    ];
    all.extend(vars.into_iter().map(|(k, v)| (k.to_string(), v)));
    let env = RunEnv {
        cwd: cwd.clone(),
        home: root.clone(),
        var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let sessions = agent_dir.join("experimental").join("sessions");
    let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
    let created = repo.create(&cwd.display().to_string(), None, None).unwrap();
    let open = session_core::core::OpenOptions {
        env,
        flags,
        session_root: sessions,
        metadata: created.metadata().clone(),
        mode: "rpc".into(),
        services: session_core::compose::Services::default(),
        auto_name: false,
    };
    drop(created);
    let (tx, actor, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
    Session {
        tx,
        actor: Some(actor),
        seen,
        cwd,
        _tmp: tmp,
    }
}

impl Session {
    fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    fn prompt(&self, text: &str) -> Value {
        self.call(Op::PromptAndWait {
            message: text.into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Waits (hang guard only) until the server saw `count` requests.
    fn wait_requests(&self, count: usize) {
        let end = Instant::now() + HANG;
        while self.seen().len() < count {
            assert!(Instant::now() < end, "timed out waiting for {count} requests");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn set_model(&self, provider: &str, id: &str) {
        self.call(Op::SetModel {
            provider: provider.into(),
            model_id: id.into(),
        })
        .unwrap();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let (done, closed) = channel();
        if self.tx.send(Input::Close(done)).is_ok()
            && closed.recv_timeout(HANG).is_ok()
            && let Some(actor) = self.actor.take()
        {
            let _ = actor.join();
        }
    }
}

fn anthropic_with_flag() -> ModelFlags {
    ModelFlags {
        model: Some("anthropic/claude-sonnet-4-5".into()),
        api_key: Some(flag_key()),
        ..Default::default()
    }
}

fn private_env(dir: &Path, vars: &[(&str, &str)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    map.insert("PI_OFFLINE".into(), "1".into());
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.join("home"),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

/// `--api-key` given for the starting provider authenticates only that provider: after a switch to another
/// provider the requests carry that provider's own key, never the flag's.
#[test]
fn session_core_request_keys_api_key_is_only_the_starting_providers() {
    let dir = tempfile::tempdir().unwrap();
    let env = private_env(dir.path(), &[("GROQ_API_KEY", "groq-env")]);
    let keys = Keys::session(Some(flag_key()), "anthropic", &env);
    assert_eq!(keys.key("anthropic"), Some(flag_key()));
    assert_eq!(keys.key("groq").as_deref(), Some("groq-env"));
    assert_eq!(keys.key("openai"), None);

    let s = session(
        in_order(vec![text("one"), text("two")]),
        two_providers,
        anthropic_with_flag(),
        vec![("OTHER_KEY", other_key())],
        None,
    );
    s.prompt("first");
    s.set_model("other", "m1");
    s.prompt("second");
    let seen = s.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body["model"], "claude-sonnet-4-5");
    assert_eq!(seen[0].header("x-api-key"), Some(flag_key().as_str()));
    assert_eq!(seen[1].body["model"], "m1");
    assert_eq!(seen[1].header("x-api-key"), Some(other_key().as_str()));
}

/// A model switch while the turn runs (its cell holds the turn) leaves the turn's next generation on the model its
/// checkpoint pinned, with that model's key; the next turn runs on the new model with its own key.
#[test]
fn session_core_request_keys_mid_turn_switch_keeps_the_generations_key() {
    let gate = "release.flag";
    let code = format!(
        "import os, time\nend = time.time() + 120\nwhile not os.path.exists({gate:?}) and time.time() < end:\n    time.sleep(0.01)\n'released'"
    );
    let s = session(
        in_order(vec![tool("t1", &code), text("first done"), text("second done")]),
        two_providers,
        anthropic_with_flag(),
        vec![("OTHER_KEY", other_key())],
        None,
    );
    let tx = s.tx.clone();
    let running = std::thread::spawn(move || {
        session_core::core::call(
            &tx,
            Op::PromptAndWait {
                message: "run the cell".into(),
                images: Vec::new(),
            },
        )
    });
    s.wait_requests(1);
    s.set_model("other", "m1");
    std::fs::write(s.cwd.join(gate), "").unwrap();
    running.join().unwrap().unwrap();
    s.prompt("again");
    let seen = s.seen();
    assert_eq!(seen.len(), 3, "{seen:#?}");
    for request in &seen[..2] {
        assert_eq!(request.body["model"], "claude-sonnet-4-5");
        assert_eq!(request.header("x-api-key"), Some(flag_key().as_str()));
    }
    assert_eq!(seen[2].body["model"], "m1");
    assert_eq!(seen[2].header("x-api-key"), Some(other_key().as_str()));
}

/// Every request of a run carries the runtime's provider session id: OpenCode's `x-opencode-session` routing header
/// (providers/opencode-headers.ts) goes out with the root's requests.
#[test]
fn session_core_request_keys_root_requests_carry_the_provider_session_id() {
    let s = session(
        in_order(vec![tool("t1", "1 + 1"), text("done")]),
        |url| json!({"providers": {"opencode": {"baseUrl": url}}}),
        ModelFlags {
            model: Some("opencode/claude-fable-5".into()),
            ..Default::default()
        },
        vec![("OPENCODE_API_KEY", ["oc", "key"].join("-"))],
        None,
    );
    s.prompt("go");
    let seen = s.seen();
    assert_eq!(seen.len(), 2);
    let session = seen[0].header("x-opencode-session").map(str::to_string);
    assert!(session.as_deref().is_some_and(|s| !s.is_empty()), "{seen:#?}");
    assert_eq!(seen[1].header("x-opencode-session").map(str::to_string), session);
}

/// A provider that keeps the options of every request and answers with a plain stop.
#[derive(Default)]
struct Kept {
    requests: Mutex<Vec<(String, SimpleStreamOptions)>>,
}

impl StreamFn for Kept {
    fn stream<'a>(
        &'a self,
        model: &'a ai::Model,
        _context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        _sink: &'a mut ai::EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        self.requests
            .lock()
            .unwrap()
            .push((model.provider.clone(), options.clone()));
        Box::pin(async move {
            let mut message = lane::error_message(model, String::new());
            message.stop_reason = ai::StopReason::Stop;
            message.error_message = None;
            message
        })
    }
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp: 0,
    })
}

fn turn_models(provider: Arc<Kept>) -> TurnModels {
    let bridge = Bridge::new(Arc::new(|request| {
        if let Request::Generation { answer, .. } = request {
            answer.give(Ok(GenerationStart {
                messages: vec![user("hi")],
            }));
        }
    }));
    let root = Arc::new(OnceLock::new());
    root.set("root-session".to_string()).unwrap();
    TurnModels {
        bridge,
        provider,
        resolve: Arc::new(|_, _| None),
        api_key: Arc::new(|provider| Some(format!("key-of-{provider}"))),
        root_session: root,
        charge: None,
        transform_child: None,
        trace: None,
    }
}

fn model_of(provider: &str) -> ai::Model {
    let mut model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    model.provider = provider.into();
    model
}

/// A durable request (the root's or a child's) is sent with its own options and provider session id, and the key
/// of its own model's provider.
#[test]
fn session_core_request_keys_durable_requests_send_their_options() {
    let kept = Arc::new(Kept::default());
    let models = turn_models(kept.clone());
    let context = TranscriptContext {
        messages: vec![user("hi")],
    };
    let options: Map<String, Value> = json!({"maxTokens": 1234, "cacheRetention": "none", "timeoutMs": 5000})
        .as_object()
        .unwrap()
        .clone();
    // A child conversation's compaction summary request.
    let child = StreamRequest {
        options: options.clone(),
        session_id: Some("child-session".into()),
        reasoning: Some("high".into()),
        conversation_id: None,
        final_note: None,
    };
    models
        .stream(&model_of("groq"), &context, &child, &Cancel::new(), &mut |_| {})
        .unwrap();
    // A root generation pinned to `other` (whatever the session's model is now).
    let root = StreamRequest {
        options,
        session_id: Some("root-session".into()),
        reasoning: None,
        conversation_id: None,
        final_note: None,
    };
    models
        .stream(&model_of("other"), &context, &root, &Cancel::new(), &mut |_| {})
        .unwrap();
    let requests = kept.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for ((provider, options), (expected, session)) in requests
        .iter()
        .zip([("groq", "child-session"), ("other", "root-session")])
    {
        assert_eq!(provider, expected);
        assert_eq!(
            options.base.api_key.as_deref(),
            Some(format!("key-of-{expected}").as_str())
        );
        assert_eq!(options.base.session_id.as_deref(), Some(session));
        assert_eq!(options.base.max_tokens, Some(1234));
        assert_eq!(options.base.cache_retention, Some(ai::CacheRetention::None));
        assert_eq!(options.base.timeout_ms, Some(5000));
        assert!(options.base.signal.is_some());
    }
    assert_eq!(requests[0].1.reasoning.as_deref(), Some("high"));
    assert_eq!(requests[1].1.reasoning, None);
}

/// A provider whose first `fail` answers are `error` (an overloaded provider), then a plain stop.
struct Flaky {
    calls: Mutex<usize>,
    fail: usize,
    error: &'static str,
}

impl StreamFn for Flaky {
    fn stream<'a>(
        &'a self,
        model: &'a ai::Model,
        _context: &'a TranscriptContext,
        _options: &'a SimpleStreamOptions,
        _sink: &'a mut ai::EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        Box::pin(async move {
            let mut message = lane::error_message(model, self.error.into());
            if call > self.fail {
                message.stop_reason = ai::StopReason::Stop;
                message.error_message = None;
            }
            message
        })
    }
}

fn retried(fail: usize, error: &'static str, policy: harness::chat::RetryPolicy, aborted: bool) -> (usize, bool) {
    let stream = Retrying {
        inner: Flaky {
            calls: Mutex::new(0),
            fail,
            error,
        },
        policy: Arc::new(move || policy),
    };
    let mut options = SimpleStreamOptions::default();
    let signal = ai::AbortSignal::new();
    if aborted {
        signal.abort();
    }
    options.base.signal = Some(signal);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let context = TranscriptContext { messages: Vec::new() };
    let message = runtime.block_on(stream.stream(&model_of("anthropic"), &context, &options, &mut sink));
    let calls = *stream.inner.calls.lock().unwrap();
    (calls, message.stop_reason == ai::StopReason::Stop)
}

/// The harness retry policy over a side lane's requests: a transient error is asked again up to `maxRetries`
/// times; a permanent one, a spent policy, retries off or an aborted request are not.
#[test]
fn session_core_request_keys_side_lanes_retry_transient_errors() {
    let policy = harness::chat::RetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 1,
        max_delay_ms: 4,
    };
    assert_eq!(retried(2, "Overloaded", policy, false), (3, true));
    assert_eq!(retried(5, "Overloaded", policy, false), (4, false));
    assert_eq!(retried(1, "Bad request", policy, false), (1, false));
    let off = harness::chat::RetryPolicy {
        enabled: false,
        ..policy
    };
    assert_eq!(retried(1, "Overloaded", off, false), (1, false));
    assert_eq!(retried(1, "Overloaded", policy, true), (1, false));
    // `settings.retry` as the settings name it (`maxRetries`, `baseDelayMs`; `enabled: false` turns it off).
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let read = |settings: Value| {
        std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
        session_core::request_keys::retry_policy(&config::settings::SettingsManager::create(dir.path(), &agent, false))
    };
    let named = read(json!({"retry": {"maxRetries": 5, "baseDelayMs": 7}}));
    assert_eq!((named.enabled, named.max_retries, named.base_delay_ms), (true, 5, 7));
    assert!(!read(json!({"retry": {"enabled": false}})).enabled);
}

/// Requests through [`Keyed`] carry the key of their model's provider, resolved when they are sent, unless the
/// caller set one; ambient AWS or Google credentials send none.
#[test]
fn session_core_request_keys_keyed_streams_resolve_per_request() {
    let current = Arc::new(Mutex::new(Some("first".to_string())));
    let read = current.clone();
    let keys = Keys::new(
        Some(("anthropic".into(), flag_key())),
        Arc::new(move |provider: &str| match provider {
            "amazon-bedrock" => Some("<authenticated>".into()),
            _ => read.lock().unwrap().clone(),
        }),
    );
    let kept = Arc::new(Kept::default());
    let keyed = Keyed {
        inner: KeptRef(kept.clone()),
        keys,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let context = TranscriptContext { messages: Vec::new() };
    let send = |provider: &str, key: Option<&str>| {
        let mut options = SimpleStreamOptions::default();
        options.base.api_key = key.map(str::to_string);
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        runtime.block_on(keyed.stream(&model_of(provider), &context, &options, &mut sink));
    };
    send("groq", None);
    *current.lock().unwrap() = Some("second".into());
    send("groq", None);
    send("anthropic", None);
    send("amazon-bedrock", None);
    send("groq", Some("caller"));
    let keys: Vec<Option<String>> = kept
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(_, o)| o.base.api_key.clone())
        .collect();
    assert_eq!(
        keys,
        [
            Some("first".into()),
            Some("second".into()),
            Some(flag_key()),
            None,
            Some("caller".into())
        ]
    );
}

struct KeptRef(Arc<Kept>);

impl StreamFn for KeptRef {
    fn stream<'a>(
        &'a self,
        model: &'a ai::Model,
        context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        sink: &'a mut ai::EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        self.0.stream(model, context, options, sink)
    }
}

/// A frame (`rlm.infer`) after a model switch calls the root lane's model now, with that provider's key, and a
/// transient provider error is retried as a harness lane retries it (`settings.retry`).
#[test]
fn session_core_request_keys_frames_follow_the_root_model_and_retry() {
    let code = "print(await rlm.infer('Say hi'))";
    let s = session(
        in_order(vec![tool("t1", code), overloaded(), text("hi there"), text("done")]),
        two_providers,
        anthropic_with_flag(),
        vec![("OTHER_KEY", other_key())],
        Some(json!({"retry": {"baseDelayMs": 1, "provider": {"maxRetries": 0}}})),
    );
    s.set_model("other", "m1");
    s.prompt("infer");
    let seen = s.seen();
    assert_eq!(seen.len(), 4, "{seen:#?}");
    for request in &seen {
        assert_eq!(request.body["model"], "m1");
        assert_eq!(request.header("x-api-key"), Some(other_key().as_str()));
    }
    let result = seen[3].body["messages"].to_string();
    assert!(result.contains("hi there"), "{result}");
}

/// The predict adapter calls the root lane's model as of each call (`createPredictAdapter({model: () =>
/// lane.getModel()})`), with that provider's key resolved for the call.
#[test]
fn session_core_request_keys_predict_follows_the_root_model() {
    let definition = json!({
        "id": "judge", "version": "1", "strategy": "predict", "instructions": "Double n.",
        "inputSchema": {"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"],
                        "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"doubled": {"type": "integer"}}, "required": ["doubled"],
                         "additionalProperties": false},
        "maxRepairs": 0, "inputDescription": "{n:integer}", "outputDescription": "{doubled:integer}",
    });
    let code = format!(
        "import json\nawait agents.register(json.loads({:?}))\nprint(await agents.invoke('judge@1', {{'n': 2}}))",
        definition.to_string()
    );
    let s = session(
        in_order(vec![tool("t1", &code), text("{\"doubled\": 4}"), text("done")]),
        two_providers,
        anthropic_with_flag(),
        vec![("OTHER_KEY", other_key())],
        None,
    );
    s.set_model("other", "m1");
    s.prompt("predict");
    let seen = s.seen();
    assert_eq!(seen.len(), 3, "{seen:#?}");
    assert!(
        seen[1].body["system"].to_string().contains("typed function"),
        "{:#?}",
        seen[1].body
    );
    for request in &seen {
        assert_eq!(request.body["model"], "m1");
        assert_eq!(request.header("x-api-key"), Some(other_key().as_str()));
    }
    let result = seen[2].body["messages"].to_string();
    assert!(result.contains("doubled") && result.contains("succeeded"), "{result}");
}

/// The Kimi For Coding OAuth credential stored after the model was chosen (`/login` while the session runs)
/// authenticates the model's next request (model-auth.json case 10: its `Authorization: Bearer`, no key).
#[test]
fn session_core_request_keys_login_applies_to_the_current_model() {
    let fixture: Value = serde_json::from_str(include_str!("../../ai/fixtures/model-auth.json")).unwrap();
    let spec = &fixture["cases"][10];
    assert_eq!(spec["name"], "kimi oauth headers go out with the models.json headers");
    let dir = tempfile::tempdir().unwrap();
    let env = private_env(dir.path(), &[("KIMI_HEADER", "k")]);
    let agent = dir.path().join("agent");
    std::fs::write(agent.join("models.json"), spec["models"].to_string()).unwrap();
    let model = lane::catalog_model("kimi-coding", "k3", &env).unwrap();
    // The login lands after the model was prepared.
    std::fs::write(agent.join("auth.json"), spec["auth"].to_string()).unwrap();
    let keys = Keys::session(None, &model.provider, &env);
    let transport = Recorder::default();
    let provider = Provider::with_router(ai::claude_subscription::fallback::global(), Arc::new(transport.clone()));
    let mut options = SimpleStreamOptions::default();
    options.base.api_key = keys.request_key(&model.provider);
    let context = TranscriptContext {
        messages: vec![user("go")],
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    runtime.block_on(provider.stream(&model, &context, &options, &mut sink));
    let request = transport.requests.lock().unwrap().last().cloned().unwrap();
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let expected = &spec["calls"][0]["results"][0]["headers"];
    assert_eq!(header("authorization").as_deref(), expected["Authorization"].as_str());
    assert_eq!(header("x-kimi").as_deref(), expected["x-kimi"].as_str());
    assert_eq!(header("x-api-key"), None);
}

/// A Bedrock AWS-profile login and a Vertex project/location login store only credential env: the session's key
/// check passes (amazon-bedrock.ts `bedrockAuth.resolve`, Vertex ADC), and no key is sent for them.
#[test]
fn session_core_request_keys_credential_env_logins_pass_the_key_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("adc.json"), "{}").unwrap();
    let env = private_env(dir.path(), &[]);
    let auth = json!({
        "amazon-bedrock": {"type": "api_key", "env": {"AWS_PROFILE": "work"}},
        "google-vertex": {"type": "api_key", "env": {
            "GOOGLE_APPLICATION_CREDENTIALS": dir.path().join("adc.json").display().to_string(),
            "GOOGLE_CLOUD_PROJECT": "proj", "GOOGLE_CLOUD_LOCATION": "europe-west4"}},
    });
    std::fs::write(dir.path().join("agent/auth.json"), auth.to_string()).unwrap();
    let keys = Keys::session(None, "amazon-bedrock", &env);
    for provider in ["amazon-bedrock", "google-vertex"] {
        assert!(
            lane::request_key(&ModelFlags::default(), &env, provider)
                .unwrap()
                .is_some(),
            "{provider}"
        );
        assert!(keys.key(provider).is_some(), "{provider}");
        assert_eq!(keys.request_key(provider), None, "{provider}");
    }
    // The stored profile reaches the request as its provider env.
    let model = lane::catalog_model("amazon-bedrock", "amazon.nova-lite-v1:0", &env).unwrap();
    let stored = model
        .request
        .as_ref()
        .and_then(|r| r.stored_auth.as_ref())
        .and_then(|resolve| resolve());
    let profile = stored.and_then(|s| s.env).and_then(|e| e.get("AWS_PROFILE").cloned());
    assert_eq!(profile.as_deref(), Some("work"));
    // A credential-chain login with no ambient credentials is not configured.
    std::fs::write(
        dir.path().join("agent/auth.json"),
        json!({"amazon-bedrock": {"type": "api_key"}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        lane::request_key(&ModelFlags::default(), &env, "amazon-bedrock").unwrap(),
        None
    );
}

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// An in-memory provider endpoint that keeps each request and answers with an empty stream.
#[derive(Clone, Default)]
struct Recorder {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let mut headers = HeaderMap::new();
        headers.insert("content-type".into(), "text/event-stream".into());
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(vec![b"data: [DONE]\n\n".to_vec()].into_iter())),
            })
        })
    }
}
