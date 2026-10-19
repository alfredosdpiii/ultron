//! The review engine (`autoreview/engine.ts`): `autoreview_api.run` executed in a Python kernel of its own, with
//! frame inference available and no root model.
//!
//! RLM-first: the pipeline stays the kernel's Python (`autoreview_api.run_file`); its only model calls are the
//! `rlm.map` frames it makes, served here by the inference crate's [`Frames`] on the model each frame names. Nothing
//! prompts a root lane, the model has no tools, and every other host request is refused: the pipeline reads the
//! repository and runs git itself, inside the kernel. The kernel runs our own pipeline (and, sandboxed, the reviewed
//! project's tests as its children), so the per-cell address-space and CPU limits meant for model-written cells are
//! off; the cap on the whole process tree stays.
//!
//! One engine serves any number of concurrent reviews (each review is a short-lived kernel), so `ultron autoreview
//! run` keeps it; `start_ms` is what opening it cost.
//!
//! Known differences from the TypeScript: the TS engine opened the whole Ultron runtime for an external root
//! (harness, registry, session file of frame traces under `sessions/`); here the frames' traces stay in memory and
//! `sessions/` holds nothing new (it is still created and pruned, so earlier runs' files age out). Host requests
//! other than inference are refused instead of served by a runtime the pipeline never uses.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use inference::{Frames, Inference, ModelSettings};
use kernel::{HostHandler, Kernel, KernelOptions, LimitOverrides, Signal};
use serde_json::{Map, Value};

/// The engine's session files (frame traces of earlier runs) are kept this long.
pub const SESSION_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Resolves a `provider/model` reference to the model and its API key.
pub type ModelResolver = Arc<dyn Fn(&str) -> Result<(ai::Model, Option<String>), String> + Send + Sync>;

/// A log line sink.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// What reviews: a spec in, an engine result (JSON, as `autoreview_api.run` returns it) out.
pub trait ReviewEngine: Send + Sync {
    fn review(&self, spec: &Value, signal: Option<&Signal>) -> Result<Value, String>;
}

/// The frames' retry of a transient provider failure (`settings.retry`; the TS harness's retry policy, which every
/// frame lane applies): up to `max_retries` more attempts, `base_delay_ms * 2^(attempt-1)` apart, capped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: f64,
    pub max_delay_ms: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000.0,
            max_delay_ms: 60_000.0,
        }
    }
}

impl RetryPolicy {
    /// `retryDelayMs`: the wait before retry `attempt` (1-indexed).
    pub fn delay_ms(&self, attempt: u32) -> f64 {
        let delay = self.base_delay_ms * 2f64.powi(attempt.saturating_sub(1).min(1023) as i32);
        delay.min(self.max_delay_ms)
    }
}

const NON_RETRYABLE: &str = "GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing|Claude Code usage limit reached";
const RETRYABLE: &str = "overloaded|currently experiencing high demand|rate.?limit|too many requests|429|500|502|503|504|520|524|service.?unavailable|server.?error|internal.?error|provider.?returned.?error|exceeded request buffer limit while retrying upstream|network.?error|connection.?error|connection.?refused|connection.?lost|other side closed|fetch failed|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect|reset before headers|socket hang up|socket connection was closed|timed? out|timeout|terminated|websocket.?closed|websocket.?error|ended without|stream ended before message_stop|stream ended before a terminal response event|http2 request did not get a response|retry delay|you can retry your request|try your request again|please retry your request|ResourceExhausted";

/// `isRetryableAssistantError` over a failure's text (the status and the provider's message).
pub fn is_retryable_failure(text: &str) -> bool {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<(regex::Regex, regex::Regex)> = OnceLock::new();
    let (never, again) = PATTERNS.get_or_init(|| {
        let compile = |pattern: &str| regex::Regex::new(&format!("(?i){pattern}")).expect("retry pattern");
        (compile(NON_RETRYABLE), compile(RETRYABLE))
    });
    !text.is_empty() && !never.is_match(text) && again.is_match(text)
}

struct Buffered(Option<Vec<u8>>);

impl ai::Body for Buffered {
    fn next_chunk(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Vec<u8>>, String>> + Send + '_>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Retries a frame's provider request that failed transiently (a connection failure, or a non-2xx response whose
/// status and message are retryable), as the TS frame lanes' harness does; the frame sees only the last attempt.
pub struct RetryingTransport {
    pub inner: Arc<dyn ai::Transport>,
    pub policy: RetryPolicy,
}

impl ai::Transport for RetryingTransport {
    fn send(
        &self,
        request: ai::HttpRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ai::HttpResponse, String>> + Send + '_>> {
        Box::pin(async move {
            let mut attempt = 0u32;
            loop {
                let copy = ai::HttpRequest {
                    method: request.method.clone(),
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                };
                let outcome = self.inner.send(copy).await;
                let retries_left = self.policy.enabled && attempt < self.policy.max_retries;
                let outcome = match outcome {
                    Ok(response) if (200..300).contains(&response.status) || !retries_left => Ok(response),
                    Ok(mut response) => {
                        let mut body = Vec::new();
                        while let Ok(Some(chunk)) = response.body.next_chunk().await {
                            body.extend(chunk);
                        }
                        let text = format!("{} {}", response.status, String::from_utf8_lossy(&body));
                        if !is_retryable_failure(&text) {
                            response.body = Box::new(Buffered(Some(body)));
                            return Ok(response);
                        }
                        Err(text)
                    }
                    Err(error) if !retries_left || !is_retryable_failure(&error) => return Err(error),
                    Err(error) => Err(error),
                };
                match outcome {
                    Ok(response) => return Ok(response),
                    Err(_) => {
                        attempt += 1;
                        let delay = self.policy.delay_ms(attempt).max(0.0);
                        tokio::time::sleep(Duration::from_secs_f64(delay / 1000.0)).await;
                    }
                }
            }
        })
    }

    /// The socket is not retried: the Codex transport keeps its own WebSocket retries and SSE fallback.
    fn websocket_available(&self) -> bool {
        self.inner.websocket_available()
    }

    fn websocket(
        &self,
        url: String,
        headers: std::collections::BTreeMap<String, String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn ai::websocket::WebSocketConnection>, String>> + Send + '_>,
    > {
        self.inner.websocket(url, headers)
    }
}

pub struct EngineOptions {
    /// `<agentDir>/autoreview`: the kernels' working directory (`work/`) and session files live under it.
    pub dir: PathBuf,
    /// runtime.py; `autoreview_api` and its modules sit next to it.
    pub runtime_path: PathBuf,
    /// The interpreter (default: the kernel's, `ULTRON_PYTHON` else /usr/bin/python3).
    pub python: Option<PathBuf>,
    /// The model frames that name none run on (the TS runtime's preferred model).
    pub model: ai::Model,
    pub api_key: Option<String>,
    /// Every other model a frame names.
    pub resolve: ModelResolver,
    pub transport: Arc<dyn ai::Transport>,
    /// Transient provider failures are retried under this policy before a frame sees them.
    pub retry: RetryPolicy,
    /// `ULTRON_RLM_FRAME_MODEL`, `ULTRON_REVIEW_MODEL`, `ULTRON_RLM_MAP_TOKENS` as the process has them.
    pub frame_env: HashMap<String, String>,
    /// The saved frame-model settings (`rlm.frameModel`, `rlm.frameThinking`, ...).
    pub settings: ModelSettings,
    /// Extra environment for the kernels (applied after the kernel's credential filter).
    pub kernel_env: Vec<(String, String)>,
    /// The environment the review kernels start from (`KernelOptions::base_env`): the caller's session environment,
    /// so a review runs its tools (git, the repository's test commands) from the session's PATH, never the server
    /// process's. None: this process's environment.
    pub base_env: Option<Vec<(String, String)>>,
    pub log: Option<Log>,
}

/// `provider/id` of a model.
pub fn model_ref(model: &ai::Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// `pruneSessions`: remove `<group>/*.jsonl` files under `session_dir` older than the retention. Never fails.
pub fn prune_sessions(session_dir: &Path, now: SystemTime) {
    let Ok(groups) = std::fs::read_dir(session_dir) else {
        return;
    };
    for group in groups.flatten() {
        if !group.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(group.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
            let jsonl = entry.file_name().to_string_lossy().ends_with(".jsonl");
            let old = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > SESSION_RETENTION);
            if is_file && jsonl && old {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Routes inference requests to the [`Frames`] of the model they name (created on first use); refuses the rest.
struct Router {
    default_ref: String,
    options: Arc<RouterOptions>,
    routes: Mutex<HashMap<String, Arc<Frames>>>,
}

struct RouterOptions {
    model: ai::Model,
    api_key: Option<String>,
    resolve: ModelResolver,
    transport: Arc<dyn ai::Transport>,
    frame_env: HashMap<String, String>,
    settings: ModelSettings,
    context_dir: PathBuf,
}

/// Only inference reaches a frame's host: the pipeline needs nothing else.
struct Refuse;

impl HostHandler for Refuse {
    fn handle(&self, kind: &str, _payload: Map<String, Value>, _signal: &Signal) -> Result<Value, String> {
        Err(format!("{kind} is not available to the review engine"))
    }
}

const INFERENCE_REQUESTS: [&str; 5] = ["rlm.map", "rlm.infer", "rlm.load", "rlm.models", "rlm.list_traces"];

impl Router {
    /// The model a request's frames run on: the one it names, else the frame model (`ULTRON_RLM_FRAME_MODEL`,
    /// then `rlm.frameModel`), else the engine's.
    fn model_of(&self, payload: &Map<String, Value>) -> String {
        if let Some(model) = payload.get("model").and_then(Value::as_str) {
            return model.to_string();
        }
        let env = self
            .options
            .frame_env
            .get("ULTRON_RLM_FRAME_MODEL")
            .map(|value| value.trim().to_string())
            .filter(|value| is_model_ref(value));
        env.or_else(|| self.options.settings.frame_model.clone())
            .unwrap_or_else(|| self.default_ref.clone())
    }

    fn frames(&self, model: &str) -> Result<Arc<Frames>, String> {
        let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(frames) = routes.get(model) {
            return Ok(frames.clone());
        }
        let options = &self.options;
        let (resolved, key) = if model == self.default_ref {
            (options.model.clone(), options.api_key.clone())
        } else {
            (options.resolve)(model)?
        };
        let inference = Inference::new(
            Arc::new(Refuse),
            resolved,
            key,
            options.transport.clone(),
            options.context_dir.clone(),
        );
        let settings = options.settings.clone();
        let frames = Arc::new(Frames::new(inference, options.frame_env.clone(), move || {
            settings.clone()
        }));
        routes.insert(model.to_string(), frames.clone());
        Ok(frames)
    }
}

fn is_model_ref(value: &str) -> bool {
    value
        .split_once('/')
        .is_some_and(|(provider, id)| !provider.is_empty() && !id.is_empty() && !value.contains(char::is_whitespace))
}

impl HostHandler for Router {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if !INFERENCE_REQUESTS.contains(&kind) {
            return Refuse.handle(kind, payload, signal);
        }
        let model = if matches!(kind, "rlm.map" | "rlm.infer") {
            self.model_of(&payload)
        } else {
            self.default_ref.clone()
        };
        // A model the engine cannot resolve fails the frames that name it, not the review.
        match self.frames(&model) {
            Ok(frames) => frames.handle(kind, payload, signal),
            Err(error) => Err(format!("Admission refused: model {model}: {error}")),
        }
    }
}

/// [`ReviewEngine`] over a Python kernel per review (`RuntimeReviewEngine`).
pub struct KernelEngine {
    dir: PathBuf,
    runtime_path: PathBuf,
    python: Option<PathBuf>,
    kernel_env: Vec<(String, String)>,
    base_env: Option<Vec<(String, String)>>,
    router: Arc<Router>,
    /// Milliseconds opening the engine took.
    pub start_ms: u64,
}

impl KernelEngine {
    /// Opens the engine: `work/` and `sessions/` under `dir` (private), earlier session files past retention pruned.
    pub fn open(options: EngineOptions) -> Result<Self, String> {
        let started = Instant::now();
        let work = options.dir.join("work");
        let sessions = options.dir.join("sessions");
        private_dir(&work)?;
        private_dir(&sessions)?;
        prune_sessions(&sessions, SystemTime::now());
        let default_ref = model_ref(&options.model);
        let router = Router {
            default_ref: default_ref.clone(),
            options: Arc::new(RouterOptions {
                model: options.model,
                api_key: options.api_key,
                resolve: options.resolve,
                transport: Arc::new(RetryingTransport {
                    inner: options.transport,
                    policy: options.retry,
                }),
                frame_env: options.frame_env,
                settings: options.settings,
                context_dir: options.dir.join("rlm-context"),
            }),
            routes: Mutex::new(HashMap::new()),
        };
        let start_ms = started.elapsed().as_millis() as u64;
        if let Some(log) = &options.log {
            log(&format!("engine ready in {start_ms} ms (model {default_ref})"));
        }
        Ok(Self {
            dir: options.dir,
            runtime_path: options.runtime_path,
            python: options.python,
            kernel_env: options.kernel_env,
            base_env: options.base_env,
            router: Arc::new(router),
            start_ms,
        })
    }

    /// `provider/model` frames that name no model run on.
    pub fn default_model(&self) -> &str {
        &self.router.default_ref
    }
}

/// The cell that runs one review: the spec and result paths as JSON strings, which Python reads as literals.
pub fn review_cell(spec_path: &str, result_path: &str) -> String {
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    format!(
        "import autoreview_api\nawait autoreview_api.run_file(rlm, {}, {})",
        quote(spec_path),
        quote(result_path)
    )
}

/// The error of a failed pipeline cell (`the review pipeline failed: <ename>: <evalue>` and the traceback's end).
pub fn pipeline_error(error: Option<&kernel::CellError>) -> String {
    let (name, value, traceback) = match error {
        Some(error) => (error.ename.as_str(), error.evalue.as_str(), error.traceback.as_slice()),
        None => ("error", "", &[][..]),
    };
    let tail = &traceback[traceback.len().saturating_sub(6)..];
    format!("the review pipeline failed: {name}: {value}\n{}", tail.join("\n"))
        .trim()
        .to_string()
}

/// The result file's object: an `error` string is the review's error; `model` and `verifyModel` default to the
/// engine's model.
pub fn finish_result(mut result: Value, default_model: &str) -> Result<Value, String> {
    let object = result
        .as_object_mut()
        .ok_or("the review pipeline wrote no result object")?;
    if let Some(Value::String(error)) = object.get("error") {
        return Err(error.clone());
    }
    let model = match object.get("model") {
        Some(Value::String(model)) => model.clone(),
        _ => default_model.to_string(),
    };
    if !matches!(object.get("model"), Some(Value::String(_))) {
        object.insert("model".into(), Value::String(model.clone()));
    }
    if !matches!(object.get("verifyModel"), Some(Value::String(_))) {
        object.insert("verifyModel".into(), Value::String(model));
    }
    Ok(result)
}

/// A private scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let base = std::env::temp_dir();
        for attempt in 0..100u32 {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let path = base.join(format!("ultron-autoreview-{}-{nanos:x}-{attempt}", std::process::id()));
            use std::os::unix::fs::DirBuilderExt;
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Err("cannot create a scratch directory".into())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))
}

impl ReviewEngine for KernelEngine {
    fn review(&self, spec: &Value, signal: Option<&Signal>) -> Result<Value, String> {
        let scratch = Scratch::new()?;
        let spec_path = scratch.0.join("spec.json");
        let result_path = scratch.0.join("result.json");
        write_private(&spec_path, &spec.to_string())?;
        let mut options = KernelOptions::new(self.dir.join("work"), &self.runtime_path);
        options.python = self.python.clone();
        options.limits = LimitOverrides {
            max_memory_mb: Some(0),
            max_cpu_seconds: Some(0),
        };
        options.env = self.kernel_env.clone();
        options.base_env = self.base_env.clone();
        let kernel = Kernel::new(options, self.router.clone()).map_err(|error| error.message().to_string())?;
        let cell = review_cell(&spec_path.to_string_lossy(), &result_path.to_string_lossy());
        let outcome = kernel.execute(&cell, signal, None);
        kernel.shutdown();
        let cell = outcome.map_err(|error| error.message().to_string())?;
        if cell.status == kernel::Status::Error {
            return Err(pipeline_error(cell.error.as_ref()));
        }
        let text =
            std::fs::read_to_string(&result_path).map_err(|error| format!("the review wrote no result: {error}"))?;
        let result: Value = serde_json::from_str(&text).map_err(|error| format!("the review's result: {error}"))?;
        finish_result(result, self.default_model())
    }
}
