//! Typed reads of the settings keys the session, the host and the TUI apply (settings-manager.ts `getShellPath`,
//! `getShellCommandPrefix`, `getThinkingBudgets`, `getWebSocketConnectTimeoutMs`, `getTerminalCapabilityOverrides`,
//! `getCodeBlockIndent`, `getLastChangelogVersion`, `getWarnings`, `getDefaultTools`), over a merged settings object
//! so a composed session (which keeps its settings as JSON) reads them as a [`SettingsManager`] does.

use std::path::Path;

use serde_json::Value;

use crate::paths::expand_tilde;
use crate::settings::{Settings, SettingsManager};

/// `getTerminalCapabilityOverrides`: what the settings force over the detected terminal capabilities. `images` is
/// `Some(None)` for `false` (no inline images), `Some(Some(protocol))` for `"kitty"`/`"iterm2"`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub images: Option<Option<&'static str>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

/// `shellPath` with `~` expanded (`normalizePath`); an empty string stays empty (it selects the default shell).
pub fn shell_path(settings: &Settings, home: &Path) -> Option<String> {
    match settings.get("shellPath")?.as_str()? {
        "" => Some(String::new()),
        path => Some(expand_tilde(path, home).to_string_lossy().into_owned()),
    }
}

/// `shellCommandPrefix`: prepended (with a newline) to a user `!` command.
pub fn shell_command_prefix(settings: &Settings) -> Option<String> {
    settings.get("shellCommandPrefix")?.as_str().map(str::to_string)
}

/// `thinkingBudgets` as configured.
pub fn thinking_budgets(settings: &Settings) -> Option<&Value> {
    settings.get("thinkingBudgets").filter(|v| !v.is_null())
}

/// `websocketConnectTimeoutMs` (`parseTimeoutSetting`): a non-negative number (floored), a numeric string, or
/// `"disabled"` (0); absent is `None`; anything else fails as TypeScript throws.
pub fn websocket_connect_timeout_ms(settings: &Settings) -> Result<Option<f64>, String> {
    let Some(value) = settings.get("websocketConnectTimeoutMs") else {
        return Ok(None);
    };
    parse_timeout(value)
        .map(Some)
        .ok_or_else(|| format!("Invalid websocketConnectTimeoutMs setting: {}", js_string(value)))
}

/// `parseHttpIdleTimeoutMs`.
fn parse_timeout(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0.0);
            }
            if trimmed.is_empty() {
                return None;
            }
            // `Number(trimmed)`: decimal, exponent, hex/octal/binary literals and Infinity are what JS accepts; a
            // finite non-negative decimal is all the setting is documented to take.
            trimmed
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(f64::floor)
        }
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(f64::floor),
        _ => None,
    }
}

/// `String(value)` for a JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `getTerminalCapabilityOverrides`.
pub fn terminal_capability_overrides(settings: &Settings) -> CapabilityOverrides {
    let terminal = settings.get("terminal");
    let field = |key: &str| terminal.and_then(|t| t.get(key));
    CapabilityOverrides {
        images: match field("images") {
            Some(Value::String(s)) if s == "kitty" => Some(Some("kitty")),
            Some(Value::String(s)) if s == "iterm2" => Some(Some("iterm2")),
            Some(Value::Bool(false)) => Some(None),
            _ => None,
        },
        true_color: field("trueColor").and_then(Value::as_bool),
        hyperlinks: field("hyperlinks").and_then(Value::as_bool),
    }
}

/// `markdown.codeBlockIndent`, default two spaces.
pub fn code_block_indent(settings: &Settings) -> String {
    settings
        .get("markdown")
        .and_then(|m| m.get("codeBlockIndent"))
        .and_then(Value::as_str)
        .unwrap_or("  ")
        .to_string()
}

/// `warnings.anthropicExtraUsage`: only an explicit `false` turns the subscription-auth warning off.
pub fn anthropic_extra_usage_warning(settings: &Settings) -> bool {
    settings.get("warnings").and_then(|w| w.get("anthropicExtraUsage")) != Some(&Value::Bool(false))
}

/// `defaultTools`.
pub fn default_tools(settings: &Settings) -> Option<Vec<String>> {
    let tools = settings.get("defaultTools")?.as_array()?;
    Some(tools.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

impl SettingsManager {
    pub fn shell_path(&self, home: &Path) -> Option<String> {
        shell_path(self.settings(), home)
    }
    pub fn shell_command_prefix(&self) -> Option<String> {
        shell_command_prefix(self.settings())
    }
    pub fn thinking_budgets(&self) -> Option<&Value> {
        thinking_budgets(self.settings())
    }
    pub fn websocket_connect_timeout_ms(&self) -> Result<Option<f64>, String> {
        websocket_connect_timeout_ms(self.settings())
    }
    pub fn terminal_capability_overrides(&self) -> CapabilityOverrides {
        terminal_capability_overrides(self.settings())
    }
    pub fn code_block_indent(&self) -> String {
        code_block_indent(self.settings())
    }
    pub fn anthropic_extra_usage_warning(&self) -> bool {
        anthropic_extra_usage_warning(self.settings())
    }
    pub fn default_tools(&self) -> Option<Vec<String>> {
        default_tools(self.settings())
    }
    /// `getLastChangelogVersion` (merged: a project value is seen too).
    pub fn last_changelog_version(&self) -> Option<String> {
        self.settings()
            .get("lastChangelogVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
    /// `setLastChangelogVersion`: written to the global settings.
    pub fn set_last_changelog_version(&mut self, version: &str) {
        self.set_global("lastChangelogVersion", Some(version.into()));
    }
}
