//! Modal editing state machine (port of omp packages/tui/src/vim.ts).
//!
//! Pure: it reads a snapshot of the buffer and returns the edits it wants applied, so the editor keeps sole
//! ownership of undo, atomic placeholder tokens and change notification ([`crate::vim_editor`] applies them).
//! Columns are UTF-8 byte offsets (omp uses UTF-16 offsets; positions map one to one on grapheme boundaries).

use std::sync::OnceLock;

use fancy_regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
    Replace,
}

impl VimMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Normal => "normal",
            Self::Visual => "visual",
            Self::VisualLine => "visual-line",
            Self::Replace => "replace",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

pub const fn pos(line: usize, col: usize) -> Pos {
    Pos { line, col }
}

/// Read-only view of the buffer.
#[derive(Clone, Copy, Debug)]
pub struct VimBuffer<'a> {
    pub lines: &'a [String],
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl VimBuffer<'_> {
    fn line(&self, i: usize) -> &str {
        self.lines.get(i).map_or("", String::as_str)
    }
    fn cursor(&self) -> Pos {
        pos(self.cursor_line, self.cursor_col)
    }
}

/// An edit for the editor to apply. `to` is always an exclusive end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VimCommand {
    Move {
        to: Pos,
    },
    Mode {
        mode: VimMode,
    },
    Yank {
        from: Pos,
        to: Pos,
        linewise: bool,
    },
    Delete {
        from: Pos,
        to: Pos,
        linewise: bool,
        insert: bool,
    },
    OpenLine {
        below: bool,
    },
    Paste {
        after: bool,
        count: usize,
    },
    Undo,
    Replace {
        from: Pos,
        to: Pos,
        text: String,
    },
    Join {
        from_line: usize,
        lines: usize,
    },
    Indent {
        from_line: usize,
        to_line: usize,
        out: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operator {
    Delete,
    Yank,
    Change,
    IndentIn,
    IndentOut,
}

impl Operator {
    fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Yank => 'y',
            Self::Change => 'c',
            Self::IndentIn => '>',
            Self::IndentOut => '<',
        }
    }
    fn from_key(k: &str) -> Option<Self> {
        Some(match k {
            "d" => Self::Delete,
            "y" => Self::Yank,
            "c" => Self::Change,
            ">" => Self::IndentIn,
            "<" => Self::IndentOut,
            _ => return None,
        })
    }
}

/// Start of the grapheme after `col`, clamped to the line length.
pub fn next_grapheme_start(text: &str, col: usize) -> usize {
    if col >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .find(|(i, _)| *i >= col)
        .map_or(text.len(), |(i, g)| (i + g.len()).min(text.len()))
}

/// Start of the grapheme before `col`, clamped to 0.
pub fn prev_grapheme_start(text: &str, col: usize) -> usize {
    if col == 0 {
        return 0;
    }
    text.grapheme_indices(true)
        .take_while(|(i, _)| *i < col)
        .last()
        .map_or(0, |(i, _)| i)
}

/// Start of the final grapheme: where a Normal-mode cursor rests.
pub fn last_grapheme_start(text: &str) -> usize {
    prev_grapheme_start(text, text.len())
}

fn first_non_blank(text: &str) -> usize {
    text.grapheme_indices(true)
        .find(|(_, g)| !g.trim().is_empty())
        .map_or(0, |(i, _)| i)
}

fn is_space_at(text: &str, i: usize) -> bool {
    text[i..].chars().next().is_some_and(char::is_whitespace)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavKind {
    Whitespace,
    Word,
    Delimiter,
    Cjk,
    Other,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap_or_else(|_| Regex::new("^$").unwrap_or_else(|_| unreachable!())))
}

/// omp `getWordNavKind`.
fn nav_kind(g: &str) -> NavKind {
    static DELIM: OnceLock<Regex> = OnceLock::new();
    static CJK: OnceLock<Regex> = OnceLock::new();
    static WORD: OnceLock<Regex> = OnceLock::new();
    let Some(c) = g.chars().next() else {
        return NavKind::Other;
    };
    let s = c.to_string();
    let m = |r: &Regex| r.is_match(&s).unwrap_or(false);
    if c.is_whitespace() {
        NavKind::Whitespace
    } else if c == '_' {
        NavKind::Word
    } else if m(re(&DELIM, r"^[\p{P}\p{S}]$")) {
        NavKind::Delimiter
    } else if m(re(&CJK, r"^[\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}]$")) {
        NavKind::Cjk
    } else if m(re(&WORD, r"^[\p{L}\p{N}]$")) {
        NavKind::Word
    } else {
        NavKind::Other
    }
}

fn is_joiner(g: &str) -> bool {
    matches!(g.chars().next(), Some('\'' | '’' | '-' | '‐' | '‑'))
}

/// omp `moveWordLeft`.
pub fn move_word_left(text: &str, cursor: usize) -> usize {
    let mut i = cursor.min(text.len());
    if i == 0 {
        return 0;
    }
    let mut gs: Vec<&str> = text[..i].graphemes(true).collect();
    while gs.last().is_some_and(|g| nav_kind(g) == NavKind::Whitespace) {
        i -= gs.pop().map_or(0, str::len);
    }
    let Some(last) = gs.last() else { return i };
    let kind = nav_kind(last);
    if kind == NavKind::Delimiter || kind == NavKind::Cjk {
        while gs.last().is_some_and(|g| nav_kind(g) == kind) {
            i -= gs.pop().map_or(0, str::len);
        }
        return i;
    }
    if kind == NavKind::Word {
        let mut has_right_word = false;
        while let Some(g) = gs.last().copied() {
            let k = nav_kind(g);
            if k == NavKind::Word {
                has_right_word = true;
                i -= gs.pop().map_or(0, str::len);
                continue;
            }
            if has_right_word
                && k == NavKind::Delimiter
                && is_joiner(g)
                && gs.len() >= 2
                && nav_kind(gs[gs.len() - 2]) == NavKind::Word
            {
                i -= gs.pop().map_or(0, str::len);
                continue;
            }
            break;
        }
        return i;
    }
    i.saturating_sub(gs.pop().map_or(0, str::len))
}

/// omp `moveWordRight`.
pub fn move_word_right(text: &str, cursor: usize) -> usize {
    let mut i = cursor.min(text.len());
    if i == text.len() {
        return i;
    }
    let gs: Vec<&str> = text[i..].graphemes(true).collect();
    let mut k = 0;
    while k < gs.len() && nav_kind(gs[k]) == NavKind::Whitespace {
        i += gs[k].len();
        k += 1;
    }
    if k == gs.len() {
        return i;
    }
    let first = nav_kind(gs[k]);
    if first == NavKind::Delimiter || first == NavKind::Cjk {
        while k < gs.len() && nav_kind(gs[k]) == first {
            i += gs[k].len();
            k += 1;
        }
        return i;
    }
    if first == NavKind::Word {
        let mut has_left_word = false;
        while k < gs.len() {
            let kind = nav_kind(gs[k]);
            if kind == NavKind::Word {
                has_left_word = true;
                i += gs[k].len();
                k += 1;
                continue;
            }
            if has_left_word
                && kind == NavKind::Delimiter
                && is_joiner(gs[k])
                && gs.get(k + 1).is_some_and(|n| nav_kind(n) == NavKind::Word)
            {
                i += gs[k].len();
                k += 1;
                continue;
            }
            break;
        }
        return i;
    }
    i + gs[k].len()
}

fn word_forward(text: &str, col: usize) -> usize {
    let mut next = move_word_right(text, col);
    while next < text.len() && is_space_at(text, next) {
        next += text[next..].chars().next().map_or(1, char::len_utf8);
    }
    next
}

fn word_end(text: &str, col: usize) -> usize {
    let mut from = next_grapheme_start(text, col);
    while from < text.len() && is_space_at(text, from) {
        from += text[from..].chars().next().map_or(1, char::len_utf8);
    }
    let end = move_word_right(text, from);
    col.max(prev_grapheme_start(text, end))
}

fn char_class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

struct Chunk {
    start: usize,
    end: usize,
    space: bool,
}

fn chunk_line(text: &str, big: bool) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    for (i, c) in text.char_indices() {
        let cls = char_class(c, big);
        match chunks.last_mut() {
            Some(last) if char_class(text[last.start..].chars().next().unwrap_or(' '), big) == cls => {
                last.end = i + c.len_utf8();
            }
            _ => chunks.push(Chunk {
                start: i,
                end: i + c.len_utf8(),
                space: cls == 0,
            }),
        }
    }
    chunks
}

fn word_object(text: &str, col: usize, around: bool, big: bool, count: usize) -> Option<(usize, usize)> {
    let chunks = chunk_line(text, big);
    if chunks.is_empty() {
        return None;
    }
    let last = chunks.len() - 1;
    let mut first = chunks.iter().position(|c| col < c.end).unwrap_or(last);
    let mut end = first;
    if !around {
        end = (first + count - 1).min(last);
    } else if chunks[first].space {
        end = (first + 2 * count - 1).min(last);
    } else {
        for n in 0..count {
            if n > 0 && end < last {
                end += 1;
            }
            if end < last && chunks[end + 1].space {
                end += 1;
            }
        }
        if !chunks[end].space && first > 0 && chunks[first - 1].space {
            first -= 1;
        }
    }
    Some((chunks[first].start, chunks[end].end))
}

fn quote_marks(text: &str, quote: char) -> Vec<usize> {
    let mut marks = Vec::new();
    let mut it = text.char_indices();
    while let Some((i, c)) = it.next() {
        if c == '\\' {
            it.next();
        } else if c == quote {
            marks.push(i);
        }
    }
    marks
}

fn quote_object(text: &str, col: usize, quote: char, around: bool) -> Option<(usize, usize)> {
    let marks = quote_marks(text, quote);
    for pair in marks.as_chunks::<2>().0 {
        let (open, close) = (pair[0], pair[1]);
        if col > close {
            continue;
        }
        if !around {
            return Some((open + 1, close));
        }
        let mut end = close + 1;
        let mut start = open;
        while let Some(c) = text[end..].chars().next().filter(|c| c.is_whitespace()) {
            end += c.len_utf8();
        }
        if end == close + 1 {
            while let Some(c) = text[..start].chars().next_back().filter(|c| c.is_whitespace()) {
                start -= c.len_utf8();
            }
        }
        return Some((start, end));
    }
    None
}

fn flatten(buf: &VimBuffer) -> (String, usize) {
    let cursor = buf.cursor_col + (0..buf.cursor_line).map(|i| buf.line(i).len() + 1).sum::<usize>();
    (buf.lines.join("\n"), cursor)
}

fn offset_to_pos(lines: &[String], offset: usize) -> Pos {
    let mut remaining = offset;
    for (line, text) in lines.iter().enumerate() {
        if remaining <= text.len() {
            return pos(line, remaining);
        }
        remaining -= text.len() + 1;
    }
    let line = lines.len().saturating_sub(1);
    pos(line, lines.get(line).map_or(0, String::len))
}

struct Range {
    from: Pos,
    to: Pos,
    linewise: bool,
}

fn bracket_object(buf: &VimBuffer, open: u8, close: u8, around: bool) -> Option<Range> {
    let (text, cursor) = flatten(buf);
    let b = text.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut depth = 0usize;
    let mut open_at = None;
    let mut i = cursor.min(b.len() - 1) as isize;
    while i >= 0 {
        let ch = b[i as usize];
        if ch == close && i as usize != cursor {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                open_at = Some(i as usize);
                break;
            }
            depth -= 1;
        }
        i -= 1;
    }
    let open_at = open_at?;
    depth = 0;
    let mut close_at = None;
    for (j, &ch) in b.iter().enumerate().skip(open_at + 1) {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                close_at = Some(j);
                break;
            }
            depth -= 1;
        }
    }
    let close_at = close_at?;
    Some(Range {
        from: offset_to_pos(buf.lines, if around { open_at } else { open_at + 1 }),
        to: offset_to_pos(buf.lines, if around { close_at + 1 } else { close_at }),
        linewise: false,
    })
}

fn paragraph_object(buf: &VimBuffer, around: bool) -> Range {
    let last = buf.lines.len().saturating_sub(1);
    let blank = |l: usize| buf.line(l).trim().is_empty();
    let target = blank(buf.cursor_line);
    let (mut first, mut end) = (buf.cursor_line, buf.cursor_line);
    while first > 0 && blank(first - 1) == target {
        first -= 1;
    }
    while end < last && blank(end + 1) == target {
        end += 1;
    }
    if around {
        let stop = end;
        while end < last && blank(end + 1) != target {
            end += 1;
        }
        if end == stop {
            while first > 0 && blank(first - 1) != target {
                first -= 1;
            }
        }
    }
    Range {
        from: pos(first, 0),
        to: pos(end, buf.line(end).len()),
        linewise: true,
    }
}

fn bracket_match(c: u8) -> Option<(u8, bool)> {
    Some(match c {
        b'(' => (b')', true),
        b')' => (b'(', false),
        b'[' => (b']', true),
        b']' => (b'[', false),
        b'{' => (b'}', true),
        b'}' => (b'{', false),
        _ => return None,
    })
}

fn quote_mate(marks: &[usize], col: usize) -> Option<usize> {
    marks.as_chunks::<2>().0.iter().find_map(|p| {
        if col == p[0] {
            Some(p[1])
        } else if col == p[1] {
            Some(p[0])
        } else {
            None
        }
    })
}

fn match_delimiter(buf: &VimBuffer) -> Option<Pos> {
    let line = buf.line(buf.cursor_line);
    let lb = line.as_bytes();
    let dq = quote_marks(line, '"');
    let sq = quote_marks(line, '\'');
    let quote_at = |c: usize| {
        if dq.contains(&c) {
            Some(&dq)
        } else if sq.contains(&c) {
            Some(&sq)
        } else {
            None
        }
    };
    let mut col = buf.cursor_col;
    while col < lb.len() && bracket_match(lb[col]).is_none() && quote_at(col).is_none() {
        col += 1;
    }
    if let Some(marks) = quote_at(col) {
        return quote_mate(marks, col).map(|m| pos(buf.cursor_line, m));
    }
    let (mate, forward) = bracket_match(*lb.get(col)?)?;
    let (text, cursor) = flatten(&VimBuffer {
        lines: buf.lines,
        cursor_line: buf.cursor_line,
        cursor_col: col,
    });
    let b = text.as_bytes();
    let (open, close) = if forward { (lb[col], mate) } else { (mate, lb[col]) };
    let mut depth = 0usize;
    if forward {
        for (i, &ch) in b.iter().enumerate().skip(cursor + 1) {
            if ch == open {
                depth += 1;
            } else if ch == close {
                if depth == 0 {
                    return Some(offset_to_pos(buf.lines, i));
                }
                depth -= 1;
            }
        }
    } else {
        for i in (0..cursor).rev() {
            let ch = b[i];
            if ch == close {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    return Some(offset_to_pos(buf.lines, i));
                }
                depth -= 1;
            }
        }
    }
    None
}

fn paragraph_boundary(lines: &[String], line: usize, forward: bool) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let last = lines.len() - 1;
    let blank = |l: usize| lines[l].trim().is_empty();
    let mut i = line.min(last);
    if forward {
        if i < last && !blank(i) {
            while i < last && !blank(i + 1) {
                i += 1;
            }
            return if i < last { i + 1 } else { last };
        }
        while i < last && blank(i) {
            i += 1;
        }
        while i < last && !blank(i) {
            i += 1;
        }
        return i;
    }
    if i > 0 && !blank(i) {
        while i > 0 && !blank(i - 1) {
            i -= 1;
        }
        return i.saturating_sub(1);
    }
    while i > 0 && blank(i) {
        i -= 1;
    }
    while i > 0 && !blank(i - 1) {
        i -= 1;
    }
    i.saturating_sub(1)
}

/// Column of the `count`th grapheme equal to `target`, strictly after (or before) `from`.
fn find_grapheme(text: &str, from: usize, target: &str, forward: bool, count: usize) -> Option<usize> {
    if count < 1 || target.is_empty() {
        return None;
    }
    let segs: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let here = segs.iter().rposition(|(i, _)| *i <= from);
    let mut seen = 0;
    if forward {
        let start = here.map_or(0, |h| h + 1);
        for (i, g) in &segs[start.min(segs.len())..] {
            if *g == target {
                seen += 1;
                if seen == count {
                    return Some(*i);
                }
            }
        }
    } else if let Some(h) = here {
        for (i, g) in segs[..h].iter().rev() {
            if *g == target {
                seen += 1;
                if seen == count {
                    return Some(*i);
                }
            }
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Motion {
    to: Pos,
    inclusive: bool,
    linewise: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FindSpec {
    forward: bool,
    till: bool,
}

/// Desired column for `j`/`k`; `Eol` is `$`'s sticky end of line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Desired {
    Col(usize),
    Eol,
}

/// The modal state.
#[derive(Clone)]
pub struct VimState {
    pub mode: VimMode,
    /// Fixed end of a Visual selection; the cursor is the moving end.
    pub anchor: Option<Pos>,
    count: String,
    operator: Option<Operator>,
    pending_g: bool,
    text_object: Option<char>,
    find_pending: Option<FindSpec>,
    last_find: Option<(FindSpec, String)>,
    replace_char: bool,
    desired: Option<Desired>,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            mode: VimMode::Normal,
            anchor: None,
            count: String::new(),
            operator: None,
            pending_g: false,
            text_object: None,
            find_pending: None,
            last_find: None,
            replace_char: false,
            desired: None,
        }
    }
}

impl VimState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A count, operator, `g`, text object, find or `r` prefix is half-typed.
    pub fn pending(&self) -> bool {
        !self.count.is_empty()
            || self.operator.is_some()
            || self.pending_g
            || self.text_object.is_some()
            || self.find_pending.is_some()
            || self.replace_char
    }

    /// The half-typed command as Vim echoes it (`2d`, `di`, `df`, `r`).
    pub fn pending_text(&self) -> String {
        let find = match self.find_pending {
            None => "",
            Some(FindSpec {
                forward: true,
                till: true,
            }) => "t",
            Some(FindSpec {
                forward: false,
                till: true,
            }) => "T",
            Some(FindSpec {
                forward: true,
                till: false,
            }) => "f",
            Some(FindSpec {
                forward: false,
                till: false,
            }) => "F",
        };
        format!(
            "{}{}{}{}{}{}",
            self.count,
            self.operator.map(|o| o.key().to_string()).unwrap_or_default(),
            if self.pending_g { "g" } else { "" },
            self.text_object.map(String::from).unwrap_or_default(),
            find,
            if self.replace_char { "r" } else { "" }
        )
    }

    pub fn visual(&self) -> bool {
        matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn clear_pending(&mut self) {
        self.count.clear();
        self.operator = None;
        self.pending_g = false;
        self.text_object = None;
        self.find_pending = None;
        self.replace_char = false;
    }

    fn peek_count(&self) -> usize {
        self.count.parse::<usize>().unwrap_or(1)
    }

    fn take_count(&mut self) -> usize {
        let c = self.peek_count().max(1);
        self.count.clear();
        c
    }

    fn clamp_normal(buf: &VimBuffer, p: Pos) -> Pos {
        let line = p.line.min(buf.lines.len().saturating_sub(1));
        pos(line, p.col.min(last_grapheme_start(buf.line(line))))
    }

    /// Handle one key: `"escape"` or a single grapheme. `None` when the key is not Vim's (the editor handles it).
    pub fn handle_key(&mut self, key: &str, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        if key == "escape" {
            return self.handle_escape(buf);
        }
        if self.mode == VimMode::Insert {
            return None;
        }
        if let Some(spec) = self.find_pending.take() {
            self.desired = None;
            self.last_find = Some((spec, key.to_string()));
            let count = self.peek_count().max(1);
            return Some(match self.seek(key, buf, spec, count, buf.cursor_col) {
                None => {
                    self.operator = None;
                    self.count.clear();
                    vec![]
                }
                Some(m) => self.apply_motion(buf, m),
            });
        }
        if self.replace_char {
            self.replace_char = false;
            let count = self.peek_count().max(1);
            let line = buf.line(buf.cursor_line);
            let end = line
                .grapheme_indices(true)
                .filter(|(i, _)| *i >= buf.cursor_col)
                .nth(count - 1)
                .map(|(i, g)| i + g.len());
            self.count.clear();
            let Some(end) = end else { return Some(vec![]) };
            return Some(vec![VimCommand::Replace {
                from: buf.cursor(),
                to: pos(buf.cursor_line, end),
                text: key.repeat(count),
            }]);
        }
        let is_digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit();
        if is_digit && (key != "0" || !self.count.is_empty()) {
            self.count.push_str(key);
            return Some(vec![]);
        }
        if self.pending_g {
            self.pending_g = false;
            if key != "g" {
                self.clear_pending();
                return Some(vec![]);
            }
            self.desired = None;
            let line = (self.take_count() - 1).min(buf.lines.len().saturating_sub(1));
            return Some(self.apply_motion(
                buf,
                Motion {
                    to: pos(line, 0),
                    inclusive: false,
                    linewise: true,
                },
            ));
        }
        if let Some(kind) = self.text_object {
            self.desired = None;
            return Some(self.apply_text_object(kind, key, buf));
        }
        if (key == "i" || key == "a") && (self.operator.is_some() || self.visual()) {
            self.text_object = key.chars().next();
            return Some(vec![]);
        }
        if matches!(key, "f" | "F" | "t" | "T") {
            self.desired = None;
            self.find_pending = Some(FindSpec {
                forward: key == "f" || key == "t",
                till: key == "t" || key == "T",
            });
            return Some(vec![]);
        }
        if (key == "r" || key == "R") && self.operator.is_none() && !self.visual() {
            self.desired = None;
            if key == "R" {
                self.take_count();
                self.mode = VimMode::Replace;
                return Some(vec![VimCommand::Mode { mode: VimMode::Replace }]);
            }
            self.replace_char = true;
            return Some(vec![]);
        }
        if key != "j" && key != "k" {
            self.desired = None;
        }
        if let Some(m) = self.resolve_motion(key, buf) {
            return Some(self.apply_motion(buf, m));
        }
        if self.visual() {
            Some(self.handle_visual_key(key, buf))
        } else {
            Some(self.handle_normal_key(key, buf))
        }
    }

    fn handle_escape(&mut self, buf: &VimBuffer) -> Option<Vec<VimCommand>> {
        if self.pending() {
            self.clear_pending();
            return Some(vec![]);
        }
        if self.visual() {
            self.mode = VimMode::Normal;
            self.anchor = None;
            return Some(vec![
                VimCommand::Mode { mode: VimMode::Normal },
                VimCommand::Move {
                    to: Self::clamp_normal(buf, buf.cursor()),
                },
            ]);
        }
        if matches!(self.mode, VimMode::Insert | VimMode::Replace) {
            self.mode = VimMode::Normal;
            let text = buf.line(buf.cursor_line);
            return Some(vec![
                VimCommand::Mode { mode: VimMode::Normal },
                VimCommand::Move {
                    to: pos(buf.cursor_line, prev_grapheme_start(text, buf.cursor_col)),
                },
            ]);
        }
        None
    }

    fn resolve_motion(&mut self, key: &str, buf: &VimBuffer) -> Option<Motion> {
        let count = self.peek_count();
        let line = buf.line(buf.cursor_line);
        let at = |col| pos(buf.cursor_line, col);
        let charwise = |to| Motion {
            to,
            inclusive: false,
            linewise: false,
        };
        let key = if key == " " { "l" } else { key };
        Some(match key {
            "h" => charwise(at((0..count).fold(buf.cursor_col, |c, _| prev_grapheme_start(line, c)))),
            "l" => charwise(at((0..count).fold(buf.cursor_col, |c, _| next_grapheme_start(line, c)))),
            "j" | "k" => {
                let target = if key == "j" {
                    (buf.cursor_line + count).min(buf.lines.len().saturating_sub(1))
                } else {
                    buf.cursor_line.saturating_sub(count)
                };
                let desired = *self.desired.get_or_insert(Desired::Col(buf.cursor_col));
                let col = match desired {
                    Desired::Col(c) => c,
                    Desired::Eol => usize::MAX,
                };
                Motion {
                    to: pos(target, col),
                    inclusive: false,
                    linewise: true,
                }
            }
            "0" => charwise(at(0)),
            "^" => charwise(at(first_non_blank(line))),
            "$" => {
                self.desired = Some(Desired::Eol);
                charwise(at(line.len()))
            }
            "w" => {
                let mut col = buf.cursor_col;
                if self.operator == Some(Operator::Change) && !is_space_at(line, col) {
                    for _ in 0..count {
                        col = word_end(line, col);
                    }
                    return Some(Motion {
                        to: at(col),
                        inclusive: true,
                        linewise: false,
                    });
                }
                for _ in 0..count {
                    col = word_forward(line, col);
                }
                charwise(at(col))
            }
            "b" => charwise(at((0..count).fold(buf.cursor_col, |c, _| move_word_left(line, c)))),
            "e" => Motion {
                to: at((0..count).fold(buf.cursor_col, |c, _| word_end(line, c))),
                inclusive: true,
                linewise: false,
            },
            "%" => {
                if !self.count.is_empty() {
                    if count > 100 {
                        return None;
                    }
                    let target = ((count * buf.lines.len()).div_ceil(100)).saturating_sub(1);
                    return Some(Motion {
                        to: pos(target, first_non_blank(buf.line(target))),
                        inclusive: false,
                        linewise: true,
                    });
                }
                Motion {
                    to: match_delimiter(buf)?,
                    inclusive: true,
                    linewise: false,
                }
            }
            "{" | "}" => {
                let mut l = buf.cursor_line;
                for _ in 0..count {
                    let next = paragraph_boundary(buf.lines, l, key == "}");
                    if next == l {
                        break;
                    }
                    l = next;
                }
                let text = buf.line(l);
                let at_end = key == "}" && l + 1 == buf.lines.len() && !text.trim().is_empty();
                charwise(pos(l, if at_end { text.len() } else { 0 }))
            }
            "G" => {
                let last = buf.lines.len().saturating_sub(1);
                let target = if self.count.is_empty() { last } else { count - 1 };
                Motion {
                    to: pos(target.min(last), 0),
                    inclusive: false,
                    linewise: true,
                }
            }
            ";" | "," => return self.repeat_find(key == ";", buf, count),
            _ => return None,
        })
    }

    fn repeat_find(&self, same: bool, buf: &VimBuffer, count: usize) -> Option<Motion> {
        let (spec, target) = self.last_find.clone()?;
        let forward = if same { spec.forward } else { !spec.forward };
        let text = buf.line(buf.cursor_line);
        let from = if spec.till {
            if forward {
                next_grapheme_start(text, buf.cursor_col)
            } else {
                prev_grapheme_start(text, buf.cursor_col)
            }
        } else {
            buf.cursor_col
        };
        self.seek(
            &target,
            buf,
            FindSpec {
                forward,
                till: spec.till,
            },
            count,
            from,
        )
    }

    fn seek(&self, target: &str, buf: &VimBuffer, spec: FindSpec, count: usize, from: usize) -> Option<Motion> {
        let text = buf.line(buf.cursor_line);
        let m = find_grapheme(text, from, target, spec.forward, count)?;
        let land = match (spec.till, spec.forward) {
            (true, true) => prev_grapheme_start(text, m),
            (true, false) => next_grapheme_start(text, m),
            _ => m,
        };
        (land != buf.cursor_col).then_some(Motion {
            to: pos(buf.cursor_line, land),
            inclusive: true,
            linewise: false,
        })
    }

    fn apply_motion(&mut self, buf: &VimBuffer, m: Motion) -> Vec<VimCommand> {
        let op = self.operator.take();
        self.take_count();
        let to_line = m.to.line.min(buf.lines.len().saturating_sub(1));
        let to = pos(to_line, m.to.col.min(buf.line(to_line).len()));
        if let Some(op @ (Operator::IndentIn | Operator::IndentOut)) = op {
            return vec![VimCommand::Indent {
                from_line: buf.cursor_line.min(m.to.line),
                to_line: buf.cursor_line.max(m.to.line),
                out: op == Operator::IndentOut,
            }];
        }
        let Some(op) = op else {
            let to = if self.visual() || self.mode == VimMode::Replace {
                to
            } else {
                Self::clamp_normal(buf, to)
            };
            return vec![VimCommand::Move { to }];
        };
        let (mut start, mut end) = (buf.cursor(), m.to);
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        if m.inclusive {
            end = pos(end.line, next_grapheme_start(buf.line(end.line), end.col));
        }
        self.operate(op, start, end, m.linewise)
    }

    fn operate(&mut self, op: Operator, from: Pos, to: Pos, linewise: bool) -> Vec<VimCommand> {
        match op {
            Operator::IndentIn | Operator::IndentOut => vec![VimCommand::Indent {
                from_line: from.line.min(to.line),
                to_line: from.line.max(to.line),
                out: op == Operator::IndentOut,
            }],
            Operator::Yank => vec![VimCommand::Yank { from, to, linewise }, VimCommand::Move { to: from }],
            Operator::Delete => vec![VimCommand::Delete {
                from,
                to,
                linewise,
                insert: false,
            }],
            Operator::Change => {
                self.mode = VimMode::Insert;
                vec![
                    VimCommand::Move { to: from },
                    VimCommand::Delete {
                        from,
                        to,
                        linewise: false,
                        insert: true,
                    },
                    VimCommand::Mode { mode: VimMode::Insert },
                ]
            }
        }
    }

    fn resolve_text_object(&self, key: &str, around: bool, buf: &VimBuffer, count: usize) -> Option<Range> {
        let line = buf.line(buf.cursor_line);
        let on_line = |r: Option<(usize, usize)>| {
            r.map(|(a, b)| Range {
                from: pos(buf.cursor_line, a),
                to: pos(buf.cursor_line, b),
                linewise: false,
            })
        };
        match key {
            "w" | "W" => on_line(word_object(line, buf.cursor_col, around, key == "W", count)),
            "\"" | "'" | "`" => on_line(quote_object(line, buf.cursor_col, key.chars().next()?, around)),
            "(" | ")" | "b" => bracket_object(buf, b'(', b')', around),
            "[" | "]" => bracket_object(buf, b'[', b']', around),
            "{" | "}" | "B" => bracket_object(buf, b'{', b'}', around),
            "<" | ">" => bracket_object(buf, b'<', b'>', around),
            "p" => Some(paragraph_object(buf, around)),
            _ => None,
        }
    }

    fn apply_text_object(&mut self, kind: char, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let around = kind == 'a';
        let op = self.operator.take();
        self.text_object = None;
        let count = self.take_count();
        let Some(range) = self.resolve_text_object(key, around, buf, count) else {
            return vec![];
        };
        if let Some(op) = op {
            return self.operate(op, range.from, range.to, range.linewise);
        }
        self.anchor = Some(range.from);
        let mut cmds = Vec::new();
        if range.linewise && self.mode == VimMode::Visual {
            self.mode = VimMode::VisualLine;
            cmds.push(VimCommand::Mode { mode: self.mode });
        }
        let to = if range.to.col > 0 {
            pos(
                range.to.line,
                prev_grapheme_start(buf.line(range.to.line), range.to.col),
            )
        } else if range.to.line > range.from.line {
            pos(range.to.line - 1, last_grapheme_start(buf.line(range.to.line - 1)))
        } else {
            range.from
        };
        cmds.push(VimCommand::Move { to });
        cmds
    }

    fn handle_normal_key(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let line = buf.line(buf.cursor_line);
        let count = self.peek_count();
        let insert = |s: &mut Self| {
            s.take_count();
            s.mode = VimMode::Insert;
        };
        match key {
            "g" => {
                self.pending_g = true;
                vec![]
            }
            "i" => {
                insert(self);
                vec![VimCommand::Mode { mode: VimMode::Insert }]
            }
            "a" | "I" | "A" => {
                insert(self);
                let col = match key {
                    "a" => next_grapheme_start(line, buf.cursor_col),
                    "I" => first_non_blank(line),
                    _ => line.len(),
                };
                vec![
                    VimCommand::Mode { mode: VimMode::Insert },
                    VimCommand::Move {
                        to: pos(buf.cursor_line, col),
                    },
                ]
            }
            "o" | "O" => {
                insert(self);
                vec![
                    VimCommand::OpenLine { below: key == "o" },
                    VimCommand::Mode { mode: VimMode::Insert },
                ]
            }
            "v" | "V" => {
                self.take_count();
                self.mode = if key == "v" {
                    VimMode::Visual
                } else {
                    VimMode::VisualLine
                };
                self.anchor = Some(buf.cursor());
                vec![VimCommand::Mode { mode: self.mode }]
            }
            "x" => {
                self.take_count();
                let mut col = buf.cursor_col;
                for _ in 0..count {
                    if col >= line.len() {
                        break;
                    }
                    col = next_grapheme_start(line, col);
                }
                if col == buf.cursor_col {
                    return vec![];
                }
                vec![VimCommand::Delete {
                    from: buf.cursor(),
                    to: pos(buf.cursor_line, col),
                    linewise: false,
                    insert: false,
                }]
            }
            "D" | "C" => {
                let span = self.take_count();
                let last = (buf.cursor_line + span - 1).min(buf.lines.len().saturating_sub(1));
                let op = if key == "C" { Operator::Change } else { Operator::Delete };
                self.operate(op, buf.cursor(), pos(last, buf.line(last).len()), false)
            }
            "d" | "y" | "c" => {
                let op = Operator::from_key(key).unwrap_or(Operator::Delete);
                if self.operator == Some(op) {
                    let span = self.take_count();
                    let last = (buf.cursor_line + span - 1).min(buf.lines.len().saturating_sub(1));
                    self.operator = None;
                    return self.operate(op, pos(buf.cursor_line, 0), pos(last, buf.line(last).len()), true);
                }
                self.operator = Some(op);
                vec![]
            }
            "p" | "P" => vec![VimCommand::Paste {
                after: key == "p",
                count: self.take_count(),
            }],
            "u" => {
                self.take_count();
                vec![VimCommand::Undo]
            }
            "J" => {
                if self.operator.is_some() {
                    self.clear_pending();
                    return vec![];
                }
                vec![VimCommand::Join {
                    from_line: buf.cursor_line,
                    lines: self.take_count().max(2),
                }]
            }
            ">" | "<" => {
                let op = Operator::from_key(key).unwrap_or(Operator::IndentIn);
                if self.operator == Some(op) {
                    let span = self.take_count();
                    let last = (buf.cursor_line + span - 1).min(buf.lines.len().saturating_sub(1));
                    self.operator = None;
                    return vec![VimCommand::Indent {
                        from_line: buf.cursor_line,
                        to_line: last,
                        out: key == "<",
                    }];
                }
                if self.operator.is_some() {
                    self.clear_pending();
                    return vec![];
                }
                self.operator = Some(op);
                vec![]
            }
            _ => {
                self.clear_pending();
                vec![]
            }
        }
    }

    fn handle_visual_key(&mut self, key: &str, buf: &VimBuffer) -> Vec<VimCommand> {
        let anchor = self.anchor.unwrap_or(buf.cursor());
        let linewise = self.mode == VimMode::VisualLine;
        match key {
            "v" | "V" => {
                let next = if key == "v" {
                    VimMode::Visual
                } else {
                    VimMode::VisualLine
                };
                if self.mode == next {
                    self.mode = VimMode::Normal;
                    self.anchor = None;
                    return vec![
                        VimCommand::Mode { mode: VimMode::Normal },
                        VimCommand::Move {
                            to: Self::clamp_normal(buf, buf.cursor()),
                        },
                    ];
                }
                self.mode = next;
                vec![VimCommand::Mode { mode: next }]
            }
            "o" => {
                self.anchor = Some(buf.cursor());
                vec![VimCommand::Move { to: anchor }]
            }
            "y" | "d" | "x" | "c" | "s" => {
                let op = match key {
                    "y" => Operator::Yank,
                    "c" | "s" => Operator::Change,
                    _ => Operator::Delete,
                };
                let (from, to) = visual_range(buf, anchor, linewise);
                self.clear_pending();
                self.anchor = None;
                if op != Operator::Change {
                    self.mode = VimMode::Normal;
                }
                let mut cmds = self.operate(op, from, to, linewise);
                if op != Operator::Change {
                    cmds.push(VimCommand::Mode { mode: VimMode::Normal });
                }
                cmds
            }
            ">" | "<" | "J" => {
                let (from, to) = visual_range(buf, anchor, linewise);
                self.clear_pending();
                self.anchor = None;
                self.mode = VimMode::Normal;
                let edit = if key == "J" {
                    VimCommand::Join {
                        from_line: from.line,
                        lines: (to.line - from.line + 1).max(2),
                    }
                } else {
                    VimCommand::Indent {
                        from_line: from.line,
                        to_line: to.line,
                        out: key == "<",
                    }
                };
                vec![edit, VimCommand::Mode { mode: VimMode::Normal }]
            }
            _ => {
                self.clear_pending();
                vec![]
            }
        }
    }
}

/// Normalized, end-exclusive span of a Visual selection (charwise includes the grapheme under the cursor).
pub fn visual_range(buf: &VimBuffer, anchor: Pos, linewise: bool) -> (Pos, Pos) {
    let (mut from, mut to) = (anchor, buf.cursor());
    if to < from {
        std::mem::swap(&mut from, &mut to);
    }
    if linewise {
        let last = to.line.min(buf.lines.len().saturating_sub(1));
        return (pos(from.line, 0), pos(last, buf.line(last).len()));
    }
    (from, pos(to.line, next_grapheme_start(buf.line(to.line), to.col)))
}
