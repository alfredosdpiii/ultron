//! `ultron mcp login|logout <server>` (`extensions/mcp/cli.ts` `login`, `waitForRedirectUrl`): sign in to an OAuth MCP
//! server through the browser outside a session, storing its tokens in `<agentDir>/mcp-auth.json`
//! (crates/mcp `oauth`), or delete them. The browser is started with the launcher found on the run's own `PATH`
//! (the Env's, never this process's), so tests put a recording opener there.

use crate::Env;
use mcp::config::ServerEntry;
use mcp::connection::{Connection, ConnectionEnv, ServerState};
use mcp::oauth::OAuthError;
use mcp::oauth::store::CredentialStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `DEFAULT_LOGIN_TIMEOUT_SECONDS`.
pub const DEFAULT_LOGIN_TIMEOUT_SECONDS: f64 = 300.0;

/// The connection environment of a command outside a session: the stored OAuth credentials, no secret masking.
pub fn connection_env(env: &Env, agent_dir: &Path) -> ConnectionEnv {
    ConnectionEnv {
        cwd: env.cwd.clone(),
        home: env.home.clone(),
        env_overlay: HashMap::new(),
        client_version: crate::version(),
        credentials: Some(Arc::new(CredentialStore::new(agent_dir, None))),
        // The run's proxies (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`), never this process's own environment.
        proxy: Some(ai::net::ProxyEnv::from_lookup(&*env.var)),
        ..ConnectionEnv::default()
    }
}

/// The browser launcher for this platform, looked up on `path` (the run's `PATH`): `open` on macOS, else `xdg-open`.
pub fn browser_launcher(path: Option<&str>) -> Option<PathBuf> {
    let name = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `openBrowser`: start the launcher on `url`, detached; a launcher that is missing or fails is ignored.
fn open_browser(env: &Env, url: &str) {
    let path = (env.var)("PATH");
    let Some(launcher) = browser_launcher(path.as_deref()) else {
        return;
    };
    let mut command = std::process::Command::new(launcher);
    command
        .arg(url)
        .env_clear()
        .env("PATH", path.unwrap_or_default())
        .env("HOME", &env.home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// The sign-in prompt of the command: the URL on stdout and in the browser; in a terminal the pasted redirect URL
/// may finish the sign-in, otherwise only the browser's callback can, until `timeout`.
struct Prompt<'a> {
    name: &'a str,
    env: &'a Env,
    timeout: Duration,
    interactive: bool,
}

impl mcp::oauth::signin::SignInPrompt for Prompt<'_> {
    fn show_authorization_url(&self, url: &str) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "Sign in to MCP server \"{}\" in your browser:\n{url}", self.name);
        let _ = out.flush();
        open_browser(self.env, url);
    }

    fn prompt_for_redirect_url(&self, cancel: &mcp::CancelToken) -> Option<String> {
        let deadline = Instant::now() + self.timeout;
        if self.interactive {
            use std::io::Write;
            let mut err = std::io::stderr();
            let _ = write!(
                err,
                "If the browser cannot reach this machine, paste the URL it was redirected to: "
            );
            let _ = err.flush();
            if let Ok(mut input) = crate::line_input::TerminalInput::open() {
                return redirect_answer(&mut input, &mut err, cancel, deadline);
            }
        }
        while !cancel.is_cancelled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

/// `waitForRedirectUrl`'s question on the terminal: the pasted line, or `None` when Esc/Ctrl+C cancelled it, the
/// input ended, the browser's callback arrived (`cancel`) or `deadline` passed. The read stops with it, so no reader
/// is left behind to take later input.
pub fn redirect_answer(
    input: &mut dyn crate::line_input::Input,
    echo: &mut dyn std::io::Write,
    cancel: &mcp::CancelToken,
    deadline: Instant,
) -> Option<String> {
    let stop = || cancel.is_cancelled() || Instant::now() >= deadline;
    match crate::line_input::read_line(input, echo, &stop) {
        crate::line_input::LineRead::Line(line) => Some(line),
        _ => None,
    }
}

/// `Number(text)` for the `--timeout` value: positive and finite, in seconds.
pub fn login_timeout(value: Option<&str>) -> Result<f64, String> {
    let seconds = match value {
        None => DEFAULT_LOGIN_TIMEOUT_SECONDS,
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("--timeout must be a positive number of seconds.".into());
    }
    Ok(seconds)
}

/// The outcome lines of `login`/`logout`: what goes to stdout, what to stderr, and the exit code.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub out: Vec<String>,
    pub err: Vec<String>,
    pub code: i32,
}

/// `logout`: delete the server's stored credentials.
pub fn logout(entry: &ServerEntry, url: &str, agent_dir: &Path) -> Outcome {
    let name = &entry.name;
    let store = CredentialStore::new(agent_dir, None);
    match store.remove(name, url) {
        Ok(true) => Outcome {
            out: vec![format!("Signed out of MCP server \"{name}\".")],
            ..Outcome::default()
        },
        Ok(false) => Outcome {
            out: vec![format!("No stored credentials for MCP server \"{name}\".")],
            ..Outcome::default()
        },
        Err(error) => Outcome {
            err: vec![error.to_string()],
            code: 1,
            ..Outcome::default()
        },
    }
}

/// `login`: connect first (already signed in, or the server's challenge), then the browser sign-in and a reconnect.
pub fn login(connection: &Arc<Connection>, url: &str, timeout_seconds: f64, env: &Env) -> Outcome {
    use std::io::IsTerminal;
    let name = connection.name().to_string();
    let mut outcome = Outcome::default();
    if connection.get_client().is_ok() {
        outcome.out.push(format!(
            "Already signed in to MCP server \"{name}\" ({} tools).",
            connection.tools().len()
        ));
        return outcome;
    }
    if connection.state() != ServerState::NeedsAuth {
        outcome.err.push(format!(
            "MCP server \"{name}\" failed to connect: {}",
            connection.error().unwrap_or_else(|| "unknown error".into())
        ));
        outcome.code = 1;
        return outcome;
    }
    let settings = match connection.oauth_settings() {
        Ok(settings) => settings,
        Err(error) => {
            outcome
                .err
                .push(format!("Sign-in to MCP server \"{name}\" failed: {error}"));
            outcome.code = 1;
            return outcome;
        }
    };
    let agent_dir = env.agent_dir();
    let store = Arc::new(CredentialStore::new(&agent_dir, None));
    let prompt = Prompt {
        name: &name,
        env,
        timeout: Duration::from_secs_f64(timeout_seconds),
        interactive: std::io::stdin().is_terminal(),
    };
    match mcp::oauth::signin::sign_in(
        url,
        &store.for_server(&name, url),
        &settings,
        connection.challenge().as_ref(),
        &prompt,
    ) {
        Ok(()) => {}
        Err(OAuthError::Cancelled) => {
            outcome.err.push(format!(
                "Sign-in to MCP server \"{name}\" was cancelled or not completed within {} seconds.",
                timeout_seconds.round()
            ));
            outcome.code = 1;
            return outcome;
        }
        Err(error) => {
            outcome
                .err
                .push(format!("Sign-in to MCP server \"{name}\" failed: {error}"));
            outcome.code = 1;
            return outcome;
        }
    }
    if let Err(error) = connection.reconnect() {
        outcome.err.push(format!("Signed in, but {error}"));
        outcome.code = 1;
        return outcome;
    }
    outcome.out.push(format!(
        "Signed in to MCP server \"{name}\" ({} tools).",
        connection.tools().len()
    ));
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_mcp_login_timeout_is_a_positive_number_of_seconds() {
        assert_eq!(login_timeout(None), Ok(300.0));
        assert_eq!(login_timeout(Some("2.5")), Ok(2.5));
        assert_eq!(login_timeout(Some(" 10 ")), Ok(10.0));
        for bad in ["0", "-1", "abc", "", "inf", "NaN"] {
            assert_eq!(
                login_timeout(Some(bad)),
                Err("--timeout must be a positive number of seconds.".into()),
                "{bad}"
            );
        }
    }

    #[test]
    fn cli_mcp_login_finds_the_browser_only_on_the_given_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(browser_launcher(None), None);
        assert_eq!(browser_launcher(Some(&dir.path().display().to_string())), None);
        let name = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::fs::write(dir.path().join(name), "").unwrap();
        let path = format!("/nonexistent-dir:{}", dir.path().display());
        assert_eq!(browser_launcher(Some(&path)), Some(dir.path().join(name)));
    }
}
