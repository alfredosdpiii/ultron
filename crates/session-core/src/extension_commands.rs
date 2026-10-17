//! Extension commands as the TypeScript worker lists them (`getRegisteredCommands` with each command's Pi
//! `SourceInfo`): in `get_commands`, the `pi.legacy-extension-commands` service the client TUI asks, and the TUI's
//! slash commands. The Rust build's third-party extensions are processes (crates/extensions/PROTOCOL.md), one
//! directory each with an `extension.json`, found where TypeScript finds extensions; a command's `SourceInfo` is
//! the one Pi gives the extension at that place (package-manager.ts): `auto` for the extensions directories of
//! the profile (`scope: "user"`, `baseDir`: the profile) and of a trusted project (`scope: "project"`, `baseDir`:
//! its `.pi`), `local` without `baseDir` for one listed in the profile's or the project's settings, the first of
//! those that finds it (project before profile, a listing before the directory). Its `path` is the manifest, where
//! TypeScript's is the extension's entry module.

use config::paths::{CONFIG_DIR_NAME, resolve};
use config::settings::SettingsManager;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// One extension command: its invocation name, description and Pi `SourceInfo`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: Option<String>,
    pub source_info: Option<Value>,
}

/// `workerProjectTrusted` (worker-settings.ts): nothing to trust, else the stored decision, else
/// `defaultProjectTrust` is not "never".
pub fn worker_project_trusted(cwd: &Path, agent_dir: &Path, home: &Path) -> bool {
    config::trust::worker_project_trusted(cwd, agent_dir, home)
}

fn entries(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("extensions")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Whether the settings entry `entry` (relative to `base`) finds the manifest at `manifest`: the manifest itself,
/// its directory, or the directory holding its directory.
fn lists(entry: &str, base: &Path, home: &Path, manifest: &Path) -> bool {
    let path = resolve(entry, base, home);
    let dir = manifest.parent();
    path == manifest || Some(path.as_path()) == dir || Some(path.as_path()) == dir.and_then(Path::parent)
}

fn in_extensions_dir(manifest: &Path, base: &Path) -> bool {
    manifest.parent().and_then(Path::parent) == Some(base.join("extensions").as_path())
}

/// The Pi `SourceInfo` of the extension whose manifest is `manifest`; `None` for one found nowhere Pi looks.
pub fn source_info(
    manifest: &Path,
    settings: &SettingsManager,
    cwd: &Path,
    agent_dir: &Path,
    home: &Path,
) -> Option<Value> {
    let project: PathBuf = cwd.join(CONFIG_DIR_NAME);
    let path = manifest.display().to_string();
    let local = |scope: &str| json!({"path": path, "source": "local", "scope": scope, "origin": "top-level"});
    let auto = |scope: &str, base: &Path| json!({"path": path, "source": "auto", "scope": scope, "origin": "top-level", "baseDir": base.display().to_string()});
    let trusted = settings.is_project_trusted();
    if trusted
        && entries(settings.project_settings())
            .iter()
            .any(|e| lists(e, &project, home, manifest))
    {
        return Some(local("project"));
    }
    if trusted && in_extensions_dir(manifest, &project) {
        return Some(auto("project", &project));
    }
    if entries(settings.global_settings())
        .iter()
        .any(|e| lists(e, agent_dir, home, manifest))
    {
        return Some(local("user"));
    }
    if in_extensions_dir(manifest, agent_dir) {
        return Some(auto("user", agent_dir));
    }
    None
}

/// `{name, description?, sourceInfo?}`: an extension command as `pi.legacy-extension-commands.list` answers it.
pub fn command_info(command: &ExtensionCommand) -> Value {
    let mut out = Map::new();
    out.insert("name".into(), command.name.clone().into());
    if let Some(description) = &command.description {
        out.insert("description".into(), description.clone().into());
    }
    if let Some(info) = &command.source_info {
        out.insert("sourceInfo".into(), info.clone());
    }
    Value::Object(out)
}

/// `get_commands` entries back as extension commands (`source: "extension"`), in their order.
pub fn from_listing(commands: &[Value]) -> Vec<ExtensionCommand> {
    commands
        .iter()
        .filter(|c| c["source"] == "extension")
        .filter_map(|c| {
            Some(ExtensionCommand {
                name: c["name"].as_str()?.to_string(),
                description: c["description"].as_str().map(str::to_string),
                source_info: c.get("sourceInfo").cloned(),
            })
        })
        .collect()
}

/// `listCommands` of the session control service: each command with `description` and `sourceInfo`, `null`
/// when it has none (`SessionCommandInfo`).
pub fn session_command_infos(commands: &[Value]) -> Vec<Value> {
    commands
        .iter()
        .map(|c| {
            json!({"name": c["name"], "description": c.get("description").cloned().unwrap_or(Value::Null),
                "source": c["source"], "sourceInfo": c.get("sourceInfo").cloned().unwrap_or(Value::Null)})
        })
        .collect()
}
