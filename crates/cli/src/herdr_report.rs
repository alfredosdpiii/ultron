//! Ultron in herdr's agents panel (crates/herdr) for the interactive TUI and the prompted modes. Inside a herdr pane
//! (and unless the `herdr` setting is `false` or `ULTRON_HERDR=0`):
//!
//! - the TUI reports its session when it opens one (start, `/new`, `/resume`, a fork), `working` while a run is
//!   active (an auto-nudge or keep-going continuation that starts at once stays `working`), `blocked` when a run
//!   ends asking the user (its final paragraph ends with a question) or failed on an auth, model or usage-limit
//!   error, `idle` otherwise (with the message on an abort or another failure), and metadata: display agent
//!   "Ultron", the session's name (else its first prompt) as title, and tokens `model`, `ctx` (context used) and
//!   `cost` (the session's $), sent when they change, at most once a second;
//! - `-p`, `--mode json` and `--mode rpc` report `working` and `idle`/`blocked` for their runs, no metadata.
//!
//! Every report goes through the herdr reporter's own thread: the watch and link threads only update its queue, and
//! the render thread never touches it. The agent is released when the TUI (or the run) exits, also on a signal.

use crate::Env;
use crate::args::Args;
use herdr::{Metadata, Reporter, Session, State};
use interactive::lane::{ModelInfo, SessionOpened};
use serde_json::Value;
use std::sync::{Mutex, OnceLock, PoisonError};

/// The display name herdr shows for the TUI.
pub const DISPLAY_AGENT: &str = "Ultron";
/// The longest message or title sent.
const MAX_TEXT: usize = 200;

/// The pane to report to: the injected env and the `herdr` setting (global or trusted project).
pub fn pane(env: &Env) -> Option<herdr::Pane> {
    let var = &*env.var;
    // Outside herdr nothing else is read.
    var("HERDR_ENV")?;
    let settings = config::settings::SettingsManager::for_worker(&env.cwd, &env.agent_dir(), &env.home);
    herdr::detect(var, settings.settings().get("herdr"))
}

type ResumeOverride = Box<dyn Fn(&str) -> Option<Vec<String>> + Send + Sync>;
static RESUME_OVERRIDE: OnceLock<ResumeOverride> = OnceLock::new();

/// Let another start path (the durable instance's home session, `ultron-s`) name the command that resumes a
/// session: `f(session_id)` returns the argv (first element a plain command name), or `None` for the default.
pub fn set_resume_override(f: impl Fn(&str) -> Option<Vec<String>> + Send + Sync + 'static) {
    let _ = RESUME_OVERRIDE.set(Box::new(f));
}

/// The command that resumes `session_id` after a herdr restart: `ultron --session <id>` with the flags that select
/// the same sessions (`--session-dir`, `--claude`), behind `env VAR=...` for an instance or agent directory the
/// environment chose.
pub fn resume_argv(parsed: &Args, env: &Env, session_id: &str) -> Vec<String> {
    if let Some(argv) = RESUME_OVERRIDE.get().and_then(|f| f(session_id)) {
        return argv;
    }
    let mut argv = Vec::new();
    let assigned: Vec<String> = ["ULTRON_SERVER_DIR", "ULTRON_CODING_AGENT_DIR"]
        .iter()
        .filter_map(|k| (env.var)(k).filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
        .collect();
    if !assigned.is_empty() {
        argv.push("env".to_string());
        argv.extend(assigned);
    }
    argv.extend(["ultron", "--session", session_id].map(String::from));
    if let Some(dir) = &parsed.session_dir {
        argv.extend(["--session-dir".to_string(), dir.clone()]);
    }
    if parsed.claude {
        argv.push("--claude".into());
    }
    argv
}

fn clip(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_TEXT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

fn first_line(text: &str) -> String {
    clip(text.trim().lines().next().unwrap_or_default())
}

/// The state a run's end reports: its status, error and the final assistant text.
pub fn run_end_state(event: &Value, final_text: &str) -> (State, Option<String>) {
    match event["status"].as_str() {
        Some("aborted") => (State::Idle, Some("Aborted".into())),
        Some("failed") => {
            let error = event["error"]["message"].as_str().unwrap_or("The run failed");
            let state = if herdr::needs_user(error) {
                State::Blocked
            } else {
                State::Idle
            };
            (state, Some(first_line(error)))
        }
        _ if herdr::ends_with_question(final_text) => {
            let question = final_text.trim().rsplit("\n\n").next().unwrap_or_default();
            (State::Blocked, Some(clip(question)))
        }
        _ => (State::Idle, None),
    }
}

fn message_cost(message: &Value) -> f64 {
    if message["role"] == "assistant" {
        message["usage"]["cost"]["total"].as_f64().unwrap_or(0.0)
    } else {
        0.0
    }
}

#[derive(Default)]
struct Track {
    session: Option<String>,
    next_source: Option<&'static str>,
    name: Option<String>,
    first_prompt: Option<String>,
    model: Option<ModelInfo>,
    /// Messages since the last compaction (context use).
    transcript: Vec<Value>,
    compacted: bool,
    cost: f64,
    final_text: String,
}

/// The TUI's reporter and what its metadata is made of.
pub struct TuiReport {
    reporter: Reporter,
    /// What [`resume_argv`] is made of.
    parsed: Args,
    env: Env,
    track: Mutex<Track>,
}

impl TuiReport {
    /// The TUI's report, when it runs in a herdr pane.
    pub fn start(parsed: &Args, env: &Env) -> Option<TuiReport> {
        let pane = pane(env)?;
        Some(TuiReport {
            reporter: Reporter::start(&pane, herdr::AGENT),
            parsed: parsed.clone(),
            env: env.clone(),
            track: Mutex::default(),
        })
    }

    fn track(&self) -> std::sync::MutexGuard<'_, Track> {
        self.track.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next opened session is a new one (`/new`), not a resumed one.
    pub fn new_session(&self) {
        self.track().next_source = Some("clear");
    }

    /// The TUI shows `session` (at start, after `/new`, `/resume`, a fork or a reattachment).
    pub fn opened(&self, session: &SessionOpened, model: &ModelInfo) {
        let mut track = self.track();
        let changed = track.session.as_deref() != Some(&session.session_id);
        let source = track.next_source.take();
        if changed {
            let source = match (&track.session, source) {
                (None, _) => "startup",
                (Some(_), Some(source)) => source,
                (Some(_), None) => "resume",
            };
            track.session = Some(session.session_id.clone());
            track.transcript = session.messages.clone();
            track.compacted = false;
            // A fold from +0.0: an empty f64 `sum` is -0.0, which prints as `$-0.000`.
            track.cost = session.messages.iter().map(message_cost).fold(0.0, |a, b| a + b);
            track.first_prompt = session
                .messages
                .iter()
                .find(|m| m["role"] == "user")
                .map(|m| first_line(&session_core::view::message_text(m)))
                .filter(|t| !t.is_empty());
            self.reporter.session(Session {
                id: session.session_id.clone(),
                path: session.session_file.clone(),
                resume_argv: Some(resume_argv(&self.parsed, &self.env, &session.session_id)),
                start_source: Some(source.into()),
            });
            self.reporter.state(State::Idle, None);
        }
        track.name = session.name.clone();
        track.model = Some(model.clone());
        self.send_metadata(&track);
    }

    pub fn model(&self, model: &ModelInfo) {
        let mut track = self.track();
        track.model = Some(model.clone());
        self.send_metadata(&track);
    }

    pub fn name(&self, name: Option<String>) {
        let mut track = self.track();
        track.name = name;
        self.send_metadata(&track);
    }

    /// One lane-watch event of the attached session (the link's watch thread).
    pub fn lane_event(&self, event: &Value) {
        let mut track = self.track();
        match event["type"].as_str() {
            Some("run_start") => {
                track.final_text.clear();
                self.reporter.state(State::Working, None);
            }
            Some("message_end") => {
                let message = &event["message"];
                if message["role"] == "assistant" {
                    track.final_text = session_core::view::message_text(message);
                    track.cost += message_cost(message);
                }
                if message["role"] == "user" && track.first_prompt.is_none() {
                    let text = first_line(&session_core::view::message_text(message));
                    track.first_prompt = (!text.is_empty()).then_some(text);
                }
                track.transcript.push(message.clone());
                self.send_metadata(&track);
            }
            Some("run_end") => {
                let (state, message) = run_end_state(event, &track.final_text);
                self.reporter.state(state, message);
            }
            Some("session_name") => {
                track.name = event["name"].as_str().map(str::to_string);
                self.send_metadata(&track);
            }
            Some("entry_added") if event["entry"]["type"] == "compaction" => {
                track.transcript.clear();
                track.compacted = true;
                self.send_metadata(&track);
            }
            _ => {}
        }
    }

    fn send_metadata(&self, track: &Track) {
        let mut metadata = Metadata {
            display_agent: Some(DISPLAY_AGENT.into()),
            title: track.name.clone().or_else(|| track.first_prompt.clone()),
            ..Metadata::default()
        };
        if let Some(model) = &track.model
            && !model.id.is_empty()
        {
            metadata.tokens.insert("model".into(), model.id.clone());
            let measured = track.transcript.iter().any(|m| m["role"] == "assistant");
            if (!track.compacted || measured)
                && let Some((_, percent)) = interactive::app::context_usage(&track.transcript, model.context_window)
            {
                metadata.tokens.insert("ctx".into(), format!("{percent:.0}%"));
            }
        }
        metadata.tokens.insert("cost".into(), format!("${:.3}", track.cost));
        self.reporter.metadata(metadata);
    }

    /// Release the agent now (a signal is about to end the process).
    pub fn release(&self) {
        self.reporter.release();
    }
}

/// A prompted run's report (`-p`, `--mode json`): `working` now, the end's state on [`PrintReport::finish`], the
/// release when dropped.
pub struct PrintReport {
    reporter: Reporter,
}

impl PrintReport {
    pub fn start(env: &Env) -> Option<PrintReport> {
        let reporter = Reporter::start(&pane(env)?, herdr::AGENT);
        reporter.state(State::Working, None);
        Some(PrintReport { reporter })
    }

    /// The run ended with this answer or error.
    pub fn finish(&self, result: Result<&str, &str>) {
        let (state, message) = match result {
            Ok(text) => run_end_state(&serde_json::json!({"status": "completed"}), text),
            Err(error) => run_end_state(
                &serde_json::json!({"status": "failed", "error": {"message": error}}),
                "",
            ),
        };
        self.reporter.state(state, message);
    }
}

/// RPC mode's report: each run's start and end.
pub struct RpcReport {
    reporter: Reporter,
    final_text: Mutex<String>,
}

impl RpcReport {
    pub fn start(env: &Env) -> Option<RpcReport> {
        let reporter = Reporter::start(&pane(env)?, herdr::AGENT);
        reporter.state(State::Idle, None);
        Some(RpcReport {
            reporter,
            final_text: Mutex::default(),
        })
    }

    pub fn event(&self, event: &Value) {
        let mut text = self.final_text.lock().unwrap_or_else(PoisonError::into_inner);
        match event["type"].as_str() {
            Some("run_start") => {
                text.clear();
                self.reporter.state(State::Working, None);
            }
            Some("message_end") if event["message"]["role"] == "assistant" => {
                *text = session_core::view::message_text(&event["message"]);
            }
            Some("run_end") => {
                let (state, message) = run_end_state(event, &text);
                self.reporter.state(state, message);
            }
            _ => {}
        }
    }
}
