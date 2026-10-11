//! `sandbox.*` settings: `mode` (`off` | `auto` | `required`, default `auto`), `writable` (extra writable paths),
//! `readable` (extra read-only paths), `hidden` (paths replaced by empty ones), `network` (default true), `env`
//! (extra environment variable names passed in), `image` (Docker image; Docker is used only with one).

use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Commands run as before: no isolation, the host's environment.
    Off,
    /// The strongest mechanism that works; without one, commands run unisolated (reported as `none`).
    #[default]
    Auto,
    /// The strongest mechanism that works; without one, commands are refused.
    Required,
}

impl Mode {
    pub fn parse(value: &Value) -> Option<Mode> {
        match value.as_str()?.trim().to_lowercase().as_str() {
            "off" => Some(Mode::Off),
            "auto" => Some(Mode::Auto),
            "required" => Some(Mode::Required),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Auto => "auto",
            Mode::Required => "required",
        }
    }
}

/// Home-relative paths that stay writable by default: package caches that builds write.
pub const DEFAULT_HOME_WRITABLE: &[&str] = &[".cache", ".npm", ".cargo/registry", ".cargo/git"];

/// Home-relative paths hidden by default: credentials and agent state.
pub const DEFAULT_HIDDEN: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".docker",
    ".kube",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".ultron",
    ".pi",
    ".claude",
    ".codex",
];

/// Environment variables passed into the sandbox (besides `sandbox.env`); `LC_*` too.
pub const DEFAULT_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "FORCE_COLOR",
    "TZ",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "PYENV_ROOT",
    "NVM_DIR",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mode: Mode,
    pub writable: Vec<String>,
    pub readable: Vec<String>,
    pub hidden: Vec<String>,
    pub network: bool,
    pub env: Vec<String>,
    pub image: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::Auto,
            writable: DEFAULT_HOME_WRITABLE.iter().map(|p| format!("~/{p}")).collect(),
            readable: Vec::new(),
            hidden: DEFAULT_HIDDEN.iter().map(|p| format!("~/{p}")).collect(),
            network: true,
            env: Vec::new(),
            image: None,
        }
    }
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

impl Settings {
    /// From the merged settings object's `sandbox` key. `writable` and `hidden` replace the defaults when set.
    pub fn from_settings(settings: &Value) -> Self {
        let mut out = Settings::default();
        let Some(sandbox) = settings.get("sandbox").and_then(Value::as_object) else {
            return out;
        };
        if let Some(mode) = sandbox.get("mode").and_then(Mode::parse) {
            out.mode = mode;
        }
        if let Some(writable) = strings(sandbox.get("writable")) {
            out.writable = writable;
        }
        if let Some(readable) = strings(sandbox.get("readable")) {
            out.readable = readable;
        }
        if let Some(hidden) = strings(sandbox.get("hidden")) {
            out.hidden = hidden;
        }
        if let Some(Value::Bool(network)) = sandbox.get("network") {
            out.network = *network;
        }
        if let Some(env) = strings(sandbox.get("env")) {
            out.env = env;
        }
        out.image = sandbox
            .get("image")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        out
    }
}

/// `~` and `~/x` against `home`, relative paths against `cwd`, then `.`/`..` folded lexically.
pub fn expand(path: &str, home: &Path, cwd: &Path) -> PathBuf {
    let joined = if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        cwd.join(path)
    };
    normalize(&joined)
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}
