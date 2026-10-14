//! Where extension processes come from: `extension.json` manifests found in the locations the TypeScript product
//! loads extensions from (`core/package-manager.ts` `resolve`), in its order:
//!
//! 1. `extensions` entries in the project's `.pi/settings.json` (trusted projects only; relative to `.pi/`),
//! 2. `extensions` entries in the agent directory's `settings.json` (relative to the agent directory),
//! 3. `<cwd>/.pi/extensions/` (trusted projects only),
//! 4. `<agentDir>/extensions/`.
//!
//! An entry is a manifest file, a directory holding `extension.json`, or a directory whose subdirectories hold one
//! (one level, like `discoverExtensionsInDir`). Duplicates (same resolved manifest) load once, at their first place.
//! JavaScript/TypeScript extensions (`*.ts`, `*.js`, `package.json` `pi.extensions`, `index.ts`) are no longer
//! loaded: each one found yields a diagnostic instead.

use config::paths::{CONFIG_DIR_NAME, resolve};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MANIFEST_FILE: &str = "extension.json";
pub const DEFAULT_EVENT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_MAX_RESTARTS: u32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Absolute path of the manifest file; errors are reported under it.
    pub path: PathBuf,
    /// The manifest's directory: the process runs there.
    pub dir: PathBuf,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// How long one event handler may take before it counts as failed.
    pub event_timeout: Duration,
    /// How long spawning plus `initialize` may take.
    pub startup_timeout: Duration,
    /// Restarts allowed after the process dies, before the extension is disabled for the session.
    pub max_restarts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Discovery {
    pub manifests: Vec<Manifest>,
    pub diagnostics: Vec<Diagnostic>,
}

fn duration_ms(map: &Map<String, Value>, key: &str, default: Duration) -> Result<Duration, String> {
    match map.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .map(Duration::from_millis)
            .ok_or_else(|| format!("\"{key}\" must be a non-negative integer of milliseconds")),
    }
}

/// Parses an `extension.json`.
pub fn parse_manifest(path: &Path, text: &str) -> Result<Manifest, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let Value::Object(map) = value else {
        return Err("manifest must be a JSON object".into());
    };
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let command = map
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .ok_or("\"command\" must be a non-empty string")?;
    // A relative command with a slash is relative to the manifest; a bare name is looked up on PATH.
    let command = if command.contains('/') && !Path::new(command).is_absolute() {
        dir.join(command).to_string_lossy().into_owned()
    } else {
        command.to_string()
    };
    let args = match map.get("args") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|a| {
                a.as_str()
                    .map(str::to_string)
                    .ok_or("\"args\" must be an array of strings")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("\"args\" must be an array of strings".into()),
    };
    let env = match map.get("env") {
        None => Vec::new(),
        Some(Value::Object(vars)) => vars
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|v| (k.clone(), v.to_string()))
                    .ok_or("\"env\" values must be strings")
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("\"env\" must be an object of strings".into()),
    };
    let name = match map.get("name") {
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        Some(_) => return Err("\"name\" must be a non-empty string".into()),
    };
    let max_restarts = match map.get("maxRestarts") {
        None => DEFAULT_MAX_RESTARTS,
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or("\"maxRestarts\" must be a non-negative integer")?,
    };
    Ok(Manifest {
        path: path.to_path_buf(),
        dir,
        name,
        command,
        args,
        env,
        event_timeout: duration_ms(&map, "eventTimeoutMs", DEFAULT_EVENT_TIMEOUT)?,
        startup_timeout: duration_ms(&map, "startupTimeoutMs", DEFAULT_STARTUP_TIMEOUT)?,
        max_restarts,
    })
}

fn is_js_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "ts" || e == "js")
}

fn is_js_package(dir: &Path) -> bool {
    dir.join("index.ts").is_file() || dir.join("index.js").is_file() || {
        let package = dir.join("package.json");
        std::fs::read_to_string(package)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|v| v.get("pi").and_then(|pi| pi.get("extensions")).is_some())
    }
}

fn js_diagnostic(path: &Path) -> Diagnostic {
    Diagnostic {
        path: path.to_path_buf(),
        message: format!(
            "JavaScript/TypeScript extensions are no longer loaded ({}); run it as an extension process with an {MANIFEST_FILE} (see crates/extensions/PROTOCOL.md)",
            path.display()
        ),
    }
}

struct Collector {
    out: Discovery,
}

impl Collector {
    fn load(&mut self, manifest_path: &Path) {
        if self.out.manifests.iter().any(|m| m.path == manifest_path) {
            return;
        }
        match std::fs::read_to_string(manifest_path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse_manifest(manifest_path, &text))
        {
            Ok(manifest) => self.out.manifests.push(manifest),
            Err(message) => self.out.diagnostics.push(Diagnostic {
                path: manifest_path.to_path_buf(),
                message: format!("Invalid extension manifest: {message}"),
            }),
        }
    }

    /// One directory entry: its own manifest, or a JavaScript package to report. True when it was one of those.
    fn entry_dir(&mut self, dir: &Path) -> bool {
        let manifest = dir.join(MANIFEST_FILE);
        if manifest.is_file() {
            self.load(&manifest);
            true
        } else if is_js_package(dir) {
            self.out.diagnostics.push(js_diagnostic(dir));
            true
        } else {
            false
        }
    }

    /// `discoverExtensionsInDir`: direct `*.json` manifests are not picked up (settings files live beside them);
    /// subdirectories with a manifest are, one level deep.
    fn scan_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                self.entry_dir(&path);
            } else if is_js_file(&path) {
                self.out.diagnostics.push(js_diagnostic(&path));
            }
        }
    }

    fn settings_entry(&mut self, entry: &str, base: &Path, home: &Path) {
        if entry.starts_with(['!', '+', '-']) || entry.contains(['*', '?', '[']) {
            self.out.diagnostics.push(Diagnostic {
                path: base.to_path_buf(),
                message: format!(
                    "Extension pattern \"{entry}\" is not supported for extension processes; list manifest paths"
                ),
            });
            return;
        }
        let path = resolve(entry, base, home);
        if path.is_dir() {
            if !self.entry_dir(&path) {
                self.scan_dir(&path);
            }
        } else if path.is_file() && is_js_file(&path) {
            self.out.diagnostics.push(js_diagnostic(&path));
        } else if path.is_file() {
            self.load(&path);
        } else {
            self.out.diagnostics.push(Diagnostic {
                path: path.clone(),
                message: format!("Extension path does not exist: {}", path.display()),
            });
        }
    }
}

fn settings_entries(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("extensions")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Finds extension manifests for a session in `cwd`. `global` and `project` are the two settings files' contents;
/// `project` is ignored unless `project_trusted`.
pub fn discover(
    cwd: &Path,
    agent_dir: &Path,
    home: &Path,
    global: &Map<String, Value>,
    project: &Map<String, Value>,
    project_trusted: bool,
) -> Discovery {
    let mut collector = Collector {
        out: Discovery::default(),
    };
    // Pi's precedence order (`resourcePrecedenceRank`): project entries, the project's directory, profile entries,
    // the profile's directory; a manifest found twice keeps its first place.
    let project_base = cwd.join(CONFIG_DIR_NAME);
    if project_trusted {
        for entry in settings_entries(project) {
            collector.settings_entry(&entry, &project_base, home);
        }
        collector.scan_dir(&project_base.join("extensions"));
    }
    for entry in settings_entries(global) {
        collector.settings_entry(&entry, agent_dir, home);
    }
    collector.scan_dir(&agent_dir.join("extensions"));
    collector.out
}

/// `discover` with the settings a `config::settings::SettingsManager` loaded.
pub fn discover_with_settings(
    cwd: &Path,
    agent_dir: &Path,
    home: &Path,
    settings: &config::settings::SettingsManager,
) -> Discovery {
    discover(
        cwd,
        agent_dir,
        home,
        settings.global_settings(),
        settings.project_settings(),
        settings.is_project_trusted(),
    )
}
