//! The settings a session worker applies beyond Pi's typed reads (settings-manager.ts `getRlmModelSettings`,
//! `getReviewModel`, `getWorktreeSettings`, `getHttpIdleTimeoutMs`, `getProviderRetrySettings`, `getTransport`,
//! `getShowHardwareCursor`, `getClearOnShrink`), session-worker.ts `providerRequestOptions`, and the worker's own
//! [`SettingsManager::for_worker`] (`SettingsManager.create(cwd, agentDir, {projectTrusted:
//! workerProjectTrusted(cwd, agentDir)})`).

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::settings::{FRAME_THINKING_LEVELS, Settings, SettingsManager, model_ref};

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` (core/http-dispatcher.ts): five minutes.
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;
/// What a disabled (0) idle timeout becomes as a request timeout (`2147483647`, the largest Node timer).
pub const DISABLED_REQUEST_TIMEOUT_MS: u64 = 2_147_483_647;

/// `getRlmModelSettings`: the models of RLM frames and subagents (global only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RlmModelSettings {
    pub frame_model: Option<String>,
    /// A `claude-code/*` child model is dropped: that provider has no tool calling, and a subagent needs tools.
    pub child_model: Option<String>,
    pub frame_thinking: Option<String>,
}

/// `getWorktreeSettings`: the default setup of subagent worktrees (`worktrees.link`, `.copy`, `.setupCommand`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeSettings {
    pub link: Option<Vec<String>>,
    pub copy: Option<Vec<String>>,
    /// The trimmed `setupCommand`, when not empty.
    pub command: Option<String>,
}

/// `getProviderRetrySettings`: `retry.provider.{timeoutMs, maxRetries, maxRetryDelayMs}` as configured (any JSON
/// value; the cap defaults to 60 s).
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRetrySettings {
    pub timeout_ms: Option<Value>,
    pub max_retries: Option<Value>,
    pub max_retry_delay_ms: Value,
}

fn paths(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `isMergeableObject`.
fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object()
}

/// `getWorktreeSettings` over merged settings.
pub fn worktree_settings(settings: &Settings) -> WorktreeSettings {
    let Some(configured) = object(settings.get("worktrees")) else {
        return WorktreeSettings::default();
    };
    let command = configured
        .get("setupCommand")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string);
    WorktreeSettings {
        link: paths(configured.get("link")),
        copy: paths(configured.get("copy")),
        command,
    }
}

/// `parseHttpIdleTimeoutMs`: a non-negative number (floored), a numeric string, or "disabled" (0).
fn parse_timeout(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            parse_timeout(&json!(trimmed.parse::<f64>().ok()?))
        }
        Value::Number(n) => n
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| n.floor() as u64),
        _ => None,
    }
}

/// `getHttpIdleTimeoutMs`: the setting, else five minutes; an invalid value fails as `parseTimeoutSetting` throws.
pub fn http_idle_timeout_ms(settings: &Settings) -> Result<u64, String> {
    match settings.get("httpIdleTimeoutMs") {
        None => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
        Some(value) => parse_timeout(value).ok_or_else(|| {
            let shown = match value {
                Value::String(s) => s.clone(),
                Value::Null => "null".into(),
                Value::Object(_) => "[object Object]".into(),
                Value::Array(items) => items
                    .iter()
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            format!("Invalid httpIdleTimeoutMs setting: {shown}")
        }),
    }
}

/// `getProviderRetrySettings` over merged settings.
pub fn provider_retry_settings(settings: &Settings) -> ProviderRetrySettings {
    let provider = settings.get("retry").and_then(|r| r.get("provider"));
    // `?.` reads: a `null` value is kept (only `??` skips it).
    let field = |key: &str| provider.and_then(|p| p.get(key)).cloned();
    ProviderRetrySettings {
        timeout_ms: field("timeoutMs"),
        max_retries: field("maxRetries"),
        max_retry_delay_ms: field("maxRetryDelayMs")
            .filter(|v| !v.is_null())
            .unwrap_or(json!(60000)),
    }
}

/// `providerRequestOptions` (session-worker.ts): the stream options of every model request, as the harness keeps
/// them (`{timeoutMs, maxRetries?, maxRetryDelayMs}`): the request timeout is `retry.provider.timeoutMs`, else the
/// HTTP idle timeout (disabled: 2147483647). An invalid idle timeout fails as the TypeScript getter throws.
pub fn provider_request_options(settings: &Settings) -> Result<Value, String> {
    let provider = provider_retry_settings(settings);
    let timeout = match provider.timeout_ms.filter(|v| !v.is_null()) {
        Some(timeout) => timeout,
        None => match http_idle_timeout_ms(settings)? {
            0 => json!(DISABLED_REQUEST_TIMEOUT_MS),
            ms => json!(ms),
        },
    };
    let mut options = Map::new();
    options.insert("timeoutMs".into(), timeout);
    if let Some(retries) = provider.max_retries {
        options.insert("maxRetries".into(), retries);
    }
    options.insert("maxRetryDelayMs".into(), provider.max_retry_delay_ms);
    Ok(Value::Object(options))
}

/// `getShowHardwareCursor`: the setting, else `PI_HARDWARE_CURSOR=1`.
pub fn show_hardware_cursor(settings: &Settings, var: &dyn Fn(&str) -> Option<String>) -> bool {
    match settings.get("showHardwareCursor").and_then(Value::as_bool) {
        Some(on) => on,
        None => var("PI_HARDWARE_CURSOR").as_deref() == Some("1"),
    }
}

/// `getClearOnShrink`: `terminal.clearOnShrink`, else `PI_CLEAR_ON_SHRINK=1`.
pub fn clear_on_shrink(settings: &Settings, var: &dyn Fn(&str) -> Option<String>) -> bool {
    match settings
        .get("terminal")
        .and_then(|t| t.get("clearOnShrink"))
        .and_then(Value::as_bool)
    {
        Some(on) => on,
        None => var("PI_CLEAR_ON_SHRINK").as_deref() == Some("1"),
    }
}

impl SettingsManager {
    /// The session worker's settings: the project's are loaded when [`crate::trust::worker_project_trusted`] says so.
    pub fn for_worker(cwd: &Path, agent_dir: &Path, home: &Path) -> SettingsManager {
        let trusted = crate::trust::worker_project_trusted(cwd, agent_dir, home);
        SettingsManager::create(cwd, agent_dir, trusted)
    }

    /// `getRlmModelSettings` (global only).
    pub fn rlm_model_settings(&self) -> RlmModelSettings {
        let Some(configured) = object(self.global_settings().get("rlm")) else {
            return RlmModelSettings::default();
        };
        let frame_thinking = configured
            .get("frameThinking")
            .and_then(Value::as_str)
            .filter(|level| FRAME_THINKING_LEVELS.contains(level))
            .map(str::to_string);
        RlmModelSettings {
            frame_model: model_ref(configured.get("frameModel")),
            child_model: model_ref(configured.get("childModel")).filter(|m| !m.starts_with("claude-code/")),
            frame_thinking,
        }
    }

    /// `getReviewModel`: `review.model` (global only).
    pub fn review_model(&self) -> Option<String> {
        model_ref(object(self.global_settings().get("review"))?.get("model"))
    }

    pub fn worktree_settings(&self) -> WorktreeSettings {
        worktree_settings(self.settings())
    }

    pub fn http_idle_timeout_ms(&self) -> Result<u64, String> {
        http_idle_timeout_ms(self.settings())
    }

    pub fn provider_retry_settings(&self) -> ProviderRetrySettings {
        provider_retry_settings(self.settings())
    }

    pub fn provider_request_options(&self) -> Result<Value, String> {
        provider_request_options(self.settings())
    }

    pub fn show_hardware_cursor(&self, var: &dyn Fn(&str) -> Option<String>) -> bool {
        show_hardware_cursor(self.settings(), var)
    }

    pub fn clear_on_shrink(&self, var: &dyn Fn(&str) -> Option<String>) -> bool {
        clear_on_shrink(self.settings(), var)
    }
}
