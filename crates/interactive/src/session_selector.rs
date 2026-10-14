//! Pi's session selector for `/resume` (`SessionSelectorComponent`, modes/interactive/components/session-selector.ts
//! and session-selector-search.ts): search (fuzzy tokens, `"phrase"`, `re:<pattern>`), sort (threaded, recent,
//! fuzzy relevance), the named filter, path display, rename and delete with confirmation.
//!
//! User requirements: it lists this working directory's sessions only (no all-projects scope, so no Tab scope
//! toggle or `◉ Current Folder | ○ All` header part), by name when a session has one. The selector owns no session:
//! it returns [`SessionAction`]s and the client switches, renames or deletes.

use std::rc::Rc;
use std::time::{Duration, Instant};

use tui::components::{Component, Text};
use tui::fuzzy::fuzzy_match;
use tui::input::{Input, InputEvent};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;
use tui::utils::{truncate_to_width, visible_width};

use crate::components::{bold, fg, key_hint, key_text};
use crate::lane::SessionListing;

/// `SortMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Threaded,
    Recent,
    Relevance,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    Selected(String),
    Cancelled,
    /// Delete the session at this path (confirmed).
    Delete(String),
    /// Rename the session at this path.
    Rename(String, String),
}

/// One parsed search token.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    Fuzzy(String),
    Phrase(String),
}

/// `parseSearchQuery`: `None` when the query cannot match (an empty or invalid `re:`).
fn parse_query(query: &str) -> Option<Result<Vec<Token>, fancy_regex::Regex>> {
    let trimmed = query.trim();
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return None;
        }
        return fancy_regex::Regex::new(&format!("(?i){pattern}")).ok().map(Err);
    }
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut quoted = false;
    let flush = |buf: &mut String, phrase: bool, tokens: &mut Vec<Token>| {
        let v = buf.trim().to_string();
        buf.clear();
        if !v.is_empty() {
            tokens.push(if phrase { Token::Phrase(v) } else { Token::Fuzzy(v) });
        }
    };
    for c in trimmed.chars() {
        if c == '"' {
            flush(&mut buf, quoted, &mut tokens);
            quoted = !quoted;
            continue;
        }
        if !quoted && c.is_whitespace() {
            flush(&mut buf, false, &mut tokens);
            continue;
        }
        buf.push(c);
    }
    if quoted {
        // Unbalanced quotes: plain whitespace tokens.
        return Some(Ok(trimmed
            .split_whitespace()
            .map(|t| Token::Fuzzy(t.to_string()))
            .collect()));
    }
    flush(&mut buf, false, &mut tokens);
    Some(Ok(tokens))
}

fn search_text(s: &SessionListing) -> String {
    format!(
        "{} {} {} {}",
        s.id,
        s.name.as_deref().unwrap_or(""),
        s.all_messages_text,
        s.cwd
    )
}

/// `matchSession`: whether it matches and its score (lower is better).
fn match_session(s: &SessionListing, parsed: &Result<Vec<Token>, fancy_regex::Regex>) -> Option<f64> {
    let text = search_text(s);
    match parsed {
        Err(regex) => {
            let found = regex.find(&text).ok().flatten()?;
            Some(text[..found.start()].encode_utf16().count() as f64 * 0.1)
        }
        Ok(tokens) => {
            let normalized = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
            let mut score = 0.0;
            for token in tokens {
                match token {
                    Token::Phrase(p) => {
                        let phrase = p.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
                        if phrase.is_empty() {
                            continue;
                        }
                        let at = normalized.find(&phrase)?;
                        score += normalized[..at].encode_utf16().count() as f64 * 0.1;
                    }
                    Token::Fuzzy(f) => {
                        let m = fuzzy_match(f, &text);
                        if !m.matches {
                            return None;
                        }
                        score += m.score;
                    }
                }
            }
            Some(score)
        }
    }
}

/// `filterAndSortSessions`.
fn filter_and_sort(sessions: &[SessionListing], query: &str, sort: SortMode) -> Vec<usize> {
    let all: Vec<usize> = (0..sessions.len()).collect();
    if query.trim().is_empty() {
        return all;
    }
    let Some(parsed) = parse_query(query) else {
        return Vec::new();
    };
    let mut scored: Vec<(usize, f64)> = all
        .into_iter()
        .filter_map(|i| match_session(&sessions[i], &parsed).map(|s| (i, s)))
        .collect();
    if sort != SortMode::Recent {
        scored.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(sessions[b.0].modified.cmp(&sessions[a.0].modified))
        });
    }
    scored.into_iter().map(|(i, _)| i).collect()
}

/// `formatSessionDate`: age as now, Nm, Nh, Nd, Nw, Nmo, Ny.
pub fn format_age(now_ms: i64, modified_ms: i64) -> String {
    let diff = now_ms - modified_ms;
    let mins = diff.div_euclid(60_000);
    let hours = diff.div_euclid(3_600_000);
    let days = diff.div_euclid(86_400_000);
    if mins < 1 {
        "now".into()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

pub struct SessionSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    home: String,
    sessions: Vec<SessionListing>,
    filtered: Vec<usize>,
    selected: usize,
    touched: bool,
    search: Input,
    sort: SortMode,
    named_only: bool,
    show_path: bool,
    confirming_delete: Option<String>,
    status: Option<(bool, String, Instant)>,
    current_path: Option<String>,
    rename: Option<(String, Input)>,
    max_visible: usize,
    now_ms: i64,
}

impl SessionSelector {
    /// `sessions`: newest first; `current_path`: the attached session's file (highlighted, never deleted).
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        sessions: Vec<SessionListing>,
        current_path: Option<String>,
        home: &str,
        now_ms: i64,
    ) -> Self {
        let mut search = Input::new(keys.clone());
        search.focused = true;
        let mut selector = SessionSelector {
            theme,
            keys,
            home: home.to_string(),
            sessions: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            touched: false,
            search,
            sort: SortMode::Threaded,
            named_only: false,
            show_path: false,
            confirming_delete: None,
            status: None,
            current_path,
            rename: None,
            max_visible: 10,
            now_ms,
        };
        selector.set_sessions(sessions);
        selector
    }

    /// `setSessions`: a fresh listing (after a delete or rename), keeping a touched selection.
    pub fn set_sessions(&mut self, mut sessions: Vec<SessionListing>) {
        sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
        let selected = self.touched.then(|| self.selected_path()).flatten();
        self.sessions = sessions;
        self.filter();
        if !self.touched {
            self.selected = 0;
        } else if let Some(path) = selected
            && let Some(i) = self.filtered.iter().position(|&i| self.sessions[i].path == path)
        {
            self.selected = i;
        }
    }

    /// A transient header message (`setStatusMessage`), hidden after `hide`.
    pub fn set_status(&mut self, error: bool, message: &str, hide: Duration) {
        self.status = Some((error, message.to_string(), Instant::now() + hide));
    }

    pub fn selected_path(&self) -> Option<String> {
        self.filtered.get(self.selected).map(|&i| self.sessions[i].path.clone())
    }

    fn filter(&mut self) {
        let query = self.search.value().to_string();
        let named: Vec<SessionListing> = self
            .sessions
            .iter()
            .filter(|s| !self.named_only || s.name.as_deref().is_some_and(|n| !n.trim().is_empty()))
            .cloned()
            .collect();
        let order = if self.sort == SortMode::Threaded && query.trim().is_empty() {
            // Native sessions carry no parent path: the threaded tree is the newest-first list.
            (0..named.len()).collect()
        } else {
            filter_and_sort(&named, &query, self.sort)
        };
        self.filtered = order
            .into_iter()
            .filter_map(|i| self.sessions.iter().position(|s| s.path == named[i].path))
            .collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn is_current(&self, path: &str) -> bool {
        self.current_path.as_deref() == Some(path)
    }

    fn shorten(&self, path: &str) -> String {
        match path.strip_prefix(&self.home) {
            Some(rest) if !self.home.is_empty() => format!("~{rest}"),
            _ => path.to_string(),
        }
    }

    pub fn handle_input(&mut self, data: &str) -> Option<SessionAction> {
        let k = |id: &str| self.keys.matches(data, id);
        if let Some((path, input)) = self.rename.as_mut() {
            if k("tui.select.cancel") {
                self.rename = None;
                return None;
            }
            if let Some(InputEvent::Submit(value)) = input.handle_input(data) {
                let next = value.trim().to_string();
                if next.is_empty() {
                    return None;
                }
                let path = path.clone();
                self.rename = None;
                return Some(SessionAction::Rename(path, next));
            }
            return None;
        }
        if let Some(path) = self.confirming_delete.clone() {
            if k("tui.select.confirm") {
                self.confirming_delete = None;
                return Some(SessionAction::Delete(path));
            }
            if k("tui.select.cancel") {
                self.confirming_delete = None;
            }
            return None;
        }
        if k("tui.input.tab") {
            // This directory's sessions only (no all-projects scope).
            return None;
        }
        if k("app.session.toggleSort") {
            self.sort = match self.sort {
                SortMode::Threaded => SortMode::Recent,
                SortMode::Recent => SortMode::Relevance,
                SortMode::Relevance => SortMode::Threaded,
            };
            self.filter();
            return None;
        }
        if k("app.session.toggleNamedFilter") {
            self.named_only = !self.named_only;
            self.filter();
            return None;
        }
        if k("app.session.togglePath") {
            self.show_path = !self.show_path;
            return None;
        }
        if k("app.session.delete") {
            self.start_delete();
            return None;
        }
        if k("app.session.rename") {
            if let Some(&i) = self.filtered.get(self.selected) {
                let session = &self.sessions[i];
                let mut input = Input::new(self.keys.clone());
                input.set_value(session.name.as_deref().unwrap_or(""));
                input.focused = true;
                self.rename = Some((session.path.clone(), input));
            }
            return None;
        }
        if k("app.session.deleteNoninvasive") {
            if self.search.value().is_empty() {
                self.start_delete();
            } else {
                self.search.handle_input(data);
                self.filter();
            }
            return None;
        }
        self.touched = true;
        let n = self.filtered.len();
        if k("tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if k("tui.select.down") {
            self.selected = (self.selected + 1).min(n.saturating_sub(1));
        } else if k("tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(self.max_visible);
        } else if k("tui.select.pageDown") {
            self.selected = (self.selected + self.max_visible).min(n.saturating_sub(1));
        } else if k("tui.select.confirm") {
            return self.selected_path().map(SessionAction::Selected);
        } else if k("tui.select.cancel") {
            return Some(SessionAction::Cancelled);
        } else {
            if let Some(InputEvent::Submit(_)) = self.search.handle_input(data) {
                return self.selected_path().map(SessionAction::Selected);
            }
            self.filter();
        }
        None
    }

    fn start_delete(&mut self) {
        let Some(path) = self.selected_path() else { return };
        if self.is_current(&path) {
            self.set_status(
                true,
                "Cannot delete the currently active session",
                Duration::from_secs(3),
            );
            return;
        }
        self.confirming_delete = Some(path);
    }

    fn header(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let left_text = bold("Resume Session (Current Folder)");
        let sort = match self.sort {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let name = if self.named_only { "Named" } else { "All" };
        let right = truncate_to_width(
            &format!(
                "{}{}  {}{}",
                fg(&t, "muted", "Name: "),
                fg(&t, "accent", name),
                fg(&t, "muted", "Sort: "),
                fg(&t, "accent", sort)
            ),
            width,
            "",
            false,
        );
        let left = truncate_to_width(&left_text, width.saturating_sub(visible_width(&right) + 1), "", false);
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right));
        let hint = |id: &str, d: &str| key_hint(&t, &key_text(&self.keys, id, false), d);
        if self
            .status
            .as_ref()
            .is_some_and(|(_, _, until)| Instant::now() >= *until)
        {
            self.status = None;
        }
        let (line1, line2) = if self.confirming_delete.is_some() {
            let confirm = format!(
                "Delete session? {} · {}",
                hint("tui.select.confirm", "confirm"),
                hint("tui.select.cancel", "cancel")
            );
            (
                fg(&t, "error", &truncate_to_width(&confirm, width, "…", false)),
                String::new(),
            )
        } else if let Some((error, message, _)) = &self.status {
            let color = if *error { "error" } else { "accent" };
            (
                fg(&t, color, &truncate_to_width(message, width, "…", false)),
                String::new(),
            )
        } else {
            let sep = fg(&t, "muted", " · ");
            let hint1 = fg(&t, "muted", "re:<pattern> regex · \"phrase\" exact");
            let path_state = if self.show_path { "(on)" } else { "(off)" };
            let hint2 = [
                hint("app.session.toggleSort", "sort"),
                hint("app.session.toggleNamedFilter", "named"),
                hint("app.session.delete", "delete"),
                hint("app.session.togglePath", &format!("path {path_state}")),
                hint("app.session.rename", "rename"),
            ]
            .join(&sep);
            (
                truncate_to_width(&hint1, width, "…", false),
                truncate_to_width(&hint2, width, "…", false),
            )
        };
        vec![format!("{left}{}{right}", " ".repeat(spacing)), line1, line2]
    }

    fn list(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = self.search.render(width);
        lines.push(String::new());
        if self.filtered.is_empty() {
            let message = if self.named_only {
                format!(
                    "  No named sessions in current folder. Press {} to show all.",
                    key_text(&self.keys, "app.session.toggleNamedFilter", false)
                )
            } else {
                "  No sessions in current folder.".to_string()
            };
            lines.push(fg(&t, "muted", &truncate_to_width(&message, width, "…", false)));
            return lines;
        }
        let n = self.filtered.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        let end = (start + self.max_visible).min(self.filtered.len());
        for row in start..end {
            let s = &self.sessions[self.filtered[row]];
            let selected = row == self.selected;
            let deleting = self.confirming_delete.as_deref() == Some(s.path.as_str());
            let current = self.is_current(&s.path);
            let display = s.name.clone().unwrap_or_else(|| {
                if s.first_message.is_empty() {
                    "(no messages)".to_string()
                } else {
                    s.first_message.clone()
                }
            });
            let normalized: String = display
                .chars()
                .map(|c| if (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c })
                .collect::<String>()
                .trim()
                .to_string();
            let mut right = format!("{} {}", s.message_count, format_age(self.now_ms, s.modified));
            if self.show_path {
                right = format!("{} {right}", self.shorten(&s.path));
            }
            let cursor = if selected {
                fg(&t, "accent", "› ")
            } else {
                "  ".to_string()
            };
            let available = width as isize - 2 - (visible_width(&right) + 2) as isize;
            let truncated = truncate_to_width(&normalized, available.max(10) as usize, "…", false);
            let color = if deleting {
                Some("error")
            } else if current {
                Some("accent")
            } else if s.name.is_some() {
                Some("warning")
            } else {
                None
            };
            let mut message = color.map_or(truncated.clone(), |c| fg(&t, c, &truncated));
            if selected {
                message = bold(&message);
            }
            let left = format!("{cursor}{}{message}", fg(&t, "dim", ""));
            let spacing = (width as isize - visible_width(&left) as isize - visible_width(&right) as isize).max(1);
            let styled_right = fg(&t, if deleting { "error" } else { "dim" }, &right);
            let mut line = format!("{left}{}{styled_right}", " ".repeat(spacing as usize));
            if selected {
                line = t.bg("selectedBg", &line).unwrap_or(line);
            }
            lines.push(truncate_to_width(&line, width, "...", false));
        }
        if start > 0 || end < self.filtered.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push(fg(&t, "muted", &truncate_to_width(&text, width, "", false)));
        }
        lines
    }
}

impl Component for SessionSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = fg(&t, "accent", &"─".repeat(width.max(1)));
        let mut lines = vec![String::new(), border.clone(), String::new()];
        if let Some((_, input)) = self.rename.as_mut() {
            lines.extend(Text::new(&bold("Rename Session"), 1, 0, None).render(width));
            lines.push(String::new());
            lines.extend(input.render(width));
            lines.push(String::new());
            let hint = format!(
                "{} to save · {} to cancel",
                key_text(&self.keys, "tui.select.confirm", false),
                key_text(&self.keys, "tui.select.cancel", false)
            );
            lines.extend(Text::new(&fg(&t, "muted", &hint), 1, 0, None).render(width));
        } else {
            lines.extend(self.header(width));
            lines.push(String::new());
            lines.extend(self.list(width));
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }
}
