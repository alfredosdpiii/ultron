//! `ultron instances` and `ultron durable [status|stop]` (user requirement; no TypeScript equivalent): the global
//! durable instance is started on demand and held open; project instances run beside it.

use crate::Env;
use daemon::instance::{self, Kind};
use daemon::server::{ServerOptions, activate_server};
use daemon::workers::WorkerModel;
use std::io::Write;

fn io(e: std::io::Error) -> String {
    e.to_string()
}

fn explicit(env: &Env) -> Option<std::path::PathBuf> {
    let instance = crate::instance(env, true);
    (instance.kind == Kind::Explicit).then_some(instance.dir)
}

/// `ultron instances`: one line per running server generation.
pub fn list(env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    let running = instance::running(&env.home, explicit(env).as_deref());
    out.write_all(instance::render(&running).as_bytes()).map_err(io)?;
    Ok(0)
}

/// `ultron durable`: start (or find) the durable instance's server; `status` lists it; `stop` ends it.
pub fn durable(args: &[String], env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    let instance = crate::instance(env, true);
    let running = || {
        instance::running(&env.home, explicit(env).as_deref())
            .into_iter()
            .filter(|r| r.dir == instance.dir)
            .collect::<Vec<_>>()
    };
    match args.first().map(String::as_str) {
        None | Some("start" | "attach") => {
            instance.prepare().map_err(io)?;
            let session_dir = crate::session::session_directory_of(None, env, &instance);
            let route = activate_server(
                &ServerOptions {
                    directory: instance.dir.clone(),
                    session_dir,
                    server_id: (env.var)("ULTRON_SERVER_ID"),
                    model: WorkerModel::default(),
                    keep_alive: true,
                    root: None,
                },
                true,
            )
            .map_err(io)?;
            writeln!(out, "{}\t{}", route.server_id, route.path.display()).map_err(io)?;
            Ok(0)
        }
        Some("status") => {
            let running = running();
            out.write_all(instance::render(&running).as_bytes()).map_err(io)?;
            Ok(if running.is_empty() { 1 } else { 0 })
        }
        Some("stop") => {
            for server in running() {
                daemon::process::kill(server.pid, rustix_term());
            }
            Ok(0)
        }
        Some(other) => Err(format!("Unknown durable command: {other}")),
    }
}

fn rustix_term() -> daemon::process::Signal {
    daemon::process::Signal::TERM
}
