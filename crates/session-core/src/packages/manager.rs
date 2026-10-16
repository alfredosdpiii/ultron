//! `DefaultPackageManager` (core/package-manager.ts) for the user and project scopes: parsing sources, installing,
//! removing and updating npm and git packages with the configured npm command and `git`, the settings entries, the
//! installed paths (including the legacy global npm root), and `resolve()` of every configured resource.
//!
//! Commands run as TypeScript runs them: npm and git inherit the terminal; version checks capture their output.
//! Updates run one after another (TypeScript runs the npm batches and up to four git updates concurrently), and so do
//! the startup update checks (`checkForAvailableUpdates`, four at a time in TypeScript). Temporary (`-e`) sources
//! install under `<agent dir>/tmp/extensions` (`resolveExtensionSources(sources, { temporary: true })`).

use super::git_url::{GitSource, parse_git_url};
use super::resources::{self, RESOURCE_TYPES, ResourceType};
use super::semver;
use config::settings::SettingsManager;
use indexmap::IndexMap;
use resources_paths::{basename, canonicalize, dirname, join, relative};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::resources::paths as resources_paths;

/// `NETWORK_TIMEOUT_MS`.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Package,
    TopLevel,
}

/// `PathMetadata`: where a resource comes from. `source` is the package source, `local` or `auto`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: String,
    pub scope: Scope,
    pub origin: Origin,
    pub base_dir: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// `ResolvedPaths`, indexed by [`ResourceType`] order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

impl ResolvedPaths {
    pub fn of(&self, kind: ResourceType) -> &[ResolvedResource] {
        match kind {
            ResourceType::Extensions => &self.extensions,
            ResourceType::Skills => &self.skills,
            ResourceType::Prompts => &self.prompts,
            ResourceType::Themes => &self.themes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NpmSource {
    pub spec: String,
    pub name: String,
    pub version: Option<String>,
    pub range: Option<String>,
    pub pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Npm(NpmSource),
    Git(GitSource),
    Local(String),
}

/// `isLocalPath`.
pub fn is_local_path(value: &str) -> bool {
    let t = value.trim();
    !["npm:", "git:", "github:", "http:", "https:", "ssh:"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// `parseNpmSpec`: `^(@?[^@]+(?:\/[^@]+)?)(?:@(.+))?$`.
fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let re = regex::Regex::new(r"^(@?[^@]+(?:/[^@]+)?)(?:@(.+))?$").expect("valid regex");
    match re.captures(spec) {
        Some(c) => (c[1].to_string(), c.get(2).map(|m| m.as_str().to_string())),
        None => (spec.to_string(), None),
    }
}

/// `parseSource`.
pub fn parse_source(source: &str) -> Parsed {
    if let Some(spec) = source.strip_prefix("npm:") {
        let spec = spec.trim().to_string();
        let (name, version) = parse_npm_spec(&spec);
        let range = version.clone().filter(|v| semver::valid_range(v));
        let pinned = version.as_deref().is_some_and(|v| semver::valid(v).is_some());
        return Parsed::Npm(NpmSource {
            spec,
            name,
            version,
            range,
            pinned,
        });
    }
    if is_local_path(source) {
        return Parsed::Local(source.to_string());
    }
    match parse_git_url(source) {
        Some(git) => Parsed::Git(git),
        None => Parsed::Local(source.to_string()),
    }
}

/// A settings `packages` entry's source.
pub fn source_string(pkg: &Value) -> String {
    match pkg {
        Value::String(s) => s.clone(),
        other => other
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// A configured package's directory, with its scope and its settings filter (an object entry's resource keys).
#[derive(Clone, Debug, PartialEq)]
pub struct PackageRoot {
    pub root: String,
    pub scope: Scope,
    pub filter: Option<Map<String, Value>>,
}

impl PackageRoot {
    fn new(root: &str, scope: Scope, filter: Option<&Map<String, Value>>) -> Self {
        PackageRoot {
            root: root.to_string(),
            scope,
            filter: filter.cloned(),
        }
    }
}

/// `ConfiguredPackage`.
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: Scope,
    pub filtered: bool,
    pub installed_path: Option<String>,
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `PI_OFFLINE`.
fn offline(var: &crate::VarLookup) -> bool {
    var("PI_OFFLINE").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

/// The process error of a spawn that failed (`spawn <command> ENOENT`).
fn spawn_error(command: &str, error: &std::io::Error) -> String {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
        _ => return format!("spawn {command} {error}"),
    };
    format!("spawn {command} {code}")
}

type Accumulator = [IndexMap<String, (PathMetadata, bool)>; 4];

fn slot(kind: ResourceType) -> usize {
    match kind {
        ResourceType::Extensions => 0,
        ResourceType::Skills => 1,
        ResourceType::Prompts => 2,
        ResourceType::Themes => 3,
    }
}

/// `addResource`: the first entry for a path wins.
fn add(acc: &mut Accumulator, kind: ResourceType, path: &str, metadata: &PathMetadata, enabled: bool) {
    if path.is_empty() {
        return;
    }
    acc[slot(kind)]
        .entry(path.to_string())
        .or_insert_with(|| (metadata.clone(), enabled));
}

/// `resourcePrecedenceRank`.
fn rank(m: &PathMetadata) -> u8 {
    if m.origin == Origin::Package {
        return 4;
    }
    let base = if m.scope == Scope::Project { 0 } else { 2 };
    base + u8::from(m.source != "local")
}

/// A `packages` entry's filter (`PackageFilter`), when the entry is an object.
fn filter_of(pkg: &Value) -> Option<&Map<String, Value>> {
    pkg.as_object()
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// The progress callback: the message of each operation that starts.
pub type Progress = Box<dyn FnMut(&str)>;

/// `PackageUpdate`: a configured package whose installed copy is behind its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageUpdate {
    pub source: String,
    /// The npm name, or `host/path` of a git source.
    pub display_name: String,
    /// `npm` or `git`.
    pub kind: &'static str,
    pub scope: Scope,
}

/// What `resolveExtensionSources(sources, { temporary: true })` gives the `-e` sources: the enabled resources by
/// kind, and the directory each package source resolved to (its install, or a local directory).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TemporaryResolved {
    pub extensions: Vec<String>,
    pub skills: Vec<String>,
    pub prompts: Vec<String>,
    pub themes: Vec<String>,
    pub roots: Vec<String>,
}

/// The proxy variables npm and git read, passed to them from the run's environment.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// `getExtensionTempFolder`'s location under the agent dir.
const EXTENSION_TEMP_FOLDER: [&str; 2] = ["tmp", "extensions"];

pub struct PackageManager {
    pub cwd: String,
    pub agent_dir: String,
    home: String,
    var: crate::VarLookup,
    pub settings: SettingsManager,
    global_npm_root: Option<(String, String)>,
    /// `setProgressCallback`: the message of each operation that starts.
    pub progress: Option<Progress>,
    /// Extra environment for the npm and git children (TypeScript's children inherit `process.env`; the Rust
    /// process passes PATH and HOME from its injected environment, plus these).
    pub child_env: Vec<(String, String)>,
    /// Send the children's stdout to stderr (a `-e` install before a print or JSON run keeps stdout clean).
    pub child_stdout_to_stderr: bool,
}

impl PackageManager {
    pub fn new(env: &crate::RunEnv, settings: SettingsManager) -> Self {
        let home = env.home.display().to_string();
        let resolve = |p: &str| config::paths::resolve(p, &env.cwd, &env.home).display().to_string();
        Self {
            cwd: resolve(&env.cwd.display().to_string()),
            agent_dir: resolve(&env.agent_dir().display().to_string()),
            home,
            var: Arc::clone(&env.var),
            settings,
            global_npm_root: None,
            progress: None,
            child_env: Vec::new(),
            child_stdout_to_stderr: false,
        }
    }

    fn start(&mut self, message: &str) {
        if let Some(progress) = self.progress.as_mut() {
            progress(message);
        }
    }

    fn resolve_from(&self, input: &str, base: &str) -> String {
        config::paths::resolve(input.trim(), Path::new(base), Path::new(&self.home))
            .display()
            .to_string()
    }

    /// `resolvePath(input)` from the working directory.
    fn resolve_input(&self, input: &str) -> String {
        self.resolve_from(input, &self.cwd.clone())
    }

    fn assert_trusted(&self, scope: Scope) -> Result<(), String> {
        if scope == Scope::Project && !self.settings.is_project_trusted() {
            return Err("Project is not trusted; refusing to access project package storage".into());
        }
        Ok(())
    }

    /// `getBaseDirForScope`.
    pub fn base_dir(&self, scope: Scope) -> Result<String, String> {
        self.assert_trusted(scope)?;
        Ok(match scope {
            Scope::Project => join(&self.cwd, config::paths::CONFIG_DIR_NAME),
            Scope::User => self.agent_dir.clone(),
        })
    }

    pub fn packages(&self, scope: Scope) -> Vec<Value> {
        let settings = match scope {
            Scope::User => self.settings.global_settings(),
            Scope::Project => self.settings.project_settings(),
        };
        settings
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_packages(&mut self, scope: Scope, packages: Vec<Value>) -> Result<(), String> {
        match scope {
            Scope::Project => self.settings.set_project("packages", Some(Value::Array(packages))),
            Scope::User => {
                self.settings.set_global("packages", Some(Value::Array(packages)));
                Ok(())
            }
        }
    }

    // ---------------------------------------------------------------------------------------- identities

    fn key_for_input(&self, source: &str) -> String {
        match parse_source(source) {
            Parsed::Npm(n) => format!("npm:{}", n.name),
            Parsed::Git(g) => format!("git:{}/{}", g.host, g.path),
            Parsed::Local(p) => format!("local:{}", self.resolve_input(&p)),
        }
    }

    fn key_for_settings(&self, source: &str, scope: Scope) -> Result<String, String> {
        Ok(match parse_source(source) {
            Parsed::Npm(n) => format!("npm:{}", n.name),
            Parsed::Git(g) => format!("git:{}/{}", g.host, g.path),
            Parsed::Local(p) => format!("local:{}", self.resolve_from(&p, &self.base_dir(scope)?)),
        })
    }

    /// `getPackageIdentity`.
    pub fn identity(&self, source: &str, scope: Option<Scope>) -> Result<String, String> {
        match scope {
            Some(scope) => self.key_for_settings(source, scope),
            None => Ok(self.key_for_input(source)),
        }
    }

    fn matches(&self, existing: &Value, input: &str, scope: Scope) -> Result<bool, String> {
        Ok(self.key_for_settings(&source_string(existing), scope)? == self.key_for_input(input))
    }

    /// `normalizePackageSourceForSettings`.
    fn normalize_for_settings(&self, source: &str, scope: Scope) -> Result<String, String> {
        let Parsed::Local(path) = parse_source(source) else {
            return Ok(source.to_string());
        };
        let rel = relative(&self.base_dir(scope)?, &self.resolve_input(&path));
        Ok(if rel.is_empty() { ".".into() } else { rel })
    }

    /// `addSourceToSettings`.
    pub fn add_source_to_settings(&mut self, source: &str, scope: Scope) -> Result<bool, String> {
        let packages = self.packages(scope);
        let normalized = self.normalize_for_settings(source, scope)?;
        let mut position = None;
        for (i, existing) in packages.iter().enumerate() {
            if self.matches(existing, source, scope)? {
                position = Some(i);
                break;
            }
        }
        let mut next = packages.clone();
        match position {
            Some(i) if source_string(&packages[i]) == normalized => return Ok(false),
            Some(i) => {
                next[i] = match &packages[i] {
                    Value::Object(map) => {
                        let mut map = map.clone();
                        map.insert("source".into(), Value::String(normalized));
                        Value::Object(map)
                    }
                    _ => Value::String(normalized),
                };
            }
            None => next.push(Value::String(normalized)),
        }
        self.set_packages(scope, next)?;
        Ok(true)
    }

    /// `removeSourceFromSettings`.
    pub fn remove_source_from_settings(&mut self, source: &str, scope: Scope) -> Result<bool, String> {
        let packages = self.packages(scope);
        let mut next = Vec::new();
        for existing in &packages {
            if !self.matches(existing, source, scope)? {
                next.push(existing.clone());
            }
        }
        if next.len() == packages.len() {
            return Ok(false);
        }
        self.set_packages(scope, next)?;
        Ok(true)
    }

    // ---------------------------------------------------------------------------------------- npm command

    /// `getNpmCommand`: the `npmCommand` setting, else `npm`.
    fn npm_command(&self) -> Result<(String, Vec<String>), String> {
        let Some(list) = self.settings.settings().get("npmCommand").and_then(Value::as_array) else {
            return Ok(("npm".into(), Vec::new()));
        };
        if list.is_empty() {
            return Ok(("npm".into(), Vec::new()));
        }
        let parts: Vec<String> = list
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        if parts[0].is_empty() {
            return Err("Invalid npmCommand: first array entry must be a non-empty command".into());
        }
        Ok((parts[0].clone(), parts[1..].to_vec()))
    }

    fn npm_configured(&self) -> bool {
        self.settings
            .settings()
            .get("npmCommand")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    }

    /// `getPackageManagerName`: the command after the last `--`, else the command, without `.cmd`/`.exe`.
    fn package_manager_name(&self) -> Result<String, String> {
        let (command, args) = self.npm_command()?;
        let mut parts = vec![command.clone()];
        parts.extend(args);
        let name = match parts.iter().rposition(|p| p == "--") {
            Some(i) => parts.get(i + 1).cloned().unwrap_or_default(),
            None => command,
        };
        let base = basename(&name).to_string();
        let lower = base.to_ascii_lowercase();
        Ok(if lower.ends_with(".cmd") || lower.ends_with(".exe") {
            base[..base.len() - 4].to_string()
        } else {
            base
        })
    }

    fn command(&self, program: &str, args: &[String], cwd: Option<&str>) -> Command {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(path) = (self.var)("PATH") {
            command.env("PATH", path);
        }
        if !self.home.is_empty() {
            command.env("HOME", &self.home);
        }
        // The run's proxies (the `httpProxy` setting becomes `HTTP_PROXY`/`HTTPS_PROXY`), which TypeScript's children
        // inherit from `process.env`.
        for name in PROXY_VARIABLES {
            if let Some(value) = (self.var)(name) {
                command.env(name, value);
            }
        }
        command.envs(self.child_env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
    }

    /// `runCommand`: the child inherits stdin, stdout and stderr.
    fn run(&self, program: &str, args: &[String], cwd: Option<&str>) -> Result<(), String> {
        let _ = std::io::stdout().flush();
        let mut command = self.command(program, args, cwd);
        if self.child_stdout_to_stderr {
            command.stdout(std::io::stderr());
        }
        let status = command.status().map_err(|e| spawn_error(program, &e))?;
        if status.success() {
            return Ok(());
        }
        let code = status.code().map_or("null".to_string(), |c| c.to_string());
        Err(format!("{program} {} failed with code {code}", args.join(" ")))
    }

    /// `runCommandCapture`: stdout trimmed; an error names the exit status and the output.
    fn capture(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Result<String, String> {
        let mut command = self.command(program, args, cwd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            command.env(k, v);
        }
        let mut child = command.spawn().map_err(|e| spawn_error(program, &e))?;
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let out_reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.read_to_end(&mut s);
            }
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            if let Some(p) = stderr_pipe.as_mut() {
                let _ = p.read_to_end(&mut s);
            }
            s
        });
        let deadline = Instant::now() + NETWORK_TIMEOUT;
        let status = loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(status) => break Some(status),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        let Some(status) = status else {
            return Err(format!(
                "{program} {} timed out after {}ms",
                args.join(" "),
                NETWORK_TIMEOUT.as_millis()
            ));
        };
        if status.success() {
            return Ok(stdout.trim().to_string());
        }
        let exit = match status.code() {
            Some(c) => format!("code {c}"),
            None => "signal unknown".into(),
        };
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(format!("{program} {} failed with {exit}: {detail}", args.join(" ")))
    }

    /// `runCommandSync`.
    fn run_sync(&self, program: &str, args: &[String]) -> Result<String, String> {
        let output = self
            .command(program, args, None)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                format!(
                    "Failed to run {program} {}: {}",
                    args.join(" "),
                    spawn_error(program, &e)
                )
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let detail = if stderr.is_empty() { stdout } else { stderr };
            return Err(format!("Failed to run {program} {}: {detail}", args.join(" ")));
        }
        Ok(if stdout.is_empty() { stderr } else { stdout }.trim().to_string())
    }

    fn run_npm(&self, args: Vec<String>, cwd: Option<&str>) -> Result<(), String> {
        let (command, mut full) = self.npm_command()?;
        full.extend(args);
        self.run(&command, &full, cwd)
    }

    fn npm_sync(&self, args: &[&str]) -> Result<String, String> {
        let (command, mut full) = self.npm_command()?;
        full.extend(args.iter().map(|s| s.to_string()));
        self.run_sync(&command, &full)
    }

    fn git(&self, args: &[&str], cwd: Option<&str>) -> Result<(), String> {
        self.run("git", &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(), cwd)
    }

    fn git_capture(&self, args: &[&str], cwd: &str) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.capture("git", &args, Some(cwd), &[])
    }

    /// `getGitDependencyInstallArgs`.
    fn git_dependency_install_args(&self) -> Vec<String> {
        if self.npm_configured() {
            vec!["install".into()]
        } else {
            vec!["install".into(), "--omit=dev".into()]
        }
    }

    /// `getNpmInstallArgs`.
    fn npm_install_args(&self, specs: &[String], root: &str) -> Result<Vec<String>, String> {
        let name = self.package_manager_name()?;
        let mut args = vec!["install".to_string()];
        args.extend(specs.iter().cloned());
        let tail: &[&str] = match name.as_str() {
            "bun" => &["--cwd", root, "--omit=peer"],
            "pnpm" => &[
                "--prefix",
                root,
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ],
            _ => &["--prefix", root, "--legacy-peer-deps"],
        };
        args.extend(tail.iter().map(|s| s.to_string()));
        Ok(args)
    }

    // ---------------------------------------------------------------------------------------- locations

    fn npm_install_root(&self, scope: Scope) -> Result<String, String> {
        Ok(match scope {
            Scope::Project => {
                self.assert_trusted(scope)?;
                join(&join(&self.cwd, config::paths::CONFIG_DIR_NAME), "npm")
            }
            Scope::User => join(&self.agent_dir, "npm"),
        })
    }

    fn managed_npm_path(&self, source: &NpmSource, scope: Scope) -> Result<String, String> {
        Ok(join(
            &join(&self.npm_install_root(scope)?, "node_modules"),
            &source.name,
        ))
    }

    /// `getGlobalNpmRoot`, cached per npm command.
    fn global_npm_root(&mut self) -> Result<String, String> {
        let (command, args) = self.npm_command()?;
        let key = std::iter::once(command).chain(args).collect::<Vec<_>>().join("\0");
        if let Some((k, root)) = &self.global_npm_root
            && *k == key
        {
            return Ok(root.clone());
        }
        let root = if self.package_manager_name()? == "bun" {
            let bin = self.npm_sync(&["pm", "bin", "-g"])?;
            join(&join(&join(&dirname(bin.trim()), "install"), "global"), "node_modules")
        } else {
            self.npm_sync(&["root", "-g"])?.trim().to_string()
        };
        self.global_npm_root = Some((key, root.clone()));
        Ok(root)
    }

    /// `getPnpmGlobalPackagePath`.
    fn pnpm_global_path(&self, name: &str) -> Result<Option<String>, String> {
        if self.package_manager_name()? != "pnpm" {
            return Ok(None);
        }
        let output = self.npm_sync(&["list", "-g", "--depth", "0", "--json"])?;
        let entries: Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
        Ok(entries.as_array().and_then(|list| {
            list.iter().find_map(|entry| {
                entry["dependencies"][name]["path"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
            })
        }))
    }

    /// `getNpmInstallPath`: the managed path, or for the user scope the legacy global install when only it exists.
    fn npm_install_path(&mut self, source: &NpmSource, scope: Scope) -> Result<String, String> {
        let managed = self.managed_npm_path(source, scope)?;
        if scope != Scope::User || Path::new(&managed).exists() {
            return Ok(managed);
        }
        let legacy = match self.pnpm_global_path(&source.name) {
            Ok(Some(path)) => Some(path),
            Ok(None) => self.global_npm_root().ok().map(|root| join(&root, &source.name)),
            Err(_) => None,
        };
        Ok(match legacy {
            Some(path) if Path::new(&path).exists() => path,
            _ => managed,
        })
    }

    fn git_install_root(&self, scope: Scope) -> Result<String, String> {
        Ok(match scope {
            Scope::Project => {
                self.assert_trusted(scope)?;
                join(&join(&self.cwd, config::paths::CONFIG_DIR_NAME), "git")
            }
            Scope::User => join(&self.agent_dir, "git"),
        })
    }

    /// `getGitInstallPath` (`resolveManagedPath`).
    fn git_install_path(&self, source: &GitSource, scope: Scope) -> Result<String, String> {
        let root = resources_paths::resolve("/", &self.git_install_root(scope)?);
        let path = resources_paths::resolve(&root, &format!("{}/{}", source.host, source.path));
        if path != root && !path.starts_with(&format!("{root}/")) {
            return Err(format!("Refusing to use path outside package install root: {path}"));
        }
        Ok(path)
    }

    /// `getInstalledPath`.
    pub fn installed_path(&mut self, source: &str, scope: Scope) -> Option<String> {
        let path = match parse_source(source) {
            Parsed::Npm(n) => self.npm_install_path(&n, scope).ok()?,
            Parsed::Git(g) => self.git_install_path(&g, scope).ok()?,
            Parsed::Local(p) => self.resolve_from(&p, &self.base_dir(scope).ok()?),
        };
        Path::new(&path).exists().then_some(path)
    }

    /// `listConfiguredPackages`.
    pub fn list_configured(&mut self) -> Vec<ConfiguredPackage> {
        let mut out = Vec::new();
        for scope in [Scope::User, Scope::Project] {
            for pkg in self.packages(scope) {
                let source = source_string(&pkg);
                let installed_path = self.installed_path(&source, scope);
                out.push(ConfiguredPackage {
                    source,
                    scope,
                    filtered: pkg.is_object(),
                    installed_path,
                });
            }
        }
        out
    }

    // ---------------------------------------------------------------------------------------- npm

    /// `ensureGitIgnore`.
    fn ensure_git_ignore(&self, dir: &str) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let path = join(dir, ".gitignore");
        if !Path::new(&path).exists() {
            std::fs::write(&path, "*\n!.gitignore\n").map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `ensureNpmProject`.
    fn ensure_npm_project(&self, root: &str) -> Result<(), String> {
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        // `markPathIgnoredByCloudSync`: Dropbox skips it (the attribute is set when `setfattr` exists).
        let _ = self
            .command(
                "setfattr",
                &["-n", "user.com.dropbox.ignored", "-v", "1", root].map(String::from),
                None,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.ensure_git_ignore(root)?;
        let package_json = join(root, "package.json");
        if !Path::new(&package_json).exists() {
            std::fs::write(
                &package_json,
                "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}",
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn install_npm(&self, source: &NpmSource, scope: Scope) -> Result<(), String> {
        let root = self.npm_install_root(scope)?;
        self.ensure_npm_project(&root)?;
        let args = self.npm_install_args(std::slice::from_ref(&source.spec), &root)?;
        self.run_npm(args, None)
    }

    fn uninstall_npm(&self, source: &NpmSource, scope: Scope) -> Result<(), String> {
        let root = self.npm_install_root(scope)?;
        if !Path::new(&root).exists() {
            return Ok(());
        }
        let name = self.package_manager_name()?;
        if name == "bun" {
            return self.run_npm(
                vec!["uninstall".into(), source.name.clone(), "--cwd".into(), root],
                None,
            );
        }
        let mut args = vec!["uninstall".into(), source.name.clone(), "--prefix".into(), root];
        if name != "pnpm" {
            args.push("--legacy-peer-deps".into());
        }
        self.run_npm(args, None)
    }

    fn installed_npm_version(path: &str) -> Option<String> {
        let text = std::fs::read_to_string(join(path, "package.json")).ok()?;
        let value: Value = serde_json::from_str(resources_paths::strip_bom(&text)).ok()?;
        value.get("version")?.as_str().map(str::to_string)
    }

    /// `getLatestNpmVersion`.
    fn latest_npm_version(&self, spec: &str, range: Option<&str>) -> Result<String, String> {
        let (command, mut args) = self.npm_command()?;
        args.extend(["view", spec, "version", "--json"].map(String::from));
        let raw = self.capture(&command, &args, Some(&self.cwd), &[])?;
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("Empty response from npm view".into());
        }
        let parsed: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if let Some(v) = parsed.as_str() {
            return Ok(v.to_string());
        }
        if let Some(list) = parsed.as_array() {
            let versions: Vec<&str> = list
                .iter()
                .filter_map(Value::as_str)
                .filter(|v| !v.is_empty())
                .collect();
            let latest = match range {
                Some(range) => semver::max_satisfying(&versions, range),
                None => semver::highest(&versions),
            };
            if let Some(latest) = latest {
                return Ok(latest.to_string());
            }
        }
        Err("Unexpected response from npm view".into())
    }

    /// `shouldUpdateNpmSource`.
    fn should_update_npm(&self, source: &NpmSource, scope: Scope) -> Result<bool, String> {
        let path = self.managed_npm_path(source, scope)?;
        let Some(installed) = Self::installed_npm_version(&path) else {
            return Ok(true);
        };
        let spec = if source.version.is_some() {
            &source.spec
        } else {
            &source.name
        };
        Ok(match self.latest_npm_version(spec, source.range.as_deref()) {
            Ok(target) => semver::gt(&target, &installed).unwrap_or(true),
            Err(_) => true,
        })
    }

    // ---------------------------------------------------------------------------------------- git

    fn marker_path(target: &str) -> String {
        join(&dirname(target), &format!(".{}.pi-update-incomplete", basename(target)))
    }

    /// `pruneEmptyGitParents`.
    fn prune_empty_parents(target: &str, root: &str) {
        let root = resources_paths::resolve("/", root);
        let mut current = dirname(target);
        while current.starts_with(&root) && current != root {
            if !Path::new(&current).exists() {
                current = dirname(&current);
                continue;
            }
            let empty = std::fs::read_dir(&current)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if !empty || std::fs::remove_dir_all(&current).is_err() {
                break;
            }
            current = dirname(&current);
        }
    }

    fn has_missing_git_dependencies(target: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(join(target, "package.json")) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(resources_paths::strip_bom(&text)) else {
            return false;
        };
        let Some(deps) = manifest.get("dependencies").and_then(Value::as_object) else {
            return false;
        };
        let modules = join(target, "node_modules");
        deps.keys().any(|name| {
            let path = resources_paths::resolve(&modules, name);
            path.starts_with(&format!("{modules}/")) && !Path::new(&path).exists()
        })
    }

    fn repair_missing_git_dependencies(&self, target: &str) -> Result<(), String> {
        if !Self::has_missing_git_dependencies(target) {
            return Ok(());
        }
        self.run_npm(self.git_dependency_install_args(), Some(target))
    }

    fn clean_and_install_git_dependencies(&self, target: &str, marker: &str) -> Result<(), String> {
        if let Err(error) = self.git(&["clean", "-fdx"], Some(target)) {
            let _ = self.repair_missing_git_dependencies(target);
            return Err(error);
        }
        if Path::new(&join(target, "package.json")).exists() {
            self.run_npm(self.git_dependency_install_args(), Some(target))?;
        }
        let _ = std::fs::remove_file(marker);
        Ok(())
    }

    /// `ensureGitRef`.
    fn ensure_git_ref(&self, target: &str, fetch: &[String], git_ref: &str) -> Result<(), String> {
        self.run("git", fetch, Some(target))?;
        let local = self.git_capture(&["rev-parse", "HEAD"], target)?;
        let commit_ref = format!("{git_ref}^{{commit}}");
        let wanted = self.git_capture(&["rev-parse", &commit_ref], target)?;
        let marker = Self::marker_path(target);
        if local.trim() == wanted.trim() {
            return if Path::new(&marker).exists() {
                self.clean_and_install_git_dependencies(target, &marker)
            } else {
                self.repair_missing_git_dependencies(target)
            };
        }
        std::fs::write(&marker, "").map_err(|e| e.to_string())?;
        self.git(&["reset", "--hard", &commit_ref], Some(target))?;
        self.clean_and_install_git_dependencies(target, &marker)
    }

    /// `getLocalGitUpdateTarget`: the upstream branch, else origin's HEAD.
    fn local_git_update_target(&self, target: &str) -> Result<(String, Vec<String>), String> {
        let fetch_branch = |branch: &str| -> Vec<String> {
            ["fetch", "--prune", "--no-tags", "origin"]
                .iter()
                .map(|s| s.to_string())
                .chain([format!("+refs/heads/{branch}:refs/remotes/origin/{branch}")])
                .collect()
        };
        let upstream = (|| -> Result<String, String> {
            let upstream = self.git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"], target)?;
            let upstream = upstream.trim();
            let branch = upstream
                .strip_prefix("origin/")
                .ok_or_else(|| format!("Unsupported upstream remote: {upstream}"))?;
            if branch.is_empty() {
                return Err("Missing upstream branch name".into());
            }
            self.git_capture(&["rev-parse", "@{upstream}"], target)?;
            Ok(branch.to_string())
        })();
        if let Ok(branch) = upstream {
            return Ok(("@{upstream}".into(), fetch_branch(&branch)));
        }
        let _ = self.git(&["remote", "set-head", "origin", "-a"], Some(target));
        self.git_capture(&["rev-parse", "origin/HEAD"], target)?;
        let head_ref = self
            .git_capture(&["symbolic-ref", "refs/remotes/origin/HEAD"], target)
            .unwrap_or_default();
        let branch = head_ref.trim();
        let branch = branch.strip_prefix("refs/remotes/origin/").unwrap_or(branch);
        if !branch.is_empty() {
            return Ok(("origin/HEAD".into(), fetch_branch(branch)));
        }
        Ok((
            "origin/HEAD".into(),
            [
                "fetch",
                "--prune",
                "--no-tags",
                "origin",
                "+HEAD:refs/remotes/origin/HEAD",
            ]
            .map(String::from)
            .to_vec(),
        ))
    }

    fn update_existing_git(&self, source: &GitSource, target: &str) -> Result<(), String> {
        if let Some(git_ref) = &source.git_ref {
            let fetch = ["fetch".to_string(), "origin".into(), git_ref.clone()];
            return self.ensure_git_ref(target, &fetch, "FETCH_HEAD");
        }
        let (git_ref, fetch) = self.local_git_update_target(target)?;
        self.ensure_git_ref(target, &fetch, &git_ref)
    }

    /// `installGit`.
    fn install_git(&self, source: &GitSource, scope: Scope) -> Result<(), String> {
        let target = self.git_install_path(source, scope)?;
        let root = self.git_install_root(scope)?;
        self.install_git_at(source, &target, Some(&root))
    }

    /// `installGit` at `target`; `root` is the install root (none for a temporary checkout).
    fn install_git_at(&self, source: &GitSource, target: &str, root: Option<&str>) -> Result<(), String> {
        let target = target.to_string();
        if Path::new(&target).exists() {
            return self.update_existing_git(source, &target);
        }
        if let Some(root) = root {
            self.ensure_git_ignore(root)?;
        }
        std::fs::create_dir_all(dirname(&target)).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(Self::marker_path(&target));
        let result = (|| -> Result<(), String> {
            self.git(&["clone", &source.repo, &target], None)?;
            if let Some(git_ref) = &source.git_ref {
                self.git(&["checkout", git_ref], Some(&target))?;
            }
            if Path::new(&join(&target, "package.json")).exists() {
                self.run_npm(self.git_dependency_install_args(), Some(&target))?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&target);
            if let Some(root) = root {
                Self::prune_empty_parents(&target, root);
            }
        }
        result
    }

    /// `updateGit`.
    fn update_git(&self, source: &GitSource, scope: Scope) -> Result<(), String> {
        let target = self.git_install_path(source, scope)?;
        if !Path::new(&target).exists() {
            return self.install_git(source, scope);
        }
        self.update_existing_git(source, &target)
    }

    fn remove_git(&self, source: &GitSource, scope: Scope) -> Result<(), String> {
        let target = self.git_install_path(source, scope)?;
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_file(Self::marker_path(&target));
        Self::prune_empty_parents(&target, &self.git_install_root(scope)?);
        Ok(())
    }

    // ---------------------------------------------------------------------------------------- operations

    /// `install`.
    pub fn install(&mut self, source: &str, scope: Scope) -> Result<(), String> {
        let parsed = parse_source(source);
        self.assert_trusted(scope)?;
        self.start(&format!("Installing {source}..."));
        match parsed {
            Parsed::Npm(n) => self.install_npm(&n, scope),
            Parsed::Git(g) => self.install_git(&g, scope),
            Parsed::Local(p) => {
                let resolved = self.resolve_input(&p);
                if !Path::new(&resolved).exists() {
                    return Err(format!("Path does not exist: {resolved}"));
                }
                Ok(())
            }
        }
    }

    pub fn install_and_persist(&mut self, source: &str, scope: Scope) -> Result<(), String> {
        self.install(source, scope)?;
        self.add_source_to_settings(source, scope)?;
        Ok(())
    }

    /// `remove`.
    pub fn remove(&mut self, source: &str, scope: Scope) -> Result<(), String> {
        let parsed = parse_source(source);
        self.assert_trusted(scope)?;
        self.start(&format!("Removing {source}..."));
        match parsed {
            Parsed::Npm(n) => self.uninstall_npm(&n, scope),
            Parsed::Git(g) => self.remove_git(&g, scope),
            Parsed::Local(_) => Ok(()),
        }
    }

    pub fn remove_and_persist(&mut self, source: &str, scope: Scope) -> Result<bool, String> {
        self.remove(source, scope)?;
        self.remove_source_from_settings(source, scope)
    }

    /// `findSuggestedConfiguredSource`.
    fn suggestion(source: &str, configured: &[Value]) -> Option<String> {
        let trimmed = source.trim();
        for pkg in configured {
            let s = source_string(pkg);
            match parse_source(&s) {
                Parsed::Npm(n) if trimmed == n.name || trimmed == n.spec => return Some(s),
                Parsed::Git(g) => {
                    let short = format!("{}/{}", g.host, g.path);
                    let with_ref = g.git_ref.as_ref().map(|r| format!("{short}@{r}"));
                    if trimmed == short || with_ref.as_deref() == Some(trimmed) {
                        return Some(s);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// `update(source)`.
    pub fn update(&mut self, source: Option<&str>) -> Result<(), String> {
        let identity = source.map(|s| self.key_for_input(s));
        let mut matched = false;
        let mut sources: Vec<(String, Scope)> = Vec::new();
        for scope in [Scope::User, Scope::Project] {
            for pkg in self.packages(scope) {
                let s = source_string(&pkg);
                if let Some(identity) = &identity
                    && self.identity(&s, Some(scope))? != *identity
                {
                    continue;
                }
                matched = true;
                sources.push((s, scope));
            }
        }
        if let Some(source) = source
            && !matched
        {
            let mut configured = self.packages(Scope::User);
            configured.extend(self.packages(Scope::Project));
            return Err(match Self::suggestion(source, &configured) {
                Some(s) => format!("No matching package found for {source}. Did you mean {s}?"),
                None => format!("No matching package found for {source}"),
            });
        }
        if offline(&self.var) || sources.is_empty() {
            return Ok(());
        }
        let mut npm: Vec<(String, Scope, NpmSource)> = Vec::new();
        let mut git: Vec<(String, Scope, GitSource)> = Vec::new();
        for (s, scope) in &sources {
            match parse_source(s) {
                Parsed::Npm(n) if !n.pinned => npm.push((s.clone(), *scope, n)),
                Parsed::Git(g) => git.push((s.clone(), *scope, g)),
                _ => {}
            }
        }
        let mut batches: [Vec<(String, NpmSource)>; 2] = [Vec::new(), Vec::new()];
        for (s, scope, n) in npm {
            if self.should_update_npm(&n, scope)? {
                batches[usize::from(scope == Scope::Project)].push((s, n));
            }
        }
        for (index, batch) in batches.iter().enumerate() {
            if batch.is_empty() {
                continue;
            }
            let scope = if index == 0 { Scope::User } else { Scope::Project };
            let message = if batch.len() == 1 {
                format!("Updating {}...", batch[0].0)
            } else {
                format!("Updating {} npm packages...", scope.name())
            };
            self.start(&message);
            let specs: Vec<String> = batch
                .iter()
                .map(|(_, n)| {
                    if n.version.is_some() {
                        n.spec.clone()
                    } else {
                        format!("{}@latest", n.name)
                    }
                })
                .collect();
            let root = self.npm_install_root(scope)?;
            self.ensure_npm_project(&root)?;
            let args = self.npm_install_args(&specs, &root)?;
            self.run_npm(args, None)?;
        }
        for (s, scope, g) in git {
            self.start(&format!("Updating {s}..."));
            self.update_git(&g, scope)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------- update checks

    /// `checkForAvailableUpdates`: the configured npm and git packages (project first, deduplicated, neither local
    /// nor pinned) that are installed and behind their source. Nothing offline. A failure to locate a package
    /// (an untrusted project's storage) fails the whole check, as in TypeScript.
    pub fn check_for_available_updates(&mut self) -> Result<Vec<PackageUpdate>, String> {
        if offline(&self.var) {
            return Ok(Vec::new());
        }
        let mut all: Vec<(Value, Scope)> = self
            .packages(Scope::Project)
            .into_iter()
            .map(|p| (p, Scope::Project))
            .collect();
        all.extend(self.packages(Scope::User).into_iter().map(|p| (p, Scope::User)));
        let mut updates = Vec::new();
        for (pkg, scope) in self.dedupe(all) {
            let source = source_string(&pkg);
            match parse_source(&source) {
                Parsed::Local(_) => {}
                Parsed::Npm(n) if n.pinned => {}
                Parsed::Git(g) if g.pinned => {}
                Parsed::Npm(n) => {
                    let path = self.npm_install_path(&n, scope)?;
                    if Path::new(&path).exists() && self.npm_has_available_update(&n, &path) {
                        updates.push(PackageUpdate {
                            source,
                            display_name: n.name.clone(),
                            kind: "npm",
                            scope,
                        });
                    }
                }
                Parsed::Git(g) => {
                    let path = self.git_install_path(&g, scope)?;
                    if Path::new(&path).exists() && self.git_has_available_update(&path) {
                        updates.push(PackageUpdate {
                            source,
                            display_name: format!("{}/{}", g.host, g.path),
                            kind: "git",
                            scope,
                        });
                    }
                }
            }
        }
        Ok(updates)
    }

    /// `npmHasAvailableUpdate`: the registry's newest matching version is above the installed one.
    fn npm_has_available_update(&self, source: &NpmSource, path: &str) -> bool {
        if offline(&self.var) {
            return false;
        }
        let Some(installed) = Self::installed_npm_version(path) else {
            return false;
        };
        let spec = if source.version.is_some() {
            &source.spec
        } else {
            &source.name
        };
        match self.latest_npm_version(spec, source.range.as_deref()) {
            // `gt` throws on an invalid version: no update.
            Ok(target) => semver::gt(&target, &installed).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// `gitHasAvailableUpdate`: HEAD differs from the remote's head of the upstream branch (else of HEAD).
    fn git_has_available_update(&self, path: &str) -> bool {
        if offline(&self.var) {
            return false;
        }
        let Ok(local) = self.git_capture(&["rev-parse", "HEAD"], path) else {
            return false;
        };
        match self.remote_git_head(path) {
            Ok(remote) => local.trim() != remote.trim(),
            Err(_) => false,
        }
    }

    /// `getRemoteGitHead`.
    fn remote_git_head(&self, path: &str) -> Result<String, String> {
        let remote = |args: &[&str]| -> Result<String, String> {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            self.capture("git", &args, Some(path), &[("GIT_TERMINAL_PROMPT", "0")])
        };
        // `getGitUpstreamRef`.
        let upstream = self
            .git_capture(&["rev-parse", "--abbrev-ref", "@{upstream}"], path)
            .ok()
            .and_then(|u| {
                let branch = u.trim().strip_prefix("origin/")?.to_string();
                (!branch.is_empty()).then(|| format!("refs/heads/{branch}"))
            });
        if let Some(upstream) = upstream {
            let out = remote(&["ls-remote", "origin", &upstream])?;
            // `/^([0-9a-f]{40})\s+/m`
            let found = out.lines().find_map(|line| {
                let (hash, rest) = line.split_at_checked(40)?;
                (is_sha(hash) && rest.starts_with(char::is_whitespace)).then(|| hash.to_string())
            });
            if let Some(hash) = found {
                return Ok(hash);
            }
        }
        let out = remote(&["ls-remote", "origin", "HEAD"])?;
        // `/^([0-9a-f]{40})\s+HEAD$/m`
        out.lines()
            .find_map(|line| {
                let (hash, rest) = line.split_at_checked(40)?;
                let gap = rest.strip_suffix("HEAD")?;
                (is_sha(hash) && !gap.is_empty() && gap.chars().all(char::is_whitespace)).then(|| hash.to_string())
            })
            .ok_or_else(|| "Failed to determine remote HEAD".to_string())
    }

    // ---------------------------------------------------------------------------------------- temporary sources

    /// `getExtensionTempFolder`: created private (0700) on every use.
    fn extension_temp_folder(&self) -> Result<String, String> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let folder = EXTENSION_TEMP_FOLDER
            .iter()
            .fold(self.agent_dir.clone(), |dir, part| join(&dir, part));
        // `mkdirSync(folder, { recursive: true, mode: 0o700 })`: every directory it creates is private.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .map_err(|e| e.to_string())?;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        Ok(folder)
    }

    /// `resolveManagedPath`: `parts` under `root`, refusing anything outside it.
    fn managed_path(root: &str, parts: &[&str]) -> Result<String, String> {
        let root = resources_paths::resolve("/", root);
        let rel: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
        let path = if rel.is_empty() {
            root.clone()
        } else {
            resources_paths::resolve(&root, &rel.join("/"))
        };
        if path != root && !path.starts_with(&format!("{root}/")) {
            return Err(format!("Refusing to use path outside package install root: {path}"));
        }
        Ok(path)
    }

    /// `getTemporaryDir`: `<temp folder>/<prefix>/<sha256(prefix-suffix)[..8]>/<suffix>`.
    fn temporary_dir(&self, prefix: &str, suffix: Option<&str>) -> Result<String, String> {
        use sha2::Digest;
        let root = Self::managed_path(&self.extension_temp_folder()?, &[prefix])?;
        let digest = sha2::Sha256::digest(format!("{prefix}-{}", suffix.unwrap_or_default()).as_bytes());
        let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        Self::managed_path(&root, &[&hash, suffix.unwrap_or_default()])
    }

    fn temporary_npm_path(&self, source: &NpmSource) -> Result<String, String> {
        Ok(join(
            &join(&self.temporary_dir("npm", None)?, "node_modules"),
            &source.name,
        ))
    }

    fn temporary_git_path(&self, source: &GitSource) -> Result<String, String> {
        self.temporary_dir(&format!("git-{}", source.host), Some(&source.path))
    }

    /// `installNpm(source, "temporary", true)`.
    fn install_npm_temporary(&self, source: &NpmSource) -> Result<(), String> {
        let root = self.temporary_dir("npm", None)?;
        self.ensure_npm_project(&root)?;
        let args = self.npm_install_args(std::slice::from_ref(&source.spec), &root)?;
        self.run_npm(args, None)
    }

    /// `resolveExtensionSources(sources, { temporary: true })`: each `-e` package installed once under the agent's
    /// temp folder (a missing one is skipped offline), an unpinned git checkout refreshed (failures keep the cached
    /// one), local paths taken from the working directory; then each package's resources.
    pub fn resolve_extension_sources(&mut self, sources: &[String]) -> Result<TemporaryResolved, String> {
        let mut acc: Accumulator = Default::default();
        let mut roots = Vec::new();
        for source in sources {
            // The metadata is internal here: the caller takes paths only.
            let mut metadata = PathMetadata {
                source: source.clone(),
                scope: Scope::User,
                origin: Origin::Package,
                base_dir: None,
            };
            match parse_source(source) {
                Parsed::Local(path) => {
                    let resolved = self.resolve_from(&path, &self.cwd.clone());
                    match std::fs::metadata(&resolved) {
                        Ok(meta) if meta.is_file() => {
                            add(&mut acc, ResourceType::Extensions, &resolved, &metadata, true);
                            roots.push(resolved);
                        }
                        Ok(meta) if meta.is_dir() => {
                            metadata.base_dir = Some(resolved.clone());
                            if !Self::collect_package_resources(&mut acc, &resolved, None, &metadata) {
                                add(&mut acc, ResourceType::Extensions, &resolved, &metadata, true);
                            }
                            roots.push(resolved);
                        }
                        _ => {}
                    }
                }
                Parsed::Npm(n) => {
                    let mut path = self.temporary_npm_path(&n)?;
                    if !Path::new(&path).exists() || !Self::npm_matches_configured(&n, &path) {
                        if offline(&self.var) {
                            continue;
                        }
                        self.install_npm_temporary(&n)?;
                        path = self.temporary_npm_path(&n)?;
                    }
                    metadata.base_dir = Some(path.clone());
                    Self::collect_package_resources(&mut acc, &path, None, &metadata);
                    roots.push(path);
                }
                Parsed::Git(g) => {
                    let path = self.temporary_git_path(&g)?;
                    if !Path::new(&path).exists() {
                        if offline(&self.var) {
                            continue;
                        }
                        self.install_git_at(&g, &path, None)?;
                    } else if !g.pinned && !offline(&self.var) {
                        // `refreshTemporaryGitSource`.
                        self.start(&format!("Refreshing {source}..."));
                        let _ = self.update_existing_git(&g, &path);
                    }
                    metadata.base_dir = Some(path.clone());
                    Self::collect_package_resources(&mut acc, &path, None, &metadata);
                    roots.push(path);
                }
            }
        }
        let resolved = to_resolved(acc);
        let enabled = |list: &[ResolvedResource]| -> Vec<String> {
            list.iter().filter(|r| r.enabled).map(|r| r.path.clone()).collect()
        };
        Ok(TemporaryResolved {
            extensions: enabled(&resolved.extensions),
            skills: enabled(&resolved.skills),
            prompts: enabled(&resolved.prompts),
            themes: enabled(&resolved.themes),
            roots,
        })
    }

    // ---------------------------------------------------------------------------------------- resolve

    fn dedupe(&self, packages: Vec<(Value, Scope)>) -> Vec<(Value, Scope)> {
        let mut result: Vec<(Value, Scope)> = Vec::new();
        let mut seen: IndexMap<String, usize> = IndexMap::new();
        for (pkg, scope) in packages {
            let Ok(identity) = self.identity(&source_string(&pkg), Some(scope)) else {
                continue;
            };
            match seen.get(&identity) {
                None => {
                    seen.insert(identity, result.len());
                    result.push((pkg, scope));
                }
                Some(&index) => {
                    let (existing, existing_scope) = &result[index];
                    if *existing_scope == Scope::Project && scope == Scope::User {
                        if existing.get("autoload") == Some(&Value::Bool(false)) {
                            result.push((pkg, scope));
                        }
                    } else if scope == Scope::Project {
                        result[index] = (pkg, scope);
                    }
                }
            }
        }
        result
    }

    /// `findAutoloadDeltaBase`.
    fn delta_base(&self, pkg: &Value, scope: Scope, sources: &[(Value, Scope)]) -> Option<(String, Scope)> {
        if scope != Scope::Project || pkg.get("autoload") != Some(&Value::Bool(false)) {
            return None;
        }
        let identity = self.identity(&source_string(pkg), Some(scope)).ok()?;
        sources
            .iter()
            .find(|(p, s)| {
                *s == Scope::User
                    && self.identity(&source_string(p), Some(Scope::User)).ok().as_ref() == Some(&identity)
            })
            .map(|(p, _)| (source_string(p), Scope::User))
    }

    /// `collectManifestFiles`: (all files, as the manifest's own patterns enable them).
    fn manifest_files(root: &str, kind: ResourceType) -> Vec<String> {
        let manifest = resources::read_pi_manifest(&join(root, "package.json"));
        if let Some(entries) = manifest
            .as_ref()
            .and_then(|m| resources::manifest_entries(m, kind))
            .filter(|e| !e.is_empty())
        {
            let all = resources::files_from_manifest_entries(entries, root, kind);
            let patterns = resources::override_patterns(entries);
            return if patterns.is_empty() {
                all
            } else {
                resources::apply_patterns(&all, &patterns, root)
            };
        }
        resources::convention_files(root, kind).unwrap_or_default()
    }

    fn add_manifest_entries(
        acc: &mut Accumulator,
        entries: &[String],
        root: &str,
        kind: ResourceType,
        m: &PathMetadata,
    ) {
        let all = resources::files_from_manifest_entries(entries, root, kind);
        let enabled = resources::apply_patterns(&all, &resources::override_patterns(entries), root);
        for f in &all {
            if enabled.contains(f) {
                add(acc, kind, f, m, true);
            }
        }
    }

    /// `collectPackageResources`; false when the package offers nothing (no filter, manifest or convention dir).
    fn collect_package_resources(
        acc: &mut Accumulator,
        root: &str,
        filter: Option<&Map<String, Value>>,
        m: &PathMetadata,
    ) -> bool {
        if let Some(filter) = filter {
            for kind in RESOURCE_TYPES {
                let patterns = string_list(filter.get(kind.key()));
                if filter.get("autoload") == Some(&Value::Bool(false)) {
                    let patterns = patterns.unwrap_or_default();
                    if patterns.is_empty() {
                        continue;
                    }
                    let all = Self::manifest_files(root, kind);
                    for (f, enabled) in resources::apply_autoload_disabled_patterns(&all, &patterns, root) {
                        add(acc, kind, &f, m, enabled);
                    }
                } else if let Some(patterns) = patterns {
                    let all = Self::manifest_files(root, kind);
                    if patterns.is_empty() {
                        for f in &all {
                            add(acc, kind, f, m, false);
                        }
                        continue;
                    }
                    let enabled = resources::apply_patterns(&all, &patterns, root);
                    for f in &all {
                        add(acc, kind, f, m, enabled.contains(f));
                    }
                } else {
                    let manifest = resources::read_pi_manifest(&join(root, "package.json"));
                    if let Some(entries) = manifest.as_ref().and_then(|mf| resources::manifest_entries(mf, kind)) {
                        Self::add_manifest_entries(acc, entries, root, kind, m);
                    } else if let Some(files) = resources::convention_files(root, kind) {
                        for f in files {
                            add(acc, kind, &f, m, true);
                        }
                    }
                }
            }
            return true;
        }
        if let Some(manifest) = resources::read_pi_manifest(&join(root, "package.json")) {
            for kind in RESOURCE_TYPES {
                if let Some(entries) = resources::manifest_entries(&manifest, kind) {
                    Self::add_manifest_entries(acc, entries, root, kind, m);
                }
            }
            return true;
        }
        let mut any = false;
        for kind in RESOURCE_TYPES {
            if let Some(files) = resources::convention_files(root, kind) {
                for f in files {
                    add(acc, kind, &f, m, true);
                }
                any = true;
            }
        }
        any
    }

    /// `installedNpmMatchesConfiguredVersion`.
    fn npm_matches_configured(source: &NpmSource, path: &str) -> bool {
        match Self::installed_npm_version(path) {
            None => false,
            Some(v) => source.range.as_deref().is_none_or(|r| semver::satisfies(&v, r)),
        }
    }

    /// `resolve()` without an `onMissing` handler: missing npm and git packages are installed (unless offline).
    pub fn resolve(&mut self) -> Result<ResolvedPaths, String> {
        self.resolve_with_roots().map(|(resolved, _)| resolved)
    }

    /// [`PackageManager::resolve`], with the directory of every configured package it found (in precedence order),
    /// where a session discovers the packages' extension manifests.
    pub fn resolve_with_roots(&mut self) -> Result<(ResolvedPaths, Vec<PackageRoot>), String> {
        let mut roots: Vec<PackageRoot> = Vec::new();
        let mut acc: Accumulator = Default::default();
        let mut all: Vec<(Value, Scope)> = self
            .packages(Scope::Project)
            .into_iter()
            .map(|p| (p, Scope::Project))
            .collect();
        all.extend(self.packages(Scope::User).into_iter().map(|p| (p, Scope::User)));
        let sources = self.dedupe(all);
        for (pkg, scope) in &sources {
            let source = source_string(pkg);
            let filter = filter_of(pkg);
            let (resolved_source, resolved_scope) = self
                .delta_base(pkg, *scope, &sources)
                .unwrap_or((source.clone(), *scope));
            let mut metadata = PathMetadata {
                source: source.clone(),
                scope: *scope,
                origin: Origin::Package,
                base_dir: None,
            };
            match parse_source(&resolved_source) {
                Parsed::Local(path) => {
                    let base = self.base_dir(resolved_scope)?;
                    let resolved = self.resolve_from(&path, &base);
                    match std::fs::metadata(&resolved) {
                        Ok(meta) if meta.is_file() => {
                            metadata.base_dir = Some(dirname(&resolved));
                            add(&mut acc, ResourceType::Extensions, &resolved, &metadata, true);
                        }
                        Ok(meta) if meta.is_dir() => {
                            metadata.base_dir = Some(resolved.clone());
                            if !Self::collect_package_resources(&mut acc, &resolved, filter, &metadata) {
                                add(&mut acc, ResourceType::Extensions, &resolved, &metadata, true);
                            }
                            roots.push(PackageRoot::new(&resolved, *scope, filter));
                        }
                        _ => {}
                    }
                }
                Parsed::Npm(n) => {
                    let mut path = self.npm_install_path(&n, resolved_scope)?;
                    if !Path::new(&path).exists() || !Self::npm_matches_configured(&n, &path) {
                        if offline(&self.var) {
                            continue;
                        }
                        self.install_npm(&n, resolved_scope)?;
                        path = self.npm_install_path(&n, resolved_scope)?;
                    }
                    metadata.base_dir = Some(path.clone());
                    Self::collect_package_resources(&mut acc, &path, filter, &metadata);
                    roots.push(PackageRoot::new(&path, *scope, filter));
                }
                Parsed::Git(g) => {
                    let path = self.git_install_path(&g, resolved_scope)?;
                    if !Path::new(&path).exists() {
                        if offline(&self.var) {
                            continue;
                        }
                        self.install_git(&g, resolved_scope)?;
                    }
                    metadata.base_dir = Some(path.clone());
                    Self::collect_package_resources(&mut acc, &path, filter, &metadata);
                    roots.push(PackageRoot::new(&path, *scope, filter));
                }
            }
        }
        let global_base = self.agent_dir.clone();
        let project_base = join(&self.cwd, config::paths::CONFIG_DIR_NAME);
        let entries = |scope: Scope, kind: ResourceType, settings: &SettingsManager| -> Vec<String> {
            let s = match scope {
                Scope::User => settings.global_settings(),
                Scope::Project => settings.project_settings(),
            };
            string_list(s.get(kind.key())).unwrap_or_default()
        };
        for kind in RESOURCE_TYPES {
            for (scope, base) in [(Scope::Project, &project_base), (Scope::User, &global_base)] {
                let list = entries(scope, kind, &self.settings);
                if list.is_empty() {
                    continue;
                }
                let (plain, patterns): (Vec<String>, Vec<String>) =
                    list.into_iter().partition(|e| !resources::is_pattern(e));
                let resolved: Vec<String> = plain.iter().map(|p| self.resolve_from(p, base)).collect();
                let files = resources::files_from_paths(&resolved, kind);
                let enabled = resources::apply_patterns(&files, &patterns, base);
                let m = PathMetadata {
                    source: "local".into(),
                    scope,
                    origin: Origin::TopLevel,
                    base_dir: None,
                };
                for f in &files {
                    add(&mut acc, kind, f, &m, enabled.contains(f));
                }
            }
        }
        self.add_auto_discovered(&mut acc, &global_base, &project_base, &entries);
        Ok((to_resolved(acc), roots))
    }

    /// `addAutoDiscoveredResources`.
    fn add_auto_discovered(
        &self,
        acc: &mut Accumulator,
        global_base: &str,
        project_base: &str,
        entries: &dyn Fn(Scope, ResourceType, &SettingsManager) -> Vec<String>,
    ) {
        let meta = |scope: Scope, base: &str| PathMetadata {
            source: "auto".into(),
            scope,
            origin: Origin::TopLevel,
            base_dir: Some(base.to_string()),
        };
        let mut put = |kind: ResourceType, paths: Vec<String>, m: &PathMetadata, overrides: &[String], base: &str| {
            for p in paths {
                let enabled = resources::enabled_by_overrides(&p, overrides, base);
                add(acc, kind, &p, m, enabled);
            }
        };
        let user_agents = join(&join(&self.home, ".agents"), "skills");
        let trusted = self.settings.is_project_trusted();
        let project_agents: Vec<String> = if trusted {
            let user_resolved = resources_paths::resolve("/", &user_agents);
            resources::ancestor_agents_skill_dirs(&self.cwd)
                .into_iter()
                .filter(|d| resources_paths::resolve("/", d) != user_resolved)
                .collect()
        } else {
            Vec::new()
        };
        let o = |scope: Scope, kind: ResourceType| entries(scope, kind, &self.settings);
        let pm = meta(Scope::Project, project_base);
        let dir = |base: &str, kind: ResourceType| join(base, kind.key());
        if trusted {
            put(
                ResourceType::Extensions,
                resources::auto_extensions(&dir(project_base, ResourceType::Extensions)),
                &pm,
                &o(Scope::Project, ResourceType::Extensions),
                project_base,
            );
            put(
                ResourceType::Skills,
                resources::auto_skills(&dir(project_base, ResourceType::Skills), false),
                &pm,
                &o(Scope::Project, ResourceType::Skills),
                project_base,
            );
        }
        for agents_dir in &project_agents {
            let base = dirname(agents_dir);
            put(
                ResourceType::Skills,
                resources::auto_skills(agents_dir, true),
                &meta(Scope::Project, &base),
                &o(Scope::Project, ResourceType::Skills),
                &base,
            );
        }
        if trusted {
            put(
                ResourceType::Prompts,
                resources::auto_prompts(&dir(project_base, ResourceType::Prompts)),
                &pm,
                &o(Scope::Project, ResourceType::Prompts),
                project_base,
            );
            put(
                ResourceType::Themes,
                resources::auto_themes(&dir(project_base, ResourceType::Themes)),
                &pm,
                &o(Scope::Project, ResourceType::Themes),
                project_base,
            );
        }
        let um = meta(Scope::User, global_base);
        put(
            ResourceType::Extensions,
            resources::auto_extensions(&dir(global_base, ResourceType::Extensions)),
            &um,
            &o(Scope::User, ResourceType::Extensions),
            global_base,
        );
        put(
            ResourceType::Skills,
            resources::auto_skills(&dir(global_base, ResourceType::Skills), false),
            &um,
            &o(Scope::User, ResourceType::Skills),
            global_base,
        );
        let user_agents_base = dirname(&user_agents);
        put(
            ResourceType::Skills,
            resources::auto_skills(&user_agents, true),
            &meta(Scope::User, &user_agents_base),
            &o(Scope::User, ResourceType::Skills),
            &user_agents_base,
        );
        put(
            ResourceType::Prompts,
            resources::auto_prompts(&dir(global_base, ResourceType::Prompts)),
            &um,
            &o(Scope::User, ResourceType::Prompts),
            global_base,
        );
        put(
            ResourceType::Themes,
            resources::auto_themes(&dir(global_base, ResourceType::Themes)),
            &um,
            &o(Scope::User, ResourceType::Themes),
            global_base,
        );
    }
}

/// `toResolvedPaths`: by precedence (stable), the first of each canonical path kept.
fn to_resolved(acc: Accumulator) -> ResolvedPaths {
    let convert = |map: &IndexMap<String, (PathMetadata, bool)>| -> Vec<ResolvedResource> {
        let mut list: Vec<ResolvedResource> = map
            .iter()
            .map(|(path, (metadata, enabled))| ResolvedResource {
                path: path.clone(),
                enabled: *enabled,
                metadata: metadata.clone(),
            })
            .collect();
        list.sort_by_key(|r| rank(&r.metadata));
        let mut seen = std::collections::HashSet::new();
        list.retain(|r| seen.insert(canonicalize(&r.path)));
        list
    };
    ResolvedPaths {
        extensions: convert(&acc[0]),
        skills: convert(&acc[1]),
        prompts: convert(&acc[2]),
        themes: convert(&acc[3]),
    }
}
