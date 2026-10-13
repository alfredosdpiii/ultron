//! The global `claudeCode` settings (`getClaudeCodeSettings` of settings-manager.ts), plus the Rust-only
//! `claudeCode.skipPermissions` (see [`crate::claude_cli::skip_permissions_args`]).

use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeCodeSettings {
    pub frame_model: Option<String>,
    pub children: Option<String>,
    pub child_model: Option<String>,
    pub ultron_child_model: Option<String>,
    pub model: Option<String>,
    pub root: bool,
    /// `false` leaves `--dangerously-skip-permissions` out of every Claude Code process Ultron starts.
    pub skip_permissions: Option<bool>,
}

impl ClaudeCodeSettings {
    /// From the global settings object (project settings never configure Claude Code mode).
    pub fn from_global(global: &serde_json::Map<String, Value>) -> ClaudeCodeSettings {
        let Some(Value::Object(configured)) = global.get("claudeCode") else {
            return ClaudeCodeSettings::default();
        };
        let text = |key: &str| {
            configured
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        ClaudeCodeSettings {
            frame_model: text("frameModel"),
            children: configured
                .get("children")
                .and_then(Value::as_str)
                .filter(|v| *v == "claude" || *v == "ultron")
                .map(str::to_string),
            child_model: text("childModel"),
            ultron_child_model: text("ultronChildModel"),
            model: text("model"),
            root: configured.get("root") == Some(&Value::Bool(true)),
            skip_permissions: configured.get("skipPermissions").and_then(Value::as_bool),
        }
    }

    pub fn load(cwd: &Path, agent_dir: &Path) -> ClaudeCodeSettings {
        let manager = config::settings::SettingsManager::create(cwd, agent_dir, false);
        ClaudeCodeSettings::from_global(manager.global_settings())
    }
}
