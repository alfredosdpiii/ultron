//! Multi-line prompt editor with its autocomplete picker (port of packages/tui/src/components/editor.ts).
//! Cursor columns are byte offsets into a logical line. Where TS arithmetic is in UTF-16 code units and the
//! result is observable (sticky visual columns, paste sizes), the port converts to code units explicitly.

mod completion;

pub use completion::AUTOCOMPLETE_DEBOUNCE_MS;

use std::rc::Rc;

use unicode_segmentation::UnicodeSegmentation;

use crate::autocomplete::AutocompleteProvider;
use crate::editing::{KillRing, find_word_backward, find_word_forward};
use crate::keybindings::KeybindingsManager;
use crate::keys::{KeyContext, decode_printable_key, matches_key};
use crate::mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use crate::renderer::CURSOR_MARKER;
use crate::select::{SelectList, SelectListTheme};
use crate::utils::{is_cjk_break, is_whitespace_char, slice_with_width, visible_width};

/// One visual chunk of a wrapped logical line; `start`/`end` are byte offsets into the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// TS `isWhitespaceChar(segment)`: `/\s/.test`, true when any character is whitespace.
fn is_ws(s: &str) -> bool {
    s.chars().any(is_whitespace_char)
}

fn is_cjk(s: &str) -> bool {
    s.chars().any(is_cjk_break)
}

fn u16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Byte offset of the first character starting at or after UTF-16 offset `u`.
fn byte_of_u16(s: &str, u: usize) -> usize {
    let mut acc = 0;
    for (b, c) in s.char_indices() {
        if acc >= u {
            return b;
        }
        acc += c.len_utf16();
    }
    s.len()
}

/// A `[paste #N( +L lines| C chars)?]` match.
struct Marker<'a> {
    start: usize,
    end: usize,
    id: &'a str,
    suffix: &'a str,
}

fn digits_at(s: &str, i: usize) -> usize {
    s[i..].bytes().take_while(u8::is_ascii_digit).count()
}

/// Parse a marker starting at byte `i` (PASTE_MARKER_REGEX, with its backtracking on the optional suffix).
fn marker_at(text: &str, i: usize) -> Option<Marker<'_>> {
    let id_start = i + "[paste #".len();
    if !text[i..].starts_with("[paste #") {
        return None;
    }
    let n = digits_at(text, id_start);
    if n == 0 {
        return None;
    }
    let after = id_start + n;
    let suffix_len = (|| {
        let rest = text[after..].strip_prefix(' ')?;
        let (plus, unit) = if rest.starts_with('+') {
            (1, " lines")
        } else {
            (0, " chars")
        };
        let d = digits_at(rest, plus);
        (d > 0 && rest[plus + d..].starts_with(unit)).then_some(1 + plus + d + unit.len())
    })();
    if let Some(len) = suffix_len
        && text[after + len..].starts_with(']')
    {
        return Some(Marker {
            start: i,
            end: after + len + 1,
            id: &text[id_start..after],
            suffix: &text[after..after + len],
        });
    }
    text[after..].starts_with(']').then(|| Marker {
        start: i,
        end: after + 1,
        id: &text[id_start..after],
        suffix: "",
    })
}

/// All non-overlapping markers, left to right (`matchAll`).
fn find_markers(text: &str) -> Vec<Marker<'_>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = text[pos..].find("[paste #") {
        let i = pos + off;
        match marker_at(text, i) {
            Some(m) => {
                pos = m.end;
                out.push(m);
            }
            None => pos = i + 1,
        }
    }
    out
}

/// TS `isPasteMarker`: the whole segment is one marker.
fn is_paste_marker(s: &str) -> bool {
    s.len() >= 10 && marker_at(s, 0).is_some_and(|m| m.end == s.len())
}

/// Grapheme segments with valid paste markers merged into single atomic segments (`segmentWithMarkers`).
fn graphemes_with_markers<'a>(text: &'a str, valid: &[u64]) -> Vec<(usize, &'a str)> {
    let spans: Vec<(usize, usize)> = if valid.is_empty() || !text.contains("[paste #") {
        Vec::new()
    } else {
        find_markers(text)
            .into_iter()
            .filter(|m| m.id.parse::<u64>().is_ok_and(|id| valid.contains(&id)))
            .map(|m| (m.start, m.end))
            .collect()
    };
    let mut out = Vec::new();
    let mut mi = 0;
    for (idx, g) in text.grapheme_indices(true) {
        while mi < spans.len() && spans[mi].1 <= idx {
            mi += 1;
        }
        match spans.get(mi) {
            Some(&(s, e)) if idx >= s && idx < e => {
                if idx == s {
                    out.push((s, &text[s..e]));
                }
            }
            _ => out.push((idx, g)),
        }
    }
    out
}

/// `wordWrapLine`: wrap at word boundaries (whitespace runs, CJK), force-breaking words wider than `max_width`.
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    wrap_segments(line, max_width, &graphemes_with_markers(line, &[]))
}

fn wrap_segments(line: &str, max_width: usize, segments: &[(usize, &str)]) -> Vec<TextChunk> {
    let chunk = |s: usize, e: usize| TextChunk {
        text: line[s..e].to_string(),
        start: s,
        end: e,
    };
    if line.is_empty() || max_width == 0 {
        return vec![chunk(0, 0)];
    }
    if visible_width(line) <= max_width {
        return vec![chunk(0, line.len())];
    }
    let mut chunks = Vec::new();
    let mut current_width = 0;
    let mut chunk_start = 0;
    let mut wrap_opp: Option<(usize, usize)> = None;
    for (i, &(char_index, grapheme)) in segments.iter().enumerate() {
        let g_width = visible_width(grapheme);
        let marker = is_paste_marker(grapheme);
        let ws = !marker && is_ws(grapheme);
        if current_width + g_width > max_width {
            match wrap_opp {
                Some((idx, w)) if current_width - w + g_width <= max_width => {
                    chunks.push(chunk(chunk_start, idx));
                    chunk_start = idx;
                    current_width -= w;
                }
                _ if chunk_start < char_index => {
                    chunks.push(chunk(chunk_start, char_index));
                    chunk_start = char_index;
                    current_width = 0;
                }
                _ => {}
            }
            wrap_opp = None;
        }
        // A wide atomic segment (paste marker) is re-wrapped visually. A single grapheme cannot be split, so it
        // stays one overflowing chunk (TS recurses forever there).
        if g_width > max_width && grapheme.graphemes(true).nth(1).is_some() {
            let sub = word_wrap_line(grapheme, max_width);
            let (last, init) = sub.split_last().expect("wordWrapLine returns at least one chunk");
            for sc in init {
                chunks.push(chunk(char_index + sc.start, char_index + sc.end));
            }
            chunk_start = char_index + last.start;
            current_width = visible_width(&last.text);
            wrap_opp = None;
            continue;
        }
        current_width += g_width;
        if let Some(&(next_index, next)) = segments.get(i + 1) {
            let next_marker = is_paste_marker(next);
            // After a whitespace run, or where either side is CJK.
            let ws_break = ws && (next_marker || !is_ws(next));
            let cjk_break = !ws && !is_ws(next) && ((!marker && is_cjk(grapheme)) || (!next_marker && is_cjk(next)));
            if ws_break || cjk_break {
                wrap_opp = Some((next_index, current_width));
            }
        }
    }
    chunks.push(chunk(chunk_start, line.len()));
    chunks
}

/// TS `isKeyRelease`: a Kitty flag-2 release event (never bracketed paste content).
pub fn is_key_release(data: &str) -> bool {
    !data.contains("\x1b[200~")
        && [":3u", ":3~", ":3A", ":3B", ":3C", ":3D", ":3H", ":3F"]
            .iter()
            .any(|p| data.contains(p))
}

/// TS `isKeyRepeat`: a Kitty flag-2 repeat event (never bracketed paste content).
pub fn is_key_repeat(data: &str) -> bool {
    !data.contains("\x1b[200~")
        && [":2u", ":2~", ":2A", ":2B", ":2C", ":2D", ":2H", ":2F"]
            .iter()
            .any(|p| data.contains(p))
}

fn scroll_border(direction: &str, hidden: usize, width: usize) -> String {
    let label = format!(" {direction} {hidden} more ");
    let label_width = visible_width(&label);
    if label_width + 2 <= width {
        let left = (width - label_width) / 2;
        return format!("{}{label}{}", "─".repeat(left), "─".repeat(width - left - label_width));
    }
    let indicator = format!("─── {direction} {hidden} more ");
    let iw = visible_width(&indicator);
    if iw <= width {
        return format!("{indicator}{}", "─".repeat(width - iw));
    }
    let ellipsis = &"..."[..width.min(3)];
    slice_with_width(&indicator, 0, width - ellipsis.len(), true).0 + ellipsis
}

#[derive(Clone, Copy)]
struct VisualLine {
    logical: usize,
    start: usize,
    len: usize,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pastes: Vec<(u64, String)>,
    paste_counter: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Receives the editor text after a change.
pub type ChangeFn = Box<dyn FnMut(&str)>;

/// Styles the top and bottom borders.
pub type StyleFn = Box<dyn Fn(&str) -> String>;

pub struct Editor {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pub focused: bool,
    /// Stands in for `tui.terminal.rows`: the visible area is 30% of it, at least 5 lines.
    pub terminal_rows: usize,
    pub border_color: StyleFn,
    pub disable_submit: bool,
    padding_x: usize,
    last_width: usize,
    scroll_offset: usize,
    /// Paste registry in `Map` insertion order.
    pastes: Vec<(u64, String)>,
    paste_counter: u64,
    paste_buffer: String,
    in_paste: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<(Vec<String>, usize, usize)>,
    history_limit: usize,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    jump_forward: Option<bool>,
    preferred_visual_col: Option<usize>,
    /// Pre-snap cursor column in UTF-16 units.
    snapped_from: Option<usize>,
    undo_stack: Vec<Snapshot>,
    keys: KeybindingsManager,
    ctx: KeyContext,
    /// TS `onChange`: called with the full text after every edit.
    pub on_change: Option<ChangeFn>,
    select_list_theme: Rc<SelectListTheme>,
    provider: Option<Box<dyn AutocompleteProvider>>,
    trigger_chars: Vec<char>,
    picker: Option<completion::Picker>,
    /// A debounced request: (force, explicit Tab).
    pending_request: Option<(bool, bool)>,
    autocomplete_max_visible: usize,
    rendered_visible_lines: usize,
    rendered_picker_height: usize,
}

impl Editor {
    /// `keys` stands in for the TS global `getKeybindings()`, `ctx` for the global key-matching state.
    pub fn new(keys: KeybindingsManager, ctx: KeyContext, border_color: StyleFn, padding_x: usize) -> Self {
        Editor {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            focused: false,
            terminal_rows: 24,
            border_color,
            disable_submit: false,
            padding_x,
            last_width: 80,
            scroll_offset: 0,
            pastes: Vec::new(),
            paste_counter: 0,
            paste_buffer: String::new(),
            in_paste: false,
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            history_limit: 100,
            kill_ring: KillRing::default(),
            last_action: None,
            jump_forward: None,
            preferred_visual_col: None,
            snapped_from: None,
            undo_stack: Vec::new(),
            keys,
            ctx,
            on_change: None,
            select_list_theme: Rc::new(SelectListTheme {
                selected_prefix: Box::new(str::to_string),
                selected_text: Box::new(str::to_string),
                description: Box::new(str::to_string),
                scroll_info: Box::new(str::to_string),
                no_match: Box::new(str::to_string),
            }),
            provider: None,
            trigger_chars: completion::DEFAULT_TRIGGER_CHARACTERS.to_vec(),
            picker: None,
            pending_request: None,
            autocomplete_max_visible: 5,
            rendered_visible_lines: 0,
            rendered_picker_height: 0,
        }
    }

    /// TS `EditorTheme.selectList`.
    pub fn set_select_list_theme(&mut self, theme: SelectListTheme) {
        self.select_list_theme = Rc::new(theme);
    }

    fn changed(&mut self) {
        let text = self.text();
        if let Some(cb) = &mut self.on_change {
            cb(&text);
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// (line, byte column).
    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_col)
    }

    pub fn expanded_text(&self) -> String {
        self.expand_paste_markers(&self.text())
    }

    fn valid_ids(&self) -> Vec<u64> {
        self.pastes.iter().map(|(id, _)| *id).collect()
    }

    fn graphemes<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        graphemes_with_markers(text, &self.valid_ids())
    }

    fn line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn max_visible_lines(&self) -> usize {
        ((self.terminal_rows as f64 * 0.3).floor() as usize).max(5)
    }

    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim_matches(is_whitespace_char);
        if trimmed.is_empty() || self.history.first().is_some_and(|h| h == trimmed) {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        self.history.truncate(self.history_limit);
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn set_history(&mut self, entries: &[&str]) {
        self.history = entries
            .iter()
            .map(|e| e.trim_matches(is_whitespace_char).to_string())
            .filter(|e| !e.is_empty())
            .take(self.history_limit)
            .collect();
        self.exit_history_browsing();
    }

    fn is_editor_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index.map_or(-1, |i| i as isize) - direction;
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index.is_none() && new_index >= 0 {
            self.push_undo();
            self.history_draft = Some((self.lines.clone(), self.cursor_line, self.cursor_col));
        }
        if new_index == -1 {
            self.history_index = None;
            match self.history_draft.take() {
                Some((lines, line, col)) => {
                    self.lines = lines;
                    self.cursor_line = line;
                    self.cursor_col = col;
                    self.preferred_visual_col = None;
                    self.snapped_from = None;
                    self.scroll_offset = 0;
                    self.changed();
                }
                None => self.set_text_internal("", false),
            }
        } else {
            let i = new_index as usize;
            self.history_index = Some(i);
            let text = self.history[i].clone();
            self.set_text_internal(&text, direction == -1);
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn set_text_internal(&mut self, text: &str, at_start: bool) {
        self.lines = text.split('\n').map(str::to_string).collect();
        self.cursor_line = if at_start { 0 } else { self.lines.len() - 1 };
        let col = if at_start {
            0
        } else {
            self.lines[self.cursor_line].len()
        };
        self.set_cursor_col(col);
        self.scroll_offset = 0;
        self.changed();
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = normalize_text(text);
        if self.text() != normalized {
            self.push_undo();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, false);
    }

    // Hooks for modal editing and attachments (crate::vim_editor, crate::attachments).

    /// Place the cursor: line clamped to the buffer, column to the line and back to a char boundary.
    pub fn set_cursor(&mut self, line: usize, col: usize) {
        self.cursor_line = line.min(self.lines.len() - 1);
        let text = &self.lines[self.cursor_line];
        let mut col = col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        self.set_cursor_col(col);
    }

    /// Replace the buffer and cursor as one edit (`undo`: record an undo step first).
    pub fn apply_edit(&mut self, lines: Vec<String>, cursor: (usize, usize), undo: bool) {
        self.cancel_autocomplete();
        if undo {
            self.push_undo();
        }
        self.last_action = None;
        self.exit_history_browsing();
        self.lines = if lines.is_empty() { vec![String::new()] } else { lines };
        self.set_cursor(cursor.0, cursor.1);
        self.changed();
    }

    /// Undo the last recorded edit.
    pub fn undo_edit(&mut self) {
        self.undo();
    }

    /// Byte spans of the valid paste markers on `line`.
    pub fn paste_marker_spans_on(&self, line: usize) -> Vec<(usize, usize)> {
        let ids = self.valid_ids();
        self.lines.get(line).map_or_else(Vec::new, |l| {
            find_markers(l)
                .into_iter()
                .filter(|m| m.id.parse::<u64>().is_ok_and(|id| ids.contains(&id)))
                .map(|m| (m.start, m.end))
                .collect()
        })
    }

    /// Registered large pastes as (id, content), in insertion order.
    pub fn pastes(&self) -> &[(u64, String)] {
        &self.pastes
    }

    /// `text` with paste markers expanded to their content.
    pub fn expand_markers(&self, text: &str) -> String {
        self.expand_paste_markers(text)
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_internal(text);
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (id, content) in &self.pastes {
            let id = id.to_string();
            let mut out = String::new();
            let mut last = 0;
            for m in find_markers(&result).into_iter().filter(|m| m.id == id) {
                out.push_str(&result[last..m.start]);
                out.push_str(content);
                last = m.end;
            }
            out.push_str(&result[last..]);
            result = out;
        }
        result
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = content_width.saturating_sub(usize::from(padding_x == 0)).max(1);
        self.last_width = layout_width;
        let layout = self.layout_text(layout_width);
        let max_visible = self.max_visible_lines();
        let cursor_index = layout.iter().position(|(_, c)| c.is_some()).unwrap_or(0);
        if cursor_index < self.scroll_offset {
            self.scroll_offset = cursor_index;
        } else if cursor_index >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_index + 1 - max_visible;
        }
        self.scroll_offset = self.scroll_offset.min(layout.len().saturating_sub(max_visible));
        let visible = &layout[self.scroll_offset..(self.scroll_offset + max_visible).min(layout.len())];
        let pad = " ".repeat(padding_x);
        let border = |hidden: usize, dir: &str| {
            (self.border_color)(&if hidden > 0 {
                scroll_border(dir, hidden, width)
            } else {
                "─".repeat(width)
            })
        };
        let mut result = vec![border(self.scroll_offset, "↑")];
        for (text, cursor) in visible {
            let mut display = text.clone();
            let mut line_width = visible_width(text);
            let mut cursor_in_padding = false;
            if let Some(pos) = *cursor {
                let (before, after) = text.split_at(pos);
                let marker = if self.focused { CURSOR_MARKER } else { "" };
                if after.is_empty() {
                    display = format!("{before}{marker}\x1b[7m \x1b[0m");
                    line_width += 1;
                    cursor_in_padding = line_width > content_width && padding_x > 0;
                } else {
                    let first = self.graphemes(after).first().map_or("", |g| g.1);
                    display = format!("{before}{marker}\x1b[7m{first}\x1b[0m{}", &after[first.len()..]);
                }
            }
            let fill = " ".repeat(content_width.saturating_sub(line_width));
            let right = if cursor_in_padding { &pad[1..] } else { &pad[..] };
            result.push(format!("{pad}{display}{fill}{right}"));
        }
        let below = layout.len() - (self.scroll_offset + visible.len());
        result.push(border(below, "↓"));
        self.rendered_visible_lines = visible.len();
        self.rendered_picker_height = 0;
        if let Some(picker) = &mut self.picker {
            let lines = crate::components::Component::render(&mut picker.list, content_width);
            self.rendered_picker_height = lines.len();
            for line in lines {
                let fill = " ".repeat(content_width.saturating_sub(visible_width(&line)));
                result.push(format!("{pad}{line}{fill}{pad}"));
            }
        }
        result
    }

    /// Layout lines as (text, cursor byte position when the cursor is on it).
    fn layout_text(&self, width: usize) -> Vec<(String, Option<usize>)> {
        if self.is_editor_empty() {
            return vec![(String::new(), Some(0))];
        }
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let current = i == self.cursor_line;
            if visible_width(line) <= width {
                out.push((line.clone(), current.then_some(self.cursor_col)));
                continue;
            }
            let chunks = wrap_segments(line, width, &self.graphemes(line));
            let n = chunks.len();
            for (ci, chunk) in chunks.into_iter().enumerate() {
                let c = self.cursor_col;
                let has = current && c >= chunk.start && (ci == n - 1 || c < chunk.end);
                let pos = has.then(|| (c - chunk.start).min(chunk.text.len()));
                out.push((chunk.text, pos));
            }
        }
        out
    }

    /// Returns the submitted text when the input submits (TS `onSubmit`).
    pub fn handle_input(&mut self, data: &str) -> Option<String> {
        let kb = |id: &str| self.keys.matches(data, id);
        if let Some(forward) = self.jump_forward {
            if kb("tui.editor.jumpForward") || kb("tui.editor.jumpBackward") {
                self.jump_forward = None;
                return None;
            }
            let printable = decode_printable_key(data)
                .or_else(|| (data.encode_utf16().next().is_some_and(|c| c >= 32)).then(|| data.to_string()));
            self.jump_forward = None;
            if let Some(p) = printable {
                self.jump_to_char(&p, forward);
                return None;
            }
        }
        let mut data = data.to_string();
        if data.contains("\x1b[200~") {
            self.in_paste = true;
            self.paste_buffer.clear();
            data = data.replacen("\x1b[200~", "", 1);
        }
        if self.in_paste {
            self.paste_buffer.push_str(&data);
            if let Some(end) = self.paste_buffer.find("\x1b[201~") {
                let content = self.paste_buffer[..end].to_string();
                let remaining = self.paste_buffer[end + 6..].to_string();
                if !content.is_empty() {
                    self.handle_paste(&content);
                }
                self.in_paste = false;
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    return self.handle_input(&remaining);
                }
            }
            return None;
        }
        let data = data.as_str();
        // Ctrl+C belongs to the parent.
        if self.keys.matches(data, "tui.input.copy") {
            return None;
        }
        if self.keys.matches(data, "tui.editor.undo") {
            self.undo();
            return None;
        }
        if self.picker.is_some() && !self.handle_picker_key(data) {
            return None;
        }
        if self.keys.matches(data, "tui.input.tab") && self.picker.is_none() {
            self.handle_tab_completion();
            return None;
        }
        let kb = |id: &str| self.keys.matches(data, id);
        let first_unit = data.encode_utf16().next().unwrap_or(0);
        if kb("tui.editor.deleteToLineEnd") {
            self.delete_to_line_end();
        } else if kb("tui.editor.deleteToLineStart") {
            self.delete_to_line_start();
        } else if kb("tui.editor.deleteWordBackward") {
            self.delete_word_backward();
        } else if kb("tui.editor.deleteWordForward") {
            self.delete_word_forward();
        } else if kb("tui.editor.deleteCharBackward") || matches_key(data, "shift+backspace", self.ctx) {
            self.handle_backspace();
        } else if kb("tui.editor.deleteCharForward") || matches_key(data, "shift+delete", self.ctx) {
            self.handle_forward_delete();
        } else if kb("tui.editor.yank") {
            self.yank();
        } else if kb("tui.editor.yankPop") {
            self.yank_pop();
        } else if kb("tui.editor.historyPrevious") {
            self.cancel_autocomplete();
            self.navigate_history(-1);
        } else if kb("tui.editor.historyNext") {
            self.cancel_autocomplete();
            self.navigate_history(1);
        } else if kb("tui.editor.cursorLineStart") {
            self.move_to_line_start();
        } else if kb("tui.editor.cursorLineEnd") {
            self.move_to_line_end();
        } else if kb("tui.editor.cursorWordLeft") {
            self.move_word_backward();
        } else if kb("tui.editor.cursorWordRight") {
            self.move_word_forward();
        } else if kb("tui.input.newLine")
            || (first_unit == 10 && data.len() > 1)
            || data == "\x1b\r"
            || data == "\x1b[13;2~"
            || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'))
            || data == "\n"
        {
            if self.should_submit_on_backslash_enter(data) {
                self.handle_backspace();
                return Some(self.submit_value());
            }
            self.add_new_line();
        } else if kb("tui.input.submit") {
            if self.disable_submit {
                return None;
            }
            if self.cursor_col > 0 && self.line().as_bytes()[self.cursor_col - 1] == b'\\' {
                self.handle_backspace();
                self.add_new_line();
                return None;
            }
            return Some(self.submit_value());
        } else if kb("tui.editor.cursorUp") {
            let first = self.current_visual_line().0 == 0;
            if first && (self.is_editor_empty() || self.history_index.is_some() || self.cursor_col == 0) {
                self.navigate_history(-1);
            } else if first {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
        } else if kb("tui.editor.cursorDown") {
            let (cur, vls) = self.current_visual_line();
            let last = cur == vls.len() - 1;
            if self.history_index.is_some() && last {
                self.navigate_history(1);
            } else if last {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
        } else if kb("tui.editor.cursorRight") {
            self.move_cursor(0, 1);
        } else if kb("tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
        } else if kb("tui.editor.pageUp") {
            self.page_scroll(-1);
        } else if kb("tui.editor.pageDown") {
            self.page_scroll(1);
        } else if kb("tui.editor.jumpForward") {
            self.jump_forward = Some(true);
        } else if kb("tui.editor.jumpBackward") {
            self.jump_forward = Some(false);
        } else if matches_key(data, "shift+space", self.ctx) {
            self.insert_character(" ");
        } else if let Some(p) = decode_printable_key(data) {
            self.insert_character(&p);
        } else if first_unit >= 32 {
            self.insert_character(data);
        }
        None
    }

    fn insert_text_internal(&mut self, text: &str) {
        self.insert_raw(&normalize_text(text));
        self.changed();
    }

    fn insert_character(&mut self, ch: &str) {
        self.exit_history_browsing();
        if is_ws(ch) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        let col = self.cursor_col;
        self.lines[self.cursor_line].insert_str(col, ch);
        self.set_cursor_col(col + ch.len());
        self.changed();
        self.after_insert_character(ch);
    }

    fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo();
        let decoded = decode_csi_u_ctrl(pasted);
        let mut filtered: String = normalize_text(&decoded)
            .chars()
            .filter(|&c| c == '\n' || c as u32 >= 32)
            .collect();
        if filtered.starts_with(['/', '~', '.']) {
            let before = self.line()[..self.cursor_col].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                filtered.insert(0, ' ');
            }
        }
        let line_count = filtered.split('\n').count();
        let total = u16_len(&filtered);
        if line_count > 10 || total > 1000 {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.set_paste(id, filtered);
            let marker = if line_count > 10 {
                format!("[paste #{id} +{line_count} lines]")
            } else {
                format!("[paste #{id} {total} chars]")
            };
            self.insert_text_internal(&marker);
            return;
        }
        self.insert_text_internal(&filtered);
    }

    /// `Map.set`: replace in place or append.
    fn set_paste(&mut self, id: u64, content: String) {
        match self.pastes.iter_mut().find(|(k, _)| *k == id) {
            Some(entry) => entry.1 = content,
            None => self.pastes.push((id, content)),
        }
    }

    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo();
        let after = self.lines[self.cursor_line].split_off(self.cursor_col);
        self.lines.insert(self.cursor_line + 1, after);
        self.cursor_line += 1;
        self.set_cursor_col(0);
        self.changed();
    }

    fn should_submit_on_backslash_enter(&self, data: &str) -> bool {
        if self.disable_submit || !matches_key(data, "enter", self.ctx) {
            return false;
        }
        let submit = self.keys.get_keys("tui.input.submit");
        if !submit.iter().any(|k| k == "shift+enter" || k == "shift+return") {
            return false;
        }
        self.cursor_col > 0 && self.line().as_bytes()[self.cursor_col - 1] == b'\\'
    }

    fn submit_value(&mut self) -> String {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.text())
            .trim_matches(is_whitespace_char)
            .to_string();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        self.changed();
        result
    }

    fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        if self.cursor_col > 0 {
            self.push_undo();
            let line = self.line().to_string();
            let last = self
                .graphemes(&line[..self.cursor_col])
                .last()
                .map_or("", |g| g.1)
                .to_string();
            if let Some(target) = marker_at(&last, 0)
                .filter(|m| m.end == last.len())
                .and_then(|m| m.id.parse::<u64>().ok())
            {
                self.pastes.retain(|(id, _)| *id != target);
                self.paste_counter = self.paste_counter.saturating_sub(1);
                let mut higher: Vec<u64> = self
                    .pastes
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| *id > target)
                    .collect();
                higher.sort_unstable();
                for id in higher {
                    if let Some(pos) = self.pastes.iter().position(|(k, _)| *k == id) {
                        let (_, content) = self.pastes.remove(pos);
                        self.set_paste(id - 1, content);
                    }
                }
                for l in &mut self.lines {
                    *l = renumber_markers(l, target);
                }
            }
            let col = self.cursor_col;
            self.lines[self.cursor_line].replace_range(col - last.len()..col, "");
            self.set_cursor_col(col - last.len());
        } else if self.cursor_line > 0 {
            self.push_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            let prev_len = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.set_cursor_col(prev_len);
        }
        self.changed();
        self.after_delete();
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from = None;
    }

    fn move_to_visual_line(&mut self, vls: &[VisualLine], current: usize, target: usize) {
        let (Some(&cur), Some(&tgt)) = (vls.get(current), vls.get(target)) else {
            return;
        };
        let is_last = |i: usize| i == vls.len() - 1 || vls[i + 1].logical != vls[i].logical;
        let cur_line = &self.lines[cur.logical];
        let current_col = match self.snapped_from {
            Some(snap) => {
                let vl = vls[find_visual_line_at(vls, cur.logical, byte_of_u16(cur_line, snap))];
                snap - u16_len(&cur_line[..vl.start])
            }
            None => u16_len(&cur_line[cur.start..self.cursor_col]),
        };
        let max_col = |vl: VisualLine, i: usize| {
            let len = u16_len(&self.lines[vl.logical][vl.start..vl.start + vl.len]);
            if is_last(i) { len } else { len.saturating_sub(1) }
        };
        let (source_max, target_max) = (max_col(cur, current), max_col(tgt, target));
        let move_to = self.vertical_move_column(current_col, source_max, target_max);
        self.cursor_line = tgt.logical;
        let line = self.lines[tgt.logical].clone();
        let col = (u16_len(&line[..tgt.start]) + move_to).min(u16_len(&line));
        let mut seg_u16 = 0;
        for (index, seg) in self.graphemes(&line) {
            if seg_u16 > col {
                break;
            }
            let len = u16_len(seg);
            if len > 1 && col < seg_u16 + len {
                if index < tgt.start && target > current {
                    let seg_end = index + seg.len();
                    let mut next = target + 1;
                    while next < vls.len() && vls[next].logical == tgt.logical && vls[next].start < seg_end {
                        next += 1;
                    }
                    if next < vls.len() {
                        self.move_to_visual_line(vls, current, next);
                        return;
                    }
                }
                self.snapped_from = Some(col);
                self.cursor_col = index;
                return;
            }
            seg_u16 += len;
        }
        self.cursor_col = byte_of_u16(&line, col);
        self.snapped_from = None;
    }

    /// Sticky-column decision table of `computeVerticalMoveColumn`.
    fn vertical_move_column(&mut self, current: usize, source_max: usize, target_max: usize) -> usize {
        let target_too_short = target_max < current;
        match self.preferred_visual_col {
            Some(preferred) if current >= source_max => {
                if target_too_short || target_max < preferred {
                    target_max
                } else {
                    self.preferred_visual_col = None;
                    preferred
                }
            }
            _ if target_too_short => {
                self.preferred_visual_col = Some(current);
                target_max
            }
            _ => {
                self.preferred_visual_col = None;
                current
            }
        }
    }

    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    fn move_to_line_end(&mut self) {
        self.last_action = None;
        self.set_cursor_col(self.line().len());
    }

    fn kill(&mut self, text: &str, prepend: bool, accumulate: bool) {
        self.kill_ring.push(text, prepend, accumulate);
        self.last_action = Some(LastAction::Kill);
    }

    fn was_kill(&self) -> bool {
        self.last_action == Some(LastAction::Kill)
    }

    fn delete_to_line_start(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col > 0 {
            self.push_undo();
            let deleted: String = self.lines[self.cursor_line].drain(..self.cursor_col).collect();
            self.kill(&deleted, true, self.was_kill());
            self.set_cursor_col(0);
        } else if self.cursor_line > 0 {
            self.push_undo();
            self.kill("\n", true, self.was_kill());
            self.join_with_previous();
        }
        self.changed();
    }

    fn delete_to_line_end(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col < self.line().len() {
            self.push_undo();
            let deleted = self.lines[self.cursor_line].split_off(self.cursor_col);
            self.kill(&deleted, false, self.was_kill());
        } else if self.cursor_line + 1 < self.lines.len() {
            self.push_undo();
            self.kill("\n", false, self.was_kill());
            self.join_with_next();
        }
        self.changed();
    }

    /// Merge the cursor line into the previous one; the cursor goes to the join.
    fn join_with_previous(&mut self) {
        let current = self.lines.remove(self.cursor_line);
        self.cursor_line -= 1;
        let prev_len = self.line().len();
        self.lines[self.cursor_line].push_str(&current);
        self.set_cursor_col(prev_len);
    }

    fn join_with_next(&mut self) {
        let next = self.lines.remove(self.cursor_line + 1);
        self.lines[self.cursor_line].push_str(&next);
    }

    fn delete_word_backward(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.push_undo();
                self.kill("\n", true, self.was_kill());
                self.join_with_previous();
            }
            self.changed();
            return;
        }
        self.push_undo();
        let was_kill = self.was_kill();
        let from = self.word_backward_target();
        let deleted: String = self.lines[self.cursor_line].drain(from..self.cursor_col).collect();
        self.kill(&deleted, true, was_kill);
        self.set_cursor_col(from);
        self.changed();
    }

    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        if self.cursor_col >= self.line().len() {
            if self.cursor_line + 1 < self.lines.len() {
                self.push_undo();
                self.kill("\n", false, self.was_kill());
                self.join_with_next();
            }
            self.changed();
            return;
        }
        self.push_undo();
        let was_kill = self.was_kill();
        let to = self.word_forward_target();
        let deleted: String = self.lines[self.cursor_line].drain(self.cursor_col..to).collect();
        self.kill(&deleted, false, was_kill);
        self.set_cursor_col(self.cursor_col);
        self.changed();
    }

    fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        let col = self.cursor_col;
        if col < self.line().len() {
            self.push_undo();
            let len = self.graphemes(&self.line()[col..]).first().map_or(1, |g| g.1.len());
            self.lines[self.cursor_line].replace_range(col..col + len, "");
        } else if self.cursor_line + 1 < self.lines.len() {
            self.push_undo();
            self.join_with_next();
        }
        self.changed();
        self.after_delete();
    }

    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            if line.is_empty() || visible_width(line) <= width {
                out.push(VisualLine {
                    logical: i,
                    start: 0,
                    len: line.len(),
                });
            } else {
                for c in wrap_segments(line, width, &self.graphemes(line)) {
                    out.push(VisualLine {
                        logical: i,
                        start: c.start,
                        len: c.end - c.start,
                    });
                }
            }
        }
        out
    }

    fn current_visual_line(&self) -> (usize, Vec<VisualLine>) {
        let vls = self.build_visual_line_map(self.last_width);
        (find_visual_line_at(&vls, self.cursor_line, self.cursor_col), vls)
    }

    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let (current, vls) = self.current_visual_line();
        if delta_line != 0 {
            let target = current as isize + delta_line;
            if target >= 0 && (target as usize) < vls.len() {
                self.move_to_visual_line(&vls, current, target as usize);
            }
        }
        let line = self.line().to_string();
        if delta_col > 0 {
            if self.cursor_col < line.len() {
                let len = self
                    .graphemes(&line[self.cursor_col..])
                    .first()
                    .map_or(1, |g| g.1.len());
                self.set_cursor_col(self.cursor_col + len);
            } else if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.set_cursor_col(0);
            } else if let Some(vl) = vls.get(current) {
                self.preferred_visual_col = Some(u16_len(&line[vl.start..self.cursor_col]));
            }
        } else if delta_col < 0 {
            if self.cursor_col > 0 {
                let len = self.graphemes(&line[..self.cursor_col]).last().map_or(1, |g| g.1.len());
                self.set_cursor_col(self.cursor_col - len);
            } else if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.set_cursor_col(self.line().len());
            }
        }
        if self.picker.is_some() {
            self.update_autocomplete();
        }
    }

    fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page = self.max_visible_lines() as isize;
        let (current, vls) = self.current_visual_line();
        let target = (current as isize + direction * page).clamp(0, vls.len() as isize - 1) as usize;
        self.move_to_visual_line(&vls, current, target);
    }

    /// Valid paste-marker spans of the cursor line.
    fn marker_spans(&self) -> Vec<(usize, usize)> {
        let ids = self.valid_ids();
        find_markers(self.line())
            .into_iter()
            .filter(|m| m.id.parse::<u64>().is_ok_and(|id| ids.contains(&id)))
            .map(|m| (m.start, m.end))
            .collect()
    }

    /// `findWordBackward` with paste markers as atomic segments.
    fn word_backward_target(&self) -> usize {
        let line = self.line();
        let spans = self.marker_spans();
        let p = line[..self.cursor_col].trim_end_matches(is_whitespace_char).len();
        if let Some(&(s, _)) = spans.iter().find(|&&(_, e)| e == p) {
            return s;
        }
        let r = find_word_backward(line, self.cursor_col);
        spans.iter().find(|&&(s, e)| s < r && r < e).map_or(r, |&(_, e)| e)
    }

    /// `findWordForward` with paste markers as atomic segments.
    fn word_forward_target(&self) -> usize {
        let line = self.line();
        let spans = self.marker_spans();
        let rest = &line[self.cursor_col..];
        let p = self.cursor_col + rest.len() - rest.trim_start_matches(is_whitespace_char).len();
        if let Some(&(_, e)) = spans.iter().find(|&&(s, _)| s == p) {
            return e;
        }
        let r = find_word_forward(line, self.cursor_col);
        spans.iter().find(|&&(s, e)| s < r && r < e).map_or(r, |&(s, _)| s)
    }

    fn move_word_backward(&mut self) {
        self.last_action = None;
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.set_cursor_col(self.line().len());
            }
            return;
        }
        let target = self.word_backward_target();
        self.set_cursor_col(target);
    }

    fn move_word_forward(&mut self) {
        self.last_action = None;
        if self.cursor_col >= self.line().len() {
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let target = self.word_forward_target();
        self.set_cursor_col(target);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        self.insert_raw(text);
        self.changed();
    }

    /// Insert `text` at the cursor as is; the cursor ends after it.
    fn insert_raw(&mut self, text: &str) {
        let parts: Vec<&str> = text.split('\n').collect();
        let after = self.lines[self.cursor_line].split_off(self.cursor_col);
        let last = parts[parts.len() - 1];
        self.lines[self.cursor_line].push_str(parts[0]);
        if parts.len() == 1 {
            self.lines[self.cursor_line].push_str(&after);
            self.set_cursor_col(self.cursor_col + text.len());
            return;
        }
        let mut tail: Vec<String> = parts[1..parts.len() - 1].iter().map(|s| s.to_string()).collect();
        tail.push(format!("{last}{after}"));
        let at = self.cursor_line + 1;
        self.lines.splice(at..at, tail);
        self.cursor_line += parts.len() - 1;
        self.set_cursor_col(last.len());
    }

    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let parts: Vec<&str> = yanked.split('\n').collect();
        if parts.len() == 1 {
            let col = self.cursor_col;
            self.lines[self.cursor_line].replace_range(col - yanked.len()..col, "");
            self.set_cursor_col(col - yanked.len());
        } else {
            let start_line = self.cursor_line - (parts.len() - 1);
            let start_col = self.lines[start_line].len() - parts[0].len();
            let after = self.line()[self.cursor_col..].to_string();
            let merged = format!("{}{after}", &self.lines[start_line][..start_col]);
            self.lines.splice(start_line..start_line + parts.len(), [merged]);
            self.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.changed();
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(s) = self.undo_stack.pop() else { return };
        self.lines = s.lines;
        self.cursor_line = s.cursor_line;
        self.cursor_col = s.cursor_col;
        self.pastes = s.pastes;
        self.paste_counter = s.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.changed();
    }

    /// Jump to the next (or previous) occurrence of `ch`, across lines, skipping the cursor position.
    fn jump_to_char(&mut self, ch: &str, forward: bool) {
        self.last_action = None;
        let n = self.lines.len();
        let mut idx = self.cursor_line;
        loop {
            let line = &self.lines[idx];
            let found = if idx != self.cursor_line {
                if forward { line.find(ch) } else { line.rfind(ch) }
            } else if forward {
                line.match_indices(ch).map(|(i, _)| i).find(|&i| i > self.cursor_col)
            } else {
                // lastIndexOf(ch, col - 1): a match starting before the cursor, or at 0 when the cursor is at 0.
                line.match_indices(ch)
                    .map(|(i, _)| i)
                    .filter(|&i| i < self.cursor_col || i == 0)
                    .last()
            };
            if let Some(i) = found {
                self.cursor_line = idx;
                self.set_cursor_col(i);
                return;
            }
            match (forward, idx) {
                (true, i) if i + 1 < n => idx += 1,
                (false, i) if i > 0 => idx -= 1,
                _ => return,
            }
        }
    }
}

fn find_visual_line_at(vls: &[VisualLine], line: usize, col: usize) -> usize {
    for (i, vl) in vls.iter().enumerate() {
        if vl.logical != line || col < vl.start {
            continue;
        }
        let offset = col - vl.start;
        let last = i == vls.len() - 1 || vls[i + 1].logical != vl.logical;
        if offset < vl.len || (last && offset == vl.len) {
            return i;
        }
    }
    vls.len().saturating_sub(1)
}

/// `\r\n` and `\r` to `\n`, tabs to four spaces.
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
}

/// Bracketed-paste CSI-u Ctrl+letter (`ESC [ code ; 5 u`) back to its control byte.
fn decode_csi_u_ctrl(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("\x1b[") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let d = digits_at(tail, 0);
        let decoded = (d > 0 && tail[d..].starts_with(";5u"))
            .then(|| tail[..d].parse::<u32>().ok())
            .flatten()
            .and_then(|cp| match cp {
                97..=122 => char::from_u32(cp - 96),
                65..=90 => char::from_u32(cp - 64),
                _ => None,
            });
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[d + 3..];
            }
            None => {
                out.push_str("\x1b[");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decrement the ids of markers above `removed` (every marker, valid or not, as the TS regex does).
fn renumber_markers(line: &str, removed: u64) -> String {
    let mut out = String::new();
    let mut last = 0;
    for m in find_markers(line) {
        let Ok(x) = m.id.parse::<u64>() else { continue };
        if x > removed {
            out.push_str(&line[last..m.start]);
            out.push_str(&format!("[paste #{}{}]", x - 1, m.suffix));
            last = m.end;
        }
    }
    out.push_str(&line[last..]);
    out
}
