//! `ultron durable [attach] [options] [prompt]` (user requirement; no TypeScript equivalent): a client of the durable
//! instance. The TUI (no prompt), `-p` and `--mode json|rpc` run as for any instance, except that they use the
//! durable instance's detached server (activated, held open, when it is not running) instead of a server of their
//! own, and leave it running when they exit: its kept children and scheduled prompts go on. Project and explicit
//! instances keep their own foreground servers as before.
//!
//! The client runs with the durable instance as its explicit server directory and a marker variable in its own
//! (injected) environment; the server it activates is the one `ultron durable start` starts.

use crate::Env;
use daemon::server::{Route, ServerOptions, activate_server};
use daemon::workers::WorkerModel;
use std::io::Write;
use std::sync::Arc;

/// Set (to `1`) in the environment of a client of the durable instance.
pub const CLIENT_ENV: &str = "__ULTRON_DURABLE_CLIENT";

/// Whether this run is a client of the durable instance.
pub fn is_client(env: &Env) -> bool {
    (env.var)(CLIENT_ENV).as_deref() == Some("1")
}

/// The durable server's route for a client with `options`: activated (held open) when it is not running. Its
/// workers start on the default worker options; the client's own session options (`--model`, `--provider`,
/// `--api-key`, `--thinking`, `--models`, `--no-session`, `--approve`/`--no-approve`, `--system-prompt`,
/// `--append-system-prompt`, `--skill`, `--no-skills`, `--no-context-files`) go with its attach (crate::link), so
/// the worker its session starts runs on them as a foreground server's would. The client's run environment
/// (`--offline`, the `httpProxy` setting) belongs to the server process: a server this client activates runs with
/// it.
pub fn activate(options: &ServerOptions) -> Result<Route, String> {
    activate_server(
        &ServerOptions {
            model: WorkerModel {
                env: options.model.env.clone(),
                ..WorkerModel::default()
            },
            keep_alive: true,
            relay: None,
            ..options.clone()
        },
        true,
    )
    .map_err(|e| e.to_string())
}

/// The command herdr runs to resume `session_id` opened through `ultron durable`: `ultron durable --session <id>`
/// (a client of the durable server again, never a foreground server on its directory), behind `env VAR=...` for
/// the instance or agent directory the caller's environment chose.
pub fn durable_resume_argv(env: &Env, session_id: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let assigned: Vec<String> = ["ULTRON_SERVER_DIR", "ULTRON_CODING_AGENT_DIR"]
        .iter()
        .filter_map(|k| (env.var)(k).filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
        .collect();
    if !assigned.is_empty() {
        argv.push("env".to_string());
        argv.extend(assigned);
    }
    argv.extend(["ultron", "durable", "--session", session_id].map(String::from));
    argv
}

/// Run `args` (an ordinary `ultron` command line) as a client of the durable instance.
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    // herdr resumes the pane as a durable client (`ultron-s` set its own override first, which stays).
    let caller = env.clone();
    crate::herdr_report::set_resume_override(move |session_id| Some(durable_resume_argv(&caller, session_id)));
    let instance = crate::instance(env, true).dir;
    // The durable instance's own directory (not an `ULTRON_SERVER_DIR` elsewhere): its sessions start on
    // `durable.model`, as its workers do (session-core `lane::is_durable`).
    let own = daemon::instance::ultron_home(&env.home).join("server");
    let sessions = (config::paths::normalize(&instance) == config::paths::normalize(&own)).then(|| "1".to_string());
    let dir = instance.display().to_string();
    let inner = env.var.clone();
    let client = Env {
        var: Arc::new(move |key| match key {
            CLIENT_ENV => Some("1".into()),
            config::settings::DURABLE_CLIENT_ENV => sessions.clone(),
            daemon::instance::ENV_SERVER_DIR => Some(dir.clone()),
            "PI_SERVER_DIR" => None,
            _ => inner(key),
        }),
        ..env.clone()
    };
    Ok(crate::run(args, &client, out, err))
}

/// The binary's name that runs [`run_home`] (`ultron-s`, a link to `ultron`).
pub const HOME_COMMAND: &str = "ultron-s";
/// The durable instance's home session id, in its directory.
pub const HOME_FILE: &str = "home-session";

const HOME_USAGE: &str = "Usage: ultron-s [--new | --session <id> [--home] | --session-id <id> | -c | -r | --fork <id> | --list] [ultron options] [prompt]";

/// Whether the binary was invoked as `ultron-s` (its file name; a path or a `.exe` suffix does not matter).
pub fn is_home_command(argv0: &str) -> bool {
    let name = std::path::Path::new(argv0)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(".exe").unwrap_or(&name) == HOME_COMMAND
}

/// The durable instance's recorded home session id.
pub fn home_session(instance_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(instance_dir.join(HOME_FILE))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|id| !id.is_empty())
}

fn set_home(instance_dir: &std::path::Path, id: &str) -> Result<(), String> {
    std::fs::create_dir_all(instance_dir).map_err(|e| e.to_string())?;
    let temp = instance_dir.join(format!("{HOME_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&temp, format!("{id}\n")).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, instance_dir.join(HOME_FILE)).map_err(|e| e.to_string())
}

/// `ultron-s [--new | --session <id> [--home] | --session-id <id> | -c | -r | --fork <id> | --list] [options]
/// [prompt]`: the durable instance's home session (one long-lived session) as a client of its server: the TUI, `-p`
/// or `--mode json|rpc`, run in the home directory. The home session is created (and recorded) when there is none or
/// the recorded one was deleted; `--new` makes a fresh one home; `--session <id>` uses another session of the
/// instance (`--home` records it as home); `--session-id <id>` opens that session, created when missing; `-c` the
/// instance's most recently active session; `-r` its session picker; `--fork <id>` a new session forked from one of
/// them; `--list` lists them. Exiting leaves the server and any run in progress going.
pub fn run_home(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match home_inner(args, env, out, err) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "Error: {error}");
            1
        }
    }
}

/// The command herdr runs to resume `session_id` opened through `ultron-s`: `ultron-s` for the home session, else
/// `ultron-s --session <id>`.
pub fn home_resume_argv(home: Option<&str>, session_id: &str) -> Vec<String> {
    let mut argv = vec![HOME_COMMAND.to_string()];
    if home != Some(session_id) {
        argv.extend(["--session".to_string(), session_id.to_string()]);
    }
    argv
}

fn home_inner(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let (mut new, mut list, mut make_home, mut session) = (false, false, false, None::<String>);
    // `-c`, `-r`, `--fork <id>` or `--session-id <id>`: the session another way than the home one.
    let mut selector: Option<Vec<String>> = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--new" => new = true,
            "--list" => list = true,
            "--home" => make_home = true,
            "--session" => {
                i += 1;
                session = Some(args.get(i).cloned().ok_or(HOME_USAGE)?);
            }
            flag @ ("-c" | "--continue" | "-r" | "--resume") => {
                if selector.is_some() {
                    return Err(HOME_USAGE.into());
                }
                selector = Some(vec![flag.to_string()]);
            }
            flag @ ("--fork" | "--session-id") => {
                i += 1;
                let value = args.get(i).cloned().ok_or(HOME_USAGE)?;
                if selector.is_some() {
                    return Err(HOME_USAGE.into());
                }
                selector = Some(vec![flag.to_string(), value]);
            }
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    if (new && session.is_some())
        || (make_home && session.is_none())
        || (list && (new || session.is_some()))
        || (selector.is_some() && (new || list || session.is_some()))
    {
        return Err(HOME_USAGE.into());
    }
    let instance = crate::instance(env, true);
    instance.prepare().map_err(|e| e.to_string())?;
    let session_dir = crate::session::session_directory_of(None, env, &instance);
    let mut sessions = daemon::catalog::list(&session_dir);
    let home = home_session(&instance.dir).filter(|id| sessions.iter().any(|m| &m.id == id));
    if list {
        sessions.sort_by(|a, b| b.modified_at.total_cmp(&a.modified_at));
        writeln!(out, "ID\tNAME\tLAST ACTIVITY\tHOME").map_err(|e| e.to_string())?;
        for meta in &sessions {
            let name = daemon::catalog::details(meta).name.unwrap_or_else(|| "-".into());
            let at = session_file::js::iso_string(meta.modified_at as i64);
            let mark = if home.as_deref() == Some(meta.id.as_str()) {
                "*"
            } else {
                ""
            };
            writeln!(out, "{}\t{name}\t{at}\t{mark}", meta.id).map_err(|e| e.to_string())?;
        }
        return Ok(0);
    }
    // Another selector: the client selects the session among the instance's (the TUI's `-r` picker lists them).
    if let Some(mut selector) = selector {
        if selector[0] == "-c" || selector[0] == "--continue" {
            // The most recently active session (none: a new one), as the client's own `-c` picks it.
            sessions.sort_by(|a, b| b.modified_at.total_cmp(&a.modified_at));
            selector = match sessions.first() {
                Some(latest) => vec!["--session-id".to_string(), latest.id.clone()],
                None => Vec::new(),
            };
        }
        let current_home = home_session(&instance.dir);
        crate::herdr_report::set_resume_override(move |session_id| {
            Some(home_resume_argv(current_home.as_deref(), session_id))
        });
        selector.extend(caller_args(&rest, &env.cwd, &env.home));
        let at_home = Env {
            cwd: env.home.clone(),
            ..env.clone()
        };
        return run(&selector, &at_home, out, err);
    }
    let id = match session {
        Some(id) => {
            if !sessions.iter().any(|m| m.id == id) {
                return Err(format!("Unknown session: {id}"));
            }
            if make_home {
                set_home(&instance.dir, &id)?;
            }
            id
        }
        None => match home.filter(|_| !new) {
            Some(id) => id,
            None => {
                let id = daemon::paths::uuid_v4();
                set_home(&instance.dir, &id)?;
                id
            }
        },
    };
    // herdr resumes the pane through `ultron-s` (the home session) or `ultron-s --session <id>`.
    let current_home = home_session(&instance.dir);
    crate::herdr_report::set_resume_override(move |session_id| {
        Some(home_resume_argv(current_home.as_deref(), session_id))
    });
    let mut client_args = vec!["--session-id".to_string(), id];
    // The client runs in the home directory: what names a file of the caller's directory is resolved there first.
    client_args.extend(caller_args(&rest, &env.cwd, &env.home));
    let at_home = Env {
        cwd: env.home.clone(),
        ..env.clone()
    };
    run(&client_args, &at_home, out, err)
}

/// Flags whose value is the next argument (so an `@...` value is not a file argument).
const VALUE_FLAGS: [&str; 24] = [
    "--provider",
    "--model",
    "--api-key",
    "--system-prompt",
    "--name",
    "-n",
    "--session",
    "--session-id",
    "--fork",
    "--session-dir",
    "--models",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
    "--thinking",
    "--export",
    "--extension",
    "-e",
    "--prompt-template",
    "--theme",
    "--mode",
    "--use-theme",
    "--tui-mode",
];

/// `args` with what names a file of the caller's directory `cwd` made absolute, for a run in another directory:
/// `@file` arguments, `--skill <path>`, and an `--append-system-prompt` value that is a file there (else its text,
/// unchanged).
pub fn caller_args(args: &[String], cwd: &std::path::Path, home: &std::path::Path) -> Vec<String> {
    let file = |arg: &str| format!("@{}", crate::file_args::resolve_read_path(arg, cwd, home).display());
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match (arg, args.get(i + 1)) {
            ("--", _) => {
                out.push(arg.to_string());
                out.extend(args[i + 1..].iter().map(|a| match a.strip_prefix('@') {
                    Some(path) => file(path),
                    None => a.clone(),
                }));
                break;
            }
            ("--append-system-prompt", Some(value)) => {
                let path = cwd.join(value);
                out.push(arg.to_string());
                out.push(if !value.is_empty() && path.exists() {
                    path.display().to_string()
                } else {
                    value.clone()
                });
                i += 1;
            }
            ("--skill", Some(value)) => {
                let path = crate::file_args::resolve_read_path(value, cwd, home);
                out.extend([arg.to_string(), path.display().to_string()]);
                i += 1;
            }
            (flag, Some(value)) if VALUE_FLAGS.contains(&flag) => {
                out.extend([arg.to_string(), value.clone()]);
                i += 1;
            }
            _ => out.push(match arg.strip_prefix('@') {
                Some(path) => file(path),
                None => arg.to_string(),
            }),
        }
        i += 1;
    }
    out
}
