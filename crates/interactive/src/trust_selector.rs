//! Pi's `/trust` (client-tui-pi-commands.ts `showTrustSelector`, modes/interactive/components/trust-selector.ts):
//! the project trust choices for the session's cwd with the saved decision and the current session's trust; a
//! choice is saved through the session worker (`setProjectTrust`) and applies on restart.

use std::rc::Rc;

use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

use crate::components::{bold, dynamic_border, fg, format_key_text, key_hint, key_text};

/// One trust choice (`ProjectTrustOption`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustChoice {
    pub label: String,
    pub trusted: bool,
    /// `(path, decision)`; `None` removes a saved decision.
    pub updates: Vec<(String, Option<bool>)>,
    /// The directory whose saved decision this choice is (`savedPath`).
    pub saved_path: Option<String>,
}

/// What `/trust` shows: the worker's `readSettings` (`cwd`, `savedTrust`, `projectTrusted`) and the choices for its
/// cwd (`getProjectTrustOptions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustView {
    pub cwd: String,
    pub choices: Vec<TrustChoice>,
    /// The saved decision that applies to `cwd` (it may be inherited from a parent directory).
    pub saved: Option<(String, bool)>,
    pub project_trusted: bool,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustAction {
    Selected {
        trusted: bool,
        updates: Vec<(String, Option<bool>)>,
    },
    Cancelled,
}

/// The status after a saved choice.
pub fn saved_status(trusted: bool) -> String {
    format!(
        "Saved trust decision: {}. Restart ultron for this to take effect.",
        if trusted { "trusted" } else { "untrusted" }
    )
}

/// `formatDecision`.
fn format_decision(trust_path: Option<&str>, decision: Option<&(String, bool)>) -> String {
    let Some((path, decided)) = decision else {
        return "none".into();
    };
    let label = if *decided { "trusted" } else { "untrusted" };
    match trust_path {
        Some(trust_path) if path != trust_path => format!("{label} (inherited from {path})"),
        _ => format!("{label} ({path})"),
    }
}

/// `TrustSelectorComponent`.
pub struct TrustSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    view: TrustView,
    selected: usize,
}

impl TrustSelector {
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, view: TrustView) -> Self {
        let mut selector = TrustSelector {
            theme,
            keys,
            view,
            selected: 0,
        };
        selector.selected = (0..selector.view.choices.len())
            .find(|i| selector.is_saved(&selector.view.choices[*i]))
            .unwrap_or(0);
        selector
    }

    fn is_saved(&self, choice: &TrustChoice) -> bool {
        match (&choice.saved_path, &self.view.saved) {
            (Some(path), Some((saved_path, decision))) => *decision == choice.trusted && saved_path == path,
            _ => false,
        }
    }

    pub fn handle_input(&mut self, data: &str) -> Option<TrustAction> {
        let last = self.view.choices.len().saturating_sub(1);
        if self.keys.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if self.keys.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(last);
        } else if self.keys.matches(data, "tui.select.confirm") || data == "\n" {
            let choice = self.view.choices.get(self.selected)?;
            return Some(TrustAction::Selected {
                trusted: choice.trusted,
                updates: choice.updates.clone(),
            });
        } else if self.keys.matches(data, "tui.select.cancel") {
            return Some(TrustAction::Cancelled);
        }
        None
    }
}

impl Component for TrustSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let text = |s: &str| Text::new(s, 1, 0, None).render(width);
        let border = |s: &str| fg(&t, "border", s);
        let mut lines = dynamic_border(&border, width);
        lines.push(String::new());
        lines.extend(text(&fg(&t, "accent", &bold("Project trust"))));
        lines.extend(text(&fg(&t, "muted", &self.view.cwd)));
        lines.push(String::new());
        let trust_path = self.view.choices.first().and_then(|c| c.saved_path.as_deref());
        let decision = format_decision(trust_path, self.view.saved.as_ref());
        lines.extend(text(&fg(&t, "muted", &format!("Saved decision: {decision}"))));
        let current = if self.view.project_trusted {
            "trusted"
        } else {
            "untrusted"
        };
        lines.extend(text(&fg(&t, "muted", &format!("Current session: {current}"))));
        lines.push(String::new());
        for (i, choice) in self.view.choices.iter().enumerate() {
            let selected = i == self.selected;
            let marker = if self.is_saved(choice) {
                fg(&t, "accent", "✓ ")
            } else {
                "  ".into()
            };
            let prefix = if selected {
                fg(&t, "accent", "→ ")
            } else {
                "  ".into()
            };
            let label = fg(&t, if selected { "accent" } else { "text" }, &choice.label);
            lines.extend(text(&format!("{prefix}{marker}{label}")));
        }
        lines.push(String::new());
        let hints = format!(
            "{}  {}  {}",
            key_hint(&t, &format_key_text("↑↓", false, false), "navigate"),
            key_hint(&t, &key_text(&self.keys, "tui.select.confirm", false), "save"),
            key_hint(&t, &key_text(&self.keys, "tui.select.cancel", false), "cancel"),
        );
        lines.extend(text(&hints));
        lines.push(String::new());
        lines.extend(dynamic_border(&border, width));
        lines
    }
}
