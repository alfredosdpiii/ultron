//! Agent directory and per-user config paths (`config.ts`), plus Node's lexical path rules they rely on.

use std::path::{Component, Path, PathBuf};

/// Project-local config directory (`.pi`, upstream-compatible).
pub const CONFIG_DIR_NAME: &str = ".pi";
/// User config root under the home directory.
pub const AGENT_CONFIG_DIR_NAME: &str = ".ultron";
/// `${APP_NAME.toUpperCase()}_CODING_AGENT_DIR` for the `ultron` app name.
pub const ENV_AGENT_DIR: &str = "ULTRON_CODING_AGENT_DIR";
/// `${APP_NAME.toUpperCase()}_CODING_AGENT_SESSION_DIR`.
pub const ENV_SESSION_DIR: &str = "ULTRON_CODING_AGENT_SESSION_DIR";

/// Node's `path.normalize` for POSIX paths, without the trailing-slash rule.
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = path.has_root();
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..".as_ref());
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let mut out = PathBuf::from(if absolute { "/" } else { "" });
    for part in parts {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `normalizePath`: `~` and `~/x` expand to the home directory; `file://` URLs become paths.
pub fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return normalize(&home.join(rest));
    }
    if let Some(rest) = input.strip_prefix("file://") {
        return PathBuf::from(percent_decode(rest));
    }
    PathBuf::from(input)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = input.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `resolvePath(input, base)`: tilde expansion, then Node's `path.resolve`.
pub fn resolve(input: &str, base: &Path, home: &Path) -> PathBuf {
    let expanded = expand_tilde(input, home);
    if expanded.is_absolute() {
        normalize(&expanded)
    } else {
        normalize(&base.join(expanded))
    }
}

/// The user's agent directory: `$ULTRON_CODING_AGENT_DIR` (tilde-expanded) when set and non-empty, else
/// `~/.ultron/agent`. `env` looks up an environment variable.
pub fn agent_dir(env: impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    match env(ENV_AGENT_DIR) {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir, home),
        _ => home.join(AGENT_CONFIG_DIR_NAME).join("agent"),
    }
}

/// Files and directories under the agent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub agent_dir: PathBuf,
}

impl AgentPaths {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }
    fn join(&self, name: &str) -> PathBuf {
        normalize(&self.agent_dir.join(name))
    }
    pub fn custom_themes_dir(&self) -> PathBuf {
        self.join("themes")
    }
    pub fn models(&self) -> PathBuf {
        self.join("models.json")
    }
    pub fn auth(&self) -> PathBuf {
        self.join("auth.json")
    }
    pub fn settings(&self) -> PathBuf {
        self.join("settings.json")
    }
    pub fn tools_dir(&self) -> PathBuf {
        self.join("tools")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.join("bin")
    }
    pub fn prompts_dir(&self) -> PathBuf {
        self.join("prompts")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.join("sessions")
    }
    pub fn debug_log(&self) -> PathBuf {
        self.join("ultron-debug.log")
    }
    pub fn trust(&self) -> PathBuf {
        self.join("trust.json")
    }
}
