//! The startup package update notice (interactive-mode.ts `showPackageUpdateNotification`): after the client's
//! `checkForAvailableUpdates` finds configured packages behind their source, the transcript shows them between two
//! warning rules with the command that updates them.

use std::rc::Rc;

use crate::components::{bold, dynamic_border, fg};
use tui::components::{Component, Text};
use tui::theme::Theme;

/// The command the notice names (`${APP_NAME} update --extensions`).
const UPDATE_COMMAND: &str = "ultron update --extensions";

pub struct PackageUpdateNotice {
    pub theme: Rc<Theme>,
    /// Display names: an npm package's name, or `host/path` of a git source.
    pub packages: Vec<String>,
}

impl Component for PackageUpdateNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let border = |s: &str| fg(t, "warning", s);
        let instruction = format!(
            "{}{}",
            fg(t, "muted", "Package updates are available. Run "),
            fg(t, "accent", UPDATE_COMMAND)
        );
        let packages = self
            .packages
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!(
            "{}\n{instruction}\n{}\n{packages}",
            bold(&fg(t, "warning", "Package Updates Available")),
            fg(t, "muted", "Packages:")
        );
        let mut lines = dynamic_border(&border, width);
        lines.extend(Text::new(&text, 1, 0, None).render(width));
        lines.extend(dynamic_border(&border, width));
        lines
    }
}
