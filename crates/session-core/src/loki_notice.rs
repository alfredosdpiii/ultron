//! Loki's one-time setup note (session-worker.ts `installLokiNotice` over loki.ts `setupLoki`'s `notice`): when the
//! session opens, a Git repository without `.loki/` gets Loki's minimal install where that is allowed
//! (loki::auto_setup), and `.loki/` is committed in the background. The note of what happened (created and
//! committed, or why not) opens the first root turn that follows it, as a displayed `loki` custom message; a commit
//! still running is waited for briefly, otherwise the note opens a later turn.

use serde_json::{Value, json};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long a turn waits for a commit still running before it starts without the note.
pub const NOTICE_WAIT: Duration = Duration::from_secs(5);

/// The custom message type of the note.
pub const LOKI_MESSAGE_TYPE: &str = "loki";

#[derive(Default)]
struct State {
    /// The note once known (`None` inside: there is nothing to say).
    known: Option<Option<String>>,
    /// The note was shown, or it is known there is none.
    shown: bool,
}

/// The session's note: unknown while `.loki/` is being committed, then shown once.
#[derive(Clone)]
pub struct LokiNotice {
    state: Arc<(Mutex<State>, Condvar)>,
}

/// No note.
impl Default for LokiNotice {
    fn default() -> Self {
        LokiNotice::known(None)
    }
}

impl LokiNotice {
    /// A note not known yet.
    fn pending() -> LokiNotice {
        LokiNotice { state: Arc::default() }
    }

    /// A note that is already known.
    pub fn known(text: Option<String>) -> LokiNotice {
        let notice = LokiNotice::pending();
        notice.settle(text);
        notice
    }

    fn settle(&self, text: Option<String>) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state.shown = state.shown || text.is_none();
        state.known = Some(text);
        changed.notify_all();
    }

    /// `setupLoki` for the session `config` describes (nothing when Loki is off): the install runs now, the commit
    /// on a thread of its own.
    pub fn start(config: &crate::compose::Config) -> LokiNotice {
        if config.loki == host::loki_guard::LokiMode::Off {
            return LokiNotice::known(None);
        }
        let settings = config::settings::SettingsManager::for_worker(&config.cwd, &config.agent_dir, &config.home);
        let settings = Value::Object(settings.loki_settings());
        let (cwd, home, var) = (config.cwd.clone(), config.home.clone(), config.var.clone());
        let lookup = move |key: &str| var(key);
        let context = loki::auto_setup::SetupContext {
            cwd: &cwd,
            home: &home,
            var: &lookup,
            settings: &settings,
        };
        match loki::auto_setup::setup(&context) {
            loki::auto_setup::Setup::Quiet => LokiNotice::known(None),
            loki::auto_setup::Setup::Notice(text) => LokiNotice::known(Some(text)),
            loki::auto_setup::Setup::Commit(root) => {
                let notice = LokiNotice::pending();
                let committing = notice.clone();
                // The commit's hooks may rewrite what it commits (a formatter on .loki/loki.json): that is not a
                // change of the cell running beside it. After-cell checks wait for the commit from now. Only .loki/
                // is pictured, so what the cell writes elsewhere meanwhile stays the cell's.
                let outside = config.outside_changes.begin();
                std::thread::spawn(move || {
                    let context = loki::auto_setup::SetupContext {
                        cwd: &cwd,
                        home: &home,
                        var: &lookup,
                        settings: &settings,
                    };
                    let snapshot = crate::compose::workspace_snapshotter();
                    let text = outside.record(&root.join(".loki"), &snapshot, || {
                        loki::auto_setup::commit_notice(&context, &root)
                    });
                    committing.settle(Some(text));
                });
                notice
            }
        }
    }

    /// `before_run` of a root turn: the note when it is due (waiting up to `wait` for a commit still running),
    /// once.
    pub fn take(&self, wait: Duration) -> Option<String> {
        let (state, changed) = &*self.state;
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        let end = Instant::now() + wait;
        while state.known.is_none() && !state.shown {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        if state.shown {
            return None;
        }
        state.shown = true;
        state.known.clone().flatten()
    }
}

/// The note as the transcript message that opens the turn.
pub fn notice_message(text: &str, timestamp: u64) -> Value {
    json!({
        "role": "custom",
        "customType": LOKI_MESSAGE_TYPE,
        "content": text,
        "display": true,
        "details": {},
        "timestamp": timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `installLokiNotice`: a note not known yet leaves the turn without it and opens a later one; it is shown once;
    /// a known absence is never waited for.
    #[test]
    fn session_core_loki_notice_waits_briefly_then_opens_a_later_turn_once() {
        let notice = LokiNotice::pending();
        assert_eq!(notice.take(Duration::ZERO), None);
        let committing = notice.clone();
        let commit = std::thread::spawn(move || committing.settle(Some("committed".into())));
        // Waits for the commit (the hang guard is the product's 5 s; the settle is immediate).
        assert_eq!(notice.take(Duration::from_secs(120)).as_deref(), Some("committed"));
        commit.join().unwrap();
        assert_eq!(notice.take(Duration::from_secs(120)), None);
        let none = LokiNotice::known(None);
        let started = Instant::now();
        assert_eq!(none.take(Duration::from_secs(120)), None);
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "a known absence is not waited for"
        );
        assert_eq!(LokiNotice::default().take(Duration::from_secs(120)), None);
    }
}
