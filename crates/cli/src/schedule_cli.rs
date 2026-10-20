//! `ultron schedule list|wake|cancel <id|name>` (Rust-only): the scheduled prompts of the instance's
//! sessions (`schedule.every` / `schedule.at` in a cell, kernel_apis::prompt_schedule).
//!
//! Schedules fire while a server of the instance runs; one that holds a pending schedule stays up until its
//! schedules are done or cancelled. `list` (and `wake`) bring the instance's server up when a schedule is active, so
//! its due fires catch up (missed slots collapse into one run), then `list` reads the session journals, where every
//! change is committed before it is reported, plus each slot's last delivery failure the server kept
//! (daemon::prompt_schedules::ERRORS_FILE). `cancel` goes through a running server of the instance (the session's
//! worker records it, so a session open in a worker is never written twice); with no server running it writes the
//! session's journal under the session's own locks. `ultron service install` keeps the durable instance's server
//! running as a systemd user service (crate::service_cli).

use crate::Env;
use daemon::instance::{self, Kind};
use daemon::prompt_schedules::{ERRORS_FILE, cancel_in_journal, read_errors, scan_sessions};
use daemon::server::{ServerOptions, activate_server};
use daemon::workers::WorkerModel;
use kernel_apis::prompt_schedule::{Schedule, clock_for};
use protocol::RpcTarget;
use serde_json::json;
use std::io::Write;

const USAGE: &str = "Usage: ultron schedule list | wake | cancel <id|name> (see --help)";

pub const HELP: &str = "\
Usage: ultron schedule <command>

Scheduled prompts (`schedule.every` / `schedule.at` in a cell) of this instance's sessions.

Commands:
  list                 List the schedules (tab-separated: ID NAME NEXT RUNS REPEAT STATE SESSION CWD ERROR).
                       Starts the instance's server when a schedule is active, so due fires catch up (missed
                       slots run once). ERROR is the last failed delivery of the pending slot, retried every 60s.
  wake                 Start the instance's server when a schedule is active.
  cancel <id|name>     Cancel an active schedule.

Schedules fire only while a server of the instance runs. A server that holds a pending schedule stays up until
its schedules are done or cancelled; an interactive, -p or rpc start, `list` and `wake` bring it up. Schedules of
`ultron durable` sessions keep running across logouts and reboots with `ultron service install` (see `ultron
service --help`).
";

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// `ultron schedule ...`; `None` when `args` is not a schedule command (a prompt that starts with "schedule").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io))
        }
        [list] if list == "list" => Some(list_schedules(env, out, err)),
        [wake] if wake == "wake" => Some(wake_schedules(env, out)),
        [cancel, target] if cancel == "cancel" => Some(cancel_schedule(env, target, out)),
        [verb, ..] if matches!(verb.as_str(), "list" | "wake" | "cancel") => {
            Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io))
        }
        _ => None,
    }
}

fn session_dir(env: &Env) -> (instance::Instance, std::path::PathBuf) {
    let instance = crate::instance(env, false);
    let dir = crate::session::session_directory_of(None, env, &instance);
    (instance, dir)
}

/// The running server generations of the instance.
fn servers(env: &Env, instance: &instance::Instance) -> Vec<instance::Running> {
    let explicit = (instance.kind == Kind::Explicit).then(|| instance.dir.clone());
    instance::running(&env.home, explicit.as_deref())
        .into_iter()
        .filter(|r| r.dir == instance.dir)
        .collect()
}

/// Start the instance's server (an automatically activated one: it stays up while it holds a pending schedule) when
/// a schedule is active and none runs; its timer then fires what is due. `Ok(true)` when one was started.
fn wake(env: &Env) -> Result<bool, String> {
    let (instance, dir) = session_dir(env);
    let active = scan_sessions(&dir)
        .iter()
        .any(|(_, schedules)| schedules.iter().any(Schedule::active));
    if !active || !servers(env, &instance).is_empty() {
        return Ok(false);
    }
    instance.prepare().map_err(io)?;
    activate_server(
        &ServerOptions {
            directory: instance.dir.clone(),
            session_dir: dir,
            server_id: (env.var)("ULTRON_SERVER_ID"),
            model: WorkerModel {
                env: crate::startup::worker_env(env),
                ..Default::default()
            },
            keep_alive: false,
            root: instance.root().map(std::path::Path::to_path_buf),
            relay: None,
        },
        false,
    )
    .map_err(io)?;
    Ok(true)
}

fn wake_schedules(env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    if wake(env)? {
        writeln!(out, "Started the instance's server for its schedules.").map_err(io)?;
    }
    Ok(0)
}

/// One line per schedule: `id<TAB>name<TAB>next<TAB>runs<TAB>repeat<TAB>state<TAB>session<TAB>cwd<TAB>error`, active
/// first.
fn list_schedules(env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    if let Err(error) = wake(env) {
        writeln!(err, "Warning: could not start the instance's server: {error}").map_err(io)?;
    }
    let (instance, dir) = session_dir(env);
    let errors = read_errors(&instance.dir.join(ERRORS_FILE));
    let mut rows: Vec<(bool, i64, String)> = Vec::new();
    for (meta, schedules) in scan_sessions(&dir) {
        for s in schedules {
            // A failure recorded for the slot still pending; one for an earlier slot is stale.
            let error = errors
                .get(&(meta.id.clone(), s.id.clone()))
                .filter(|(_, _, due)| s.active() && s.next_due == Some(*due))
                .map(|(error, _, _)| error.replace(['\t', '\n', '\r'], " "));
            rows.push((
                !s.active(),
                s.next_due.unwrap_or(i64::MAX),
                format!("{}\t{}", row(&s, &meta.id, &meta.cwd), error.as_deref().unwrap_or("-")),
            ));
        }
    }
    if rows.is_empty() {
        writeln!(out, "No schedules.").map_err(io)?;
        return Ok(0);
    }
    rows.sort();
    writeln!(out, "ID\tNAME\tNEXT\tRUNS\tREPEAT\tSTATE\tSESSION\tCWD\tERROR").map_err(io)?;
    for (_, _, line) in rows {
        writeln!(out, "{line}").map_err(io)?;
    }
    Ok(0)
}

fn row(s: &Schedule, session: &str, cwd: &str) -> String {
    let view = s.view();
    let text = |key: &str| view[key].as_str().unwrap_or("-").to_string();
    let runs = match s.max_runs {
        Some(max) => format!("{}/{max}", s.runs),
        None => s.runs.to_string(),
    };
    let repeat = view["every"]
        .as_str()
        .map_or("once".to_string(), |e| format!("every {e}"));
    format!(
        "{}\t{}\t{}\t{runs}\t{repeat}\t{}\t{session}\t{cwd}",
        s.id,
        s.name,
        text("next"),
        text("state")
    )
}

fn cancel_schedule(env: &Env, target: &str, out: &mut dyn Write) -> Result<i32, String> {
    let (instance, dir) = session_dir(env);
    let mut matches: Vec<(daemon::catalog::Meta, Schedule)> = Vec::new();
    for (meta, schedules) in scan_sessions(&dir) {
        for s in schedules.into_iter().filter(Schedule::active) {
            if s.id == target || s.name == target {
                matches.push((meta.clone(), s));
            }
        }
    }
    // An id names one schedule; a name may be in use in several sessions.
    if let Some(i) = matches.iter().position(|(_, s)| s.id == target) {
        matches = vec![matches.swap_remove(i)];
    }
    let (meta, schedule) = match matches.len() {
        0 => return Err(format!("No active schedule {target}")),
        1 => matches.remove(0),
        n => return Err(format!("{n} active schedules are named {target}; cancel one by its id")),
    };
    let servers = servers(env, &instance);
    let cancelled = match servers.first() {
        Some(server) => {
            let socket = instance::socket_dir(&server.dir).join(format!("{}.sock", server.server_id));
            let connection =
                client::runtime::Connection::connect(&socket, &server.server_id).map_err(|e| e.to_string())?;
            connection
                .call(
                    RpcTarget::Server {
                        server_id: server.server_id.clone(),
                    },
                    "ultron.schedules",
                    "cancel",
                    json!([meta.id, schedule.id]),
                )
                .map_err(|e| e.to_string())?;
            schedule
        }
        None => cancel_in_journal(&meta.path, &schedule.id, clock_for(&*env.var))?,
    };
    writeln!(
        out,
        "Cancelled {} ({}) of session {}",
        cancelled.name, cancelled.id, meta.id
    )
    .map_err(io)?;
    Ok(0)
}
