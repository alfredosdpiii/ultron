//! `ultron install|remove|uninstall|update|list` and `ultron config` (`package-manager-cli.ts`): argument handling,
//! help, the package operations of [`crate::packages::manager`], `update --models`, the self-update check, and the
//! resource selector ([`crate::config_selector`]).
//!
//! The self-update: the Rust build is one self-contained binary, as TypeScript's Bun binary is, so it asks the npm
//! registry for the latest release (as TypeScript does) and, when one is newer, says where to download it instead of
//! replacing itself (TypeScript's `bun-binary` install method).

use crate::Env;
use crate::packages::manager::{PackageManager, Scope};
use config::settings::SettingsManager;
use serde_json::Value;
use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    Install,
    Remove,
    Update,
    List,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Command::Install => "install",
            Command::Remove => "remove",
            Command::Update => "update",
            Command::List => "list",
        }
    }

    fn usage(self) -> &'static str {
        match self {
            Command::Install => "ultron install <source> [-l] [--approve|--no-approve]",
            Command::Remove => "ultron remove <source> [-l] [--approve|--no-approve]",
            Command::Update => {
                "ultron update [source|self] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]"
            }
            Command::List => "ultron list [--approve|--no-approve]",
        }
    }

    fn help(self) -> String {
        let usage = self.usage();
        match self {
            Command::Install => format!(
                "Usage:
  {usage}

Install a package and add it to settings.

Options:
  -l, --local       Install project-locally (.pi/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  ultron install npm:@foo/bar
  ultron install git:github.com/user/repo
  ultron install git:git@github.com:user/repo
  ultron install https://github.com/user/repo
  ultron install ssh://git@github.com/user/repo
  ultron install ./local/path
"
            ),
            Command::Remove => format!(
                "Usage:
  {usage}

Remove a package and its source from settings.
Alias: ultron uninstall <source> [-l]

Options:
  -l, --local       Remove from project settings (.pi/settings.json)
  -a, --approve     Trust project-local files for this command
  -na, --no-approve Ignore project-local files for this command

Examples:
  ultron remove npm:@foo/bar
  ultron uninstall npm:@foo/bar
"
            ),
            Command::Update => format!(
                "Usage:
  {usage}

Update ultron, installed packages, or model catalogs.
ultron updates itself from the npm package ultron-agent.

Options:
  --self                  Update ultron only (default when no target is given)
  --extensions            Update installed packages only
  --models                Refresh model catalogs only
  --all                   Update ultron and installed packages
  --extension <source>    Update one package only
  -a, --approve           Trust project-local files for this command
  -na, --no-approve       Ignore project-local files for this command
  --force                 Reinstall ultron even if the current version is latest

Short forms:
  ultron update                Update ultron only
  ultron update --all          Update ultron and all extensions
  ultron update --models       Refresh model catalogs only
  ultron update <source>       Update one package
  ultron update self           Update ultron only (ultron and pi work as aliases)
"
            ),
            Command::List => format!(
                "Usage:
  {usage}

List installed packages from user and project settings.

Options:
  -a, --approve      Trust project-local files for this command
  -na, --no-approve  Ignore project-local files for this command
"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum UpdateTarget {
    All,
    SelfOnly,
    Extensions(Option<String>),
    Models,
}

#[derive(Debug, Default)]
struct Options {
    command: Option<Command>,
    source: Option<String>,
    update_target: Option<UpdateTarget>,
    show_extensions_skipped_note: bool,
    local: bool,
    force: bool,
    trust_override: Option<bool>,
    help: bool,
    invalid_option: Option<String>,
    invalid_argument: Option<String>,
    missing_option_value: Option<String>,
    conflicting_options: Option<String>,
}

/// `parsePackageCommand`.
fn parse(args: &[String]) -> Option<Options> {
    let command = match args.first().map(String::as_str) {
        Some("uninstall" | "remove") => Command::Remove,
        Some("install") => Command::Install,
        Some("update") => Command::Update,
        Some("list") => Command::List,
        _ => return None,
    };
    let mut o = Options {
        command: Some(command),
        ..Options::default()
    };
    let (mut self_flag, mut extensions_flag, mut models_flag, mut all_flag) = (false, false, false, false);
    let mut extension_source: Option<String> = None;
    let rest = &args[1..];
    let mut index = 0;
    let invalid = |o: &mut Options, arg: &str| {
        o.invalid_option.get_or_insert_with(|| arg.to_string());
    };
    let update = command == Command::Update;
    while index < rest.len() {
        let arg = rest[index].as_str();
        match arg {
            "-h" | "--help" => o.help = true,
            "-l" | "--local" => {
                if matches!(command, Command::Install | Command::Remove) {
                    o.local = true;
                } else {
                    invalid(&mut o, arg);
                }
            }
            "--self" if update => self_flag = true,
            "--extensions" if update => extensions_flag = true,
            "--models" if update => models_flag = true,
            "--all" if update => all_flag = true,
            "--force" if update => o.force = true,
            "--self" | "--extensions" | "--models" | "--all" | "--force" => invalid(&mut o, arg),
            "--approve" | "-a" => o.trust_override = Some(true),
            "--no-approve" | "-na" => o.trust_override = Some(false),
            "--extension" => {
                if !update {
                    invalid(&mut o, arg);
                } else {
                    match rest.get(index + 1).filter(|v| !v.is_empty() && !v.starts_with('-')) {
                        None => {
                            o.missing_option_value.get_or_insert_with(|| arg.to_string());
                        }
                        Some(_) if extension_source.is_some() => {
                            o.conflicting_options
                                .get_or_insert_with(|| "--extension can only be provided once".into());
                            index += 1;
                        }
                        Some(value) => {
                            extension_source = Some(value.clone());
                            index += 1;
                        }
                    }
                }
            }
            _ if arg.starts_with('-') => invalid(&mut o, arg),
            _ => {
                if o.source.is_none() {
                    o.source = Some(arg.to_string());
                } else {
                    o.invalid_argument.get_or_insert_with(|| arg.to_string());
                }
            }
        }
        index += 1;
    }
    if update {
        let mut conflict = |message: &str| {
            o.conflicting_options.get_or_insert_with(|| message.to_string());
        };
        if all_flag && (self_flag || extensions_flag || models_flag || extension_source.is_some()) {
            conflict("--all cannot be combined with --self, --extensions, --models, or --extension");
        }
        if all_flag && o.source.is_some() {
            conflict("--all cannot be combined with a positional source");
        }
        let target = if models_flag {
            if self_flag || extensions_flag || all_flag || extension_source.is_some() {
                conflict("--models cannot be combined with --self, --extensions, --all, or --extension");
            }
            if o.source.is_some() {
                conflict("--models cannot be combined with a positional source");
            }
            UpdateTarget::Models
        } else if let Some(source) = extension_source {
            if self_flag || extensions_flag || all_flag {
                conflict("--extension cannot be combined with --self, --extensions, or --all");
            }
            if o.source.is_some() {
                conflict("--extension cannot be combined with a positional source");
            }
            UpdateTarget::Extensions(Some(source))
        } else if let Some(source) = o.source.clone() {
            if matches!(source.as_str(), "self" | "pi" | "ultron") {
                if extensions_flag {
                    UpdateTarget::All
                } else {
                    UpdateTarget::SelfOnly
                }
            } else {
                if extensions_flag || self_flag || all_flag {
                    conflict("positional update targets cannot be combined with --self, --extensions, or --all");
                }
                UpdateTarget::Extensions(Some(source))
            }
        } else if all_flag || (self_flag && extensions_flag) {
            UpdateTarget::All
        } else if self_flag {
            UpdateTarget::SelfOnly
        } else if extensions_flag {
            UpdateTarget::Extensions(None)
        } else {
            o.show_extensions_skipped_note = true;
            UpdateTarget::SelfOnly
        };
        o.update_target = Some(target);
    }
    Some(o)
}

struct Io<'a> {
    out: &'a mut dyn std::io::Write,
    err: &'a mut dyn std::io::Write,
}

/// Where the self-update asks for the latest release (`LATEST_VERSION_URL`).
pub const LATEST_VERSION_URL: &str = "https://registry.npmjs.org/ultron-agent/latest";
/// `RELEASES_URL`.
pub const RELEASES_URL: &str = "https://github.com/alfredosdpiii/ultron/releases";

/// What a package command reaches outside the machine (tests point it at a local server).
pub struct Endpoints {
    pub latest_version_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            latest_version_url: LATEST_VERSION_URL.into(),
        }
    }
}

/// Handle a package command; None when `argv` is not one.
pub fn run_package(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Option<i32> {
    run_package_with(argv, env, out, err, &Endpoints::default())
}

/// [`run_package`] with explicit endpoints.
pub fn run_package_with(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    endpoints: &Endpoints,
) -> Option<i32> {
    let options = parse(argv)?;
    let mut io = Io { out, err };
    Some(package_command(&options, env, &mut io, endpoints))
}

fn usage_error(io: &mut Io, message: &str, usage: &str) -> i32 {
    let _ = writeln!(io.err, "{message}");
    let _ = writeln!(io.err, "Usage: {usage}");
    1
}

/// Settings with the project trust resolved without asking anyone: the override, the saved decision (only, with
/// `saved_only`), else nothing to trust, the saved decision or `defaultProjectTrust`; undecided is untrusted. For
/// background work (the startup update check, `-e` installs) that must not start extensions or prompt.
pub(crate) fn settings_manager(env: &Env, trust_override: Option<bool>, saved_only: bool) -> SettingsManager {
    let agent_dir = env.agent_dir();
    let mut settings = SettingsManager::create(&env.cwd, &agent_dir, false);
    let store = config::trust::ProjectTrustStore::new(&agent_dir);
    let trusted = if saved_only {
        trust_override.unwrap_or(store.get(&env.cwd).ok().flatten() == Some(true))
    } else {
        match config::trust::resolve_project_trusted(
            &env.cwd,
            &env.home,
            &store,
            trust_override,
            settings.default_project_trust(),
        ) {
            Ok(config::trust::TrustResolution::Trusted(t)) => t,
            _ => false,
        }
    };
    settings.set_project_trusted(trusted);
    settings
}

/// `createCommandSettingsManager` (package-manager-cli.ts) for the package and config commands: with neither an
/// override nor `saved_only`, a project with trust-requiring resources first asks the `project_trust` handlers of the
/// extensions that load without the project (`loadProjectTrustExtensions`: user and global ones); the first yes/no
/// wins and `remember` saves it. Load failures and handler errors come back as warnings
/// (`reportProjectTrustWarnings` prints each as `Warning: <text>`). An undecided project without a saved decision or
/// default is untrusted (the TypeScript command prompts in a terminal; this one does not).
pub(crate) fn command_settings(
    env: &Env,
    trust_override: Option<bool>,
    saved_only: bool,
) -> (SettingsManager, Vec<String>) {
    use std::io::IsTerminal;
    if saved_only
        || trust_override.is_some()
        || !config::trust::has_trust_requiring_project_resources(&env.cwd, &env.home)
    {
        return (settings_manager(env, trust_override, saved_only), Vec::new());
    }
    let agent_dir = env.agent_dir();
    let mut warnings = Vec::new();
    let untrusted = SettingsManager::create(&env.cwd, &agent_dir, false);
    let discovery = extensions::discover_with_settings(&env.cwd, &agent_dir, &env.home, &untrusted);
    for diagnostic in &discovery.diagnostics {
        warnings.push(format!(
            "Failed to load extension \"{}\": {}",
            diagnostic.path.display(),
            diagnostic.message
        ));
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let manifests = discovery.manifests;
    let decision = if manifests.is_empty() {
        None
    } else {
        let host = extensions::process::ExtensionHost::load(
            manifests,
            extensions::process::SessionInfo {
                cwd: env.cwd.display().to_string(),
                mode: if interactive { "tui" } else { "print" }.into(),
                has_ui: interactive,
                flags: Default::default(),
            },
            std::sync::Arc::new(extensions::process::NoHost),
        );
        for diagnostic in &host.diagnostics {
            warnings.push(format!(
                "Failed to load extension \"{}\": {}",
                diagnostic.path.display(),
                diagnostic.message
            ));
        }
        let (result, errors) = host
            .router()
            .emit_project_trust(&serde_json::json!({ "type": "project_trust", "cwd": env.cwd.display().to_string() }));
        for error in errors {
            warnings.push(format!(
                "Extension \"{}\" project_trust error: {}",
                error.extension_path, error.error
            ));
        }
        host.shutdown();
        result
    };
    let Some(result) = decision else {
        return (settings_manager(env, None, false), warnings);
    };
    let trusted = result.get("trusted") == Some(&serde_json::Value::from("yes"));
    if result.get("remember") == Some(&serde_json::Value::Bool(true)) {
        let _ = config::trust::ProjectTrustStore::new(&agent_dir).set(&env.cwd, Some(trusted));
    }
    let mut settings = SettingsManager::create(&env.cwd, &agent_dir, false);
    settings.set_project_trusted(trusted);
    (settings, warnings)
}

/// `reportProjectTrustWarnings`.
fn report_trust_warnings(err: &mut dyn Write, warnings: &[String]) {
    for warning in warnings {
        let _ = writeln!(err, "Warning: {warning}");
    }
}

fn package_command(o: &Options, env: &Env, io: &mut Io, endpoints: &Endpoints) -> i32 {
    let command = o.command.unwrap_or(Command::List);
    if o.help {
        let _ = writeln!(io.out, "{}", command.help());
        return 0;
    }
    if let Some(option) = &o.invalid_option {
        let _ = writeln!(io.err, "Unknown option {option} for \"{}\".", command.name());
        let _ = writeln!(io.err, "Use \"ultron --help\" or \"{}\".", command.usage());
        return 1;
    }
    if let Some(option) = &o.missing_option_value {
        return usage_error(io, &format!("Missing value for {option}."), command.usage());
    }
    if let Some(argument) = &o.invalid_argument {
        return usage_error(io, &format!("Unexpected argument {argument}."), command.usage());
    }
    if let Some(conflict) = &o.conflicting_options {
        return usage_error(io, conflict, command.usage());
    }
    if matches!(command, Command::Install | Command::Remove) && o.source.is_none() {
        return usage_error(io, &format!("Missing {} source.", command.name()), command.usage());
    }
    if o.update_target == Some(UpdateTarget::Models) {
        // `refreshModelCatalogs`: every provider with a credential refreshes its stored catalog.
        return match session_core::model_refresh::refresh_model_catalogs(
            env,
            ai::model_refresh::DEFAULT_CATALOG_BASE_URL,
            &ai::model_refresh::management_user_agent(&crate::version()),
        ) {
            Ok(()) => {
                let _ = writeln!(io.out, "Model catalogs refreshed");
                0
            }
            Err(error) => {
                let _ = writeln!(io.err, "Error: {error}");
                1
            }
        };
    }
    let writes_project = matches!(command, Command::Install | Command::Remove) && o.local;
    let (settings, warnings) = command_settings(env, o.trust_override, command == Command::Update);
    report_trust_warnings(io.err, &warnings);
    if !settings.is_project_trusted() && writes_project {
        let _ = writeln!(
            io.err,
            "Project is not trusted. Use --approve to modify local package config."
        );
        return 1;
    }
    let mut manager = PackageManager::new(env, settings);
    let scope = if o.local { Scope::Project } else { Scope::User };
    let result = match command {
        Command::List => list(&mut manager, io),
        Command::Install => {
            let source = o.source.clone().unwrap_or_default();
            with_progress(&mut manager, io, |m| m.install_and_persist(&source, scope)).map(|()| {
                let _ = writeln!(io.out, "Installed {source}");
                0
            })
        }
        Command::Remove => {
            let source = o.source.clone().unwrap_or_default();
            with_progress(&mut manager, io, |m| m.remove_and_persist(&source, scope)).map(|removed| {
                if removed {
                    let _ = writeln!(io.out, "Removed {source}");
                    0
                } else {
                    let _ = writeln!(io.err, "No matching package found for {source}");
                    1
                }
            })
        }
        Command::Update => update(&mut manager, env, o, io, endpoints),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(io.err, "Error: {message}");
            1
        }
    }
}

/// Run `operation` with the progress callback writing each start message to stdout (`setProgressCallback`).
fn with_progress<T>(
    manager: &mut PackageManager,
    io: &mut Io,
    operation: impl FnOnce(&mut PackageManager) -> Result<T, String>,
) -> Result<T, String> {
    // The messages go out as they come, on the process's stdout, before any child process inherits it.
    let _ = io.out.flush();
    manager.progress = Some(Box::new(move |message: &str| {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{message}");
        let _ = stdout.flush();
    }));
    let result = operation(manager);
    manager.progress = None;
    let _ = io.out.flush();
    result
}

fn list(manager: &mut PackageManager, io: &mut Io) -> Result<i32, String> {
    let configured = manager.list_configured();
    if configured.is_empty() {
        let _ = writeln!(io.out, "No packages installed.");
        return Ok(0);
    }
    let show = |scope: Scope, io: &mut Io| {
        for pkg in configured.iter().filter(|p| p.scope == scope) {
            let display = if pkg.filtered {
                format!("{} (filtered)", pkg.source)
            } else {
                pkg.source.clone()
            };
            let _ = writeln!(io.out, "  {display}");
            if let Some(path) = &pkg.installed_path {
                let _ = writeln!(io.out, "    {path}");
            }
        }
    };
    let user = configured.iter().any(|p| p.scope == Scope::User);
    let project = configured.iter().any(|p| p.scope == Scope::Project);
    if user {
        let _ = writeln!(io.out, "User packages:");
        show(Scope::User, io);
    }
    if project {
        if user {
            let _ = writeln!(io.out);
        }
        let _ = writeln!(io.out, "Project packages:");
        show(Scope::Project, io);
    }
    Ok(0)
}

fn update(
    manager: &mut PackageManager,
    env: &Env,
    o: &Options,
    io: &mut Io,
    endpoints: &Endpoints,
) -> Result<i32, String> {
    let target = o.update_target.clone().unwrap_or(UpdateTarget::SelfOnly);
    if o.show_extensions_skipped_note {
        let _ = writeln!(
            io.out,
            "Extensions are skipped. Run ultron update --extensions to update extensions."
        );
    }
    if matches!(target, UpdateTarget::Extensions(_) | UpdateTarget::All) {
        let source = if let UpdateTarget::Extensions(s) = &target {
            s.clone()
        } else {
            None
        };
        let _ = io.out.flush();
        with_progress(manager, io, |m| m.update(source.as_deref()))?;
        let _ = match &source {
            Some(s) => writeln!(io.out, "Updated {s}"),
            None => writeln!(io.out, "Updated packages"),
        };
    }
    if matches!(target, UpdateTarget::All | UpdateTarget::SelfOnly) {
        return self_update(env, o.force, io, endpoints);
    }
    Ok(0)
}

/// `formatVersionCheckError` for a request that got no response: `fetch failed`, with the socket error's code.
fn fetch_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "The operation was aborted due to timeout".into();
    }
    let mut source: Option<&dyn std::error::Error> = Some(error);
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            let code = match io.kind() {
                std::io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
                std::io::ErrorKind::ConnectionReset => Some("ECONNRESET"),
                std::io::ErrorKind::TimedOut => Some("ETIMEDOUT"),
                _ => None,
            };
            if let Some(code) = code {
                return format!("fetch failed ({code})");
            }
        }
        source = e.source();
    }
    "fetch failed".into()
}

/// `getLatestRelease(VERSION, { retry: true })`: up to two retries after a failure or a transient status, within
/// ten seconds. `Ok(None)`: offline, an error status, or no valid version in the document.
fn latest_release(env: &Env, url: &str) -> Result<Option<String>, String> {
    if (env.var)("PI_OFFLINE").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let user_agent = ai::model_refresh::management_user_agent(&crate::version());
    runtime.block_on(async {
        let client = reqwest::Client::new();
        let work = async {
            const RETRYABLE: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];
            let mut attempt = 0;
            loop {
                let sent = client
                    .get(url)
                    .header("user-agent", &user_agent)
                    .header("accept", "application/json")
                    .send()
                    .await;
                match sent {
                    Ok(response) if RETRYABLE.contains(&response.status().as_u16()) && attempt < 2 => {}
                    Ok(response) => {
                        if !response.status().is_success() {
                            return Ok(None);
                        }
                        let text = response.text().await.unwrap_or_default();
                        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let version = data.get("version").and_then(Value::as_str).map(str::trim);
                        return Ok(version
                            .filter(|v| crate::packages::semver::valid(v).is_some())
                            .map(str::to_string));
                    }
                    Err(error) if attempt >= 2 => return Err(fetch_error(&error)),
                    Err(_) => {}
                }
                attempt += 1;
            }
        };
        match tokio::time::timeout(std::time::Duration::from_secs(10), work).await {
            Ok(result) => result,
            Err(_) => Err("The operation was aborted due to timeout".into()),
        }
    })
}

/// `isNewerPackageVersion`.
fn is_newer(candidate: &str, current: &str) -> bool {
    match crate::packages::semver::compare(candidate.trim(), current.trim()) {
        Some(order) => order == std::cmp::Ordering::Greater,
        None => candidate.trim() != current.trim(),
    }
}

/// `ultron update --self` for the self-contained binary.
fn self_update(env: &Env, force: bool, io: &mut Io, endpoints: &Endpoints) -> Result<i32, String> {
    let env_version = crate::version();
    let latest = match latest_release(env, &endpoints.latest_version_url) {
        Ok(Some(version)) => version,
        Ok(None) => return Err("Could not determine latest ultron version.".into()),
        Err(error) => return Err(format!("Could not determine latest ultron version: {error}")),
    };
    if !force && !is_newer(&latest, &env_version) {
        let _ = writeln!(io.out, "ultron is already up to date (v{env_version})");
        return Ok(0);
    }
    // `printSelfUpdateUnavailable` for a self-contained binary (TypeScript's `bun-binary` method).
    let _ = writeln!(io.err, "error: ultron cannot self-update this installation.");
    let _ = writeln!(io.err, "Download from: {RELEASES_URL}/latest");
    if let Ok(exe) = std::env::current_exe() {
        let _ = writeln!(io.err);
        let _ = writeln!(io.err, "Location of ultron executable: {}", exe.display());
    }
    Ok(1)
}

/// `ultron config`: argument checks, then the resource selector.
pub fn run_config(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("config") {
        return None;
    }
    let rest = &argv[1..];
    let usage = "ultron config [-l] [--approve|--no-approve]";
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        let _ = writeln!(
            out,
            "Usage:
  {usage}

Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/.pi/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides (.pi/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l
"
        );
        return Some(0);
    }
    let mut local = false;
    let mut trust_override = None;
    for arg in rest {
        match arg.as_str() {
            "-l" | "--local" => local = true,
            "-a" | "--approve" => trust_override = Some(true),
            "-na" | "--no-approve" => trust_override = Some(false),
            a if a.starts_with('-') => {
                let _ = writeln!(err, "Unknown option {a} for \"config\".");
                let _ = writeln!(err, "Use \"ultron --help\" or \"{usage}\".");
                return Some(1);
            }
            a => {
                let _ = writeln!(err, "Unexpected argument {a}.");
                let _ = writeln!(err, "Usage: {usage}");
                return Some(1);
            }
        }
    }
    let (settings, warnings) = command_settings(env, trust_override, false);
    report_trust_warnings(err, &warnings);
    if local && !settings.is_project_trusted() {
        let _ = writeln!(
            err,
            "Project is not trusted. Use --approve to modify local resource config."
        );
        return Some(1);
    }
    match crate::config_selector::run(env, settings, local) {
        Ok(()) => Some(0),
        Err(message) => {
            let _ = writeln!(err, "Error: {message}");
            Some(1)
        }
    }
}
