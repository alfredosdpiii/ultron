//! The root lane every prompted mode runs on (print mode, `--mode json`, `--mode rpc`): the model, its key and the
//! native worker's system prompt (`session-worker.ts`), the single `rlm` tool whose cells run in a persistent kernel
//! (crates/kernel) over the host stack the composition root builds ([`crate::compose`]), the provider call, and the
//! clock lines (`ultron/clock-context.ts`) a request carries.

use crate::compose::{Config, Runtime, RuntimeEvents, Stack};
use crate::{ModelFlags, RunEnv as Env};
use agent::{AgentTool, AgentToolResult, StreamFn, ToolUpdate};
use ai::http::{BoxFuture, ReqwestTransport};
use ai::model_registry::ModelRegistry;
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
    system_prompt_with_guide(env, cwd, loki_root, extensions, &Default::default())
}

/// [`system_prompt`] whose runtime guide gets `guide` (the REPL's extension tools and MCP servers), in the mode the
/// global settings name ([`PromptMode`]).
pub fn system_prompt_with_guide(
    env: &Env,
    cwd: &str,
    loki_root: Option<&Path>,
    extensions: Option<&str>,
    guide: &resources::runtime_guide::RuntimeGuideOptions,
) -> Result<String, String> {
    let settings = config::settings::SettingsManager::create(Path::new(cwd), &env.agent_dir(), false);
    let mode = PromptMode::from_settings(&settings);
    system_prompt_in_mode(env, cwd, loki_root, extensions, guide, &mode).map(|(prompt, _)| prompt)
}

/// What the settings change about the system prompt. Engineering mode (the `engineering` setting, `/engineering`,
/// `/mode`) puts the always-on skills (`alwaysSkills`) in the prompt; off, they are listed like any skill.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptMode {
    pub engineering: bool,
    pub always_skills: Vec<String>,
}

impl PromptMode {
    /// `getEngineering` and `getAlwaysSkills` (the string entries of `alwaysSkills`).
    pub fn from_settings(settings: &config::settings::SettingsManager) -> PromptMode {
        let settings = settings.settings();
        PromptMode {
            engineering: settings.get("engineering") == Some(&Value::Bool(true)),
            always_skills: settings
                .get("alwaysSkills")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        }
    }
}

/// [`system_prompt_with_guide`] in an explicit mode, with the `alwaysSkills` problems (a skill that is not loaded,
/// cannot be read, or would pass the always-on size limit) as warnings.
pub fn system_prompt_in_mode(
    env: &Env,
    cwd: &str,
    loki_root: Option<&Path>,
    extensions: Option<&str>,
    guide: &resources::runtime_guide::RuntimeGuideOptions,
    mode: &PromptMode,
) -> Result<(String, Vec<String>), String> {
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
    // Skills the user set always on carry their text into the prompt (resource-loader.ts `applyAlwaysSkills`).
    let warnings = resources::skills::apply_always_skills(&mut skills, &mode.always_skills)
        .into_iter()
        .map(|d| d.message)
        .collect();
    let runtime = resources::runtime_guide::rlm_runtime_prompt(&["rlm"], guide).unwrap_or_default();
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
        engineering: mode.engineering,
        ..BuildOptions::new(cwd)
    };
    Ok((build_system_prompt(&options, None)?, warnings))
}

/// The profile's models.json over the built-in catalog (custom providers and models, overrides, keys, headers).
/// The registry also restores stored catalogs (models-store.json beside models.json) and imports a Radius provider's
/// legacy credential catalog on first sight (`ModelRuntime.create`'s offline refresh).
pub fn registry(env: &Env) -> ModelRegistry {
    let paths = config::paths::AgentPaths::new(env.agent_dir());
    let models = paths.models();
    let mut registry = ModelRegistry::load(&models, &env.home);
    if let Some(credentials) = std::fs::read_to_string(paths.auth())
        .ok()
        .and_then(|text| config::json::parse(text.trim_start_matches('\u{feff}')).ok())
    {
        let store = config::models_store::ModelsStore::new(models.with_file_name("models-store.json"));
        registry.import_legacy_radius(&credentials, &store, now_ms());
    }
    registry
}

/// The model `provider`/`id` (built-in or from models.json), ready for requests.
pub fn catalog_model(provider: &str, id: &str, env: &Env) -> Result<Model, String> {
    let registry = registry(env);
    let model = registry
        .find(provider, id)
        .ok_or_else(|| format!("Model \"{provider}/{id}\" not found"))?;
    Ok(for_requests(&registry, model, env))
}

/// `model` ready for requests: models.json headers resolved per request, and the base URL of the provider's stored
/// OAuth credential (GitHub Copilot's token endpoint) read per request, over the catalog's or models.json's.
pub fn for_requests(registry: &ModelRegistry, model: Model, env: &Env) -> Model {
    let oauth = stored_oauth(env, &model.provider);
    prepare(registry, model, env, oauth)
}

/// Whether the provider's stored credential is an OAuth one.
fn stored_oauth(env: &Env, provider: &str) -> bool {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    config::auth::read_stored_credential(&auth, provider)
        .is_some_and(|c| c.get("type").and_then(Value::as_str) == Some("oauth"))
}

fn prepare(registry: &ModelRegistry, model: Model, env: &Env, oauth: bool) -> Model {
    let mut model = registry.for_requests(model, env.var.clone());
    if oauth {
        let (env, provider) = (env.clone(), model.provider.clone());
        model.request.get_or_insert_default().stored_auth = Some(Arc::new(move || {
            crate::stored_auth::oauth_request_auth(&env, &provider)
        }));
    }
    model
}

/// `--provider`/`--model` against the built-in catalog and models.json (`resolveCliModel`: fuzzy ids, `provider/`
/// prefixes, `<pattern>:<thinking>`, the custom-id fallback), ready for requests; without `--model`, the initial
/// model of a new session.
pub fn resolve_model(flags: &ModelFlags, env: &Env) -> Result<Model, String> {
    resolve_choice(flags, env, false).map(|choice| choice.model)
}

/// The model a session starts on and the thinking level its choice names explicitly (a `:<thinking>` suffix, a
/// scoped pattern's level); `None` leaves the level to the settings (sdk.ts: per-model level, default, medium).
#[derive(Clone, Debug)]
pub struct ModelChoice {
    pub model: Model,
    pub thinking: Option<String>,
    /// `resolveCliModel`'s warning (an invalid suffix, a custom model id).
    pub warning: Option<String>,
}

/// `resolveCliModel({cliProvider, cliModel, modelRuntime})` over the profile's models; the model as listed, not
/// prepared for requests. With `authenticated`, the providers that have configured credentials break ties
/// (`hasConfiguredAuth`); without, none do (a runtime created without its first refresh has no configured providers
/// yet, as `ultron auth check`'s).
pub fn resolve_cli_model(
    env: &Env,
    provider: Option<&str>,
    model: &str,
    authenticated: bool,
) -> ai::model_resolver::CliModel {
    let registry = registry(env);
    let configured = if authenticated {
        configured_providers(env, &registry)
    } else {
        Vec::new()
    };
    registry.resolve_cli(provider, model, None, &|p: &str| configured.iter().any(|c| c == p))
}

/// [`resolve_model`] with the thinking level the choice carries. `continuing`: the session already has history,
/// so scoped models (`--models`, `enabledModels`) do not pick its model.
pub fn resolve_choice(flags: &ModelFlags, env: &Env, continuing: bool) -> Result<ModelChoice, String> {
    let Some(spec) = flags.model.as_deref() else {
        return initial_choice(flags, env, continuing)
            .ok_or_else(|| "No model selected: pass --model <provider>/<id>".to_string());
    };
    let registry = registry(env);
    let configured = configured_providers(env, &registry);
    let has_auth = |p: &str| configured.iter().any(|c| c == p);
    let resolved = registry.resolve_cli(flags.provider.as_deref(), spec, flags.thinking.as_deref(), &has_auth);
    let model = match (resolved.model, resolved.error) {
        (Some(model), _) => model,
        (None, error) => return Err(error.unwrap_or_else(|| format!("Model \"{spec}\" not found"))),
    };
    Ok(ModelChoice {
        model: for_requests(&registry, model, env),
        thinking: resolved.thinking_level,
        warning: resolved.warning,
    })
}

/// `defaultModelPerProvider` of model-resolver.ts.
pub use ai::model_resolver::DEFAULT_MODEL_PER_PROVIDER;

/// The model a session starts on without `--model` (`findInitialModel` of model-resolver.ts with Pi's
/// `buildSessionOptions` scope rule): a new session with scoped models (`--models`, else settings `enabledModels`,
/// resolved against the available models) starts on the saved default when it is in scope, else the first scoped
/// model, with that pattern's thinking level; otherwise the settings' `defaultProvider`/`defaultModel` when its
/// provider has credentials, else an available model preferring a known provider's default. Claude Code CLI models
/// never drive a root lane, so they are skipped.
pub fn initial_model(env: &Env) -> Option<Model> {
    initial_choice(&ModelFlags::default(), env, false).map(|choice| choice.model)
}

fn initial_choice(flags: &ModelFlags, env: &Env, continuing: bool) -> Option<ModelChoice> {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let all = settings.settings();
    let text = |v: Option<Value>| v.and_then(|v| v.as_str().map(str::to_string));
    let registry = registry(env);
    let available = available_models(env);
    let patterns: Vec<String> = match &flags.models {
        Some(patterns) => patterns.clone(),
        None => all
            .get("enabledModels")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
    };
    let (default_provider, default_model) = (text(settings.default_provider()), text(settings.default_model()));
    let choice = |model: Model, thinking: Option<String>| ModelChoice {
        model: for_requests(&registry, model, env),
        thinking,
        warning: None,
    };
    if !continuing && !patterns.is_empty() {
        let (scoped, _) = ai::model_resolver::resolve_model_scope(&patterns, &available);
        // `buildSessionOptions`: the saved default when it is in scope, else the first scoped model.
        let saved = scoped.iter().find(|s| {
            default_provider.as_deref() == Some(s.model.provider.as_str())
                && default_model.as_deref() == Some(s.model.id.as_str())
        });
        if let Some(pick) = saved.or(scoped.first()) {
            return Some(choice(pick.model.clone(), pick.thinking_level.clone()));
        }
    }
    let levels = all
        .get("modelThinkingLevels")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let initial = ai::model_resolver::InitialSettings {
        default_provider,
        default_model_id: default_model,
        default_thinking_level: all
            .get("defaultThinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_string),
        model_thinking_levels: levels,
    };
    let configured: Vec<String> = available.iter().map(|m| m.provider.clone()).collect();
    let has_auth = |p: &str| configured.iter().any(|c| c == p);
    let mut models = registry.all_models();
    // A saved default on the dynamic claude-subscription catalog lists no models of its own.
    if let (Some(p), Some(id)) = (&initial.default_provider, &initial.default_model_id)
        && let Some(found) = registry.find(p, id)
        && !models.iter().any(|m| m.provider == found.provider && m.id == found.id)
    {
        models.push(found);
    }
    // sdk.ts takes only the model: its thinking level comes from the settings, as for any other start.
    let (model, _) = ai::model_resolver::find_initial_model(&[], continuing, &initial, &models, &available, &has_auth);
    model.map(|model| choice(model, None))
}

/// The thinking level a session starts with when nothing names one: the settings' level for `model`
/// (`modelThinkingLevels`), else `defaultThinkingLevel`, else medium.
fn initial_level(all: &serde_json::Map<String, Value>, model: &Model) -> String {
    all.get("modelThinkingLevels")
        .and_then(|levels| levels.get(format!("{}/{}", model.provider, model.id)))
        .and_then(Value::as_str)
        .or_else(|| all.get("defaultThinkingLevel").and_then(Value::as_str))
        .unwrap_or(ai::model_resolver::DEFAULT_THINKING_LEVEL)
        .to_string()
}

/// The thinking level a session starts with when `--thinking` is not given: the settings' level for `model`
/// (`modelThinkingLevels`), else `defaultThinkingLevel`, else medium. Stored as chosen, like TS: requests clamp it
/// to what the model supports.
pub fn initial_thinking(model: &Model, env: &Env) -> String {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    initial_level(settings.settings(), model)
}

/// The providers with credentials (`hasConfiguredAuth`): a stored credential, the models.json key, or the built-in
/// provider's environment key.
fn configured_providers(env: &Env, registry: &ModelRegistry) -> Vec<String> {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    registry
        .provider_ids()
        .into_iter()
        .filter(|p| {
            config::auth::read_stored_credential(&auth, p).is_some()
                || registry.auth_configured(p, &*env.var, env_api_key_with(env, registry, p).is_some())
        })
        .collect()
}

/// The model runtime's available snapshot: the models of providers with credentials (a stored credential, the
/// models.json key, or the built-in provider's environment key), ready for requests.
pub fn available_models(env: &Env) -> Vec<Model> {
    let registry = registry(env);
    let mut models: Vec<Model> = configured_providers(env, &registry)
        .into_iter()
        .flat_map(|p| {
            let oauth = stored_oauth(env, &p);
            let registry = &registry;
            registry
                .models(&p)
                .into_iter()
                .map(move |m| prepare(registry, m, env, oauth))
        })
        .collect();
    models.extend(subscription_models(env, &registry, &models));
    models
}

/// The Claude subscription's pinned models, when Claude Code is installed. Its catalog takes any id, so the registry
/// lists none of them; without this they are reachable only as `--model claude-subscription/<id>`. Models already
/// listed (a models.json entry for the provider) are not repeated.
fn subscription_models(env: &Env, registry: &ModelRegistry, listed: &[Model]) -> Vec<Model> {
    use ai::claude_subscription::{PROVIDER, catalog, process};
    let mut lookup: ai::claude_subscription::Env = [process::COMMAND_ENV, "PATH", "HOME"]
        .into_iter()
        .filter_map(|k| (env.var)(k).map(|v| (k.to_string(), v)))
        .collect();
    lookup
        .entry("HOME".into())
        .or_insert_with(|| env.home.to_string_lossy().into_owned());
    if process::resolve_claude(None, &lookup).is_none() {
        return Vec::new();
    }
    catalog::pinned_routes()
        .into_iter()
        .filter(|(_, id, _)| !listed.iter().any(|m| m.provider == PROVIDER && m.id == *id))
        .filter_map(|(_, id, _)| registry.find(PROVIDER, id))
        .map(|m| prepare(registry, m, env, false))
        .collect()
}

/// The key a request of `provider` carries (`ModelRuntime.getAuth`): `--api-key` (a runtime key), else the auth.json
/// credential (an api key, or an OAuth access token; a stored credential owns the provider), else the models.json
/// key, else the environment; `None` when there is none or the configured one does not resolve.
pub fn api_key(flags: &ModelFlags, env: &Env, provider: &str) -> Option<String> {
    request_key(flags, env, provider).ok().flatten()
}

/// [`api_key`] with the error of a key that does not resolve (a models.json `$VAR` unset, `!command` failing, an
/// unreadable auth.json, a failed OAuth refresh).
pub fn request_key(flags: &ModelFlags, env: &Env, provider: &str) -> Result<Option<String>, String> {
    if let Some(key) = flags.api_key.as_deref() {
        return Ok(Some(key.to_string()));
    }
    match crate::stored_auth::stored_key(env, provider)? {
        crate::stored_auth::Stored::Owns(key) => Ok(key),
        crate::stored_auth::Stored::Nothing => {
            let registry = registry(env);
            let inherited = env_api_key_with(env, &registry, provider);
            registry.api_key(provider, None, &*env.var, inherited)
        }
    }
}

/// Whether `provider`'s stored OAuth credential authenticates requests by its headers alone (Kimi For Coding's
/// `Authorization: Bearer`, `toAuth` with no key): `getAuth` resolves, so a request needs no API key.
pub fn header_auth(env: &Env, provider: &str) -> bool {
    crate::stored_auth::oauth_request_auth(env, provider).is_some_and(|auth| auth.headers.is_some())
}

/// The provider's key from the environment and the profile's credential files. A models.json Radius provider
/// inherits the built-in Radius key (`RADIUS_API_KEY`).
pub fn env_api_key(env: &Env, provider: &str) -> Option<String> {
    env_api_key_with(env, &registry(env), provider)
}

fn env_api_key_with(env: &Env, registry: &ModelRegistry, provider: &str) -> Option<String> {
    let provider = if registry.radius_gateway(provider).is_some() {
        "radius"
    } else {
        provider
    };
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
        "anthropic-messages" | "openai-completions" | "openai-responses" | "google-generative-ai" | "pi-messages"
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
            // A stored OAuth credential may name the request's base URL (Copilot's token endpoint) and headers
            // (Kimi's bearer); models.json headers (and `authHeader`'s bearer) go out over them, under the caller's.
            let (routed, prepared) = match ai::model_registry::prepare_request(model, options) {
                Ok(prepared) => prepared,
                Err(error) => return error_message(model, error),
            };
            let model = routed.as_ref().unwrap_or(model);
            let options = prepared.as_ref().unwrap_or(options);
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
                // Radius gateways (the built-in provider and models.json `"oauth": "radius"` providers).
                "pi-messages" => Ok(ai::pi_messages::stream_simple(model, context, options, transport, sink).await),
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
    /// The session's masker: values MCP servers resolve (env, headers) are registered in it.
    masker: Arc<OutputMasker>,
    mask_secrets: Option<String>,
    max_bytes: usize,
    spill_dir: PathBuf,
    /// Situational hints (`ULTRON_HINTS=on`): one `[hint:<tag>]` line at the end of a cell's result.
    hints: Option<Arc<kernel_apis::cell_hints::CellHints>>,
}

impl Rlm {
    /// The hint the cell earned with this result text (or error message), if any.
    fn hint(&self, text: &str, ename: Option<&str>) -> Option<String> {
        let outcome = kernel_apis::cell_hints::CellOutcome { text, ename };
        self.hints
            .as_ref()
            .and_then(|hints| hints.end_cell(&self.lane, &outcome))
    }

    /// The cell's result text (or error message) with the hint its cell earned, if any.
    fn with_hint(&self, text: String, ename: Option<&str>) -> String {
        match self.hint(&text, ename) {
            Some(hint) => format!("{text}\n{hint}"),
            None => text,
        }
    }

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
            if let Some(hints) = &self.hints {
                hints.begin_cell(&self.lane, &code);
            }
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
                CellOutcome::Done(result) => result.map_err(|e| self.with_hint(e, Some("Error")))?,
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
            let output = cell_output(&result, &format)
                .map_err(|failure| self.with_hint(failure.message, Some(&failure.ename)))?;
            // Only a hint is appended: output that itself starts with a newline stays as it is.
            let text = match (output.text.is_empty(), self.hint(&output.text, None)) {
                (true, None) => "(no result)".to_string(),
                (true, Some(hint)) => format!("(no result)\n{hint}"),
                (false, Some(hint)) => format!("{}\n{hint}", output.text),
                (false, None) => output.text,
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

    /// Shut down for a successor worker: shell jobs keep running for it.
    pub fn hand_over(&self) {
        self.stack.hand_over();
    }
}

/// The root lane's name.
pub const MAIN: &str = "main";

/// The `rlm` tool for `lane` of `lanes`.
pub(crate) fn rlm_over(
    config: &Config,
    lanes: Lanes,
    lane: &str,
    spill_dir: PathBuf,
    hints: Option<Arc<kernel_apis::cell_hints::CellHints>>,
) -> Rlm {
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
        masker: config.masker.clone(),
        mask_secrets: config.mask_secrets.clone(),
        max_bytes: kernel::rlm_output_budget(config.output_bytes.as_deref()) as usize,
        spill_dir,
        hints,
    }
}
