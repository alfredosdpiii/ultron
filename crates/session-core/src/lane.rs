//! The root lane every prompted mode runs on (print mode, `--mode json`, `--mode rpc`): the model, its key and the
//! native worker's system prompt (`session-worker.ts`), the single `rlm` tool whose cells run in a persistent kernel
//! (crates/kernel) over the host stack the composition root builds ([`crate::compose`]), the provider call, and the
//! clock lines (`ultron/clock-context.ts`) a request carries.

use crate::compose::{Config, Runtime, RuntimeEvents, Stack};
use crate::{ModelFlags, RunEnv as Env};
use agent::{AgentTool, AgentToolResult, StreamFn, ToolUpdate};
use ai::http::{BoxFuture, ReqwestTransport};
use ai::{
    AbortSignal, AssistantMessage, EventSink, Message, Model, SimpleStreamOptions, StopReason, TextContent, Tool,
    TranscriptContext, Transport, UserContent, UserMessage, UserMessageContent,
};
use delegation::lanes::{CellOutcome, Lanes};
use host::Host;
use host::cell::{CellFormat, cell_output};
use host::secrets::OutputMasker;
use kernel::Kernel;
use serde_json::{Value, json};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `RLM_TOOL_DESCRIPTION` and `RLM_TOOL_SNIPPET` from ultron/rlm/prompt.ts.
const RLM_DESCRIPTION: &str = "Run a Python cell in your persistent REPL: variables, imports and functions persist across calls, top-level `await` works, and the last expression's value is shown with anything printed (kept as `_`).\nPre-imported: `bash`, `read`, `edit`, `write`, `view_image`, `rlm`, `agents`, `workflows`, `background`, `tools`, `mcp`, `ctx`, `skills`, `memory`, `hints`, `agent`, `Budget`, `state`, `preview`, `asyncio`; `help(obj)` shows any API's docs.";
const RLM_SNIPPET: &str = "Python REPL for files, shell (`bash`), edits (`edit`), data and subagents";

/// What an aborted `rlm` cell reports (the harness's tool abort text).
pub const ABORT_REQUESTED: &str = "Abort requested";

/// The model's only tool. Every capability is a kernel API reached from a cell; nothing adds a second tool.
pub fn rlm_tool() -> Tool {
    Tool {
        name: "rlm".into(),
        description: RLM_DESCRIPTION.into(),
        parameters: json!({
            "type": "object",
            "properties": {"code": {"type": "string", "description": "Python code for persistent RLM computation, typed agents, and data processing"}},
            "required": ["code"],
        }),
        constrained_sampling: None,
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn text_block(text: String) -> UserContent {
    UserContent::Text(TextContent {
        text,
        text_signature: None,
    })
}

/// `formatClock` for UTC: "2026-10-05 14:03 UTC+00:00".
pub fn format_clock_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02} UTC+00:00",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// `CLOCK_GAP_MS`: a user message this long after the one before it gets a clock line.
pub const CLOCK_GAP_MS: i64 = 10 * 60 * 1000;

/// `formatGap`: "12 min", "3 h", "2 d".
pub fn format_gap(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000);
    if minutes < 120 {
        return format!("{minutes} min");
    }
    let hours = minutes / 60;
    if hours < 48 {
        format!("{hours} h")
    } else {
        format!("{} d", hours / 24)
    }
}

/// `clockContextEnabled`: `ULTRON_CLOCK_CONTEXT=off|0|false` disables clock lines.
pub fn clock_enabled(env: &Env) -> bool {
    !(env.var)("ULTRON_CLOCK_CONTEXT")
        .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"))
}

/// The clock lines of `withClockLines` as `(index, text, timestamp)`: one before each user message that opens the
/// conversation or follows a gap of `CLOCK_GAP_MS` or more; `index` is the user message the line precedes.
fn clock_texts(messages: &[Value]) -> Vec<(usize, String, i64)> {
    let mut out = Vec::new();
    let mut previous: Option<i64> = None;
    for (index, message) in messages.iter().enumerate() {
        let at = message
            .get("timestamp")
            .and_then(Value::as_f64)
            .filter(|t| t.is_finite() && *t > 0.0)
            .map(|t| t as i64);
        if let Some(at) = at
            && message.get("role").and_then(Value::as_str) == Some("user")
            && previous.is_none_or(|p| at - p >= CLOCK_GAP_MS)
        {
            let gap = previous.map_or(String::new(), |p| {
                format!(", {} after the previous message", format_gap(at - p))
            });
            out.push((index, format!("[clock] {}{gap}", format_clock_utc(at)), at));
        }
        if at.is_some() {
            previous = at;
        }
    }
    out
}

/// One clock line (a user message) per [`clock_texts`] entry, with the index of the message it precedes.
pub fn clock_lines(messages: &[Value]) -> Vec<(usize, Message)> {
    clock_texts(messages)
        .into_iter()
        .map(|(index, text, at)| {
            let line = Message::User(UserMessage {
                content: UserMessageContent::Blocks(vec![text_block(text)]),
                timestamp: at,
            });
            (index, line)
        })
        .collect()
}

/// `withClockLines`: the messages with their clock lines in place; `None` when no line was added. Lines are derived,
/// never stored.
pub fn with_clock_lines(messages: &[Value]) -> Option<Vec<Value>> {
    let lines = clock_texts(messages);
    if lines.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(messages.len() + lines.len());
    let mut pending = lines.into_iter().peekable();
    for (index, message) in messages.iter().enumerate() {
        if let Some((_, text, at)) = pending.next_if(|(i, _, _)| *i == index) {
            out.push(json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": at}));
        }
        out.push(message.clone());
    }
    Some(out)
}

/// Session messages as the model sees them (`convertToLlm`): custom roles become user messages, excluded bash runs
/// are dropped.
pub fn to_llm(messages: &[Value]) -> Vec<Message> {
    compaction::convert_to_llm(messages)
        .into_iter()
        .filter_map(|m| serde_json::from_value(m).ok())
        .collect()
}

/// The native worker's system prompt for a lane whose only tool is `rlm`. `loki_root` is where Loki's policy is read
/// (None: Loki is off, so no policy note); `extensions` is the runtime-guide section listing extension functions.
pub fn system_prompt(
    env: &Env,
    cwd: &str,
    loki_root: Option<&Path>,
    extensions: Option<&str>,
) -> Result<String, String> {
    use resources::system_prompt::{BuildOptions, build_system_prompt};
    let agent_dir = env.agent_dir().display().to_string();
    let bundled = config::paths::normalize(&env.docs.join("../skills"))
        .display()
        .to_string();
    let mut skills = resources::skills::load_skills(&resources::skills::LoadSkillsOptions {
        cwd,
        agent_dir: &agent_dir,
        skill_paths: &[],
        include_defaults: true,
    })
    .skills;
    skills.extend(resources::skills::load_skills_from_dir(&bundled, "bundled").skills);
    let runtime = resources::runtime_guide::rlm_runtime_prompt(&["rlm"], &Default::default()).unwrap_or_default();
    // loki.ts `readContext`: a policy the engine cannot read gives no note (the guard still runs and reports it).
    let loki_note = loki_root
        .and_then(|root| loki::config::load_config(root).ok())
        .and_then(|config| loki::injected_context(&config, Some("ultron")).ok());
    let options = BuildOptions {
        selected_tools: vec!["rlm".into()],
        tool_snippets: vec![("rlm".into(), RLM_SNIPPET.into())],
        tool_guidelines: vec![("rlm".into(), resources::runtime_guide::rlm_tool_guidelines(&["rlm"]))],
        include_harness_docs: false,
        context_files: resources::context_files::load_project_context_files(cwd, &agent_dir),
        skills,
        bundled_skills_dir: Some(bundled),
        sections: [("runtime".to_string(), runtime)]
            .into_iter()
            .chain(extensions.map(|text| ("extensions".to_string(), text.to_string())))
            .chain(loki_note.map(|note| ("loki".to_string(), note)))
            .collect(),
        ..BuildOptions::new(cwd)
    };
    build_system_prompt(&options, None)
}

/// `models.json` `providers.<provider>.baseUrl` applied to a built-in model.
fn apply_models_json(mut model: Model, env: &Env) -> Result<Model, String> {
    let models_json = config::paths::AgentPaths::new(env.agent_dir()).models();
    if let Ok(text) = std::fs::read_to_string(&models_json) {
        let value: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", models_json.display()))?;
        if let Some(url) = value["providers"][&model.provider]["baseUrl"].as_str() {
            model.base_url = url.to_string();
        }
    }
    Ok(model)
}

/// The model of `provider`/`id` from the built-in catalog, with `models.json` `providers.<name>.baseUrl` applied.
pub fn catalog_model(provider: &str, id: &str, env: &Env) -> Result<Model, String> {
    let model = ai::models::builtin_model(provider, id)
        .cloned()
        .ok_or_else(|| format!("Model \"{provider}/{id}\" not found"))?;
    apply_models_json(model, env)
}

/// `--provider`/`--model` (`provider/id` or a bare id with `--provider`) against the built-in catalog, with
/// `models.json` `providers.<name>.baseUrl` applied.
pub fn resolve_model(flags: &ModelFlags, env: &Env) -> Result<Model, String> {
    let spec = flags
        .model
        .as_deref()
        .ok_or("No model selected: pass --model <provider>/<id>")?;
    let (provider, id) = match (flags.provider.as_deref(), spec.split_once('/')) {
        (Some(p), _) => (p, spec),
        (None, Some((p, id))) => (p, id),
        (None, None) => return Err(format!("Model \"{spec}\" needs a provider: use <provider>/<id>")),
    };
    catalog_model(provider, id, env)
}

/// The model runtime's available snapshot: built-in models whose provider has credentials, `models.json` base
/// URLs applied.
pub fn available_models(env: &Env) -> Vec<Model> {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &Path| p.exists(),
        home: Some(env.home.clone()),
    };
    ai::models::builtin_providers()
        .filter(|p| {
            ai::env::env_api_key_in(p, &source).is_some() || config::auth::read_stored_credential(&auth, p).is_some()
        })
        .flat_map(ai::models::builtin_models)
        .filter_map(|m| apply_models_json(m.clone(), env).ok())
        .collect()
}

/// `--api-key`, else the provider's key from the environment.
pub fn api_key(flags: &ModelFlags, env: &Env, provider: &str) -> Option<String> {
    flags.api_key.clone().or_else(|| env_api_key(env, provider))
}

/// The provider's key from the environment and the profile's credential files.
pub fn env_api_key(env: &Env, provider: &str) -> Option<String> {
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &Path| p.exists(),
        home: Some(env.home.clone()),
    };
    ai::env::env_api_key_in(provider, &source)
}

/// Whether the Rust providers stream this API.
pub fn api_supported(api: &str) -> bool {
    matches!(
        api,
        "anthropic-messages" | "openai-completions" | "openai-responses" | "google-generative-ai"
    )
}

/// The model call for every API the Rust providers cover. A request whose signal is already aborted is not sent:
/// it ends as an empty aborted message. `api_key` fills requests that carry none (a subagent's loop).
pub struct Provider {
    transport: Arc<dyn Transport>,
    api_key: Option<String>,
    /// Routes `claude-subscription` calls (subscription first, API credits when out of usage): the process's.
    router: Arc<ai::claude_subscription::fallback::Router>,
}

impl Provider {
    pub fn new() -> Self {
        Provider {
            transport: Arc::new(ReqwestTransport::new(None)),
            api_key: None,
            router: ai::claude_subscription::fallback::global(),
        }
    }

    /// A provider over `transport` whose Claude subscription calls go through `router` (tests and embedders).
    pub fn with_router(router: Arc<ai::claude_subscription::fallback::Router>, transport: Arc<dyn Transport>) -> Self {
        Provider {
            transport,
            api_key: None,
            router,
        }
    }

    pub fn with_key(api_key: Option<String>) -> Self {
        Provider {
            api_key,
            ..Provider::new()
        }
    }

    /// A provider whose requests fail when headers or the stream stall for `idle` (`httpIdleTimeoutMs`).
    pub fn with_idle_timeout(idle: Option<std::time::Duration>) -> Self {
        Provider {
            transport: Arc::new(ReqwestTransport::new(None).with_idle_timeout(idle)),
            ..Provider::new()
        }
    }
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFn for Provider {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            if options.base.signal.as_ref().is_some_and(AbortSignal::aborted) {
                let mut message = error_message(model, String::new());
                message.stop_reason = StopReason::Aborted;
                message.error_message = None;
                return message;
            }
            let keyed;
            let options = if options.base.api_key.is_none() && self.api_key.is_some() {
                let mut with_key = options.clone();
                with_key.base.api_key = self.api_key.clone();
                keyed = with_key;
                &keyed
            } else {
                options
            };
            let transport = &*self.transport;
            let result = match model.api.as_str() {
                "anthropic-messages" => ai::anthropic::stream_simple(model, context, options, transport, sink).await,
                "openai-completions" => {
                    ai::openai_completions::stream_simple(model, context, options, transport, sink).await
                }
                "openai-responses" => {
                    ai::openai_responses::stream_simple(model, context, options, transport, sink).await
                }
                "google-generative-ai" => ai::google::stream_simple(model, context, options, transport, sink).await,
                "claude-subscription" => Ok(self.router.stream(model, context, options, transport, sink).await),
                api => Err(format!("API \"{api}\" is not ported to the Rust build yet")),
            };
            result.unwrap_or_else(|e| error_message(model, e))
        })
    }
}

pub fn error_message(model: &Model, error: String) -> AssistantMessage {
    let mut message: AssistantMessage = serde_json::from_value(json!({
        "content": [], "api": model.api, "provider": model.provider, "model": model.id,
        "usage": ai::Usage::default(), "stopReason": "stop", "timestamp": now_ms(),
    }))
    .expect("an empty assistant message deserializes");
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error);
    message
}

/// Resolves with `work`'s output, or `None` once `signal` aborts first.
async fn until_aborted<T>(work: impl Future<Output = T>, signal: &AbortSignal) -> Option<T> {
    let mut work = pin!(work);
    let mut cancelled = pin!(signal.cancelled());
    std::future::poll_fn(|cx| {
        if let Poll::Ready(value) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(value));
        }
        match cancelled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// How long a cell may run before its tool call returns with the cell still running (detached). Cells that park
/// on a host-side wait detach at once; others run as long as they need, as in TypeScript.
const CELL_BUDGET: Duration = Duration::from_secs(24 * 60 * 60);

/// The `rlm` tool (`createUltronRlmTool` for one lane): each call runs its cell in the lane of the session's REPL
/// lanes (crates/delegation `Lanes`). Aborting the turn cancels the running cell.
pub struct Rlm {
    tool: Tool,
    lanes: Lanes,
    lane: String,
    masker: OutputMasker,
    mask_secrets: Option<String>,
    max_bytes: usize,
    spill_dir: PathBuf,
}

impl Rlm {
    /// The cell `lane` is running now.
    fn running_cell(&self) -> Option<String> {
        self.lanes.list().as_array().and_then(|lanes| {
            lanes
                .iter()
                .find(|l| l["lane"] == self.lane.as_str())
                .and_then(|l| l["cell"].as_str().map(str::to_string))
        })
    }
}

impl AgentTool for Rlm {
    fn tool(&self) -> &Tool {
        &self.tool
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        signal: &'a AbortSignal,
        _update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let code = args["code"].as_str().ok_or("rlm needs a `code` string")?.to_string();
            let (lanes, lane) = (self.lanes.clone(), self.lane.clone());
            let mut cell = tokio::task::spawn_blocking(move || lanes.run(&lane, &code, CELL_BUDGET));
            let outcome = match until_aborted(&mut cell, signal).await {
                Some(done) => done,
                None => {
                    if let Some(running) = self.running_cell() {
                        let _ = self.lanes.cancel(&running, ABORT_REQUESTED);
                    }
                    let _ = cell.await;
                    return Err(ABORT_REQUESTED.into());
                }
            };
            let outcome = outcome.map_err(|e| e.to_string())??;
            if signal.aborted() {
                return Err(ABORT_REQUESTED.into());
            }
            let result = match outcome {
                CellOutcome::Done(result) => result?,
                CellOutcome::Running { cell, parked } => {
                    if !parked {
                        self.lanes.detach(&cell)?;
                    }
                    return Ok(AgentToolResult {
                        content: vec![text_block(format!(
                            "Cell {cell} is still running in the background (it waits on the host); its end arrives as a <runtime_event kind=\"cell_done\" id=\"{cell}\">. Do other work or end your turn."
                        ))],
                        details: json!({}),
                        usage: None,
                        terminate: None,
                    });
                }
            };
            let format = CellFormat {
                masker: &self.masker,
                mask_secrets: self.mask_secrets.as_deref(),
                max_bytes: self.max_bytes,
                spill_dir: Some(&self.spill_dir),
            };
            let output = cell_output(&result, &format).map_err(|failure| failure.message)?;
            let text = if output.text.is_empty() {
                "(no result)".to_string()
            } else {
                output.text
            };
            Ok(AgentToolResult {
                content: vec![text_block(text)],
                details: json!({}),
                usage: None,
                terminate: None,
            })
        })
    }
}

/// `sessionDirectoryName` of the JSONL session repo: the cwd without its leading separator, separators and colons
/// as dashes, between `--`.
pub fn session_directory_name(cwd: &str) -> String {
    session_journal::repo::session_directory_name(cwd)
}

/// One session's root lane: the `rlm` tool over the "main" REPL lane of the session's host stack.
pub struct Lane {
    /// The host operations (shell, writes) behind the stack, for host-side commands such as RPC `bash`.
    pub host: Arc<Host>,
    pub stack: Arc<Stack>,
    pub tool: Arc<Rlm>,
    /// Completions of work the session's runs started (subagents, detached cells), delivered between turns.
    pub events: Arc<RuntimeEvents>,
}

impl Lane {
    /// Start the root lane ("main") of `session_id` over `runtime`'s host stack.
    pub fn open(runtime: &Runtime, model: &Model, api_key: Option<String>, session_id: &str) -> Result<Lane, String> {
        Lane::open_with(runtime, model, api_key, session_id, None)
    }

    /// [`Lane::open`] whose subagents run as `subagents` says.
    pub fn open_with(
        runtime: &Runtime,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        subagents: Option<crate::compose::SubagentHost>,
    ) -> Result<Lane, String> {
        let stack = runtime.stack_with(model, api_key, session_id, subagents)?;
        let tool = Arc::new(stack.rlm(MAIN)?);
        Ok(Lane {
            host: stack.host.clone(),
            events: stack.events.clone(),
            tool,
            stack,
        })
    }

    /// The transcript's first message: the system prompt and the declared `rlm` tool.
    pub fn system_message(&self, system: &str) -> Option<Message> {
        ai::transcript::create_initial_system_message(Some(system), Some(std::slice::from_ref(&self.tool.tool)))
            .map(Message::System)
    }

    /// The model's tool list: exactly the lane's `rlm` tool.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![self.tool.clone() as Arc<dyn AgentTool>]
    }

    /// The root lane's kernel, once open.
    pub fn kernel(&self) -> Option<Arc<Kernel>> {
        self.stack.lanes.kernel(MAIN)
    }

    pub fn shutdown(&self) {
        self.stack.close();
    }
}

/// The root lane's name.
pub const MAIN: &str = "main";

/// The `rlm` tool for `lane` of `lanes`.
pub(crate) fn rlm_over(config: &Config, lanes: Lanes, lane: &str, spill_dir: PathBuf) -> Rlm {
    let mut tool = rlm_tool();
    // `codeSkillsToolSection`: the tool description lists the profile's active code skills.
    let listing = kernel_apis::CodeSkills::new(kernel_apis::CodeSkillsOptions::new(
        &config.skills_dir,
        &config.runtime_path,
    ))
    .describe();
    if !listing.is_empty() {
        tool.description = format!("{}\n{listing}", tool.description);
    }
    Rlm {
        tool,
        lanes,
        lane: lane.to_string(),
        masker: OutputMasker::default(),
        mask_secrets: config.mask_secrets.clone(),
        max_bytes: kernel::rlm_output_budget(config.output_bytes.as_deref()) as usize,
        spill_dir,
    }
}
