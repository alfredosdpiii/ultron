//! `/autoreview [on|off]` (`setAutoreview` of client-tui-pi-commands.ts): automatic pull-request reviews. `on` saves
//! `autoreview.enabled` and the account the TUI's picker chose (`autoreview.accounts`), writes the user service and
//! starts it (systemd or launchd; elsewhere it says to run `ultron autoreview run`). `off` saves the setting (the
//! loop and the service then review nothing), stops the service and removes its file. Alone: the status.

use interactive::lane::{AutoreviewRequest, LaneEvent};
use serde_json::{Value, json};

use crate::Env;
use crate::autoreview_cli::user_service;

const APP_NAME: &str = "ultron";

/// What a command printed and how it exited.
pub struct Ran {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a command; `Err` when it could not be started (`runCommand`'s `error`).
pub type Runner<'a> = &'a dyn Fn(&str, &[&str]) -> Result<Ran, String>;

/// The real runner: the command with no stdin, its output captured.
pub fn run_command(command: &str, args: &[&str]) -> Result<Ran, String> {
    let output = std::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    Ok(Ran {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// One service line (`systemctl --user daemon-reload`): `None` when it succeeded, else why it failed.
fn run_line(run: Runner, line: &str) -> Option<String> {
    let mut parts = line.split_whitespace();
    let command = parts.next()?;
    let args: Vec<&str> = parts.collect();
    match run(command, &args) {
        Err(error) => Some(error),
        Ok(ran) if ran.code == 0 => None,
        Ok(ran) => Some(
            [ran.stderr.trim(), ran.stdout.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .map_or_else(|| format!("exit {}", ran.code), str::to_string),
        ),
    }
}

/// Answers one `/autoreview` request; `platform` is upstream's (`linux`, `darwin`, ...).
pub fn autoreview(request: AutoreviewRequest, env: &Env, platform: &str, run: Runner) -> LaneEvent {
    let status = LaneEvent::Status;
    let service = user_service(env, platform).ok().flatten();
    let mut settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    match request {
        AutoreviewRequest::Status => {
            let saved = settings.autoreview_settings();
            let accounts: Vec<&str> = saved
                .get("accounts")
                .and_then(Value::as_array)
                .map(|accounts| accounts.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let reviewing_as = if accounts.is_empty() {
                "every logged-in gh account".to_string()
            } else {
                accounts.join(", ")
            };
            let installed = match &service {
                Some(file) if std::path::Path::new(&file.path).exists() => format!("installed ({})", file.path),
                _ => "not installed".into(),
            };
            let on = saved.get("enabled") != Some(&Value::Bool(false));
            status(format!(
                "Autoreview is {}: reviewing as {reviewing_as}; service {installed}. /autoreview on|off",
                if on { "on" } else { "off" }
            ))
        }
        AutoreviewRequest::Accounts => match run("gh", &["auth", "status", "--json", "hosts"]) {
            Err(_) => status(
                "Autoreview needs the GitHub CLI (gh): install it and run gh auth login, then /autoreview on".into(),
            ),
            Ok(ran) => LaneEvent::AutoreviewAccounts(
                review::accounts::parse_auth_status(&ran.stdout, &ran.stderr)
                    .into_iter()
                    .map(|account| (account.login, account.host))
                    .collect(),
            ),
        },
        AutoreviewRequest::Off => {
            settings.set_autoreview_settings(&[("enabled", Some(Value::Bool(false)))]);
            let mut notes = Vec::new();
            if let Some(file) = service.filter(|file| std::path::Path::new(&file.path).exists()) {
                for line in &file.disable {
                    if let Some(failed) = run_line(run, line) {
                        notes.push(format!("`{line}` failed: {failed}"));
                    }
                }
                review::service::uninstall_service(&file);
                notes.insert(0, format!("service removed ({})", file.path));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!("; {}", notes.join("; "))
            };
            status(format!(
                "Autoreview off: {APP_NAME} autoreview run and its service review nothing{notes}"
            ))
        }
        AutoreviewRequest::On { accounts, reviewing_as } => {
            settings.set_autoreview_settings(&[
                ("enabled", Some(Value::Bool(true))),
                ("accounts", accounts.map(|accounts| json!(accounts))),
            ]);
            let Some(file) = service else {
                return status(format!(
                    "Autoreview on, reviewing as {reviewing_as}. No user service on {platform}: run {APP_NAME} autoreview run under your own supervisor"
                ));
            };
            if let Err(error) = review::service::install_service(&file) {
                return status(format!("Error: {error}"));
            }
            let failures: Vec<String> = file
                .enable
                .iter()
                .filter_map(|line| run_line(run, line).map(|failed| format!("`{line}` failed: {failed}")))
                .collect();
            status(if failures.is_empty() {
                format!(
                    "Autoreview on, reviewing as {reviewing_as}: the service is started ({}); it reviews pull requests that request or mention the account",
                    file.path
                )
            } else {
                format!(
                    "Autoreview on, reviewing as {reviewing_as}; the service file is written ({}) but could not be started: {}. Start it by hand: {}",
                    file.path,
                    failures.join("; "),
                    file.enable.join(" && ")
                )
            })
        }
    }
}
