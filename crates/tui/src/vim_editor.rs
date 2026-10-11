//! Vim mode for [`Editor`] (port of omp packages/tui/src/components/editor.ts `#handleVimInput` and
//! `#applyVimCommands`): routes keys through [`VimState`] and applies its commands to the editor buffer.
//!
//! Paste markers and attachment placeholders (`[Image #1, 800x600]`) are atomic: a range that cuts one swallows
//! the whole token, and `r`/Replace never overwrite part of one. Yanks fill the unnamed register and are handed to
//! `on_yank` (the system clipboard) with paste markers expanded.

use unicode_segmentation::UnicodeSegmentation;

use crate::editor::Editor;
use crate::keys::{KeyContext, decode_printable_key, parse_key};
use crate::vim::{
    Pos, VimBuffer, VimCommand, VimMode, VimState, last_grapheme_start, next_grapheme_start, visual_range,
};

struct ReplaceEntry {
    line: usize,
    col: usize,
    removed: String,
    text: String,
}

/// Mode, pending text and selection height, for the status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimIndicator {
    pub mode: VimMode,
    pub pending: String,
    pub selected_lines: usize,
}

pub type YankFn = Box<dyn FnMut(&str)>;

/// Vim layer over one [`Editor`].
pub struct VimEditor {
    pub state: VimState,
    register: Option<String>,
    replace_log: Vec<ReplaceEntry>,
    replacing: bool,
    pub ctx: KeyContext,
    /// Called with every yanked text (paste markers expanded): wire the system clipboard here.
    pub on_yank: Option<YankFn>,
}

fn nav_key(canonical: &str) -> Option<&'static str> {
    Some(match canonical {
        "up" => "k",
        "down" => "j",
        "left" => "h",
        "right" => "l",
        "home" => "0",
        "end" => "$",
        "backspace" => "h",
        "delete" => "x",
        "space" => "l",
        _ => return None,
    })
}

impl VimEditor {
    /// Starts in Insert mode, so typing keeps working when the setting is turned on.
    pub fn new(ctx: KeyContext) -> Self {
        let mut state = VimState::new();
        state.mode = VimMode::Insert;
        Self {
            state,
            register: None,
            replace_log: Vec::new(),
            replacing: false,
            ctx,
            on_yank: None,
        }
    }

    pub fn mode(&self) -> VimMode {
        self.state.mode
    }

    /// The unnamed register (linewise text ends with a newline).
    pub fn register(&self) -> Option<&str> {
        self.register.as_deref()
    }

    /// Escape belongs to Vim unless Normal mode is quiet; then the app may interrupt or clear.
    pub fn consumes_escape(&self) -> bool {
        self.state.mode != VimMode::Normal || self.state.pending()
    }

    /// Active Visual selection as an end-exclusive range, and whether it is linewise.
    pub fn selection(&self, ed: &Editor) -> Option<(Pos, Pos, bool)> {
        let anchor = self.state.anchor.filter(|_| self.state.visual())?;
        let (line, col) = ed.cursor();
        let linewise = self.state.mode == VimMode::VisualLine;
        let buf = VimBuffer {
            lines: ed.lines(),
            cursor_line: line,
            cursor_col: col,
        };
        let (from, to) = visual_range(&buf, anchor, linewise);
        Some((from, to, linewise))
    }

    pub fn indicator(&self, ed: &Editor) -> VimIndicator {
        VimIndicator {
            mode: self.state.mode,
            pending: self.state.pending_text(),
            selected_lines: self.selection(ed).map_or(0, |(f, t, _)| t.line - f.line + 1),
        }
    }

    /// Route one input chunk. True when Vim consumed it; otherwise pass it to [`Editor::handle_input`].
    /// An open autocomplete popup gets the first Escape.
    pub fn handle_input(&mut self, ed: &mut Editor, data: &str) -> bool {
        let canonical = parse_key(data, self.ctx);
        if canonical.as_deref() == Some("escape") {
            return !ed.is_showing_autocomplete() && self.run(ed, "escape");
        }
        if self.state.mode == VimMode::Insert {
            return false;
        }
        let printable = decode_printable_key(data)
            .or_else(|| (!data.is_empty() && !data.chars().any(|c| c.is_control())).then(|| data.to_string()));
        if self.state.mode == VimMode::Replace {
            if matches!(canonical.as_deref(), Some("backspace" | "shift+backspace")) {
                self.replace_backspace(ed);
                return true;
            }
            if let Some(p) = &printable {
                for g in p.graphemes(true) {
                    self.overwrite(ed, g);
                }
                return true;
            }
        }
        let pending = self.state.pending_text();
        let awaiting = pending.ends_with(['f', 'F', 't', 'T', 'r']);
        if awaiting && (canonical.as_deref() == Some("space") || data == " ") {
            return self.run(ed, " ");
        }
        let mapped = canonical.as_deref().and_then(nav_key);
        if awaiting && mapped.is_some() {
            return self.run(ed, "escape");
        }
        if let Some(m) = mapped {
            return self.run(ed, m);
        }
        let Some(printable) = printable else {
            if self.state.pending() {
                self.run(ed, "escape");
            }
            return false;
        };
        for (i, g) in printable.grapheme_indices(true) {
            if self.state.mode == VimMode::Replace {
                self.overwrite(ed, g);
                continue;
            }
            if self.run(ed, g) {
                continue;
            }
            ed.handle_input(&printable[i..]);
            return true;
        }
        true
    }

    fn run(&mut self, ed: &mut Editor, key: &str) -> bool {
        let before = self.state.mode;
        let (line, col) = ed.cursor();
        let cmds = {
            let buf = VimBuffer {
                lines: ed.lines(),
                cursor_line: line,
                cursor_col: col,
            };
            self.state.handle_key(key, &buf)
        };
        let Some(cmds) = cmds else { return false };
        self.apply(ed, &cmds);
        if before != self.state.mode && (before == VimMode::Replace || self.state.mode == VimMode::Replace) {
            self.replace_log.clear();
            self.replacing = false;
        }
        true
    }

    fn atomic_tokens(ed: &Editor, line: usize) -> Vec<(usize, usize)> {
        let mut spans = ed.paste_marker_spans_on(line);
        if let Some(text) = ed.lines().get(line) {
            spans.extend(crate::attachments::placeholder_spans(text));
        }
        spans.sort_unstable();
        spans
    }

    fn token_at(ed: &Editor, line: usize, col: usize) -> Option<(usize, usize)> {
        Self::atomic_tokens(ed, line)
            .into_iter()
            .find(|(s, e)| *s <= col && col < *e)
    }

    fn apply(&mut self, ed: &mut Editor, cmds: &[VimCommand]) {
        for c in cmds {
            if self.state.mode == VimMode::Replace
                && !matches!(
                    c,
                    VimCommand::Move { .. } | VimCommand::Mode { .. } | VimCommand::Yank { .. }
                )
            {
                self.replace_log.clear();
            }
            match c {
                VimCommand::Move { to } => ed.set_cursor(to.line, to.col),
                VimCommand::Mode { .. } => self.replacing = false,
                VimCommand::Yank { from, to, linewise } => {
                    let body = slice_range(ed.lines(), *from, *to, *linewise);
                    let text = if *linewise { format!("{body}\n") } else { body };
                    if !text.is_empty() {
                        self.register = Some(text.clone());
                        let expanded = ed.expand_markers(&text);
                        if let Some(cb) = &mut self.on_yank {
                            cb(&expanded);
                        }
                    }
                }
                VimCommand::Delete { from, to, linewise, .. } => self.delete_range(ed, *from, *to, *linewise),
                VimCommand::OpenLine { below } => {
                    let mut lines = ed.lines().to_vec();
                    let at = ed.cursor().0 + usize::from(*below);
                    lines.insert(at, String::new());
                    ed.apply_edit(lines, (at, 0), true);
                }
                VimCommand::Paste { after, count } => self.paste(ed, *after, *count),
                VimCommand::Undo => ed.undo_edit(),
                VimCommand::Replace { from, to, text } => {
                    let line = &ed.lines()[from.line];
                    let cuts = Self::atomic_tokens(ed, from.line)
                        .iter()
                        .any(|(s, e)| *s < to.col && *e > from.col);
                    if cuts {
                        continue;
                    }
                    let new = format!("{}{text}{}", &line[..from.col], &line[to.col..]);
                    let last = text.grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
                    let mut lines = ed.lines().to_vec();
                    lines[from.line] = new;
                    ed.apply_edit(lines, (from.line, from.col + last), true);
                }
                VimCommand::Join { from_line, lines } => self.join(ed, *from_line, *lines),
                VimCommand::Indent {
                    from_line,
                    to_line,
                    out,
                } => indent(ed, *from_line, *to_line, *out),
            }
        }
        if self.state.mode == VimMode::Normal {
            let (line, col) = ed.cursor();
            let last = last_grapheme_start(&ed.lines()[line]);
            if col > last {
                ed.set_cursor(line, last);
            }
        }
    }

    fn delete_range(&mut self, ed: &mut Editor, from: Pos, to: Pos, linewise: bool) {
        let mut lines = ed.lines().to_vec();
        if linewise {
            let last = to.line.min(lines.len() - 1);
            let removed = slice_range(&lines, from, to, true);
            if removed.is_empty() && from.line == last && lines.len() == 1 {
                return;
            }
            self.register = Some(format!("{removed}\n"));
            lines.drain(from.line..=last);
            if lines.is_empty() {
                lines.push(String::new());
            }
            let line = from.line.min(lines.len() - 1);
            ed.apply_edit(lines, (line, 0), true);
            return;
        }
        let (mut start, mut end) = (from, to);
        if let Some((s, _)) = Self::token_at(ed, start.line, start.col).filter(|(s, _)| *s < start.col) {
            start.col = s;
        }
        if end.col > 0
            && (end.line != start.line || end.col > start.col)
            && let Some((_, e)) = Self::token_at(ed, end.line, end.col - 1).filter(|(_, e)| *e > end.col)
        {
            end.col = e;
        }
        let removed = slice_range(&lines, start, end, false);
        if removed.is_empty() {
            return;
        }
        self.register = Some(removed);
        let last = end.line.min(lines.len() - 1);
        let joined = format!(
            "{}{}",
            &lines[start.line][..start.col],
            &lines[last][end.col.min(lines[last].len())..]
        );
        lines.splice(start.line..=last, [joined]);
        ed.apply_edit(lines, (start.line, start.col), true);
    }

    fn paste(&mut self, ed: &mut Editor, after: bool, count: usize) {
        let Some(entry) = self.register.clone() else { return };
        let mut lines = ed.lines().to_vec();
        let (line, col) = ed.cursor();
        if let Some(body) = entry.strip_suffix('\n') {
            let at = if after { line + 1 } else { line };
            let payload: Vec<String> = (0..count).flat_map(|_| body.split('\n').map(str::to_string)).collect();
            lines.splice(at..at, payload);
            ed.apply_edit(lines, (at, 0), true);
        } else {
            let text = &lines[line];
            let at = if after { next_grapheme_start(text, col) } else { col };
            let payload = entry.repeat(count);
            lines[line] = format!("{}{payload}{}", &text[..at], &text[at..]);
            let last = payload.grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
            ed.apply_edit(lines, (line, at + last), true);
        }
    }

    fn join(&mut self, ed: &mut Editor, from_line: usize, span: usize) {
        let mut lines = ed.lines().to_vec();
        let start = from_line.min(lines.len() - 1);
        let last = (start + span.max(2) - 1).min(lines.len() - 1);
        if last <= start {
            return;
        }
        let mut text = lines[start].clone();
        let mut cursor = text.len();
        for (i, raw) in lines.iter().enumerate().take(last + 1).skip(start + 1) {
            let mut trim = raw.len() - raw.trim_start_matches([' ', '\t']).len();
            if let Some((s, _)) = Self::token_at(ed, i, 0).filter(|(s, _)| *s < trim) {
                trim = s;
            }
            let next = &raw[trim..];
            if i == start + 1 {
                cursor = text.len();
            }
            let gap = if text.is_empty() || next.is_empty() || text.ends_with([' ', '\t']) || next.starts_with(')') {
                ""
            } else if text.ends_with(['.', '!', '?']) {
                "  "
            } else {
                " "
            };
            text = format!("{text}{gap}{next}");
        }
        lines.splice(start..=last, [text]);
        ed.apply_edit(lines, (start, cursor), true);
    }

    fn overwrite(&mut self, ed: &mut Editor, g: &str) {
        let (line_idx, col) = ed.cursor();
        if Self::token_at(ed, line_idx, col).is_some() {
            return;
        }
        let line = ed.lines()[line_idx].clone();
        if !self.replacing {
            self.replace_log.clear();
        }
        let end = if col >= line.len() {
            col
        } else {
            next_grapheme_start(&line, col)
        };
        self.replace_log.push(ReplaceEntry {
            line: line_idx,
            col,
            removed: line[col..end].to_string(),
            text: g.to_string(),
        });
        let mut lines = ed.lines().to_vec();
        lines[line_idx] = format!("{}{g}{}", &line[..col], &line[end..]);
        ed.apply_edit(lines, (line_idx, col + g.len()), !self.replacing);
        self.replacing = true;
    }

    fn replace_backspace(&mut self, ed: &mut Editor) {
        if !self.replacing {
            self.replace_log.clear();
            return;
        }
        let Some(entry) = self.replace_log.pop() else { return };
        let line = ed.lines()[entry.line].clone();
        let still = line.get(entry.col..entry.col + entry.text.len()) == Some(entry.text.as_str());
        let at_cursor = ed.cursor() == (entry.line, entry.col + entry.text.len());
        if !still {
            return;
        }
        if !at_cursor {
            self.replace_log.push(entry);
            return;
        }
        let mut lines = ed.lines().to_vec();
        lines[entry.line] = format!(
            "{}{}{}",
            &line[..entry.col],
            entry.removed,
            &line[entry.col + entry.text.len()..]
        );
        ed.apply_edit(lines, (entry.line, entry.col), false);
    }
}

/// Text in `[from, to)`; linewise ranges take whole lines.
fn slice_range(lines: &[String], from: Pos, to: Pos, linewise: bool) -> String {
    let get = |i: usize| lines.get(i).map_or("", String::as_str);
    if linewise {
        let last = to.line.min(lines.len().saturating_sub(1));
        return lines[from.line..=last].join("\n");
    }
    let clamp = |s: &str, c: usize| c.min(s.len());
    if from.line == to.line {
        let l = get(from.line);
        return l[clamp(l, from.col)..clamp(l, to.col).max(clamp(l, from.col))].to_string();
    }
    let first = get(from.line);
    let mut parts = vec![first[clamp(first, from.col)..].to_string()];
    for i in from.line + 1..to.line {
        parts.push(get(i).to_string());
    }
    let last = get(to.line);
    parts.push(last[..clamp(last, to.col)].to_string());
    parts.join("\n")
}

/// `>`/`<`: two spaces (a tab for tab-indented lines); empty lines stay empty.
fn indent(ed: &mut Editor, from_line: usize, to_line: usize, out: bool) {
    let mut lines = ed.lines().to_vec();
    let first = from_line.min(lines.len() - 1);
    let last = to_line.min(lines.len() - 1).max(first);
    let mut changed = false;
    for line in &mut lines[first..=last] {
        let next = if !out {
            if line.is_empty() {
                line.clone()
            } else if line.starts_with('\t') {
                format!("\t{line}")
            } else {
                format!("  {line}")
            }
        } else if let Some(rest) = line.strip_prefix('\t') {
            rest.to_string()
        } else {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            line[spaces.min(2)..].to_string()
        };
        changed |= next != *line;
        *line = next;
    }
    if !changed {
        return;
    }
    let col = lines[first].len() - lines[first].trim_start().len();
    ed.apply_edit(lines, (first, col), true);
}
