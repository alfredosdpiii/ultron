//! interactive-mode.ts's startup notices: the "What's New" block for the changelog entries newer than the
//! `lastChangelogVersion` setting (`getChangelogForDisplay` / `showStartupNoticesIfNeeded`, one condensed line under
//! `collapseChangelog`), the "Update Available" block when the registry has a newer release
//! (`showNewVersionNotification`), and the Anthropic subscription-auth warning
//! (`maybeWarnAboutAnthropicSubscriptionAuth`, off with `warnings.anthropicExtraUsage: false`).

use std::rc::Rc;

use tui::components::{Component, Spacer, Text};
use tui::markdown::{Markdown, MarkdownOptions};
use tui::marked::markdown_tokens;
use tui::theme::Theme;

use crate::changelog::{Entry, normalize_links, parse};
use crate::components::{bold, dynamic_border, fg, markdown_theme};

/// `ANTHROPIC_SUBSCRIPTION_AUTH_WARNING`.
pub const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str = "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and is billed per token, not your Claude plan limits. Manage extra usage at https://claude.ai/settings/usage. Disable this warning in /settings.";

/// `RELEASES_URL`.
pub const RELEASES_URL: &str = "https://github.com/alfredosdpiii/ultron/releases";

/// The command the update notice names (`${APP_NAME} update`).
const UPDATE_COMMAND: &str = "ultron update";

/// What `getChangelogForDisplay` decided: the markdown to show, and the version to save as `lastChangelogVersion`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangelogDisplay {
    pub markdown: Option<String>,
    pub save: Option<String>,
}

/// `getChangelogForDisplay`: nothing for a session with messages; a first run (no `lastChangelogVersion`) records
/// `current` and shows nothing; otherwise the entries newer than `last` (in file order, links pinned) and `current`
/// is recorded.
pub fn changelog_for_display(
    content: Option<&str>,
    last: Option<&str>,
    current: &str,
    has_messages: bool,
) -> ChangelogDisplay {
    if has_messages {
        return ChangelogDisplay::default();
    }
    let Some(last) = last.filter(|l| !l.is_empty()) else {
        return ChangelogDisplay {
            markdown: None,
            save: Some(current.to_string()),
        };
    };
    let entries = content.map(parse).unwrap_or_default();
    let floor = loose_version(last);
    let new: Vec<&Entry> = entries.iter().filter(|e| e.version > floor).collect();
    if new.is_empty() {
        return ChangelogDisplay::default();
    }
    ChangelogDisplay {
        markdown: Some(
            new.iter()
                .map(|e| normalize_links(&e.content, e.version))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        save: Some(current.to_string()),
    }
}

/// `getNewEntries`'s `lastVersion.split(".").map(Number)` with `|| 0`: a part that is not a number counts as 0.
fn loose_version(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(js_number_part);
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// `Number(part) || 0` for a version part (whitespace-trimmed decimal; anything else is 0).
fn js_number_part(part: &str) -> u64 {
    let trimmed = part.trim();
    match trimmed.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => n as u64,
        _ => 0,
    }
}

/// `maybeWarnAboutAnthropicSubscriptionAuth`'s decision for the session's model: an `anthropic` model whose
/// credential is a subscription login (stored OAuth, or a key that is an OAuth token, `sk-ant-oat…`), unless the
/// `warnings.anthropicExtraUsage` setting is off.
pub fn warns_about_subscription_auth(
    provider: &str,
    enabled: bool,
    auth_type: Option<&str>,
    api_key: Option<&str>,
) -> bool {
    enabled
        && provider == "anthropic"
        && (auth_type == Some("oauth") || api_key.is_some_and(|key| key.starts_with("sk-ant-oat")))
}

/// `showWarning`'s text (the transcript adds its spacer).
pub fn warning_text(theme: &Theme, message: &str) -> String {
    fg(theme, "warning", &format!("Warning: {message}"))
}

/// `showStartupNoticesIfNeeded`'s block.
pub struct WhatsNewNotice {
    pub theme: Rc<Theme>,
    pub markdown: String,
    /// `collapseChangelog`: one line naming the newest version instead of the entries.
    pub collapse: bool,
    /// This build's version, named when the markdown has no version heading.
    pub version: String,
    /// `markdown.codeBlockIndent` (`None`: the render settings').
    pub code_block_indent: Option<String>,
    pub hyperlinks: bool,
}

impl Component for WhatsNewNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let border = |s: &str| fg(t, "border", s);
        let mut lines = dynamic_border(&border, width);
        if self.collapse {
            let latest = newest_version(&self.markdown).unwrap_or_else(|| self.version.clone());
            let text = format!(
                "Updated to v{latest}. Use {} to view full changelog.",
                bold("/changelog")
            );
            lines.extend(Text::new(&text, 1, 0, None).render(width));
        } else {
            lines.extend(Text::new(&bold(&fg(t, "accent", "What's New")), 1, 0, None).render(width));
            lines.extend(Spacer(1).render(width));
            let mut theme = markdown_theme(t);
            if self.code_block_indent.is_some() {
                theme.code_block_indent = self.code_block_indent.clone();
            }
            let mut md = Markdown {
                tokens: markdown_tokens(self.markdown.trim()),
                padding_x: 1,
                padding_y: 0,
                theme,
                style: None,
                options: MarkdownOptions {
                    hyperlinks: self.hyperlinks,
                    ..MarkdownOptions::default()
                },
            };
            lines.extend(md.render(width));
            lines.extend(Spacer(1).render(width));
        }
        lines.extend(dynamic_border(&border, width));
        lines
    }
}

/// `/##\s+\[?(\d+\.\d+\.\d+)\]?/`: the first version heading.
fn newest_version(markdown: &str) -> Option<String> {
    let re = fancy_regex::Regex::new(r"##\s+\[?(\d+\.\d+\.\d+)\]?").expect("version pattern");
    re.captures(markdown).ok().flatten().map(|caps| caps[1].to_string())
}

/// `showNewVersionNotification`'s block.
pub struct NewVersionNotice {
    pub theme: Rc<Theme>,
    pub version: String,
    /// `getCapabilities().hyperlinks`: the release notes address as an OSC 8 link.
    pub hyperlinks: bool,
}

impl Component for NewVersionNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let border = |s: &str| fg(t, "warning", s);
        let instruction = format!(
            "{}{}",
            fg(t, "muted", &format!("New version {} is available. Run ", self.version)),
            fg(t, "accent", UPDATE_COMMAND)
        );
        let url = format!("{RELEASES_URL}/tag/v{}", self.version);
        let link = if self.hyperlinks {
            tui::image::hyperlink(&fg(t, "accent", &url), &url)
        } else {
            fg(t, "accent", &url)
        };
        let notes = format!("{}{link}", fg(t, "muted", "Release notes: "));
        let mut lines = dynamic_border(&border, width);
        lines.extend(
            Text::new(
                &format!("{}\n{instruction}", bold(&fg(t, "warning", "Update Available"))),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.extend(Text::new(&notes, 1, 0, None).render(width));
        lines.extend(dynamic_border(&border, width));
        lines
    }
}
