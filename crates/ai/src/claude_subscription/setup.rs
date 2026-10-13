//! Setup-time probes of the user's Claude CLI (`directsdk_setup.py`): its login state from `claude auth status`
//! (the local credential store; this module never reads it) and its own model picker from the stream-json
//! `initialize` handshake, run behind an admission relay that proves no Messages request was sent. Anything
//! unexpected answers `None` so callers fall back to the pinned catalog.

use super::admission::Admission;
use super::catalog::{catalog_label, native_model, pinned_routes};
use super::process::{apply_traffic_policy, resolve_claude, telemetry_enabled};
use super::{Config, Env, INSTALL_HINT, LOGIN_HINT};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupStatus {
    pub available: bool,
    pub logged_in: bool,
    pub plan: String,
    pub detail: String,
    pub login_command: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub label: String,
    pub note: String,
    pub upstream_requests: u64,
}

/// `"pro"` (auth status) and `"Claude Pro"` (handshake) name the same plan.
pub fn plan_label(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    if raw.to_ascii_lowercase().starts_with("claude") {
        return raw.to_string();
    }
    let words: Vec<String> = raw
        .replace('_', " ")
        .split(' ')
        .map(|w| {
            let lower = w.to_ascii_lowercase();
            match lower.get(..1) {
                Some(first) => first.to_uppercase() + &lower[1..],
                None => lower,
            }
        })
        .collect();
    format!("Claude {}", words.join(" "))
}

fn child_env(config: &Config, env: &Env) -> Env {
    let mut child = env.clone();
    // The probes report the plan login, never an API credential the CLI would prefer.
    for key in super::process::API_CREDENTIALS {
        child.remove(*key);
    }
    let telemetry = telemetry_enabled(config, &child);
    apply_traffic_policy(&mut child, telemetry);
    child
}

/// Run `argv` with `input` on stdin, killing it after `timeout`; (exit success, stdout).
fn run_bounded(
    argv: &[String],
    env: &Env,
    cwd: Option<&std::path::Path>,
    input: &str,
    timeout: Duration,
) -> Option<(bool, String)> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    status.map(|s| (s.success(), text))
}

/// Installed and logged in, from the CLI's own `auth status`.
pub fn setup_status(config: &Config, env: &Env, timeout: Duration) -> SetupStatus {
    let Some(resolved) = resolve_claude(config.command.as_deref(), env) else {
        return SetupStatus {
            available: false,
            logged_in: false,
            plan: String::new(),
            detail: INSTALL_HINT.into(),
            login_command: None,
        };
    };
    let mut login = resolved.clone();
    login.extend(["auth".to_string(), "login".to_string()]);
    let mut status = resolved;
    status.extend(["auth".to_string(), "status".to_string()]);
    let auth: Value = run_bounded(&status, &child_env(config, env), None, "", timeout)
        .map(|(_, out)| out)
        .filter(|out| out.trim_start().starts_with('{'))
        .and_then(|out| serde_json::from_str(&out).ok())
        .unwrap_or(json!({}));
    let logged_in = auth["loggedIn"] == json!(true);
    SetupStatus {
        available: true,
        logged_in,
        plan: plan_label(auth["subscriptionType"].as_str().unwrap_or_default()),
        detail: if logged_in { String::new() } else { LOGIN_HINT.into() },
        login_command: Some(login),
    }
}

/// The account's live picker in Ultron route ids (`discover_models`), or `None` when the CLI is missing, logged
/// out, the handshake fails or it reached the upstream. Pinned routes the picker omits follow its rows.
pub fn discover_models(config: &Config, env: &Env, timeout: Duration) -> Option<Vec<DiscoveredModel>> {
    let resolved = resolve_claude(config.command.as_deref(), env)?;
    let probe = Config {
        command: Some(resolved.clone()),
        ..config.clone()
    };
    if !setup_status(&probe, env, timeout).logged_in {
        return None;
    }
    let mut child = child_env(config, env);
    let mut gate = Admission::new(&config.upstream, timeout, None, config.idle, &child).ok()?;
    child.insert("ANTHROPIC_BASE_URL".into(), gate.url.clone());
    let mut argv = resolved;
    argv.extend(
        [
            "-p",
            "--model",
            "sonnet",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--disable-slash-commands",
            "--no-session-persistence",
        ]
        .map(str::to_string),
    );
    let handshake =
        json!({"type": "control_request", "request_id": "ultron-picker", "request": {"subtype": "initialize"}});
    let temp_root = config.temp_root.clone().unwrap_or_else(std::env::temp_dir);
    let cwd = temp_root.join(format!("ultron-claude-picker-{}", super::admission::random_token(6)));
    std::fs::create_dir(&cwd).ok()?;
    let output = run_bounded(&argv, &child, Some(&cwd), &format!("{handshake}\n"), timeout);
    let _ = std::fs::remove_dir_all(&cwd);
    gate.close();
    let upstream = u64::from(gate.outcome().used);
    let (_, stdout) = output?;
    let response = stdout
        .lines()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["type"] == "control_response")?["response"]
        .clone();
    let native = response["response"]["models"].as_array().cloned().unwrap_or_default();
    if upstream > 0 || native.is_empty() {
        return None;
    }
    // Pro / Team-standard seats bill Fable to usage credits from the first request.
    let plan = response["response"]["account"]["subscriptionType"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let credit_billed = |canonical: &str| !plan.is_empty() && !plan.contains("max") && canonical == "claude-fable-5-1";
    let pinned = pinned_routes();
    let announced: Vec<String> = native
        .iter()
        .map(|row| {
            row["resolvedModel"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| row["value"].as_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let long_context: Vec<&str> = announced.iter().filter_map(|m| m.strip_suffix("[1m]")).collect();
    let mut routes: Vec<DiscoveredModel> = Vec::new();
    for (row, model) in native.iter().zip(&announced) {
        let base = model.strip_suffix("[1m]").unwrap_or(model);
        if base.is_empty() {
            continue;
        }
        let Ok(mut route) = native_model(base) else { continue };
        let is_pinned = pinned.iter().any(|(r, _, _)| *r == route);
        if !is_pinned {
            // A `value` the CLI did not resolve (`default`, `best`) is an alias row, not a model.
            if row["resolvedModel"].as_str().is_none_or(str::is_empty) {
                continue;
            }
            if long_context.contains(&base) {
                route = format!("{base}[1m]");
            }
        }
        let description = row["description"].as_str().unwrap_or_default();
        let label = description.split('·').next().unwrap_or_default().trim();
        let label = if label.is_empty() {
            route.clone()
        } else {
            label.to_string()
        };
        let index = match routes.iter().position(|r| r.id == route) {
            Some(index) => index,
            None => {
                routes.push(DiscoveredModel {
                    id: route.clone(),
                    label,
                    note: if is_pinned { String::new() } else { "unpinned".into() },
                    upstream_requests: upstream,
                });
                routes.len() - 1
            }
        };
        let canonical = super::catalog::canonical(base);
        if description.to_ascii_lowercase().contains("usage credit") || credit_billed(canonical) {
            routes[index].note = if is_pinned {
                "usage credits".into()
            } else {
                "usage credits · unpinned".into()
            };
        }
    }
    if routes.is_empty() {
        return None;
    }
    for (route, canonical, _) in pinned {
        if routes.iter().any(|r| r.id == route) {
            continue;
        }
        routes.push(DiscoveredModel {
            id: route,
            label: catalog_label(canonical),
            note: if credit_billed(canonical) {
                "usage credits".into()
            } else {
                String::new()
            },
            upstream_requests: upstream,
        });
    }
    Some(routes)
}
