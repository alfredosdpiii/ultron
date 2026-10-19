//! `ultron update` and `ultron config` (`package-manager-cli.ts` without the package commands): argument handling,
//! help, `update --models`, the self-update check, and the resource selector ([`crate::config_selector`]).
//!
//! Pi packages are not supported by ultron-rs: `ultron install|remove|uninstall|list` and the package targets of
//! `ultron update` (`--extensions`, `--extension <source>`, `--all`, a package source) say so and exit 1.
//!
//! The self-update: the Rust build is one self-contained binary, as TypeScript's Bun binary is, so it asks the npm
//! registry for the latest release (as TypeScript does) and, when one is newer, says where to download it instead of
//! replacing itself (TypeScript's `bun-binary` install method).

use crate::Env;
use config::settings::SettingsManager;
use serde_json::Value;

/// What the removed package commands answer.
pub const PACKAGES_UNSUPPORTED: &str = "Pi packages are not supported by ultron-rs";

const UPDATE_USAGE: &str = "ultron update [self] [--self|--models] [--force]";

const UPDATE_HELP: &str = "Usage:
  ultron update [self] [--self|--models] [--force]

Update ultron or model catalogs.
ultron updates itself from the npm package ultron-agent.

Options:
  --self                  Update ultron only (the default)
  --models                Refresh model catalogs only
  --force                 Reinstall ultron even if the current version is latest

Short forms:
  ultron update                Update ultron only
  ultron update --models       Refresh model catalogs only
  ultron update self           Update ultron only (ultron and pi work as aliases)
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateTarget {
    SelfOnly,
    Models,
}

#[derive(Debug)]
enum Parsed {
    Help,
    /// `install`, `remove`, `uninstall`, `list`, or a package target of `update`.
    Unsupported,
    InvalidOption(String),
    UnexpectedArgument(String),
    Conflict(&'static str),
    Update {
        target: UpdateTarget,
        force: bool,
    },
}

/// `parsePackageCommand` for `update` (and the removed package commands); None for anything else.
fn parse(args: &[String]) -> Option<Parsed> {
    match args.first().map(String::as_str) {
        Some("install" | "remove" | "uninstall" | "list") => return Some(Parsed::Unsupported),
        Some("update") => {}
        _ => return None,
    }
    let (mut self_flag, mut models_flag, mut force) = (false, false, false);
    let mut source: Option<&str> = None;
    for arg in &args[1..] {
        match arg.as_str() {
            "-h" | "--help" => return Some(Parsed::Help),
            "--self" => self_flag = true,
            "--models" => models_flag = true,
            "--force" => force = true,
            "--extensions" | "--extension" | "--all" => return Some(Parsed::Unsupported),
            // Project trust only mattered for package storage.
            "--approve" | "-a" | "--no-approve" | "-na" => {}
            a if a.starts_with('-') => return Some(Parsed::InvalidOption(a.to_string())),
            a if source.is_none() => source = Some(a),
            a => return Some(Parsed::UnexpectedArgument(a.to_string())),
        }
    }
    match source {
        Some("self" | "pi" | "ultron") | None => {}
        Some(_) => return Some(Parsed::Unsupported),
    }
    if models_flag && (self_flag || source.is_some()) {
        return Some(Parsed::Conflict(
            "--models cannot be combined with --self or a positional target",
        ));
    }
    let target = if models_flag {
        UpdateTarget::Models
    } else {
        UpdateTarget::SelfOnly
    };
    Some(Parsed::Update { target, force })
}

struct Io<'a> {
    out: &'a mut dyn std::io::Write,
    err: &'a mut dyn std::io::Write,
}

/// Where the self-update asks for the latest release (`LATEST_VERSION_URL`).
pub const LATEST_VERSION_URL: &str = "https://registry.npmjs.org/ultron-agent/latest";
/// `RELEASES_URL`.
pub const RELEASES_URL: &str = "https://github.com/alfredosdpiii/ultron/releases";

/// What `ultron update` reaches outside the machine (tests point it at a local server).
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

/// Handle `ultron update` (and the removed package commands); None when `argv` is neither.
pub fn run_update(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Option<i32> {
    run_update_with(argv, env, out, err, &Endpoints::default())
}

/// [`run_update`] with explicit endpoints.
pub fn run_update_with(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    endpoints: &Endpoints,
) -> Option<i32> {
    let parsed = parse(argv)?;
    let mut io = Io { out, err };
    Some(update_command(parsed, env, &mut io, endpoints))
}

fn usage_error(io: &mut Io, message: &str) -> i32 {
    let _ = writeln!(io.err, "{message}");
    let _ = writeln!(io.err, "Usage: {UPDATE_USAGE}");
    1
}

/// Settings with the project trust resolved without asking anyone: the override, else nothing to trust, the saved
/// decision or `defaultProjectTrust`; undecided is untrusted.
pub(crate) fn settings_manager(env: &Env, trust_override: Option<bool>) -> SettingsManager {
    let agent_dir = env.agent_dir();
    let mut settings = SettingsManager::create(&env.cwd, &agent_dir, false);
    let store = config::trust::ProjectTrustStore::new(&agent_dir);
    let trusted = match config::trust::resolve_project_trusted(
        &env.cwd,
        &env.home,
        &store,
        trust_override,
        settings.default_project_trust(),
    ) {
        Ok(config::trust::TrustResolution::Trusted(t)) => t,
        _ => false,
    };
    settings.set_project_trusted(trusted);
    settings
}

fn update_command(parsed: Parsed, env: &Env, io: &mut Io, endpoints: &Endpoints) -> i32 {
    let (target, force) = match parsed {
        Parsed::Help => {
            let _ = writeln!(io.out, "{UPDATE_HELP}");
            return 0;
        }
        Parsed::Unsupported => {
            let _ = writeln!(io.err, "{PACKAGES_UNSUPPORTED}");
            return 1;
        }
        Parsed::InvalidOption(option) => {
            let _ = writeln!(io.err, "Unknown option {option} for \"update\".");
            let _ = writeln!(io.err, "Use \"ultron --help\" or \"{UPDATE_USAGE}\".");
            return 1;
        }
        Parsed::UnexpectedArgument(argument) => return usage_error(io, &format!("Unexpected argument {argument}.")),
        Parsed::Conflict(conflict) => return usage_error(io, conflict),
        Parsed::Update { target, force } => (target, force),
    };
    if target == UpdateTarget::Models {
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
    match self_update(env, force, io, endpoints) {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(io.err, "Error: {message}");
            1
        }
    }
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

/// `getLatestRelease(VERSION, { retry: true })`: up to `retries` (2 with `retry`, else 0) retries after a failure or a
/// transient status, within ten seconds. `Ok(None)`: offline, an error status, or no valid version in the document.
pub(crate) fn latest_release(env: &Env, url: &str, retries: u32) -> Result<Option<String>, String> {
    if (env.var)("PI_OFFLINE").is_some_and(|v| !v.is_empty()) {
        return Ok(None);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let user_agent = ai::model_refresh::management_user_agent(&crate::version());
    runtime.block_on(async {
        // The run's proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`, the `httpProxy` setting), as Pi's HTTP stack.
        let client = ai::net::ClientConfig::from_lookup(&*env.var).client()?;
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
                    Ok(response) if RETRYABLE.contains(&response.status().as_u16()) && attempt < retries => {}
                    Ok(response) => {
                        if !response.status().is_success() {
                            return Ok(None);
                        }
                        let text = response.text().await.unwrap_or_default();
                        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        let version = data.get("version").and_then(Value::as_str).map(str::trim);
                        return Ok(version
                            .filter(|v| crate::semver::valid(v).is_some())
                            .map(str::to_string));
                    }
                    Err(error) if attempt >= retries => return Err(fetch_error(&error)),
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
pub(crate) fn is_newer(candidate: &str, current: &str) -> bool {
    match crate::semver::compare(candidate.trim(), current.trim()) {
        Some(order) => order == std::cmp::Ordering::Greater,
        None => candidate.trim() != current.trim(),
    }
}

/// `ultron update --self` for the self-contained binary.
fn self_update(env: &Env, force: bool, io: &mut Io, endpoints: &Endpoints) -> Result<i32, String> {
    let env_version = crate::version();
    let latest = match latest_release(env, &endpoints.latest_version_url, 2) {
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
    let settings = settings_manager(env, trust_override);
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
