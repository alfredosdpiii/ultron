//! Reverse incremental search over prompt history (port of experimental/prompt-history-search.ts, Alt+R by default).
//!
//! Typing narrows the matches (every term must occur, case-insensitive), most recent first. Up/Down or the search
//! key again move between matches (`app.historySearch.older` / `.newer`, Ctrl+R / Ctrl+S, also step), Enter or Tab
//! puts the selected entry in the editor, and Esc or Ctrl+C cancels and restores the draft.

use std::rc::Rc;

use tui::components::Component;
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;
use tui::utils::{truncate_to_width, visible_width};

use crate::components::{bold, fg};

const MAX_VISIBLE_MATCHES: usize = 8;

/// `searchPromptHistory`: entries containing every whitespace-separated term, case-insensitive, in order.
pub fn search_prompt_history(history: &[String], query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    let terms: Vec<&str> = lower.split_whitespace().collect();
    history
        .iter()
        .filter(|entry| {
            let entry = entry.to_lowercase();
            terms.iter().all(|t| entry.contains(t))
        })
        .cloned()
        .collect()
}

/// What the owner should do after a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistorySearchEvent {
    None,
    /// Replace the editor text with this entry.
    Accept(String),
    /// Close and keep the draft.
    Cancel,
}

pub struct PromptHistorySearch {
    history: Vec<String>,
    input: Input,
    matches: Vec<String>,
    selected: usize,
    theme: Rc<Theme>,
    keys: KeybindingsManager,
}

impl PromptHistorySearch {
    pub fn new(history: Vec<String>, theme: Rc<Theme>, keys: KeybindingsManager, query: &str) -> Self {
        let mut input = Input::new(keys.clone()).with_options(Some(""), None, None, None);
        input.set_value(query);
        let matches = search_prompt_history(&history, query);
        PromptHistorySearch {
            history,
            input,
            matches,
            selected: 0,
            theme,
            keys,
        }
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.input.focused = focused;
    }

    pub fn query(&self) -> &str {
        self.input.value()
    }

    pub fn matches(&self) -> &[String] {
        &self.matches
    }

    pub fn selected(&self) -> Option<&str> {
        self.matches.get(self.selected).map(String::as_str)
    }

    pub fn handle_input(&mut self, data: &str) -> HistorySearchEvent {
        let k = &self.keys;
        if k.matches(data, "tui.select.cancel") {
            return HistorySearchEvent::Cancel;
        }
        if k.matches(data, "tui.select.confirm") || k.matches(data, "tui.input.tab") {
            return match self.selected() {
                Some(s) => HistorySearchEvent::Accept(s.to_string()),
                None => HistorySearchEvent::Cancel,
            };
        }
        if k.matches(data, "app.history.search")
            || k.matches(data, "app.historySearch.older")
            || k.matches(data, "tui.select.up")
        {
            self.step(1);
            return HistorySearchEvent::None;
        }
        if k.matches(data, "app.historySearch.newer") || k.matches(data, "tui.select.down") {
            self.step(-1);
            return HistorySearchEvent::None;
        }
        let before = self.input.value().to_string();
        self.input.handle_input(data);
        if self.input.value() != before {
            self.matches = search_prompt_history(&self.history, self.input.value());
            self.selected = 0;
        }
        HistorySearchEvent::None
    }

    fn step(&mut self, delta: i64) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected as i64 + delta).clamp(0, self.matches.len() as i64 - 1) as usize;
    }

    /// `highlight`: matched terms bold in `warning`, the selected row's other text in `accent`.
    fn highlight(&self, text: &str, terms: &[String], selected: bool) -> String {
        let t = &self.theme;
        let base = |s: &str| if selected { fg(t, "accent", s) } else { s.to_string() };
        if terms.is_empty() {
            return base(text);
        }
        let chars: Vec<char> = text.chars().collect();
        let lower: Vec<char> = chars
            .iter()
            .map(|c| {
                let mut l = c.to_lowercase();
                match (l.next(), l.next()) {
                    (Some(one), None) => one,
                    _ => *c,
                }
            })
            .collect();
        let mut marks = vec![false; chars.len()];
        for term in terms {
            let term: Vec<char> = term.chars().collect();
            if term.is_empty() || term.len() > lower.len() {
                continue;
            }
            let mut from = 0;
            while from + term.len() <= lower.len() {
                if lower[from..from + term.len()] == term[..] {
                    marks[from..from + term.len()].fill(true);
                    from += term.len();
                } else {
                    from += 1;
                }
            }
        }
        let mut out = String::new();
        let mut run = String::new();
        let mut run_marked = marks.first().copied().unwrap_or(false);
        let flush = |run: &mut String, marked: bool, out: &mut String| {
            if run.is_empty() {
                return;
            }
            out.push_str(&if marked {
                bold(&fg(t, "warning", run))
            } else {
                base(run)
            });
            run.clear();
        };
        for (i, c) in chars.iter().enumerate() {
            if marks[i] != run_marked {
                flush(&mut run, run_marked, &mut out);
                run_marked = marks[i];
            }
            run.push(*c);
        }
        flush(&mut run, run_marked, &mut out);
        out
    }
}

impl Component for PromptHistorySearch {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let inner = width.saturating_sub(2).max(1);
        let label = fg(&t, "accent", "history search: ");
        let input_lines = self.input.render(inner.saturating_sub(visible_width(&label)).max(1));
        let count = if self.matches.is_empty() {
            fg(&t, "warning", "no match")
        } else {
            fg(&t, "dim", &format!("{}/{}", self.selected + 1, self.matches.len()))
        };
        let mut lines = vec![
            fg(&t, "border", &"─".repeat(width)),
            format!(" {label}{}", input_lines.first().cloned().unwrap_or_default()),
            format!(" {count}"),
        ];
        let start = self
            .selected
            .saturating_sub(MAX_VISIBLE_MATCHES / 2)
            .min(self.matches.len().saturating_sub(MAX_VISIBLE_MATCHES));
        let lower = self.query().to_lowercase();
        let terms: Vec<String> = lower.split_whitespace().map(str::to_string).collect();
        let end = self.matches.len().min(start + MAX_VISIBLE_MATCHES);
        for index in start..end {
            let one_line = collapse_whitespace_runs(&self.matches[index]);
            let selected = index == self.selected;
            let prefix = if selected {
                fg(&t, "accent", "→ ")
            } else {
                "  ".into()
            };
            let body = truncate_to_width(
                &self.highlight(&one_line, &terms, selected),
                inner.saturating_sub(2).max(1),
                "…",
                false,
            );
            lines.push(format!(" {prefix}{body}"));
        }
        let hint = fg(&t, "dim", "↑↓ move · enter/tab use · esc cancel");
        lines.push(format!(" {}", truncate_to_width(&hint, inner, "", false)));
        lines.push(fg(&t, "border", &"─".repeat(width)));
        lines
    }
}

/// `replace(/\s+/gu, " ")`: every whitespace run becomes one space (edges included).
fn collapse_whitespace_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if tui::utils::is_whitespace_char(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}
