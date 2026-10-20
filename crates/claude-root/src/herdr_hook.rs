//! `ultron claude` in herdr's agents panel (crates/herdr). The launcher runs Claude Code with `--setting-sources ""`,
//! so herdr's own Claude hook never fires; Ultron's hook server reports instead, from the root `ultron mcp` server
//! (never a subagent's): SessionStart reports the session (resumable with `ultron claude --resume <id>`) and
//! `idle`, UserPromptSubmit `working`, Stop `blocked` when the answer ends with a question and `idle` otherwise
//! (`working` stays while the persona's nudge continues the turn), a Notification for a permission prompt, an idle
//! prompt or a dialog `blocked`, and SessionEnd releases the agent (a later SessionStart, after `/clear`, reports
//! again). Agent `ultron`, display agent "Ultron (Claude)". Inside herdr the launcher adds the Notification and
//! SessionEnd hooks ([`HERDR_HOOK_EVENTS`]).

use herdr::{Metadata, Reporter, Session, State};
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use std::sync::{Mutex, PoisonError};

/// The hook commands only herdr reporting uses, and their Claude Code events.
pub const HERDR_HOOK_EVENTS: [(&str, &str); 2] = [("notification", "Notification"), ("session-end", "SessionEnd")];

pub const DISPLAY_AGENT: &str = "Ultron (Claude)";

/// Notification types that wait for the user.
const BLOCKING_NOTIFICATIONS: [&str; 3] = ["permission_prompt", "idle_prompt", "elicitation_dialog"];

fn text(input: &Map<String, Value>, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The resume command of a Claude Code session.
pub fn resume_argv(claude_session_id: &str) -> Vec<String> {
    ["ultron", "claude", "--resume", claude_session_id]
        .map(String::from)
        .to_vec()
}

/// The root server's herdr reporting; inert outside herdr, when turned off, and in a subagent's server.
pub struct HerdrHook {
    pane: Option<herdr::Pane>,
    reporter: Mutex<Option<Reporter>>,
}

impl HerdrHook {
    pub fn start(env: &RunEnv, child: bool) -> HerdrHook {
        let pane = (!child && (env.var)("HERDR_ENV").is_some())
            .then(|| {
                let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
                herdr::detect(&*env.var, settings.global_settings().get("herdr"))
            })
            .flatten();
        HerdrHook {
            pane,
            reporter: Mutex::new(None),
        }
    }

    /// Whether this server reports to herdr.
    pub fn active(&self) -> bool {
        self.pane.is_some()
    }

    fn with(&self, start: bool, f: impl FnOnce(&Reporter)) {
        let Some(pane) = &self.pane else { return };
        let mut reporter = self.reporter.lock().unwrap_or_else(PoisonError::into_inner);
        if reporter.is_none() && start {
            *reporter = Some(Reporter::start(pane, herdr::AGENT));
        }
        if let Some(reporter) = reporter.as_ref() {
            f(reporter);
        }
    }

    /// A hook event before the server answers it: `Some(answer)` for the events only herdr uses (answered here),
    /// `None` for the others (answered by the server as before).
    pub fn before(&self, event: &str, input: &Map<String, Value>) -> Option<Value> {
        match event {
            "session-start" => self.with(true, |reporter| {
                if let Some(id) = text(input, "session_id") {
                    reporter.session(Session {
                        resume_argv: Some(resume_argv(&id)),
                        id,
                        path: text(input, "transcript_path"),
                        start_source: text(input, "source"),
                    });
                }
                reporter.state(State::Idle, None);
                reporter.metadata(Metadata {
                    display_agent: Some(DISPLAY_AGENT.into()),
                    ..Metadata::default()
                });
            }),
            "user-prompt" => self.with(true, |reporter| reporter.state(State::Working, None)),
            "notification" => {
                let kind = text(input, "notification_type").unwrap_or_default();
                if BLOCKING_NOTIFICATIONS.contains(&kind.as_str()) {
                    self.with(false, |reporter| reporter.state(State::Blocked, text(input, "message")));
                }
                return Some(json!({}));
            }
            "session-end" => {
                // Dropped: the agent is released; a later SessionStart starts a new reporter.
                let reporter = self.reporter.lock().unwrap_or_else(PoisonError::into_inner).take();
                drop(reporter);
                return Some(json!({}));
            }
            _ => {}
        }
        None
    }

    /// The Stop hook's answer is `answer`: a turn the persona continues stays `working`.
    pub fn stopped(&self, input: &Map<String, Value>, answer: &Value) {
        if self.pane.is_none() || answer["decision"] == "block" {
            return;
        }
        let final_text = text(input, "last_assistant_message")
            .unwrap_or_else(|| resources::persona::last_turn(&crate::persona_hook::transcript_of(input)).0);
        let (state, message) = if herdr::ends_with_question(&final_text) {
            let question = final_text.trim().rsplit("\n\n").next().unwrap_or_default().trim();
            let clipped: String = question.chars().take(200).collect();
            (State::Blocked, Some(clipped))
        } else {
            (State::Idle, None)
        };
        self.with(false, |reporter| reporter.state(state, message));
    }

    /// The server shuts down: release the agent.
    pub fn release(&self) {
        let reporter = self.reporter.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(reporter);
    }
}
