//! The TUI's subscription (OAuth) login (`runProviderLogin` of experimental/client-tui-auth.ts): the provider's flow
//! runs in this client process, which owns the terminal and the browser. Its steps go to the TUI's login dialog as
//! [`LaneEvent::Login`] and the dialog's answers come back through [`LoginSlot`]; the credential is saved to the
//! profile's `auth.json` as `ModelRuntime.login` saves it. Every provider TypeScript signs in with an account:
//! Anthropic, OpenAI Codex, GitHub Copilot, xAI, Kimi For Coding, Meta, OpenRouter and Radius (the built-in one and
//! models.json `"oauth": "radius"` ones).

use crate::Env;
use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt};
use ai::http::{AbortSignal, BoxFuture};
use interactive::lane::{LaneEvent, LoginEvent, LoginPrompt, LoginStep};
use std::sync::{Arc, Mutex};

/// The pending prompt of the login running now, and its signal.
#[derive(Default)]
pub struct LoginSlot {
    reply: Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>,
    signal: Mutex<Option<AbortSignal>>,
}

impl LoginSlot {
    /// The dialog's answer to the pending prompt; `None` stops the login (`Login cancelled`).
    pub fn reply(&self, answer: Option<String>) {
        if answer.is_none()
            && let Some(signal) = self.signal.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            signal.abort();
        }
        if let Some(sender) = self.reply.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = sender.send(answer);
        }
    }
}

/// `CANCELLED`.
const CANCELLED: &str = "Login cancelled";

/// Whether this client signs in to `provider` with an account.
pub fn supported(env: &Env, provider: &str) -> bool {
    session_core::stored_auth::oauth_handler_with(env, provider, &options(env)).is_some()
}

/// The login's transport and callback port. `ULTRON_OAUTH_CALLBACK_PORT` (a Rust-only test hook) moves a browser
/// login's callback server off its registered port (`0`: any free port), so tests never hold a machine-wide port.
pub fn options(env: &Env) -> session_core::stored_auth::OAuthOptions {
    session_core::stored_auth::OAuthOptions {
        callback_port: (env.var)("ULTRON_OAUTH_CALLBACK_PORT").and_then(|p| p.trim().parse().ok()),
        ..Default::default()
    }
}

/// `browserReach()` (utils/remote-browser.ts): `local`, `container` or `ssh`.
pub fn browser_reach(env: &Env) -> String {
    let set = |name: &str| (env.var)(name).is_some_and(|v| !v.trim().is_empty());
    if set("ULTRON_OAUTH_CALLBACK_HOST") || set("PI_OAUTH_CALLBACK_HOST") {
        return "local".into();
    }
    let container = set("container")
        || set("KUBERNETES_SERVICE_HOST")
        || std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
            ["docker", "containerd", "kubepods", "libpod", "lxc"]
                .iter()
                .any(|marker| cgroup.contains(marker))
        });
    if container {
        return "container".into();
    }
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        "ssh".into()
    } else {
        "local".into()
    }
}

/// `openBrowser`: hand the address to the desktop's opener, never waiting on it. The opener is found on the run's
/// own PATH (`env`), not this process's: a run whose environment has no PATH (a test's) opens nothing.
fn open_browser(env: &Env, url: &str) {
    let name = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let Some(path) = (env.var)("PATH") else {
        return;
    };
    let Some(opener) = std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
    else {
        return;
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .env("PATH", &path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

type Send = Arc<dyn Fn(LaneEvent) + std::marker::Send + Sync>;

struct DialogInteraction {
    send: Send,
    slot: Arc<LoginSlot>,
    signal: AbortSignal,
    reach: String,
    env: Env,
}

impl AuthInteraction for DialogInteraction {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let view = match prompt {
                AuthPrompt::Select { message, options } => LoginPrompt::Select {
                    message,
                    options: options.into_iter().map(|o| (o.id, o.label)).collect(),
                },
                AuthPrompt::ManualCode { message, .. } => LoginPrompt::ManualCode { message },
                AuthPrompt::Text { message, placeholder } => LoginPrompt::Text {
                    message,
                    placeholder,
                    secret: false,
                },
                AuthPrompt::Secret { message, placeholder } => LoginPrompt::Text {
                    message,
                    placeholder,
                    secret: true,
                },
            };
            if self.signal.aborted() || cancel.as_ref().is_some_and(AbortSignal::aborted) {
                return Err(CANCELLED.into());
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            *self.slot.reply.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
            (self.send)(LaneEvent::Login(LoginStep::Prompt(view)));
            let cancelled = async {
                match &cancel {
                    Some(cancel) => cancel.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                answer = receiver => match answer {
                    Ok(Some(answer)) => Ok(answer),
                    _ => Err(CANCELLED.into()),
                },
                () = self.signal.cancelled() => Err(CANCELLED.into()),
                () = cancelled => Err(CANCELLED.into()),
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        let view = match event {
            AuthEvent::AuthUrl { url, instructions } => {
                if self.reach == "local" {
                    open_browser(&self.env, &url);
                }
                LoginEvent::AuthUrl { url, instructions }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => LoginEvent::DeviceCode {
                user_code,
                verification_uri,
            },
            AuthEvent::Info { message } => LoginEvent::Info {
                message,
                links: Vec::new(),
            },
            AuthEvent::Progress { message } => LoginEvent::Progress(message),
        };
        (self.send)(LaneEvent::Login(LoginStep::Event(view)));
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(self.signal.clone())
    }
}

/// Run `provider`'s account login on its own thread. On success the credential is in `auth.json` and `saved` runs
/// (the statuses and catalog that follow a login); a failure reports `Error: Failed to login to <name>: <error>`, a
/// cancel nothing.
pub fn start(
    env: Env,
    provider: String,
    name: String,
    send: Send,
    slot: Arc<LoginSlot>,
    saved: Box<dyn FnOnce() + std::marker::Send>,
) {
    let options = options(&env);
    start_with(env, provider, name, send, slot, saved, options);
}

/// [`start`] with the provider reached through `options` (tests answer its endpoints).
pub fn start_with(
    env: Env,
    provider: String,
    name: String,
    send: Send,
    slot: Arc<LoginSlot>,
    saved: Box<dyn FnOnce() + std::marker::Send>,
    options: session_core::stored_auth::OAuthOptions,
) {
    let reach = browser_reach(&env);
    let signal = AbortSignal::new();
    *slot.signal.lock().unwrap_or_else(|e| e.into_inner()) = Some(signal.clone());
    send(LaneEvent::Login(LoginStep::Started {
        name: name.clone(),
        reach: reach.clone(),
    }));
    std::thread::spawn(move || {
        let interaction = DialogInteraction {
            send: send.clone(),
            slot: slot.clone(),
            signal: signal.clone(),
            reach,
            env: env.clone(),
        };
        let outcome = (|| -> Result<(), String> {
            let oauth = session_core::stored_auth::oauth_handler_with(&env, &provider, &options)
                .ok_or_else(|| format!("{name} does not support oauth login"))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let credential = runtime.block_on(oauth.login(&interaction))?;
            if signal.aborted() {
                return Err(CANCELLED.into());
            }
            let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
            config::auth::AuthStorage::create(auth)
                .modify(&provider, |_| Some(credential.to_json()))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })();
        *slot.signal.lock().unwrap_or_else(|e| e.into_inner()) = None;
        slot.reply.lock().unwrap_or_else(|e| e.into_inner()).take();
        send(LaneEvent::Login(LoginStep::Finished));
        match outcome {
            Ok(()) => saved(),
            Err(error) if error == CANCELLED || signal.aborted() => {}
            Err(error) => send(LaneEvent::Status(format!("Error: Failed to login to {name}: {error}"))),
        }
    });
}
