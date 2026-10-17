//! Keybinding ids of the omp-inspired overlays (TUI QoL batch 2): retry, display reset, the agent hub, the
//! `/copy` and Esc-Esc rewind pickers, the model hub, prompt-history search and the transcript browser.
//!
//! `app_keybindings` stays a byte-for-byte port of Ultron's TypeScript `KEYBINDINGS` (its fixture test compares
//! the whole list), so these ids live here and the composition root resolves [`native_app_keybindings`]. Every
//! overlay matches keys through a [`crate::keybindings::KeybindingsManager`] built from it, never with literal key
//! checks, so a user override in `<agentDir>/keybindings.json` (loaded by the composition root through
//! [`crate::keybindings_file`], and again on `/reload`) reaches the overlays too.
//!
//! Global ids reuse omp's names and defaults (oh-my-pi packages/tui/src/app-keybindings.ts) except where a key is
//! already Ultron's: omp binds `app.retry` to `["f5", "alt+r"]`, but `alt+r` is Ultron's `app.history.search`
//! (omp searches history with `ctrl+r`, which Ultron gives to `app.rlm.toggle`). Retry keeps omp's leading key,
//! F5, which every terminal delivers verbatim.

use std::time::{Duration, Instant};

use crate::keybindings::{KeybindingDefinition, Platform, app_keybindings};

fn def(id: &'static str, keys: &[&str], description: &'static str) -> KeybindingDefinition {
    KeybindingDefinition {
        id,
        default_keys: keys.iter().map(|k| (*k).to_string()).collect(),
        description,
    }
}

/// The batch-2 bindings, in display order.
pub fn qol_keybindings() -> Vec<KeybindingDefinition> {
    vec![
        def("app.retry", &["f5"], "Retry last failed assistant turn"),
        def("app.display.reset", &["alt+l"], "Reset terminal display"),
        def("app.agents.hub", &["alt+a"], "Open the agent hub"),
        // Fullscreen transcript browsing shared by /copy, rewind and the agent transcript viewer.
        def("app.transcript.scrollUp", &["shift+up"], "Transcript: scroll up fast"),
        def(
            "app.transcript.scrollDown",
            &["shift+down"],
            "Transcript: scroll down fast",
        ),
        // /copy picker.
        def(
            "app.copy.blocks",
            &["right"],
            "Copy picker: step into the turn's blocks",
        ),
        def("app.copy.back", &["left"], "Copy picker: back to the transcript"),
        def(
            "app.copy.open",
            &["o", "shift+o"],
            "Copy picker: open the selected link",
        ),
        // Esc-Esc rewind selector.
        def("app.rewind.prevUser", &["left"], "Rewind: previous user turn"),
        def("app.rewind.nextUser", &["right"], "Rewind: next user turn"),
        def("app.rewind.filter", &["f"], "Rewind: filter turns by words"),
        // Agent hub table and transcript viewer.
        def("app.agentHub.up", &["k"], "Agent hub: previous agent"),
        def("app.agentHub.down", &["j"], "Agent hub: next agent"),
        def("app.agentHub.filter", &["/"], "Agent hub: filter agents"),
        def(
            "app.agentHub.tree",
            &["t"],
            "Agent hub: flat roster or grouped by parent",
        ),
        def("app.agentHub.revive", &["r"], "Agent hub: revive a parked agent"),
        def("app.agentHub.kill", &["x"], "Agent hub: abort and release an agent"),
        def(
            "app.agentHub.close",
            &["escape"],
            "Agent hub: close (or leave the transcript)",
        ),
        def("app.agentHub.top", &["g"], "Agent transcript: scroll to the top"),
        def(
            "app.agentHub.bottom",
            &["shift+g"],
            "Agent transcript: scroll to the bottom",
        ),
        // Model hub.
        def(
            "app.modelHub.focus",
            &["tab"],
            "Model hub: move focus between scopes and models",
        ),
        def("app.modelHub.scopes", &["left"], "Model hub: focus the scopes sidebar"),
        def("app.modelHub.models", &["right"], "Model hub: focus the model list"),
        // Prompt-history search (prompt-history-search.ts: the search key and Ctrl+R step older, Ctrl+S newer).
        def("app.historySearch.older", &["ctrl+r"], "History search: older match"),
        def("app.historySearch.newer", &["ctrl+s"], "History search: newer match"),
    ]
}

/// What the native TUI resolves: the TypeScript `KEYBINDINGS` followed by [`qol_keybindings`].
pub fn native_app_keybindings(p: Platform) -> Vec<KeybindingDefinition> {
    let mut defs = app_keybindings(p);
    defs.extend(qol_keybindings());
    defs
}

/// omp's Esc-Esc detector: a second tap within the window fires once (a third tap starts over).
#[derive(Clone, Copy, Debug)]
pub struct DoubleTap {
    window: Duration,
    last: Option<Instant>,
}

/// omp's `doubleEscapeAction` window.
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

impl Default for DoubleTap {
    fn default() -> Self {
        Self::new(DOUBLE_TAP_WINDOW)
    }
}

impl DoubleTap {
    pub fn new(window: Duration) -> Self {
        DoubleTap { window, last: None }
    }

    /// Record a tap at `now`; true when it completes a double tap.
    pub fn tap(&mut self, now: Instant) -> bool {
        match self.last {
            Some(prev) if now.saturating_duration_since(prev) <= self.window => {
                self.last = None;
                true
            }
            _ => {
                self.last = Some(now);
                false
            }
        }
    }

    /// Any other key breaks the sequence.
    pub fn reset(&mut self) {
        self.last = None;
    }
}
