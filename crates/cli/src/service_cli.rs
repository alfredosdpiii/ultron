//! `ultron service install|uninstall|status|restart` (Rust-only): the durable instance's server (`ultron durable`) as
//! a supervised systemd user service. The unit runs `ultron service run`, the durable instance's server in the
//! foreground (its directory, server ID and session store as `ultron durable` resolves them), in the home directory,
//! restarted whenever it exits, started at login (`WantedBy=default.target`). `ultron durable` clients (the TUI, `-p`,
//! `--mode json|rpc`) attach to it, so its kept children and scheduled prompts keep running after they exit.
//! Project instances are not affected: their clients keep their own servers.
//!
//! The unit goes under the injected environment's `$XDG_CONFIG_HOME/systemd/user` (else
//! `<home>/.config/systemd/user`), named after the instance directory. `systemctl` is taken only from the injected
//! environment's `PATH`; without it there is no systemd to install into and the command says so and fails. The unit
//! keeps location variables only, never keys: credentials come from the agent directory's stored auth or from the
//! optional `<agent dir>/service.env` the user writes.

use crate::Env;
use daemon::server::{ServerOptions, serve};
use daemon::workers::WorkerModel;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Variables the unit keeps from the installing environment (locations only; never keys).
const KEPT_VARS: [&str; 6] = [
    "ULTRON_SERVER_DIR",
    "PI_SERVER_DIR",
    "ULTRON_SERVER_ID",
    "PI_SERVER_ID",
    "ULTRON_CODING_AGENT_DIR",
    "ULTRON_CODING_AGENT_SESSION_DIR",
];
/// Variables `systemctl --user` needs to reach the user manager, passed from the injected environment.
const SYSTEMCTL_VARS: [&str; 4] = ["PATH", "HOME", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];

pub const USAGE: &str = "Usage: ultron service install|uninstall|status|restart (see --help)";

pub const HELP: &str = "\
Usage: ultron service <command>

Run the durable instance's server (`ultron durable`) as a systemd user service in your home directory: started at
login, restarted when it exits. `ultron durable` (the TUI), `ultron durable -p ...` and `ultron durable --mode
json|rpc` attach to it, so kept children and scheduled prompts keep running after the client exits. Project
instances (plain `ultron` in a project) keep their own servers.

Commands:
  install    Write the unit under $XDG_CONFIG_HOME/systemd/user (else ~/.config/systemd/user), enable and start it;
             prints what it wrote. Needs systemctl on PATH.
  uninstall  Stop, disable and remove it.
  status     Show the unit's state and the instance's running servers (id, pid, sessions, active schedules).
  restart    Restart it (after installing a new ultron binary).

The unit keeps no keys: the server uses the agent directory's stored credentials, and the optional file
<agent dir>/service.env (KEY=value lines) for environment variables such as provider keys.
";

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// The user unit directory: `$XDG_CONFIG_HOME/systemd/user` when it is absolute, else `<home>/.config/systemd/user`.
pub fn unit_dir(env: &Env) -> PathBuf {
    let config = (env.var)("XDG_CONFIG_HOME")
        .filter(|dir| Path::new(dir).is_absolute())
        .map_or_else(|| env.home.join(".config"), PathBuf::from);
    config.join("systemd").join("user")
}

/// `ultron-durable-<12 hex of the instance directory>.service`.
pub fn unit_name(instance_dir: &Path) -> String {
    let digest = Sha256::digest(instance_dir.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    format!("ultron-durable-{hex}.service")
}

/// The first executable `systemctl` on the injected `PATH`.
pub fn systemctl(env: &Env) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = (env.var)("PATH").unwrap_or_default();
    path.split(':').filter(|dir| !dir.is_empty()).find_map(|dir| {
        let candidate = Path::new(dir).join("systemctl");
        let meta = std::fs::metadata(&candidate).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}

/// A word of a unit file's command line: double-quoted, with `\`, `"`, `%` and `$` escaped.
fn quote(word: &str) -> String {
    let escaped = word
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The service: `ultron service run` in `cwd` (the home directory), restarted whenever it exits.
pub fn unit_text(exe: &Path, cwd: &Path, instance_dir: &Path, agent_dir: &Path, vars: &[(String, String)]) -> String {
    let mut text = format!(
        "[Unit]\nDescription=Ultron durable server ({})\n\n[Service]\nType=simple\nWorkingDirectory={}\n",
        instance_dir.display(),
        cwd.display().to_string().replace('%', "%%")
    );
    for (key, value) in vars {
        text.push_str(&format!("Environment={}\n", quote(&format!("{key}={value}"))));
    }
    text.push_str(&format!(
        "EnvironmentFile=-{}\nExecStart={} service run\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
        agent_dir.join("service.env").display().to_string().replace('%', "%%"),
        quote(&exe.display().to_string())
    ));
    text
}

/// Run `systemctl --user args` with only the injected environment's variables it needs: `Ok(stdout)`, or the
/// failure with its stderr.
fn run_systemctl(binary: &Path, env: &Env, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(binary);
    command
        .arg("--user")
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in SYSTEMCTL_VARS {
        if let Some(value) = (env.var)(key) {
            command.env(key, value);
        }
    }
    let output = command
        .output()
        .map_err(|e| format!("systemctl --user {}: {e}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(format!(
        "systemctl --user {} failed ({}): {}",
        args.join(" "),
        output.status,
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

/// The options of the durable instance's server as `ultron durable start` resolves them.
pub fn durable_options(env: &Env) -> Result<ServerOptions, String> {
    let instance = crate::instance(env, true);
    instance.prepare().map_err(io)?;
    Ok(ServerOptions {
        directory: instance.dir.clone(),
        session_dir: crate::session::session_directory_of(None, env, &instance),
        server_id: (env.var)("ULTRON_SERVER_ID"),
        // `--offline` and the `httpProxy` setting (already in `env`) hold for its workers.
        model: WorkerModel {
            env: crate::startup::worker_env(env),
            ..WorkerModel::default()
        },
        keep_alive: true,
        root: None,
        relay: Some(crate::radius_relay::relay_config(env)),
    })
}

/// `ultron service ...`; `None` when `args` is not a service command (a prompt that starts with "service").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    let action = match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            return Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io));
        }
        // The unit's command: the durable instance's server in this process until a signal.
        [run] if run == "run" => return Some(durable_options(env).and_then(|o| serve(&o)).map(|()| 0)),
        [action] if matches!(action.as_str(), "install" | "uninstall" | "status" | "restart") => action.as_str(),
        [verb, ..] if matches!(verb.as_str(), "install" | "uninstall" | "status" | "restart" | "run") => {
            return Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io));
        }
        _ => return None,
    };
    Some(manage(action, env, out, err))
}

fn manage(action: &str, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let Some(binary) = systemctl(env) else {
        writeln!(
            err,
            "No systemd found: systemctl is not on PATH. `ultron durable` starts the durable server on demand instead."
        )
        .map_err(io)?;
        return Ok(1);
    };
    let instance = crate::instance(env, true);
    let name = unit_name(&instance.dir);
    let unit = unit_dir(env).join(&name);
    match action {
        "install" => {
            let exe = std::env::current_exe().map_err(io)?;
            let vars: Vec<(String, String)> = KEPT_VARS
                .iter()
                .filter_map(|key| (env.var)(key).filter(|v| !v.is_empty()).map(|v| (key.to_string(), v)))
                .collect();
            let body = unit_text(&exe, &env.home, &instance.dir, &env.agent_dir(), &vars);
            std::fs::create_dir_all(unit_dir(env)).map_err(io)?;
            std::fs::write(&unit, &body).map_err(io)?;
            let enabled = run_systemctl(&binary, env, &["daemon-reload"])
                .and_then(|_| run_systemctl(&binary, env, &["enable", "--now", &name]));
            if let Err(error) = enabled {
                let _ = std::fs::remove_file(&unit);
                writeln!(err, "{error}").map_err(io)?;
                writeln!(err, "The unit was not installed.").map_err(io)?;
                return Ok(1);
            }
            writeln!(out, "Wrote {}:\n{body}", unit.display()).map_err(io)?;
            writeln!(out, "Enabled and started {name}: it serves {}.", instance.dir.display()).map_err(io)?;
            Ok(0)
        }
        "uninstall" => {
            if !unit.exists() {
                writeln!(out, "Not installed ({name}).").map_err(io)?;
                return Ok(0);
            }
            if let Err(error) = run_systemctl(&binary, env, &["disable", "--now", &name]) {
                writeln!(err, "Warning: {error}").map_err(io)?;
            }
            std::fs::remove_file(&unit).map_err(io)?;
            writeln!(out, "Removed {}", unit.display()).map_err(io)?;
            if let Err(error) = run_systemctl(&binary, env, &["daemon-reload"]) {
                writeln!(err, "Warning: {error}").map_err(io)?;
            }
            Ok(0)
        }
        "restart" => {
            if !unit.exists() {
                writeln!(err, "Not installed ({name}).").map_err(io)?;
                return Ok(1);
            }
            match run_systemctl(&binary, env, &["restart", &name]) {
                Ok(_) => {
                    writeln!(out, "Restarted {name}.").map_err(io)?;
                    Ok(0)
                }
                Err(error) => {
                    writeln!(err, "{error}").map_err(io)?;
                    Ok(1)
                }
            }
        }
        _ => {
            let installed = unit.exists();
            let state = if installed { "installed" } else { "missing" };
            writeln!(out, "Unit: {} ({state})", unit.display()).map_err(io)?;
            // `is-enabled` / `is-active` exit non-zero for a disabled or inactive unit; their output is the state.
            let query = |verb: &str| {
                run_systemctl(&binary, env, &[verb, &name])
                    .unwrap_or_else(|error| error.rsplit_once(": ").map_or(error.clone(), |(_, s)| s.to_string()))
            };
            writeln!(out, "Enabled: {}", query("is-enabled")).map_err(io)?;
            writeln!(out, "Active: {}", query("is-active")).map_err(io)?;
            servers(env, &instance, out)?;
            Ok(if installed { 0 } else { 1 })
        }
    }
}

/// The durable instance's reachable server generations (`Server: <id> pid <pid> sessions <n>`) and its active
/// schedules.
fn servers(env: &Env, instance: &daemon::instance::Instance, out: &mut dyn Write) -> Result<(), String> {
    let socket_dir = daemon::instance::socket_dir(&instance.dir);
    let mut any = false;
    for server in daemon::instance::generations(&instance.dir) {
        let socket = socket_dir.join(format!("{}.sock", server.server_id));
        match client::runtime::Connection::connect(&socket, &server.server_id) {
            Ok(connection) => connection.disconnect(),
            Err(_) => continue,
        }
        any = true;
        writeln!(
            out,
            "Server: {} pid {} sessions {}",
            server.server_id, server.pid, server.sessions
        )
        .map_err(io)?;
    }
    if !any {
        writeln!(out, "Server: none reachable").map_err(io)?;
    }
    let session_dir = crate::session::session_directory_of(None, env, instance);
    let active = daemon::prompt_schedules::scan_sessions(&session_dir)
        .iter()
        .flat_map(|(_, schedules)| schedules.iter().filter(|s| s.active()))
        .count();
    writeln!(out, "Schedules: {active} active").map_err(io)?;
    Ok(())
}
