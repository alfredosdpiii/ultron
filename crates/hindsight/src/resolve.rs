//! Which Hindsight server and bank Ultron uses, and the identity tags of a session's memory.
//!
//! Every Ultron process on a machine resolves the same server and bank: `ULTRON_HINDSIGHT_URL`, else the global
//! `hindsightUrl` setting (project settings never redirect memory), else `http://localhost:8888`; "off", "none", "0"
//! or "false" (any case) turn memory off. The bank is `ULTRON_HINDSIGHT_BANK`, else "ultron". So the global
//! durable instance and every per-project instance share one memory: a `global` memory is tagged with the home
//! directory's identity and a `project` memory with the project directory's, never with the instance.

use crate::backend::{HindsightBackend, HindsightOptions};
use kernel_apis::MemoryError;
use kernel_apis::util::{js_trim, sha256_hex};
use serde_json::{Map, Value, json};
use std::path::Path;

/// `DEFAULT_HINDSIGHT_URL` (core/defaults.ts).
pub const DEFAULT_HINDSIGHT_URL: &str = "http://localhost:8888";
/// The bank when `ULTRON_HINDSIGHT_BANK` is unset or empty.
pub const DEFAULT_BANK: &str = "ultron";

fn off(value: &str) -> bool {
    matches!(value.to_lowercase().as_str(), "off" | "none" | "0" | "false")
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(js_trim).filter(|v| !v.is_empty())
}

/// `hindsightUrl(configured, setting)` of session-worker.ts, also `hindsightUrlFrom(env, setting, DEFAULT)` of
/// autoreview/lessons.ts: the environment, else the setting, else the default; off: `None`.
pub fn hindsight_url(env: Option<&str>, setting: Option<&str>) -> Option<String> {
    match trimmed(env).or_else(|| trimmed(setting)) {
        None => Some(DEFAULT_HINDSIGHT_URL.into()),
        Some(value) if off(value) => None,
        Some(value) => Some(value.into()),
    }
}

/// `effectiveHindsightUrl` of cli/setup/core.ts: the URL (`None`: off) and where it came from.
pub fn effective_hindsight_url(env: Option<&str>, setting: Option<&str>) -> (Option<String>, &'static str) {
    let source = if trimmed(env).is_some() {
        "env"
    } else if trimmed(setting).is_some() {
        "setting"
    } else {
        "default"
    };
    (hindsight_url(env, setting), source)
}

/// `SettingsManager.getHindsightUrl()`: the global `hindsightUrl` setting, trimmed, when a non-blank string.
pub fn setting_url(manager: &config::settings::SettingsManager) -> Option<String> {
    manager
        .global_settings()
        .get("hindsightUrl")
        .and_then(Value::as_str)
        .map(js_trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// `process.env.ULTRON_HINDSIGHT_BANK || "ultron"`.
pub fn bank(env: Option<&str>) -> String {
    env.filter(|v| !v.is_empty()).unwrap_or(DEFAULT_BANK).to_string()
}

/// A stable, non-reversible id for a directory (`projectIdentity` of worker-services.ts).
pub fn directory_identity(path: &str) -> String {
    sha256_hex(path.as_bytes())[..16].to_string()
}

/// A session's scope tags: its own id, its project directory's identity and the home directory's identity.
pub fn worker_scope_tags(session_id: &str, cwd: &str, home: &str) -> Map<String, Value> {
    let tags = json!({
        "session": [format!("ultron:session:{session_id}")],
        "project": [format!("ultron:project:{}", directory_identity(cwd))],
        "global": [format!("ultron:global:{}", directory_identity(home))],
    });
    tags.as_object().cloned().unwrap_or_default()
}

/// The server and bank this process uses (`None` from [`Endpoint::resolve`]: memory is off).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub bank: String,
}

impl Endpoint {
    /// From `ULTRON_HINDSIGHT_URL`, the global `hindsightUrl` setting under `agent_dir`, and `ULTRON_HINDSIGHT_BANK`.
    pub fn resolve(var: &dyn Fn(&str) -> Option<String>, global_setting: Option<String>) -> Option<Endpoint> {
        let url = hindsight_url(var("ULTRON_HINDSIGHT_URL").as_deref(), global_setting.as_deref())?;
        Some(Endpoint {
            url,
            bank: bank(var("ULTRON_HINDSIGHT_BANK").as_deref()),
        })
    }

    /// Like [`Endpoint::resolve`], reading the global settings file under `agent_dir`.
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>, cwd: &Path, agent_dir: &Path) -> Option<Endpoint> {
        let manager = config::settings::SettingsManager::create(cwd, agent_dir, false);
        Self::resolve(var, setting_url(&manager))
    }

    /// The session worker's backend (`createWorkerServices`): this bank, created on first use, tagged for the
    /// session, its project directory and the home directory.
    pub fn session_backend(&self, session_id: &str, cwd: &str, home: &str) -> Result<HindsightBackend, MemoryError> {
        let mut options = HindsightOptions::new(&self.url, &self.bank, worker_scope_tags(session_id, cwd, home));
        options.ensure_bank = true;
        HindsightBackend::new(options)
    }
}
