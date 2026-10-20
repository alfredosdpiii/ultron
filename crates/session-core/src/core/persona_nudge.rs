//! The persona's auto-nudge on `main` (resources::persona): a root run that ended normally by announcing its next
//! step, after at least one `rlm` call, is continued with a nudge message (a displayed custom message, journaled
//! like any run's prompt), at most [`resources::persona::MAX_NUDGES`] times since the user's last input. Only while
//! the persona is on (the session's own toggle, else the settings' `persona`); a queued user message or other
//! waiting input wins over the nudge.
//!
//! Keep-going mode (resources::keep_going) goes first: a user prompt with a keep-going phrase starts it, and while
//! it lasts every run that made an `rlm` call is continued with the keep-going nudge, up to the ceiling. The toggle,
//! the mode and its counters are the session value `ultron.module/persona`, so a restarted worker resumes them.

use super::{Core, lock};
use crate::durable_store::lock as lock_session;
use resources::keep_going::{self, Limits, Next, RunEnd, STATE_KEY, STATE_NAMESPACE, SessionPersona, Toggle};
use serde_json::{Value, json};
use session_journal::Write;

impl Core {
    /// The session's persona state as the session keeps it; the system prompt follows its toggle.
    pub(super) fn persona_load(&mut self) {
        let saved = lock_session(&self.session).value(STATE_NAMESPACE, STATE_KEY).cloned();
        self.persona_session = SessionPersona::from_json(saved.as_ref());
        self.runtime.system.set_persona(self.persona_session.prompt_override());
    }

    /// Keep the persona state in the session (through the session values, which stop writing once the session is
    /// handed to a successor) and apply its toggle to the system prompt.
    fn persona_save(&mut self) {
        self.runtime.system.set_persona(self.persona_session.prompt_override());
        let value = self.persona_session.to_json();
        // A state that cannot be written stays in memory; the next change writes it again.
        let _ = match &self.runtime.services.values {
            Some(values) => values.set(STATE_NAMESPACE, STATE_KEY, &value),
            None => lock_session(&self.session)
                .commit(&[Write::set(STATE_NAMESPACE, STATE_KEY, value)])
                .map(drop),
        };
    }

    /// The keep-going ceiling (`ULTRON_KEEPGOING_MAX_CONTINUES`, `ULTRON_KEEPGOING_MAX_MS` over 100 and 8 hours).
    fn persona_limits(&self) -> Limits {
        Limits::from_env(&*self.env.var)
    }

    /// The user sent input (a prompt or a queued message) with `text`: the nudge count starts over, a keep-going
    /// phrase starts keep-going mode (and turns the persona on), other input ends it. The one-line notice, if any.
    pub(super) fn persona_user_input(&mut self, text: &str) -> Option<String> {
        self.persona.user_input();
        let before = self.persona_session.clone();
        let notice = self.persona_session.user_input(text, crate::lane::now_ms());
        if self.persona_session != before {
            self.persona_save();
        }
        notice
    }

    /// `/persona on|off|auto` and `/keepgoing on|off`: `{text, persona, effective, keepGoing}`.
    pub(super) fn persona_command(&mut self, command: &str, args: &str) -> Result<Value, String> {
        let global = self.persona_on();
        let limits = self.persona_limits();
        let before = self.persona_session.clone();
        let text = self
            .persona_session
            .command(command, args, crate::lane::now_ms(), global, limits)?;
        if self.persona_session != before {
            self.persona_save();
        }
        let state = self.persona_session.to_json();
        Ok(json!({
            "text": text,
            "persona": state["persona"],
            "effective": self.persona_session.effective(global),
            "keepGoing": state["keepGoing"],
        }))
    }

    /// An assistant message of the run ended.
    pub(super) fn persona_assistant(&mut self, message: &Value) {
        if self.run.is_none() {
            return;
        }
        self.persona.assistant_message(message);
        let calls = message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "toolCall"));
        // Keep-going remembers the run's work in the session, so a run resumed after a restart still counts it.
        if calls && self.persona_session.worked() {
            self.persona_save();
        }
    }

    /// Whether the persona is on for this session: its own toggle, else the settings.
    fn persona_effective(&self) -> bool {
        match self.persona_session.toggle {
            Toggle::On => true,
            Toggle::Off => false,
            Toggle::Auto => self.persona_on(),
        }
    }

    /// Whether the settings name the persona now (read at the run's end, never on the paint path).
    fn persona_on(&self) -> bool {
        let config = &self.runtime.config;
        let settings =
            config::settings::SettingsManager::create(&self.env.cwd, &config.agent_dir, config.project_trusted);
        resources::persona::enabled(settings.settings())
    }

    /// The run ended (`aborted`, `failed`): start the keep-going or nudge run when one is due.
    pub(super) fn persona_run_ended(&mut self, aborted: bool, failed: bool) {
        let pending = !lock(&self.queues).items.is_empty() || !self.deferred.is_empty() || !self.after_run.is_empty();
        let limits = self.persona_limits();
        let before = self.persona_session.clone();
        let next = keep_going::after_run(
            &mut self.persona_session,
            &RunEnd {
                aborted,
                failed,
                pending_input: pending,
                tool_calls: self.persona.tool_calls,
                text: &self.persona.text,
            },
            crate::lane::now_ms(),
            limits,
        );
        if self.persona_session != before {
            self.persona_save();
        }
        match next {
            Next::Persona => {}
            Next::Stop => {
                // An abort still ends the persona's own chain until the user's next input.
                self.persona.run_ended(false, aborted, failed, pending);
                return;
            }
            Next::Continue { .. } | Next::Final { .. } => {
                if matches!(next, Next::Final { .. }) {
                    // After the final report nothing continues until the user's next input.
                    self.persona.nudges = resources::persona::MAX_NUDGES;
                }
                if let Some(message) = keep_going::nudge_message(&next, limits, crate::lane::now_ms()) {
                    // A nudge that cannot start is dropped, as the persona's is.
                    let _ = self.prompt_with(message, None, true);
                }
                return;
            }
        }
        // Settings are read only when the turn would otherwise be nudged.
        let candidate = !aborted
            && !failed
            && !pending
            && resources::persona::should_nudge(&self.persona.text, self.persona.tool_calls);
        let persona = candidate && self.persona_effective();
        let Some(n) = self.persona.run_ended(persona, aborted, failed, pending) else {
            return;
        };
        let message = resources::persona::nudge_message(n, crate::lane::now_ms());
        // A nudge that cannot start (no key, a journal error) is dropped; the user's next input starts over.
        let _ = self.prompt_with(message, None, true);
    }

    /// What clients are shown for a message of the run: a nudge or fired schedule run's prompt (which reaches the
    /// loop as a user message) is shown as that custom message, so clients draw its one line instead of a user
    /// bubble.
    pub(super) fn persona_shown(&self, message: Value) -> Value {
        match self.run.as_ref() {
            Some(run)
                if run.prompt_pending
                    && message["role"] == "user"
                    && (run.prompt["customType"] == resources::persona::NUDGE_CUSTOM_TYPE
                        || run.prompt["customType"] == kernel_apis::prompt_schedule::CUSTOM_TYPE) =>
            {
                run.prompt.clone()
            }
            _ => message,
        }
    }

    /// A run starts: its calls and text are counted afresh.
    pub(super) fn persona_run_started(&mut self) {
        self.persona.run_started();
    }
}
