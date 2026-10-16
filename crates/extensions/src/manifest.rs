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
//! Settings entries may also be patterns, applied as the package manager applies them to an extension's entry file
//! (here its `extension.json`): plain globs and `!glob` filter a scope's listed manifests, `+path` / `-path` force
//! one in or out, and the `!`/`+`/`-` entries also apply to that scope's auto-discovered directory.
//! JavaScript/TypeScript extensions (`*.ts`, `*.js`, `package.json` `pi.extensions`, `index.ts`) are no longer
//! loaded: each one found yields a diagnostic instead.

mod cli_paths;
pub use cli_paths::{discover_cli_paths, is_local_path, merge_discoveries};

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
    /// Manifests the settings' patterns turned off (`!glob`, `-path`, or not matched by a plain glob), in the order
    /// they were found; they are not loaded.
    pub disabled: Vec<PathBuf>,
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

/// `isPattern` of package-manager.ts: an override (`!`, `+`, `-`) or a glob (`*`, `?`).
fn is_pattern(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-']) || entry.contains(['*', '?'])
}

/// `path.relative(from, to)` for absolute paths, `/`-separated (`""` for the same path).
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(
        to[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// `matchesAnyPattern`: a glob matching the path relative to `base`, its file name, or the whole path.
fn matches_any_pattern(path: &Path, patterns: &[&str], base: &Path) -> bool {
    let rel = relative(base, path);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let full = path.to_string_lossy();
    patterns.iter().any(|pattern| {
        crate::glob::minimatch(&rel, pattern)
            || crate::glob::minimatch(&name, pattern)
            || crate::glob::minimatch(&full, pattern)
    })
}

/// `matchesAnyExactPattern`: the path relative to `base` (a leading `./` ignored) or the whole path.
fn matches_any_exact_pattern(path: &Path, patterns: &[&str], base: &Path) -> bool {
    let rel = relative(base, path);
    let full = path.to_string_lossy();
    patterns.iter().any(|pattern| {
        let normalized = pattern.strip_prefix("./").unwrap_or(pattern);
        normalized == rel || normalized == full
    })
}

/// `applyPatterns`: plain patterns include (all when there are none), `!` excludes, `+path` adds an exact path back,
/// `-path` removes one whatever else said. Returns the enabled paths.
pub fn apply_patterns(paths: &[PathBuf], patterns: &[&str], base: &Path) -> Vec<PathBuf> {
    let (mut includes, mut excludes, mut force_in, mut force_out) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for pattern in patterns {
        if let Some(p) = pattern.strip_prefix('+') {
            force_in.push(p);
        } else if let Some(p) = pattern.strip_prefix('-') {
            force_out.push(p);
        } else if let Some(p) = pattern.strip_prefix('!') {
            excludes.push(p);
        } else {
            includes.push(*pattern);
        }
    }
    let mut result: Vec<PathBuf> = if includes.is_empty() {
        paths.to_vec()
    } else {
        paths
            .iter()
            .filter(|p| matches_any_pattern(p, &includes, base))
            .cloned()
            .collect()
    };
    if !excludes.is_empty() {
        result.retain(|p| !matches_any_pattern(p, &excludes, base));
    }
    for path in paths {
        if !force_in.is_empty() && !result.contains(path) && matches_any_exact_pattern(path, &force_in, base) {
            result.push(path.clone());
        }
    }
    if !force_out.is_empty() {
        result.retain(|p| !matches_any_exact_pattern(p, &force_out, base));
    }
    result
}

/// `isEnabledByOverrides`: an auto-discovered path is enabled unless a `!` glob matches it; `+path` enables it again,
/// `-path` disables it.
pub fn is_enabled_by_overrides(path: &Path, entries: &[String], base: &Path) -> bool {
    let tagged = |prefix: char| -> Vec<&str> { entries.iter().filter_map(|e| e.strip_prefix(prefix)).collect() };
    let (excludes, force_in, force_out) = (tagged('!'), tagged('+'), tagged('-'));
    let mut enabled = true;
    if !excludes.is_empty() && matches_any_pattern(path, &excludes, base) {
        enabled = false;
    }
    if !force_in.is_empty() && matches_any_exact_pattern(path, &force_in, base) {
        enabled = true;
    }
    if !force_out.is_empty() && matches_any_exact_pattern(path, &force_out, base) {
        enabled = false;
    }
    enabled
}

impl Collector {
    /// `addResource`: the first place a manifest is found decides whether it loads; a disabled one keeps its place.
    fn load(&mut self, manifest_path: &Path, enabled: bool) {
        if self.out.manifests.iter().any(|m| m.path == manifest_path)
            || self.out.disabled.iter().any(|d| d == manifest_path)
        {
            return;
        }
        if !enabled {
            self.out.disabled.push(manifest_path.to_path_buf());
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

    /// One directory entry: its own manifest, or a JavaScript package to report; `None` for anything else.
    fn entry_dir(dir: &Path) -> Option<Result<PathBuf, Diagnostic>> {
        let manifest = dir.join(MANIFEST_FILE);
        if manifest.is_file() {
            Some(Ok(manifest))
        } else if is_js_package(dir) {
            Some(Err(js_diagnostic(dir)))
        } else {
            None
        }
    }

    /// `discoverExtensionsInDir`: direct `*.json` manifests are not picked up (settings files live beside them);
    /// subdirectories with a manifest are, one level deep. Manifests and JavaScript extensions to report, in sorted
    /// order.
    fn scan_dir(dir: &Path) -> Vec<Result<PathBuf, Diagnostic>> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        paths
            .into_iter()
            .filter_map(|path| {
                if path.is_dir() {
                    Self::entry_dir(&path)
                } else if is_js_file(&path) {
                    Some(Err(js_diagnostic(&path)))
                } else {
                    None
                }
            })
            .collect()
    }

    /// The manifests one plain settings entry names (`collectFilesFromPaths`).
    fn settings_entry(&mut self, entry: &str, base: &Path, home: &Path) -> Vec<PathBuf> {
        let path = resolve(entry, base, home);
        let mut found = Vec::new();
        if path.is_dir() {
            let items = match Self::entry_dir(&path) {
                Some(item) => vec![item],
                None => Self::scan_dir(&path),
            };
            for item in items {
                match item {
                    Ok(manifest) => found.push(manifest),
                    Err(diagnostic) => self.out.diagnostics.push(diagnostic),
                }
            }
        } else if path.is_file() && is_js_file(&path) {
            self.out.diagnostics.push(js_diagnostic(&path));
        } else if path.is_file() {
            found.push(path);
        } else {
            self.out.diagnostics.push(Diagnostic {
                path: path.clone(),
                message: format!("Extension path does not exist: {}", path.display()),
            });
        }
        found
    }

    /// `resolveLocalEntries`: the manifests the plain entries name, enabled by the scope's patterns
    /// ([`apply_patterns`]: plain globs filter, `!`/`+`/`-` override), relative to the settings' directory.
    fn settings_scope(&mut self, entries: &[String], base: &Path, home: &Path) {
        let (patterns, plain): (Vec<&String>, Vec<&String>) = entries.iter().partition(|e| is_pattern(e));
        let mut found = Vec::new();
        for entry in plain {
            found.extend(self.settings_entry(entry, base, home));
        }
        let patterns: Vec<&str> = patterns.iter().map(|p| p.as_str()).collect();
        let enabled = apply_patterns(&found, &patterns, base);
        for path in found {
            let on = enabled.contains(&path);
            self.load(&path, on);
        }
    }

    /// An auto-discovered extensions directory, each manifest enabled by the scope's override entries
    /// ([`is_enabled_by_overrides`]) relative to `base`.
    fn auto_dir(&mut self, dir: &Path, entries: &[String], base: &Path) {
        for item in Self::scan_dir(dir) {
            match item {
                Ok(path) => {
                    let on = is_enabled_by_overrides(&path, entries, base);
                    self.load(&path, on);
                }
                Err(diagnostic) => self.out.diagnostics.push(diagnostic),
            }
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
    // the profile's directory; a manifest found twice keeps its first place (and its enabled state).
    let project_base = cwd.join(CONFIG_DIR_NAME);
    if project_trusted {
        let entries = settings_entries(project);
        collector.settings_scope(&entries, &project_base, home);
        collector.auto_dir(&project_base.join("extensions"), &entries, &project_base);
    }
    let entries = settings_entries(global);
    collector.settings_scope(&entries, agent_dir, home);
    collector.auto_dir(&agent_dir.join("extensions"), &entries, agent_dir);
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
