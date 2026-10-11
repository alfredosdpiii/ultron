//! Project trust (`trust-manager.ts`, `project-trust.ts`): `<agent>/trust.json` maps canonical directories to
//! `true`/`false`; a decision applies to every directory below it.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::json::{parse, stringify_pretty};
use crate::lock::{SYNC_ATTEMPTS, SYNC_STALE, acquire};
use crate::paths::{CONFIG_DIR_NAME, normalize};

const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: [&str; 8] = [
    "settings.json",
    "mcp.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// `canonicalizePath(resolvePath(cwd))`.
pub fn normalize_cwd(cwd: &Path) -> PathBuf {
    let absolute = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(cwd)
    };
    let resolved = normalize(&absolute);
    fs::canonicalize(&resolved).unwrap_or(resolved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustUpdate {
    pub path: PathBuf,
    /// `None` removes the stored decision.
    pub decision: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustOption {
    pub label: String,
    pub trusted: bool,
    pub updates: Vec<TrustUpdate>,
    pub saved_path: Option<PathBuf>,
}

pub fn parent_path(cwd: &Path) -> Option<PathBuf> {
    normalize_cwd(cwd).parent().map(Path::to_path_buf)
}

/// The choices of the trust prompt, in display order.
pub fn trust_options(cwd: &Path, include_session_only: bool) -> Vec<TrustOption> {
    let path = normalize_cwd(cwd);
    let update = |path: &Path, decision| TrustUpdate {
        path: path.to_path_buf(),
        decision,
    };
    let mut options = vec![TrustOption {
        label: "Trust".into(),
        trusted: true,
        updates: vec![update(&path, Some(true))],
        saved_path: Some(path.clone()),
    }];
    if let Some(parent) = parent_path(cwd) {
        options.push(TrustOption {
            label: format!("Trust parent folder ({})", parent.display()),
            trusted: true,
            updates: vec![update(&parent, Some(true)), update(&path, None)],
            saved_path: Some(parent),
        });
    }
    if include_session_only {
        options.push(TrustOption {
            label: "Trust (this session only)".into(),
            trusted: true,
            updates: vec![],
            saved_path: None,
        });
    }
    options.push(TrustOption {
        label: "Do not trust".into(),
        trusted: false,
        updates: vec![update(&path, Some(false))],
        saved_path: Some(path),
    });
    if include_session_only {
        options.push(TrustOption {
            label: "Do not trust (this session only)".into(),
            trusted: false,
            updates: vec![],
            saved_path: None,
        });
    }
    options
}

/// Whether `cwd` has project resources that need trust: a `.pi` entry, or any `.agents/skills` from `cwd` up to
/// the root other than the user's own `~/.agents/skills`.
pub fn has_trust_requiring_project_resources(cwd: &Path, home: &Path) -> bool {
    let user_skills = normalize_cwd(home).join(".agents").join("skills");
    let mut dir = normalize_cwd(cwd);
    let config_dir = dir.join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }
    loop {
        let skills = dir.join(".agents").join("skills");
        if skills != user_skills && skills.exists() {
            return true;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return false,
        }
    }
}

pub struct ProjectTrustStore {
    path: PathBuf,
}

impl ProjectTrustStore {
    pub fn new(agent_dir: &Path) -> Self {
        let agent_dir = if agent_dir.is_absolute() {
            normalize(agent_dir)
        } else {
            normalize(&std::env::current_dir().unwrap_or_default().join(agent_dir))
        };
        ProjectTrustStore {
            path: agent_dir.join("trust.json"),
        }
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let dir = self.path.parent().expect("trust.json has a parent");
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut lock = self.path.as_os_str().to_os_string();
        lock.push(".lock");
        let _lock = acquire(Path::new(&lock), SYNC_STALE, SYNC_ATTEMPTS).map_err(|e| e.to_string())?;
        f()
    }

    fn read(&self) -> Result<Map<String, Value>, String> {
        let display = self.path.display();
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(format!("Failed to read trust store {display}: {e}")),
        };
        let parsed = parse(&content).map_err(|e| format!("Failed to read trust store {display}: {e}"))?;
        let Value::Object(data) = parsed else {
            return Err(format!("Invalid trust store {display}: expected an object"));
        };
        for (key, value) in &data {
            if !(value.is_boolean() || value.is_null()) {
                let key = Value::String(key.clone());
                return Err(format!(
                    "Invalid trust store {display}: value for {key} must be true, false, or null"
                ));
            }
        }
        Ok(data)
    }

    /// The nearest stored decision at or above `cwd`, with the directory it was stored for.
    pub fn get_entry(&self, cwd: &Path) -> Result<Option<(PathBuf, bool)>, String> {
        self.with_lock(|| {
            let data = self.read()?;
            let mut dir = normalize_cwd(cwd);
            loop {
                if let Some(Value::Bool(decision)) = data.get(&*dir.to_string_lossy()) {
                    return Ok(Some((dir, *decision)));
                }
                match dir.parent() {
                    Some(parent) => dir = parent.to_path_buf(),
                    None => return Ok(None),
                }
            }
        })
    }

    pub fn get(&self, cwd: &Path) -> Result<Option<bool>, String> {
        Ok(self.get_entry(cwd)?.map(|(_, decision)| decision))
    }

    pub fn set(&self, cwd: &Path, decision: Option<bool>) -> Result<(), String> {
        self.set_many(&[TrustUpdate {
            path: cwd.to_path_buf(),
            decision,
        }])
    }

    /// Apply the updates and rewrite the file with sorted keys and a trailing newline.
    pub fn set_many(&self, updates: &[TrustUpdate]) -> Result<(), String> {
        self.with_lock(|| {
            let mut data = self.read()?;
            for update in updates {
                let key = normalize_cwd(&update.path).to_string_lossy().into_owned();
                match update.decision {
                    Some(decision) => data.insert(key, decision.into()),
                    None => data.shift_remove(&key),
                };
            }
            let mut keys: Vec<&String> = data.keys().collect();
            // JavaScript's default sort compares UTF-16 code units.
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let sorted: Map<String, Value> = keys.into_iter().map(|k| (k.clone(), data[k].clone())).collect();
            let text = format!("{}\n", stringify_pretty(&Value::Object(sorted)));
            fs::write(&self.path, text).map_err(|e| e.to_string())
        })
    }
}

/// Outcome of `resolveProjectTrusted` before extensions and the interactive prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustResolution {
    Trusted(bool),
    /// No stored decision and `defaultProjectTrust` is `ask`: show the prompt (or refuse without a UI).
    Ask,
}

/// `resolveProjectTrusted` without project-trust extensions: override, then "nothing to trust", then the stored
/// decision, then `defaultProjectTrust`.
pub fn resolve_project_trusted(
    cwd: &Path,
    home: &Path,
    store: &ProjectTrustStore,
    trust_override: Option<bool>,
    default_project_trust: &str,
) -> Result<TrustResolution, String> {
    if let Some(trusted) = trust_override {
        return Ok(TrustResolution::Trusted(trusted));
    }
    if !has_trust_requiring_project_resources(cwd, home) {
        return Ok(TrustResolution::Trusted(true));
    }
    if let Some(decision) = store.get(cwd)? {
        return Ok(TrustResolution::Trusted(decision));
    }
    Ok(match default_project_trust {
        "always" => TrustResolution::Trusted(true),
        "never" => TrustResolution::Trusted(false),
        _ => TrustResolution::Ask,
    })
}
