//! `ultron durable [attach] [options] [prompt]` (user requirement; no TypeScript equivalent): a client of the durable
//! instance. The TUI (no prompt), `-p` and `--mode json|rpc` run as for any instance, except that they use the
//! durable instance's detached server (activated, held open, when it is not running) instead of a server of their
//! own, and leave it running when they exit: its kept children and scheduled prompts go on. Project and explicit
//! instances keep their own foreground servers as before.
//!
//! The client runs with the durable instance as its explicit server directory and a marker variable in its own
//! (injected) environment; the server it activates is the one `ultron durable start` starts.

use crate::Env;
use daemon::server::{Route, ServerOptions, activate_server, attach_compatible};
use daemon::workers::WorkerModel;
use std::io::Write;
use std::sync::Arc;

/// Set (to `1`) in the environment of a client of the durable instance.
pub const CLIENT_ENV: &str = "__ULTRON_DURABLE_CLIENT";

/// Whether this run is a client of the durable instance.
pub fn is_client(env: &Env) -> bool {
    (env.var)(CLIENT_ENV).as_deref() == Some("1")
}

/// The durable server's route for a client with `options`: activated (held open) when it is not running. Its workers
/// start on the default worker options, so a client that needs its own cannot use it.
pub fn activate(options: &ServerOptions) -> Result<Route, String> {
    if !attach_compatible(options) {
        return Err(
            "`ultron durable` runs on the durable server's worker options: --model, --provider, --api-key, \
                    --thinking, --models, --system-prompt, --append-system-prompt, --no-skills, --no-context-files \
                    and --no-session are not supported (set them in settings, or run without `durable`)"
                .into(),
        );
    }
    activate_server(
        &ServerOptions {
            model: WorkerModel::default(),
            keep_alive: true,
            relay: None,
            ..options.clone()
        },
        true,
    )
    .map_err(|e| e.to_string())
}

/// Run `args` (an ordinary `ultron` command line) as a client of the durable instance.
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let dir = crate::instance(env, true).dir.display().to_string();
    let inner = env.var.clone();
    let client = Env {
        var: Arc::new(move |key| match key {
            CLIENT_ENV => Some("1".into()),
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

const HOME_USAGE: &str = "Usage: ultron-s [--new | --session <id> [--home] | --list] [ultron options] [prompt]";

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

/// `ultron-s [--new | --session <id> [--home] | --list] [options] [prompt]`: the durable instance's home session (one
/// long-lived session) as a client of its server: the TUI, `-p` or `--mode json|rpc`, run in the home directory. The
/// home session is created (and recorded) when there is none or the recorded one was deleted; `--new` makes a fresh
/// one home; `--session <id>` uses another session of the instance (`--home` records it as home); `--list` lists
/// them. Exiting leaves the server and any run in progress going.
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
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    if (new && session.is_some()) || (make_home && session.is_none()) || (list && (new || session.is_some())) {
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
    client_args.extend(rest);
    let at_home = Env {
        cwd: env.home.clone(),
        ..env.clone()
    };
    run(&client_args, &at_home, out, err)
}
