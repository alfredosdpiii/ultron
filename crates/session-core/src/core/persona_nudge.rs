//! The persona's auto-nudge on `main` (resources::persona): a root run that ended normally by announcing its next
//! step, after at least one `rlm` call, is continued with a nudge message (a displayed custom message, journaled
//! like any run's prompt), at most [`resources::persona::MAX_NUDGES`] times since the user's last input. Only while
//! the settings' `persona` is `ultron`; a queued user message or other waiting input wins over the nudge.

use super::{Core, lock};
use serde_json::Value;

impl Core {
    /// The user sent input (a prompt or a queued message): the nudge count starts over.
    pub(super) fn persona_user_input(&mut self) {
        self.persona.user_input();
    }

    /// An assistant message of the run ended.
    pub(super) fn persona_assistant(&mut self, message: &Value) {
        if self.run.is_some() {
            self.persona.assistant_message(message);
        }
    }

    /// Whether the settings name the persona now (read at the run's end, never on the paint path).
    fn persona_on(&self) -> bool {
        let config = &self.runtime.config;
        let settings =
            config::settings::SettingsManager::create(&self.env.cwd, &config.agent_dir, config.project_trusted);
        resources::persona::enabled(settings.settings())
    }

    /// The run ended (`aborted`, `failed`): start the nudge run when one is due.
    pub(super) fn persona_run_ended(&mut self, aborted: bool, failed: bool) {
        let pending = !lock(&self.queues).items.is_empty() || !self.deferred.is_empty() || !self.after_run.is_empty();
        // Settings are read only when the turn would otherwise be nudged.
        let candidate = !aborted
            && !failed
            && !pending
            && resources::persona::should_nudge(&self.persona.text, self.persona.tool_calls);
        let persona = candidate && self.persona_on();
        let Some(n) = self.persona.run_ended(persona, aborted, failed, pending) else {
            return;
        };
        let message = resources::persona::nudge_message(n, crate::lane::now_ms());
        // A nudge that cannot start (no key, a journal error) is dropped; the user's next input starts over.
        let _ = self.prompt_with(message, None, true);
    }

    /// What clients are shown for a message of the run: a nudge run's prompt (which reaches the loop as a user
    /// message) is shown as the nudge, so clients draw its one line instead of a user bubble.
    pub(super) fn persona_shown(&self, message: Value) -> Value {
        match self.run.as_ref() {
            Some(run)
                if run.prompt_pending
                    && message["role"] == "user"
                    && run.prompt["customType"] == resources::persona::NUDGE_CUSTOM_TYPE =>
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
