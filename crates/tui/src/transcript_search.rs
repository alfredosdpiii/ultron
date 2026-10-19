//! Transcript search (port of packages/tui/src/alt-screen-search.ts and the selection/highlight logic of
//! tui-alt-screen.ts), applied to the main-screen scrollback/viewport split.
//!
//! The rendered rows are flattened into one corpus where every whitespace run is a single space, so a phrase
//! matches across wrapped rows; each match maps back to per-row column segments. [`TranscriptSearch`] is the
//! fullscreen view: it pages the whole transcript (retired scrollback rows plus the live viewport rows,
//! [`crate::viewport::Transcript::all_rows`]) with every visible match underlined, the current one inverted, and
//! the TS search bar at the bottom. Rows are rendered once when the search opens, never on the paint path.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;

use crate::components::Component;
use crate::input::Input;
use crate::keybindings::KeybindingsManager;
use crate::outline::ScrollBody;
use crate::sgr_mouse::parse_sgr_mouse;
use crate::utils::{
    ansi_len, is_whitespace_char, slice_with_width, strip_terminal_sequences, truncate_to_width, visible_width,
};

#[derive(Clone, Copy, Debug)]
struct Span {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
    linear: bool,
}

struct Corpus {
    text: String,
    spans: Vec<Span>,
}

/// Columns `[start_col, end_col)` of one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// One match: its segments in row order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub segments: Vec<SearchSegment>,
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

struct CorpusBuilder {
    corpus: Corpus,
    pending_separator: bool,
}

impl CorpusBuilder {
    fn space(&mut self) {
        if !self.corpus.text.is_empty() {
            self.pending_separator = true;
        }
    }

    fn push(&mut self, piece: &str, row: usize, start_col: usize, width: usize, linear: bool) {
        let text = &mut self.corpus.text;
        if self.pending_separator {
            text.push(' ');
            self.pending_separator = false;
        }
        let text_start = text.len();
        text.push_str(piece);
        self.corpus.spans.push(Span {
            text_start,
            text_end: text.len(),
            row,
            start_col,
            end_col: start_col + width,
            linear,
        });
    }
}

fn build_corpus(lines: &[String]) -> Corpus {
    let mut b = CorpusBuilder {
        corpus: Corpus {
            text: String::new(),
            spans: Vec::new(),
        },
        pending_separator: false,
    };
    for (row, line) in lines.iter().enumerate() {
        let line = strip_terminal_sequences(line);
        let mut column = 0;
        if is_printable_ascii(&line) {
            for (i, word) in line.split(' ').enumerate() {
                if i > 0 {
                    b.space();
                    column += 1;
                }
                if !word.is_empty() {
                    b.push(word, row, column, word.len(), true);
                    column += word.len();
                }
            }
        } else {
            for g in line.graphemes(true) {
                let width = visible_width(g);
                if g.chars().all(is_whitespace_char) {
                    b.space();
                } else {
                    b.push(g, row, column, width, false);
                }
                column += width;
            }
        }
        b.space();
    }
    b.corpus
}

/// `normalizeQuery`: whitespace runs collapse to one space, trimmed.
pub fn normalize_query(query: &str) -> String {
    query
        .split(is_whitespace_char)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_matches(corpus: &Corpus, query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(re) = fancy_regex::Regex::new(&format!("(?i){}", fancy_regex::escape(query))) else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    let mut span_index = 0;
    for found in re.find_iter(&corpus.text).flatten() {
        let (start, end) = (found.start(), found.end());
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }
        let mut segments: Vec<SearchSegment> = Vec::new();
        for span in &corpus.spans[span_index..] {
            if span.text_start >= end {
                break;
            }
            if span.text_end <= start {
                continue;
            }
            let (start_col, end_col) = if span.linear {
                (
                    span.start_col + start.max(span.text_start) - span.text_start,
                    span.start_col + end.min(span.text_end) - span.text_start,
                )
            } else {
                (span.start_col, span.end_col)
            };
            match segments.last_mut() {
                Some(prev) if prev.row == span.row && start_col <= prev.end_col => {
                    prev.end_col = prev.end_col.max(end_col);
                }
                _ => segments.push(SearchSegment {
                    row: span.row,
                    start_col,
                    end_col,
                }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segments.is_empty() {
            matches.push(SearchMatch { segments });
        }
    }
    matches
}

/// `getAltScreenSearchMatchKey`.
pub fn match_key(m: &SearchMatch) -> String {
    match (m.segments.first(), m.segments.last()) {
        (Some(f), Some(l)) => format!("{}:{}:{}:{}", f.row, f.start_col, l.row, l.end_col),
        _ => String::new(),
    }
}

/// `AltScreenSearchIndex`: the corpus and matches stay cached while the rows and query are unchanged.
#[derive(Default)]
pub struct SearchIndex {
    lines: Option<Vec<String>>,
    corpus: Option<Corpus>,
    query: Option<String>,
    matches: Vec<SearchMatch>,
}

impl SearchIndex {
    /// The matches, and whether they may have changed.
    pub fn search(&mut self, lines: &[String], query: &str) -> (&[SearchMatch], bool) {
        let source_changed = self.lines.as_deref() != Some(lines);
        if source_changed || self.corpus.is_none() {
            self.lines = Some(lines.to_vec());
            self.corpus = Some(build_corpus(lines));
        }
        let query = normalize_query(query);
        let changed = source_changed || self.query.as_deref() != Some(&query);
        if changed {
            self.matches = self
                .corpus
                .as_ref()
                .map(|c| find_matches(c, &query))
                .unwrap_or_default();
            self.query = Some(query);
        }
        (&self.matches, changed)
    }
}

/// How the next refresh picks the current match (`SearchSelectionMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// The query changed: the first match at or below the anchor row.
    Query,
    /// Keep the current match.
    Retain,
    Next,
    Previous,
}

/// tui-alt-screen.ts `refreshSearch` selection: the new current match index (`None` without matches).
pub fn select_match(
    matches: &[SearchMatch],
    mode: SelectionMode,
    changed: bool,
    anchor_row: usize,
    selected: Option<usize>,
    selected_key: Option<&str>,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let exact = if changed {
        selected_key.and_then(|key| matches.iter().position(|m| match_key(m) == key))
    } else {
        selected
    };
    let base = || exact.or(selected.map(|s| s.min(matches.len() - 1)));
    Some(match mode {
        SelectionMode::Query => {
            let low = matches.partition_point(|m| m.segments.first().map_or(0, |s| s.row) < anchor_row);
            if low < matches.len() { low } else { 0 }
        }
        SelectionMode::Next => base().map_or(0, |b| (b + 1) % matches.len()),
        SelectionMode::Previous => base().map_or(matches.len() - 1, |b| (b + matches.len() - 1) % matches.len()),
        SelectionMode::Retain => exact.unwrap_or_else(|| selected.unwrap_or(0).min(matches.len() - 1)),
    })
}

/// `applySearchTextHighlight`: style the plain runs of `text`, keeping its escape codes between them.
pub fn style_plain_runs(text: &str, style: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut plain_start = 0;
    let mut i = 0;
    while i < text.len() {
        match ansi_len(text, i) {
            Some(len) => {
                if i > plain_start {
                    out.push_str(&style(&text[plain_start..i]));
                }
                out.push_str(&text[i..i + len]);
                i += len;
                plain_start = i;
            }
            None => i += text[i..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if plain_start < text.len() {
        out.push_str(&style(&text[plain_start..]));
    }
    out
}

/// `applySearchHighlights` for one row: `ranges` are `(start_col, end_col, current)`.
pub fn highlight_row(
    line: &str,
    ranges: &[(usize, usize, bool)],
    match_style: &dyn Fn(&str) -> String,
    current_style: &dyn Fn(&str) -> String,
) -> String {
    let mut line = line.to_string();
    let width = visible_width(&line);
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (start, end, current) in sorted {
        let (start, end) = (start.min(width), end.min(width));
        if end <= start {
            continue;
        }
        let before = slice_with_width(&line, 0, start, true).0;
        let mid = slice_with_width(&line, start, end - start, true).0;
        let after = slice_with_width(&line, end, width.saturating_sub(end), true).0;
        let style = if current { current_style } else { match_style };
        line = format!("{before}{}{after}", style_plain_runs(&mid, style));
    }
    line
}

/// Styles a navigation button: `(text, hovered)`.
pub type ButtonStyle = Box<dyn Fn(&str, bool) -> String>;

/// `AltScreenSearchComponent`: the three-row search bar with result count and ↑/↓ buttons.
pub struct SearchBar {
    input: Input,
    result: Option<(usize, usize)>,
    count: usize,
    previous_key: String,
    next_key: String,
    button_style: ButtonStyle,
    hovered: Option<i8>,
    buttons: Option<[(usize, usize); 2]>,
}

/// `formatKey` of the search bar: `ctrl+g` -> `Ctrl+G`, `alt` is `Option` on macOS.
pub fn format_bar_key(key: Option<&str>, darwin: bool) -> String {
    let Some(key) = key else {
        return "Unbound".into();
    };
    key.split('+')
        .map(|part| {
            if darwin && part.eq_ignore_ascii_case("alt") {
                return "Option".to_string();
            }
            let mut chars = part.chars();
            chars
                .next()
                .map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl SearchBar {
    pub fn new(keys: &KeybindingsManager, darwin: bool, button_style: ButtonStyle) -> Self {
        let first = |id: &str| keys.get_keys(id).into_iter().next();
        let input = Input::new(keys.clone()).with_options(
            Some(" "),
            Some("Find in transcript"),
            Some(Box::new(|t| format!("\x1b[2m{t}\x1b[22m"))),
            None,
        );
        SearchBar {
            input,
            result: None,
            count: 0,
            previous_key: format_bar_key(first("tui.altScreen.searchPrevious").as_deref(), darwin),
            next_key: format_bar_key(first("tui.altScreen.searchNext").as_deref(), darwin),
            button_style,
            hovered: None,
            buttons: None,
        }
    }

    pub fn query(&self) -> &str {
        self.input.value()
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.input.focused = focused;
    }

    /// `setResult(index, count)`; `None` index is TS `-1`.
    pub fn set_result(&mut self, index: Option<usize>, count: usize) {
        self.result = index.map(|i| (i, count));
        self.count = count;
    }

    /// Edit the query; true when it changed.
    pub fn handle_input(&mut self, data: &str) -> bool {
        let before = self.input.value().to_string();
        self.input.handle_input(data);
        self.input.value() != before
    }

    /// `getNavigationDirectionAt(row, column)` in bar-local cells.
    pub fn navigation_at(&self, row: usize, col: usize) -> Option<i8> {
        let [prev, next] = self.buttons?;
        if row != 2 {
            return None;
        }
        if col >= prev.0 && col < prev.1 {
            return Some(-1);
        }
        if col >= next.0 && col < next.1 {
            return Some(1);
        }
        None
    }

    /// `setHoveredNavigationDirection`; true when it changed.
    pub fn set_hovered(&mut self, direction: Option<i8>) -> bool {
        let changed = self.hovered != direction;
        self.hovered = direction;
        changed
    }
}

impl Component for SearchBar {
    fn render(&mut self, width: usize) -> Vec<String> {
        let safe = width.max(1);
        let inner = safe.saturating_sub(2);
        let query = self.input.value();
        let result = if query.is_empty() {
            String::new()
        } else if self.count == 0 {
            "No matches".to_string()
        } else {
            let index = self.result.map_or(0, |(i, _)| i + 1);
            format!("{index}/{}", self.count)
        };
        let shown = truncate_to_width(&result, inner.saturating_sub(3), "", false);
        let result_text = if shown.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m {shown} \x1b[22m")
        };
        let input_width = inner.saturating_sub(visible_width(&result_text));
        let rendered = self
            .input
            .render(input_width.max(1))
            .into_iter()
            .next()
            .unwrap_or_default();
        let input_line = truncate_to_width(&rendered, input_width, "", false);
        let input_pad = " ".repeat(input_width.saturating_sub(visible_width(&input_line)));
        let content = format!("{input_line}{input_pad}{result_text}");

        let mut prev = format!("↑ {}", self.previous_key);
        let mut next = format!("↓ {}", self.next_key);
        let mut sep = " · ";
        let gap = 1;
        let available = inner.saturating_sub(gap * 2 + 1);
        let mut controls = visible_width(&prev) + visible_width(sep) + visible_width(&next);
        if controls > available {
            prev = "↑".into();
            next = "↓".into();
            sep = " ";
            controls = visible_width(&prev) + visible_width(sep) + visible_width(&next);
        }
        let show = controls <= available;
        let buttons = if show {
            format!(
                "{}{sep}{}",
                (self.button_style)(&prev, self.hovered == Some(-1)),
                (self.button_style)(&next, self.hovered == Some(1))
            )
        } else {
            String::new()
        };
        let gaps = if show { gap * 2 } else { 0 };
        let right_rule = usize::from(!buttons.is_empty() && inner > controls + gaps);
        let left_rule = inner.saturating_sub(if show { controls } else { 0 } + gaps + right_rule);
        self.buttons = show.then(|| {
            let prev_start = 1 + left_rule + gap;
            let prev_end = prev_start + visible_width(&prev);
            let next_start = prev_end + visible_width(sep);
            [(prev_start, prev_end), (next_start, next_start + visible_width(&next))]
        });
        if safe == 1 {
            return vec!["┌".into(), "│".into(), "└".into()];
        }
        let pad = if buttons.is_empty() { "" } else { " " };
        vec![
            format!("┌{}┐", "─".repeat(inner)),
            format!("│{content}│"),
            format!(
                "└{}{pad}{buttons}{pad}{}┘",
                "─".repeat(left_rule),
                "─".repeat(right_rule)
            ),
        ]
    }
}

/// What the owner should do after a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchOutcome {
    None,
    Render,
    Close,
}

/// The fullscreen transcript search view.
pub struct TranscriptSearch {
    rows: Vec<String>,
    keys: KeybindingsManager,
    bar: SearchBar,
    index: SearchIndex,
    matches: Vec<SearchMatch>,
    selected: Option<usize>,
    selected_key: Option<String>,
    mode: SelectionMode,
    anchor_row: usize,
    query: String,
    body: ScrollBody,
    body_rows: usize,
    match_style: Box<dyn Fn(&str) -> String>,
    current_style: Box<dyn Fn(&str) -> String>,
}

impl TranscriptSearch {
    /// `rows`: every transcript row at the current width (scrollback then viewport). Opens at the bottom.
    pub fn new(rows: Vec<String>, keys: KeybindingsManager, darwin: bool) -> Self {
        let mut bar = SearchBar::new(&keys, darwin, Box::new(|t, _| t.to_string()));
        bar.set_focused(true);
        let mut body = ScrollBody::default();
        body.offset = usize::MAX / 2;
        TranscriptSearch {
            rows,
            keys,
            bar,
            index: SearchIndex::default(),
            matches: Vec::new(),
            selected: None,
            selected_key: None,
            mode: SelectionMode::Retain,
            anchor_row: 0,
            query: String::new(),
            body,
            body_rows: 0,
            match_style: Box::new(|t| format!("\x1b[4m{t}\x1b[24m")),
            current_style: Box::new(|t| format!("\x1b[1;7m{t}\x1b[22;27m")),
        }
    }

    pub fn set_styles(&mut self, match_style: Box<dyn Fn(&str) -> String>, current_style: Box<dyn Fn(&str) -> String>) {
        self.match_style = match_style;
        self.current_style = current_style;
    }

    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn scroll_offset(&self) -> usize {
        self.body.offset
    }

    fn navigate(&mut self, direction: i8) {
        if self.query.is_empty() {
            return;
        }
        self.mode = if direction < 0 {
            SelectionMode::Previous
        } else {
            SelectionMode::Next
        };
    }

    pub fn handle_input(&mut self, data: &str) -> SearchOutcome {
        if let Some(mouse) = parse_sgr_mouse(data) {
            if let Some(wheel) = mouse.wheel {
                return if self.body.scroll(wheel * 3) {
                    SearchOutcome::Render
                } else {
                    SearchOutcome::None
                };
            }
            let bar_row = mouse.row - self.body_rows as i64;
            let direction = (bar_row >= 0 && mouse.col >= 0)
                .then(|| self.bar.navigation_at(bar_row as usize, mouse.col as usize))
                .flatten();
            let hovered = self.bar.set_hovered(direction);
            if mouse.left_click
                && let Some(d) = direction
            {
                self.navigate(d);
                return SearchOutcome::Render;
            }
            return if hovered {
                SearchOutcome::Render
            } else {
                SearchOutcome::None
            };
        }
        let k = &self.keys;
        if k.matches(data, "tui.altScreen.search") || k.matches(data, "tui.altScreen.searchClose") {
            return SearchOutcome::Close;
        }
        if k.matches(data, "tui.altScreen.searchNext") {
            self.navigate(1);
            return SearchOutcome::Render;
        }
        if k.matches(data, "tui.altScreen.searchPrevious") {
            self.navigate(-1);
            return SearchOutcome::Render;
        }
        let scrolled = if k.matches(data, "tui.altScreen.pageUp") {
            Some(self.body.page(-1))
        } else if k.matches(data, "tui.altScreen.pageDown") {
            Some(self.body.page(1))
        } else {
            None
        };
        if let Some(moved) = scrolled {
            return if moved {
                SearchOutcome::Render
            } else {
                SearchOutcome::None
            };
        }
        if self.bar.handle_input(data) {
            let query = self.bar.query().to_string();
            let selected_row = self
                .selected
                .and_then(|i| self.matches.get(i))
                .and_then(|m| m.segments.first())
                .map(|s| s.row);
            self.anchor_row = selected_row.unwrap_or(self.body.offset);
            self.query = query;
            self.mode = SelectionMode::Query;
            self.bar.set_result(None, 0);
        }
        SearchOutcome::Render
    }

    /// `refreshSearch`: recompute matches, pick the current one and scroll it into view.
    fn refresh(&mut self) {
        if self.query.trim().is_empty() {
            self.matches.clear();
            self.selected = None;
            self.selected_key = None;
            self.mode = SelectionMode::Retain;
            self.bar.set_result(None, 0);
            return;
        }
        let reveal = self.mode != SelectionMode::Retain;
        let (matches, changed) = self.index.search(&self.rows, &self.query);
        self.matches = matches.to_vec();
        if !changed && self.mode == SelectionMode::Retain {
            return;
        }
        let selected = select_match(
            &self.matches,
            self.mode,
            changed,
            self.anchor_row,
            self.selected,
            self.selected_key.as_deref(),
        );
        self.selected = selected;
        self.selected_key = selected.map(|i| match_key(&self.matches[i]));
        self.mode = SelectionMode::Retain;
        self.bar.set_result(selected, self.matches.len());
        if !reveal {
            return;
        }
        let Some(m) = selected.map(|i| &self.matches[i]) else {
            return;
        };
        let (first, last) = (m.segments[0].row, m.segments[m.segments.len() - 1].row);
        let height = self.body_rows;
        if height == 0 {
            return;
        }
        let top = self.body.offset;
        if first < top || last > top + height - 1 {
            self.body.scroll(first as i64 - (height / 3) as i64 - top as i64);
        }
    }

    /// Exactly `height` rows: the transcript window over the three-row search bar.
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        self.body_rows = height.saturating_sub(3);
        self.body.set_geometry(self.rows.len(), self.body_rows);
        self.refresh();
        let top = self.body.offset;
        let mut ranges: Vec<Vec<(usize, usize, bool)>> = vec![Vec::new(); self.body_rows];
        if self.selected.is_some() {
            for (i, m) in self.matches.iter().enumerate() {
                for s in &m.segments {
                    if s.row >= top && s.row < top + self.body_rows {
                        ranges[s.row - top].push((s.start_col, s.end_col, Some(i) == self.selected));
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(height);
        for (i, row_ranges) in ranges.iter().enumerate() {
            let line = self
                .rows
                .get(top + i)
                .map_or(Cow::Borrowed(""), |l| Cow::Borrowed(l.as_str()));
            let line = if visible_width(&line) > width {
                Cow::Owned(truncate_to_width(&line, width, "", false))
            } else {
                line
            };
            out.push(if row_ranges.is_empty() {
                line.into_owned()
            } else {
                highlight_row(&line, row_ranges, &*self.match_style, &*self.current_style)
            });
        }
        out.extend(self.bar.render(width));
        out.truncate(height);
        out
    }
}
