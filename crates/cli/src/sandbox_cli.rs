//! `ultron sandbox doctor`: the session sandbox's mode, mechanism and isolation for this directory and profile
//! (`sandbox.*` settings), and the self-check that tries the escapes it must stop (crates/sandbox `doctor`).
//! Prints the report as JSON; exits 0 when isolation holds (or the sandbox is off), 1 otherwise.

use crate::Env;
use crate::compose::Config;
use sandbox::launcher::{Launcher, Policy};
use std::io::Write;

pub const USAGE: &str = "Usage: ultron sandbox doctor\n\nChecks the shell sandbox a session in this directory gets: which mechanism runs commands (bubblewrap,\nunshare or Docker), what it isolates, and a self-check that tries to write outside the project, read hidden\ncredentials and reach the host's /tmp. Prints a JSON report; exits 1 when isolation does not hold.\n";

/// The launcher a session in `env.cwd` gets (as `Host::sandboxed` builds it, without session directories).
pub fn launcher(env: &Env) -> Launcher {
    let config = Config::from_env(env);
    let settings = sandbox::Settings::from_settings(&config.settings);
    // The session's environment (its PATH finds the sandbox launcher), as a session's host gets it.
    let host_env = config.child_env();
    let policy = Policy::new(&settings, &config.cwd, &config.home, &[], None, &host_env);
    Launcher::new(settings.mode, policy)
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    match argv.first().map(String::as_str) {
        Some("doctor") if argv.len() == 1 => {
            let report = sandbox::doctor::doctor(&launcher(env));
            let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
            writeln!(out, "{text}").map_err(io)?;
            Ok(if report["ok"] == true { 0 } else { 1 })
        }
        Some("--help" | "-h") | None => {
            out.write_all(USAGE.as_bytes()).map_err(io)?;
            Ok(if argv.is_empty() { 1 } else { 0 })
        }
        Some(other) => {
            writeln!(err, "Error: unknown sandbox command: {other}").map_err(io)?;
            err.write_all(USAGE.as_bytes()).map_err(io)?;
            Ok(1)
        }
    }
}
