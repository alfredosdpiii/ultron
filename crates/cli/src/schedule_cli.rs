//! `ultron schedule list` and `ultron schedule cancel <id|name>` (Rust-only): the scheduled prompts of the instance's
//! sessions (`schedule.every` / `schedule.at` in a cell, kernel_apis::prompt_schedule).
//!
//! `list` reads the session journals, where every change is committed before it is reported (the server keeps no
//! other record). `cancel` goes through a running server of the instance (the session's worker records it, so a
//! session open in a worker is never written twice); with no server running it writes the session's journal under
//! the session's own locks.

use crate::Env;
use daemon::instance::{self, Kind};
use daemon::prompt_schedules::{cancel_in_journal, scan_sessions};
use kernel_apis::prompt_schedule::{Schedule, clock_for};
use protocol::RpcTarget;
use serde_json::json;
use std::io::Write;

const USAGE: &str = "Usage: ultron schedule list | ultron schedule cancel <id|name>";

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// `ultron schedule ...`; `None` when `args` is not a schedule command (a prompt that starts with "schedule").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    match args {
        [list] if list == "list" => Some(list_schedules(env, out)),
        [cancel, target] if cancel == "cancel" => Some(cancel_schedule(env, target, out)),
        [verb, ..] if verb == "list" || verb == "cancel" => Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io)),
        _ => None,
    }
}

fn session_dir(env: &Env) -> (instance::Instance, std::path::PathBuf) {
    let instance = crate::instance(env, false);
    let dir = crate::session::session_directory_of(None, env, &instance);
    (instance, dir)
}

/// One line per schedule: `id<TAB>name<TAB>next<TAB>runs<TAB>repeat<TAB>state<TAB>session<TAB>cwd`, active first.
fn list_schedules(env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    let (_, dir) = session_dir(env);
    let mut rows: Vec<(bool, i64, String)> = Vec::new();
    for (meta, schedules) in scan_sessions(&dir) {
        for s in schedules {
            rows.push((
                !s.active(),
                s.next_due.unwrap_or(i64::MAX),
                row(&s, &meta.id, &meta.cwd),
            ));
        }
    }
    if rows.is_empty() {
        writeln!(out, "No schedules.").map_err(io)?;
        return Ok(0);
    }
    rows.sort();
    writeln!(out, "ID\tNAME\tNEXT\tRUNS\tREPEAT\tSTATE\tSESSION\tCWD").map_err(io)?;
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
    let explicit = (instance.kind == Kind::Explicit).then(|| instance.dir.clone());
    let servers: Vec<instance::Running> = instance::running(&env.home, explicit.as_deref())
        .into_iter()
        .filter(|r| r.dir == instance.dir)
        .collect();
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
