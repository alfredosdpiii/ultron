//! Claude models: the subscription first, Anthropic API credits second.
//!
//! Problem: a Max plan's five-hour or weekly window runs out mid-session, and every later `claude -p` call fails
//! until the reset. Max plans also carry monthly API credits on a linked Console organization; a key from that
//! organization bills those credits (and stops with a billing error once they are gone, never charging the plan).
//!
//! Every `claude-subscription` call goes through [`Router::stream`] (`dispatch`, the session lane and inference
//! frames all call it), so no path can skip it. The model's tool list is untouched: this is host-side routing
//! of one Messages call, the transcript and its `[rlm]` tool list go to the API exactly as they went to the CLI.
//!
//! Trace: turn 1 runs `claude -p`; it fails with the plan's usage limit ([`super::limit`]) before any content
//! reached the sink, so the same model id and context stream from `anthropic/<id>` with the configured key, and
//! the message carries one [`ROUTE_DIAGNOSTIC`] ("Claude subscription limit reached; using API credits until
//! <reset>"). Turns 2..n go straight to the API until the reset (or [`DEFAULT_HOLD_MS`] when none was reported),
//! with no CLI spawned. The first turn after the reset tries the subscription again and, unless it is still out of
//! usage, carries a route diagnostic naming the switch back. A transient 429/529, an error after streamed content,
//! fallback turned off (`claude.apiFallback: false`) or no API key leave the subscription's error as it was (no
//! key adds [`KEY_HINT`]). The hold lives in this process only.

use super::{Config, ERROR_DIAGNOSTIC, catalog};
use crate::http::Transport;
use crate::options::{SimpleStreamOptions, now_ms};
use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, EventSink, Model, StopReason,
    TranscriptContext,
};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

/// The diagnostic on the message where routing switched; `details.route` is `api` or `subscription`, and
/// `details.notice` is the line a UI shows.
pub const ROUTE_DIAGNOSTIC: &str = "claude_subscription_route";
/// How long calls stay on the API when the subscription named no reset time.
pub const DEFAULT_HOLD_MS: i64 = 60 * 60 * 1000;
/// Appended to an out-of-usage error when no API key is configured.
pub const KEY_HINT: &str = "To continue on API credits until the limit resets, set ANTHROPIC_API_KEY (or store an `anthropic` api_key credential in auth.json) to a key from the Console organization that receives your plan's monthly API credits; Claude Code itself never sees that key.";
/// Starts the error when the API credits are exhausted too.
pub const BOTH_EXHAUSTED: &str = "Claude subscription usage limit reached and the Anthropic API credits are exhausted";

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type Lookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where the router reads time, keys and settings, and (tests) how it runs the subscription.
#[derive(Clone)]
pub struct RouterOptions {
    /// Epoch milliseconds.
    pub clock: Clock,
    /// The process environment (`ANTHROPIC_API_KEY`, `ULTRON_CODING_AGENT_DIR`, `HOME`).
    pub env: Lookup,
    /// The agent directory holding `auth.json` and `settings.json`; else resolved from `env`.
    pub agent_dir: Option<PathBuf>,
    /// The subscription's config; else [`super::config_for`] per call.
    pub subscription: Option<Config>,
    /// The API's base URL; else `providers.anthropic.baseUrl` of the agent directory's models.json; else the
    /// Anthropic model's.
    pub api_base_url: Option<String>,
}

impl Default for RouterOptions {
    fn default() -> Self {
        RouterOptions {
            clock: Arc::new(now_ms),
            env: Arc::new(|name| std::env::var(name).ok().filter(|v| !v.is_empty())),
            agent_dir: None,
            subscription: None,
            api_base_url: None,
        }
    }
}

/// The routing state of one process: until when calls skip the subscription.
pub struct Router {
    options: RouterOptions,
    hold: Mutex<Option<i64>>,
}

static GLOBAL: LazyLock<Arc<Router>> = LazyLock::new(|| Arc::new(Router::new(RouterOptions::default())));

/// The process's router.
pub fn global() -> Arc<Router> {
    GLOBAL.clone()
}

/// One `claude-subscription` call through the process's router.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
    transport: &dyn Transport,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    GLOBAL.stream(model, context, options, transport, sink).await
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Holds `Start` until content arrives and the terminal event until the call is decided, so a call that failed
/// before any output can be replaced without the sink seeing it, and the terminal event carries the final message.
struct Gate<'s, 'e> {
    sink: &'s mut EventSink<'e>,
    start: Option<AssistantMessageEvent>,
    committed: bool,
}

impl<'s, 'e> Gate<'s, 'e> {
    fn new(sink: &'s mut EventSink<'e>) -> Self {
        Gate {
            sink,
            start: None,
            committed: false,
        }
    }

    fn event(&mut self, event: &AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::Start { .. } if !self.committed => self.start = Some(event.clone()),
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {}
            _ => {
                self.commit();
                (self.sink)(event);
            }
        }
    }

    fn commit(&mut self) {
        self.committed = true;
        if let Some(start) = self.start.take() {
            (self.sink)(&start);
        }
    }

    /// Flush what was held and end the stream with `message`.
    fn finish(mut self, message: &AssistantMessage) {
        self.commit();
        let reason = message.stop_reason;
        let event = if matches!(reason, StopReason::Error | StopReason::Aborted) {
            AssistantMessageEvent::Error {
                reason,
                error: message.clone(),
            }
        } else {
            AssistantMessageEvent::Done {
                reason,
                message: message.clone(),
            }
        };
        (self.sink)(&event);
    }
}

/// `Some(reset)` when the subscription's message failed with the plan's usage limit ([`super::limit`]).
pub fn usage_limit_of(message: &AssistantMessage) -> Option<Option<i64>> {
    let details = message
        .diagnostics
        .iter()
        .flatten()
        .find(|d| d.kind == ERROR_DIAGNOSTIC)?
        .details
        .as_ref()?;
    (details.get("usage_limit") == Some(&Value::Bool(true))).then(|| details.get("resets_at").and_then(Value::as_i64))
}

/// Anthropic's answer once the credits are gone: `Your credit balance is too low ... Plans & Billing` (400) or a
/// `billing_error` (402); the recorded native text is `Credit balance is too low`.
fn billing_exhausted(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("credit balance") || text.contains("billing")
}

/// `isAnthropicAdaptiveThinkingModel` of `packages/ai/scripts/generate-models.ts`: families that think adaptively.
fn adaptive_family(id: &str) -> bool {
    [
        "opus-4-6",
        "opus-4.6",
        "opus-4-7",
        "opus-4.7",
        "opus-4-8",
        "opus-4.8",
        "opus-5",
        "opus.5",
        "sonnet-4-6",
        "sonnet-4.6",
        "sonnet-5",
        "sonnet.5",
        "fable-5",
        "mythos-5",
        "haiku-5",
        "haiku.5",
    ]
    .iter()
    .any(|family| id.contains(family))
}

/// The newest older release of `id`'s family in the generated catalog: `claude-sonnet-5-5` -> `claude-sonnet-5`.
fn family_template(id: &str) -> Option<&'static Model> {
    let mut stem = id;
    while let Some((head, version)) = stem.rsplit_once('-') {
        if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if let Some(model) = crate::models::builtin_model("anthropic", head) {
            return Some(model);
        }
        stem = head;
    }
    None
}

/// `anthropic/<id>` for `claude-subscription/<id>`: aliases and `[1m]` resolved to the canonical id.
///
/// The catalog is generated (models.dev through `packages/ai/scripts/generate-models.ts`, then
/// `data/generate-models.mjs`) and may lag a release the subscription already serves (`claude-sonnet-5-5`). Such
/// an id takes the family's newest catalog entry as its template (limits, cost, compat), with the flags the
/// generator gives the family: `forceAdaptiveThinking` for adaptive families, and no `off` thinking level where
/// the API rejects `thinking: disabled` ([`catalog::MANDATORY_THINKING`]).
pub fn api_model(model: &Model) -> Model {
    let id = catalog::canonical(&model.id);
    if let Some(known) = crate::models::builtin_model("anthropic", id) {
        return known.clone();
    }
    let mut api = family_template(id).cloned().unwrap_or_else(|| {
        let mut api = model.clone();
        api.api = "anthropic-messages".into();
        api.provider = "anthropic".into();
        api
    });
    api.id = id.to_string();
    api.name = model.name.clone();
    api.base_url = catalog::UPSTREAM.into();
    if adaptive_family(id) {
        api.reasoning = true;
        api.compat
            .get_or_insert_default()
            .insert("forceAdaptiveThinking".into(), Value::Bool(true));
    }
    if catalog::MANDATORY_THINKING.iter().any(|family| id.starts_with(family)) {
        let levels = api.thinking_level_map.get_or_insert_default();
        levels.insert("off".into(), None);
        levels.insert("minimal".into(), None);
    }
    api
}

fn route(route: &str, notice: String, extra: Map<String, Value>) -> AssistantMessageDiagnostic {
    let mut details = Map::new();
    details.insert("route".into(), route.into());
    details.insert("notice".into(), notice.into());
    details.extend(extra);
    AssistantMessageDiagnostic {
        kind: ROUTE_DIAGNOSTIC.into(),
        timestamp: now_ms(),
        error: None,
        details: Some(details),
    }
}

fn iso(ms: i64) -> String {
    crate::claude_code_cli::iso_string(ms as f64)
}

/// Text `notice` of the switch to the API.
pub fn api_notice(until: i64) -> String {
    format!(
        "Claude subscription limit reached; using API credits until {}",
        iso(until)
    )
}

pub const SUBSCRIPTION_NOTICE: &str = "Claude subscription available again; back on the subscription";

impl Router {
    pub fn new(options: RouterOptions) -> Self {
        Router {
            options,
            hold: Mutex::new(None),
        }
    }

    /// Until when calls go straight to the API (epoch ms), if they do.
    pub fn held_until(&self) -> Option<i64> {
        *lock(&self.hold)
    }

    fn agent_dir(&self) -> PathBuf {
        self.options.agent_dir.clone().unwrap_or_else(|| {
            let env = &self.options.env;
            let home = env("HOME").map(PathBuf::from).unwrap_or_default();
            config::paths::agent_dir(|name| env(name), &home)
        })
    }

    /// `providers.anthropic.baseUrl` of the agent directory's models.json: the fallback goes where every other
    /// `anthropic` call goes (a proxy, a gateway, a test's stub), never past that override to the public API.
    fn configured_base_url(&self) -> Option<String> {
        let path = config::paths::AgentPaths::new(self.agent_dir()).models();
        crate::model_config::ModelConfig::load(Some(&path.to_string_lossy()))
            .provider("anthropic")?
            .get("baseUrl")?
            .as_str()
            .filter(|url| !url.trim().is_empty())
            .map(str::to_string)
    }

    /// `claude.apiFallback` of the global settings (default on).
    fn enabled(&self, agent_dir: &std::path::Path) -> bool {
        config::settings::SettingsManager::create(agent_dir, agent_dir, false).claude_api_fallback()
    }

    /// The fallback's API key: `ANTHROPIC_API_KEY` (the request's env, then the process's), else the auth store's
    /// `anthropic` api_key credential. Never an OAuth token: that is the subscription again.
    fn api_key(&self, options: &SimpleStreamOptions, agent_dir: &std::path::Path) -> Option<String> {
        let usable = |key: &String| !key.trim().is_empty() && !key.contains("sk-ant-oat");
        let name = crate::env::ANTHROPIC_API_KEY_ENV;
        let request = options.base.env.as_ref().and_then(|env| env.get(name)).cloned();
        if let Some(key) = request.or_else(|| (self.options.env)(name)).filter(usable) {
            return Some(key);
        }
        let path = config::paths::AgentPaths::new(agent_dir).auth();
        let stored = config::auth::read_stored_credential(&path, "anthropic")?;
        if stored.get("type").and_then(Value::as_str) != Some("api_key") {
            return None;
        }
        // The store resolves `$VAR` templates the way every other reader does.
        let resolved = config::auth::AuthStorage::create(path).read("anthropic").ok()??;
        resolved
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(usable)
    }

    async fn subscription(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: &SimpleStreamOptions,
        sink: &mut EventSink<'_>,
    ) -> AssistantMessage {
        match &self.options.subscription {
            Some(config) => super::stream_with(model, context, options, config.clone(), sink).await,
            None => super::stream_simple(model, context, options, sink).await,
        }
    }

    /// One `claude-subscription` call: the subscription, or the API while the plan is out of usage.
    pub async fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: &SimpleStreamOptions,
        transport: &dyn Transport,
        sink: &mut EventSink<'_>,
    ) -> AssistantMessage {
        let agent_dir = self.agent_dir();
        if !self.enabled(&agent_dir) {
            *lock(&self.hold) = None;
            return self.subscription(model, context, options, sink).await;
        }
        let now = (self.options.clock)();
        let held = self.held_until();
        if let Some(until) = held {
            if now < until
                && let Some(key) = self.api_key(options, &agent_dir)
            {
                return self.api(model, context, options, transport, sink, &key, None).await;
            }
            *lock(&self.hold) = None;
        }
        let mut gate = Gate::new(sink);
        let mut message = {
            let mut forward = |event: &AssistantMessageEvent| gate.event(event);
            self.subscription(model, context, options, &mut forward).await
        };
        let limit = usage_limit_of(&message);
        match limit {
            Some(resets_at) if !gate.committed => match self.api_key(options, &agent_dir) {
                Some(key) => {
                    let until = resets_at.filter(|r| *r > now).unwrap_or(now + DEFAULT_HOLD_MS);
                    *lock(&self.hold) = Some(until);
                    let switched = Switch {
                        until,
                        reason: message.error_message.clone().unwrap_or_default(),
                    };
                    return self
                        .api(model, context, options, transport, gate.sink, &key, Some(switched))
                        .await;
                }
                None => {
                    let error = message.error_message.get_or_insert_default();
                    error.push_str(&format!(" ({KEY_HINT})"));
                }
            },
            _ if held.is_some() => {
                let mut extra = Map::new();
                extra.insert("provider".into(), super::PROVIDER.into());
                message
                    .diagnostics
                    .get_or_insert_default()
                    .insert(0, route("subscription", SUBSCRIPTION_NOTICE.into(), extra));
            }
            _ => {}
        }
        gate.finish(&message);
        message
    }

    /// The call on API credits: same model id, same context, the fallback key.
    #[allow(clippy::too_many_arguments)]
    async fn api(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: &SimpleStreamOptions,
        transport: &dyn Transport,
        sink: &mut EventSink<'_>,
        key: &str,
        switched: Option<Switch>,
    ) -> AssistantMessage {
        let mut api_model = api_model(model);
        if let Some(base) = self.options.api_base_url.clone().or_else(|| self.configured_base_url()) {
            api_model.base_url = base;
        }
        let mut request = options.clone();
        request.base.api_key = Some(key.to_string());
        let mut gate = Gate::new(sink);
        let result = {
            let mut forward = |event: &AssistantMessageEvent| gate.event(event);
            crate::anthropic::stream_simple(&api_model, context, &request, transport, &mut forward).await
        };
        let mut message = result.unwrap_or_else(|error| {
            let mut message = AssistantMessage::pending(&api_model, now_ms());
            message.stop_reason = StopReason::Error;
            message.error_message = Some(error);
            message
        });
        if message.stop_reason == StopReason::Error
            && let Some(error) = message.error_message.clone().filter(|e| billing_exhausted(e))
        {
            let until = self
                .held_until()
                .map(|u| format!(" until {}", iso(u)))
                .unwrap_or_default();
            message.error_message = Some(format!(
                "{BOTH_EXHAUSTED}: the subscription is out of usage{until}, and the API answered: {error}"
            ));
        }
        if let Some(switched) = switched {
            let mut extra = Map::new();
            extra.insert("provider".into(), "anthropic".into());
            extra.insert("model".into(), api_model.id.clone().into());
            extra.insert("until".into(), switched.until.into());
            extra.insert("reason".into(), switched.reason.into());
            message
                .diagnostics
                .get_or_insert_default()
                .insert(0, route("api", api_notice(switched.until), extra));
        }
        gate.finish(&message);
        message
    }
}

struct Switch {
    until: i64,
    reason: String,
}
