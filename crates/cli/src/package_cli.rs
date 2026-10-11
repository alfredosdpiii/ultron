//! `ultron install|remove|uninstall|update|list` and `ultron config` (`package-manager-cli.ts`, parts of
//! `core/package-manager.ts`): argument handling, help, `list`, and installing or removing local-path packages
//! (which only edits settings).
//!
//! Not ported: npm and git package installs, removals and updates (they run npm and git against the network), the
//! self-update, `update --models`, and the `config` resource selector (a TUI). Those report that they are not
//! implemented after their arguments are checked. A package recorded in settings by npm is listed with its managed
//! install path only (the legacy global npm root, found by running `npm root -g`, is not consulted).

use crate::Env;
use config::settings::SettingsManager;
use serde_json::Value;
use std::path::{Path, PathBuf};

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
            "--force" if update => {}
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

#[derive(Clone, Debug, PartialEq)]
enum Source {
    Npm { name: String },
    Git { host: String, path: String },
    Local { path: String },
}

fn is_local_path(value: &str) -> bool {
    let t = crate::js::trim(value);
    !["npm:", "git:", "github:", "http:", "https:", "ssh:"]
        .iter()
        .any(|p| t.starts_with(p))
}

/// `parseNpmSpec`'s name: the spec up to its version separator (a scoped name keeps its leading `@`).
fn npm_name(spec: &str) -> String {
    let start = usize::from(spec.starts_with('@'));
    match spec[start..].find('@') {
        Some(at) => spec[..start + at].to_string(),
        None => spec.to_string(),
    }
}

/// A subset of `parseGitUrl`: `git:` prefixes, `git@host:path`, `scheme://host/path` and `host/path`, with an
/// `@ref` suffix dropped. Hosted-git-info shorthands are not recognized.
fn parse_git(source: &str) -> Option<Source> {
    let trimmed = source.trim();
    let has_prefix = trimmed.starts_with("git:");
    let url = if has_prefix { trimmed[4..].trim() } else { trimmed };
    let lower = url.to_ascii_lowercase();
    let schemes = ["https://", "http://", "ssh://", "git://"];
    if !has_prefix && !schemes.iter().any(|s| lower.starts_with(s)) {
        return None;
    }
    let (host, path) = if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string())
    } else if let Some(scheme) = schemes.iter().find(|s| lower.starts_with(*s)) {
        let rest = &url[scheme.len()..];
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        (host.to_ascii_lowercase(), path.to_string())
    } else {
        let (host, path) = url.split_once('/')?;
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        (host.to_string(), path.to_string())
    };
    let path = path.split('@').next().unwrap_or(&path);
    let path = path.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).to_string();
    (!host.is_empty() && path.split('/').count() >= 2).then_some(Source::Git { host, path })
}

fn parse_source(source: &str) -> Source {
    if let Some(spec) = source.strip_prefix("npm:") {
        return Source::Npm {
            name: npm_name(spec.trim()),
        };
    }
    if is_local_path(source) {
        return Source::Local {
            path: source.to_string(),
        };
    }
    parse_git(source).unwrap_or_else(|| Source::Local {
        path: source.to_string(),
    })
}

struct Manager<'a> {
    env: &'a Env,
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings: SettingsManager,
}

impl Manager<'_> {
    fn resolve_from(&self, input: &str, base: &Path) -> PathBuf {
        config::paths::resolve(crate::js::trim(input), base, &self.env.home)
    }

    fn base_dir(&self, project: bool) -> PathBuf {
        if project {
            self.cwd.join(".pi")
        } else {
            self.agent_dir.clone()
        }
    }

    fn packages(&self, project: bool) -> Vec<Value> {
        let settings = if project {
            self.settings.project_settings()
        } else {
            self.settings.global_settings()
        };
        settings
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn installed_path(&self, source: &str, project: bool) -> Option<PathBuf> {
        let path = match parse_source(source) {
            Source::Npm { name } => self.base_dir(project).join("npm").join("node_modules").join(name),
            Source::Git { host, path } => self.base_dir(project).join("git").join(host).join(path),
            Source::Local { path } => self.resolve_from(&path, &self.base_dir(project)),
        };
        path.exists().then_some(path)
    }

    fn match_key_for_settings(&self, source: &str, project: bool) -> String {
        match parse_source(source) {
            Source::Npm { name } => format!("npm:{name}"),
            Source::Git { host, path } => format!("git:{host}/{path}"),
            Source::Local { path } => format!("local:{}", self.resolve_from(&path, &self.base_dir(project)).display()),
        }
    }

    fn match_key_for_input(&self, source: &str) -> String {
        match parse_source(source) {
            Source::Local { path } => format!("local:{}", self.resolve_from(&path, &self.cwd).display()),
            _ => self.match_key_for_settings(source, false),
        }
    }

    fn set_packages(&mut self, project: bool, packages: Vec<Value>) -> Result<(), String> {
        if project {
            self.settings.set_project("packages", Some(Value::Array(packages)))
        } else {
            self.settings.set_global("packages", Some(Value::Array(packages)));
            Ok(())
        }
    }
}

fn source_string(pkg: &Value) -> String {
    match pkg {
        Value::String(s) => s.clone(),
        other => other
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// `path.relative(from, to)` for absolute, normalized paths.
fn relative(from: &Path, to: &Path) -> String {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), a.len() - common).collect();
    parts.extend(b[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

struct Io<'a> {
    out: &'a mut dyn std::io::Write,
    err: &'a mut dyn std::io::Write,
}

/// Handle a package command; None when `argv` is not one.
pub fn run_package(
    argv: &[String],
    env: &Env,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Option<i32> {
    let options = parse(argv)?;
    let mut io = Io { out, err };
    Some(package_command(&options, env, &mut io))
}

fn usage_error(io: &mut Io, message: &str, usage: &str) -> i32 {
    let _ = writeln!(io.err, "{message}");
    let _ = writeln!(io.err, "Usage: {usage}");
    1
}

fn settings_manager(env: &Env, trust_override: Option<bool>, saved_only: bool) -> SettingsManager {
    let agent_dir = env.agent_dir();
    let mut settings = SettingsManager::create(&env.cwd, &agent_dir, false);
    let store = config::trust::ProjectTrustStore::new(&agent_dir);
    let trusted = if saved_only {
        trust_override.unwrap_or(store.get(&env.cwd).ok().flatten() == Some(true))
    } else {
        // ponytail: project_trust extension hooks are not run; without a UI an undecided project is untrusted.
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

fn package_command(o: &Options, env: &Env, io: &mut Io) -> i32 {
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
        let _ = writeln!(
            io.err,
            "Error: `ultron update --models` is not implemented in the Rust build yet"
        );
        return 1;
    }
    let writes_project = matches!(command, Command::Install | Command::Remove) && o.local;
    let settings = settings_manager(env, o.trust_override, command == Command::Update);
    if !settings.is_project_trusted() && writes_project {
        let _ = writeln!(
            io.err,
            "Project is not trusted. Use --approve to modify local package config."
        );
        return 1;
    }
    let mut manager = Manager {
        env,
        cwd: config::paths::normalize(&env.cwd),
        agent_dir: config::paths::resolve(&env.agent_dir().to_string_lossy(), &env.cwd, &env.home),
        settings,
    };
    let result = match command {
        Command::List => list(&manager, io),
        Command::Install => install(&mut manager, o, io),
        Command::Remove => remove(&mut manager, o, io),
        Command::Update => update(&manager, o, io),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(io.err, "Error: {message}");
            1
        }
    }
}

fn list(manager: &Manager, io: &mut Io) -> Result<i32, String> {
    let collect = |project: bool| -> Vec<(String, bool, Option<PathBuf>)> {
        manager
            .packages(project)
            .iter()
            .map(|pkg| {
                let source = source_string(pkg);
                let installed = manager.installed_path(&source, project);
                (source, pkg.is_object(), installed)
            })
            .collect()
    };
    let (user, project) = (collect(false), collect(true));
    if user.is_empty() && project.is_empty() {
        let _ = writeln!(io.out, "No packages installed.");
        return Ok(0);
    }
    let show = |pkgs: &[(String, bool, Option<PathBuf>)], io: &mut Io| {
        for (source, filtered, installed) in pkgs {
            let _ = writeln!(
                io.out,
                "  {}",
                if *filtered {
                    format!("{source} (filtered)")
                } else {
                    source.clone()
                }
            );
            if let Some(path) = installed {
                let _ = writeln!(io.out, "    {}", path.display());
            }
        }
    };
    if !user.is_empty() {
        let _ = writeln!(io.out, "User packages:");
        show(&user, io);
    }
    if !project.is_empty() {
        if !user.is_empty() {
            let _ = writeln!(io.out);
        }
        let _ = writeln!(io.out, "Project packages:");
        show(&project, io);
    }
    Ok(0)
}

fn not_ported(source: &str, verb: &str) -> String {
    format!("{verb} npm and git packages ({source}) is not implemented in the Rust build yet")
}

fn install(manager: &mut Manager, o: &Options, io: &mut Io) -> Result<i32, String> {
    let source = o.source.clone().unwrap_or_default();
    let Source::Local { path } = parse_source(&source) else {
        return Err(not_ported(&source, "Installing"));
    };
    let _ = writeln!(io.out, "Installing {source}...");
    let resolved = manager.resolve_from(&path, &manager.cwd.clone());
    if !resolved.exists() {
        return Err(format!("Path does not exist: {}", resolved.display()));
    }
    let project = o.local;
    let packages = manager.packages(project);
    let normalized = {
        let rel = relative(&manager.base_dir(project), &resolved);
        if rel.is_empty() { ".".to_string() } else { rel }
    };
    let input_key = manager.match_key_for_input(&source);
    let position = packages
        .iter()
        .position(|existing| manager.match_key_for_settings(&source_string(existing), project) == input_key);
    let next = match position {
        Some(i) if source_string(&packages[i]) == normalized => None,
        Some(i) => {
            let mut next = packages.clone();
            next[i] = match &packages[i] {
                Value::Object(map) => {
                    let mut map = map.clone();
                    map.insert("source".into(), Value::String(normalized.clone()));
                    Value::Object(map)
                }
                _ => Value::String(normalized.clone()),
            };
            Some(next)
        }
        None => {
            let mut next = packages.clone();
            next.push(Value::String(normalized));
            Some(next)
        }
    };
    if let Some(next) = next {
        manager.set_packages(project, next)?;
    }
    let _ = writeln!(io.out, "Installed {source}");
    Ok(0)
}

fn remove(manager: &mut Manager, o: &Options, io: &mut Io) -> Result<i32, String> {
    let source = o.source.clone().unwrap_or_default();
    if !matches!(parse_source(&source), Source::Local { .. }) {
        return Err(not_ported(&source, "Removing"));
    }
    let _ = writeln!(io.out, "Removing {source}...");
    let project = o.local;
    let packages = manager.packages(project);
    let input_key = manager.match_key_for_input(&source);
    let next: Vec<Value> = packages
        .iter()
        .filter(|existing| manager.match_key_for_settings(&source_string(existing), project) != input_key)
        .cloned()
        .collect();
    if next.len() == packages.len() {
        let _ = writeln!(io.err, "No matching package found for {source}");
        return Ok(1);
    }
    manager.set_packages(project, next)?;
    let _ = writeln!(io.out, "Removed {source}");
    Ok(0)
}

fn update(manager: &Manager, o: &Options, io: &mut Io) -> Result<i32, String> {
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
        let mut configured: Vec<(String, bool)> = Vec::new();
        for project in [false, true] {
            for pkg in manager.packages(project) {
                configured.push((source_string(&pkg), project));
            }
        }
        let matching: Vec<&(String, bool)> = match &source {
            None => configured.iter().collect(),
            Some(s) => {
                let key = manager.match_key_for_input(s);
                configured
                    .iter()
                    .filter(|(c, p)| manager.match_key_for_settings(c, *p) == key)
                    .collect()
            }
        };
        if let Some(s) = &source
            && matching.is_empty()
        {
            return Err(format!("No matching package found for {s}"));
        }
        if let Some((c, _)) = matching
            .iter()
            .find(|(c, _)| !matches!(parse_source(c), Source::Local { .. }))
        {
            return Err(not_ported(c, "Updating"));
        }
        let _ = match &source {
            Some(s) => writeln!(io.out, "Updated {s}"),
            None => writeln!(io.out, "Updated packages"),
        };
    }
    if matches!(target, UpdateTarget::All | UpdateTarget::SelfOnly) {
        // ponytail: the self-update asks the npm registry for the latest release; not ported.
        return Err("`ultron update --self` is not implemented in the Rust build yet".into());
    }
    Ok(0)
}

/// `ultron config`: help and argument checks; the resource selector itself is a TUI and not ported.
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
    let settings = settings_manager(env, trust_override, false);
    if local && !settings.is_project_trusted() {
        let _ = writeln!(
            err,
            "Project is not trusted. Use --approve to modify local resource config."
        );
        return Some(1);
    }
    // ponytail: the resource selector (cli/config-selector.ts) is an interactive TUI; not ported.
    let _ = writeln!(err, "Error: `ultron config` is not implemented in the Rust build yet");
    Some(1)
}
