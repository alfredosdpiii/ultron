//! Config value resolution for MCP server `env`, `headers` and `oauth.clientSecret` (`core/resolve-config-value.ts`,
//! the uncached `OrThrow` path the MCP runtime uses), and `~` expansion of stdio paths (`runtime.ts` `expandHome`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

enum Part {
    Literal(String),
    Env(String),
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn push_literal(parts: &mut Vec<Part>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(Part::Literal(last)) = parts.last_mut() {
        last.push_str(value);
    } else {
        parts.push(Part::Literal(value.into()));
    }
}

/// `$NAME` and `${NAME}` are environment references; `$$` and `$!` escape `$` and `!`.
fn parse_template(config: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        match after.chars().next() {
            Some(c @ ('$' | '!')) => {
                push_literal(&mut parts, &c.to_string());
                rest = &after[1..];
            }
            Some('{') => match after[1..].find('}') {
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
                Some(end) => {
                    let name = &after[1..1 + end];
                    if is_name(name) {
                        parts.push(Part::Env(name.into()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + end + 3]);
                    }
                    rest = &after[end + 2..];
                }
            },
            _ => {
                let len = if after.chars().next().is_some_and(is_name_start) {
                    after
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(after.len())
                } else {
                    0
                };
                if len > 0 {
                    parts.push(Part::Env(after[..len].into()));
                } else {
                    push_literal(&mut parts, "$");
                }
                rest = &after[len..];
            }
        }
    }
    push_literal(&mut parts, rest);
    parts
}

/// Environment lookup: `env` first, then the process environment; empty counts as unset.
fn env_value(name: &str, env: &HashMap<String, String>) -> Option<String> {
    env.get(name)
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

/// Runs `command` with `/bin/sh -c` and `env` added to the environment, stdin and stderr ignored, for at most 10 s. Trimmed stdout, or None on failure
/// or empty output.
fn run_command(command: &str, env: &HashMap<String, String>) -> Option<String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut out);
        out
    });
    let end = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `resolveConfigValueUncached`: `!cmd` runs a shell command, anything else is a template. `env` overlays the
/// process environment.
pub fn resolve_config_value(config: &str, env: &HashMap<String, String>) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command, env);
    }
    let mut out = String::new();
    for part in parse_template(config) {
        match part {
            Part::Literal(text) => out.push_str(&text),
            Part::Env(name) => out.push_str(&env_value(&name, env)?),
        }
    }
    Some(out)
}

/// `resolveConfigValueOrThrow`, with its error messages.
pub fn resolve_config_value_or_err(
    config: &str,
    description: &str,
    env: &HashMap<String, String>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value(config, env) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    let mut missing: Vec<String> = Vec::new();
    for part in parse_template(config) {
        if let Part::Env(name) = part
            && !missing.contains(&name)
            && env_value(&name, env).is_none()
        {
            missing.push(name);
        }
    }
    Err(match missing.len() {
        0 => format!("Failed to resolve {description}"),
        1 => format!(
            "Failed to resolve {description} from environment variable: {}",
            missing[0]
        ),
        _ => format!(
            "Failed to resolve {description} from environment variables: {}",
            missing.join(", ")
        ),
    })
}

/// `resolveHeadersOrThrow`: every header resolved, None when there are none.
pub fn resolve_headers_or_err(
    headers: &[(String, String)],
    description: &str,
    env: &HashMap<String, String>,
) -> Result<Option<Vec<(String, String)>>, String> {
    let mut out = Vec::new();
    for (key, value) in headers {
        out.push((
            key.clone(),
            resolve_config_value_or_err(value, &format!("{description} header \"{key}\""), env)?,
        ));
    }
    Ok((!out.is_empty()).then_some(out))
}

/// `~` and `~/…` name `home`, like in a shell.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}
