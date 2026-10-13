//! The `ultron` command line: argument parsing (`cli/args.ts`), help, version, `--list-models` (`cli/list-models.ts`)
//! native session selection (`native-command.ts`) and prompted runs (`-p`, `--mode json`, see [`print_mode`]).

/// The `claude-subscription` provider's inert MCP inventory server (`ultron __internal inert-mcp`).
pub use ai::claude_subscription::inert_mcp;
pub use session_core::{compose, journal, lane, naming};

pub mod args;
pub mod auth;
pub mod export_html;
pub mod instances;
pub mod interactive;
pub mod js;
pub mod link;
pub mod list_models;
pub mod localtime;
pub mod mcp_cli;
pub mod migrate;
pub mod package_cli;
pub mod print_mode;
pub mod rpc;
pub mod sandbox_cli;
pub mod session;
pub mod setup;
pub mod usage;

use args::{Args, DiagnosticKind, ListModels, normalize_session_name, parse_args};
use print_mode::Target;
use session_journal::names::{self, SessionListing};
use session_journal::repo::Metadata;
use session_journal::{SessionRepo, SystemClock};
use std::io::Write;
use std::path::Path;

pub const HELP: &str = include_str!("help.txt");

/// Subcommands the TypeScript entry handles before option parsing.
const COMMANDS: [&str; 15] = [
    "auth",
    "setup",
    "migrate",
    "usage",
    "autoreview",
    "mcp",
    "claude",
    "hook",
    "watch",
    "guide",
    "install",
    "remove",
    "update",
    "list",
    "config",
];

pub fn version() -> String {
    let package: serde_json::Value = serde_json::from_str(include_str!("../../../packages/coding-agent/package.json"))
        .expect("packages/coding-agent/package.json parses");
    package["version"]
        .as_str()
        .expect("package.json has a version")
        .to_string()
}

/// An environment variable lookup usable from any thread.
pub use session_core::VarLookup;

/// The process environment and locations one run sees (the session runtime's own).
pub type Env = session_core::RunEnv;

/// The instance a command uses (crates/daemon instance.rs): `ULTRON_SERVER_DIR`, else `durable`, else the project.
pub fn instance(env: &Env, durable: bool) -> daemon::instance::Instance {
    daemon::instance::resolve(&*env.var, &env.home, &env.cwd, durable)
}

/// Run the CLI; returns the exit code.
pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, env, out, err) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "Error: {e}");
            1
        }
    }
}

fn run_inner(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    match argv.first().map(String::as_str) {
        Some("instances") => return instances::list(env, out),
        Some("durable") => return instances::durable(&argv[1..], env, out),
        _ => {}
    }
    if let Some(code) = auth::run(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = setup::run(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = migrate::run(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = usage::run(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = mcp_cli::run(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = package_cli::run_package(argv, env, out, err) {
        return Ok(code);
    }
    if let Some(code) = package_cli::run_config(argv, env, out, err) {
        return Ok(code);
    }
    // `ultron claude|mcp|hook|watch|guide` (bare `ultron mcp` is Ultron's own MCP server for Claude Code).
    if let Some(code) = claude_root::cli::run(argv, env, out, err) {
        return Ok(code);
    }
    if argv.first().is_some_and(|a| a == "sandbox") {
        return sandbox_cli::run(&argv[1..], env, out, err);
    }
    if let Some(command) = argv.first().filter(|a| COMMANDS.contains(&a.as_str())) {
        return Err(format!("`ultron {command}` is not implemented in the Rust build yet"));
    }
    let mut parsed = parse_args(argv);
    for d in &parsed.diagnostics {
        let label = if d.kind == DiagnosticKind::Error {
            "Error"
        } else {
            "Warning"
        };
        writeln!(err, "{label}: {}", d.message).map_err(io)?;
    }
    if parsed.diagnostics.iter().any(|d| d.kind == DiagnosticKind::Error) {
        return Ok(1);
    }
    if parsed.version {
        writeln!(out, "{}", version()).map_err(io)?;
        return Ok(0);
    }
    if let Some(input) = &parsed.export {
        let output = parsed.messages.first().map(String::as_str);
        return match export_html::export(input, output, env) {
            Ok(path) => {
                writeln!(out, "Exported to: {path}").map_err(io)?;
                Ok(0)
            }
            Err(message) => {
                writeln!(err, "Error: {message}").map_err(io)?;
                Ok(1)
            }
        };
    }
    if parsed.help {
        out.write_all(HELP.as_bytes()).map_err(io)?;
        return Ok(0);
    }
    if let Some(list) = &parsed.list_models {
        let search = match list {
            ListModels::All => None,
            ListModels::Search(p) => Some(p.as_str()),
        };
        let models = list_models::available_models(env);
        out.write_all(list_models::render(&models, search, &env.docs).as_bytes())
            .map_err(io)?;
        return Ok(0);
    }
    // `--claude`: the root lane runs on Claude Code (worker-root.ts `prepareClaudeRoot`).
    let claude_setting = claude_root::settings::ClaudeCodeSettings::load(&env.cwd, &env.agent_dir()).root;
    if parsed.claude || claude_root::worker_root::claude_root_requested(&*env.var) || claude_setting {
        match claude_root::worker_root::prepare_claude_root(
            parsed.provider.as_deref(),
            parsed.model.as_deref(),
            &*env.var,
        ) {
            Ok(selection) => {
                parsed.model = Some(selection.model);
                parsed.provider = selection.provider;
            }
            Err(message) => {
                writeln!(err, "Error: {message}").map_err(io)?;
                return Ok(1);
            }
        }
    }
    let json_mode = parsed.mode.as_deref() == Some("json");
    let rpc_mode = parsed.mode.as_deref() == Some("rpc");
    if rpc_mode && !parsed.file_args.is_empty() {
        writeln!(err, "Error: @file arguments are not supported in RPC mode").map_err(io)?;
        return Ok(1);
    }
    let prompted = rpc_mode || ((parsed.print || json_mode) && !parsed.messages.is_empty());
    let selection = match native_command(&parsed, env, prompted) {
        Ok(selection) => selection,
        Err(message) => {
            writeln!(err, "Error: {message}").map_err(io)?;
            if let Some(hint) = no_model_hint(env) {
                writeln!(err, "{hint}").map_err(io)?;
            }
            return Ok(1);
        }
    };
    let interactive_mode = !prompted && parsed.messages.is_empty() && interactive::is_terminal();
    let target = match selection {
        // On a terminal, `ultron -r` picks one of this directory's sessions and opens it in the TUI.
        Selection::List(sessions) if interactive_mode => {
            let stdin = std::io::stdin();
            match interactive::pick_session(&sessions, &mut std::io::stdout(), &mut stdin.lock()) {
                Some(metadata) => Target::Open(metadata),
                None => Target::New { id: None },
            }
        }
        Selection::List(sessions) => {
            out.write_all(render_session_list(&sessions, &env.cwd.display().to_string()).as_bytes())
                .map_err(io)?;
            return Ok(0);
        }
        Selection::Target(target) => target,
    };
    if rpc_mode {
        return rpc::remote::run(
            &parsed,
            env,
            target,
            Box::new(std::io::stdin()),
            Box::new(std::io::stdout()),
        );
    }
    if prompted {
        // ponytail: piped stdin and @file arguments are not folded into the prompt yet.
        let prompt = parsed.messages.join("\n\n");
        return match print_mode::run_prompt(&parsed, env, prompt, target) {
            Ok(prompted) => {
                out.write_all(print_mode::render(json_mode, &prompted).as_bytes())
                    .map_err(io)?;
                Ok(0)
            }
            Err(message) => {
                writeln!(err, "Error: {message}").map_err(io)?;
                Ok(1)
            }
        };
    }
    if interactive_mode {
        return interactive::run_interactive(&parsed, env, target);
    }
    Err("running a prompt or an interactive session is not implemented in the Rust build yet".into())
}

/// What the session flags select.
enum Selection {
    /// `ultron -r` without a selector or prompt: the sessions of this directory.
    List(Vec<SessionListing>),
    Target(Target),
}

/// The listing `ultron -r` prints: one line per session of the cwd, newest first, as
/// `<id>\t<modified ISO>\t<name>\t<first message (60 chars)>`.
pub fn render_session_list(sessions: &[SessionListing], cwd: &str) -> String {
    if sessions.is_empty() {
        return format!("No sessions in {cwd}\n");
    }
    let mut out = String::new();
    for session in sessions {
        let preview: String = session
            .first_message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(60)
            .collect();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            session.metadata.id,
            session_file::js::iso_string(session.metadata.modified_at as i64),
            session.name.as_deref().unwrap_or(""),
            preview
        ));
    }
    out
}

/// The session metadata of `id` among every session (`resolveSession` of the server).
fn session_by_id(repo: &SessionRepo, id: &str) -> Result<Option<Metadata>, String> {
    let matches: Vec<Metadata> = repo.list(None).into_iter().filter(|m| m.id == id).collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(format!(
            "Session {id} is ambiguous: more than one session file has this id"
        )),
    }
}

/// The checks `runNativeUltronCommand` makes before it starts a server, and its session selection.
fn native_command(parsed: &Args, env: &Env, prompted: bool) -> Result<Selection, String> {
    if parsed.fork.is_some()
        && (parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.session_id.is_some())
    {
        return Err("--fork cannot be combined with another session selector".into());
    }
    if let Some(dir) = &parsed.session_dir
        && !Path::new(dir).is_absolute()
    {
        return Err("--session-dir must be an absolute path for the native ultron command".into());
    }
    if let Some(name) = &parsed.name
        && normalize_session_name(name).is_none()
    {
        return Err("--name requires a non-empty value".into());
    }
    let session_dir = session::session_directory(parsed.session_dir.as_deref(), env);
    let repo = SessionRepo::new(&session_dir, SystemClock::shared());
    let needs_sessions = parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.fork.is_some();
    let sessions = if needs_sessions {
        session::list_native_sessions(&session_dir, &env.cwd)?
    } else {
        Vec::new()
    };
    if parsed.fork.is_some() && parsed.no_session {
        return Err("--fork cannot be combined with --no-session".into());
    }
    let cwd = env.cwd.display().to_string();
    let open_or_new = |id: String| -> Result<Selection, String> {
        Ok(Selection::Target(match session_by_id(&repo, &id)? {
            Some(metadata) => Target::Open(metadata),
            None => Target::New { id: Some(id) },
        }))
    };
    if let Some(fork) = &parsed.fork {
        let id = session::session_id_from_selector(fork, &sessions, &env.cwd)?;
        let metadata = session_by_id(&repo, &id)?.ok_or_else(|| format!("Unknown session: {id}"))?;
        return Ok(Selection::Target(Target::Fork(metadata)));
    }
    if let Some(id) = &parsed.session_id {
        return open_or_new(id.clone());
    }
    if parsed.resume {
        let listed = names::list_sessions(Path::new(repo.root()), &cwd);
        if let Some(selector) = &parsed.resume_selector {
            let found = names::resolve_resume(&listed, selector)?;
            return Ok(Selection::Target(Target::Open(found.metadata.clone())));
        }
        if !prompted {
            return Ok(Selection::List(listed));
        }
        // ponytail: the interactive picker is not ported; a prompted run takes this directory's newest session.
        return Ok(Selection::Target(match listed.into_iter().next() {
            Some(newest) => Target::Open(newest.metadata),
            None => Target::New { id: None },
        }));
    }
    if parsed.continue_ {
        // The newest format-4 session of this directory, else the newest session the server lists (any cwd).
        if let Some(newest) = sessions.first() {
            return open_or_new(newest.id.clone());
        }
        let all = names::list_all_sessions(&repo);
        return Ok(Selection::Target(match all.into_iter().next() {
            Some(newest) => Target::Open(newest.metadata),
            None => Target::New { id: None },
        }));
    }
    if let Some(selector) = &parsed.session {
        return open_or_new(session::session_id_from_selector(selector, &sessions, &env.cwd)?);
    }
    Ok(Selection::Target(if parsed.no_session {
        Target::Ephemeral
    } else {
        Target::New { id: None }
    }))
}

/// `noModelHint` for the "none" state: no saved default model and no model with credentials.
fn no_model_hint(env: &Env) -> Option<String> {
    let agent_dir = env.agent_dir();
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
    let has_default = settings.default_provider().is_some() && settings.default_model().is_some();
    if has_default || !list_models::available_models(env).is_empty() {
        // ponytail: the "default-without-credentials" hint is not ported; it needs the model registry.
        return None;
    }
    Some(
        "No model is configured. Run \"ultron setup\", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY."
            .into(),
    )
}
