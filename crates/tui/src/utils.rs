//! ANSI-aware measurement, truncation and wrapping (port of packages/tui/src/utils.ts).

use crate::tables;
use unicode_segmentation::UnicodeSegmentation;

fn in_table(table: &[(u32, u32)], cp: char) -> bool {
    let cp = cp as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// JavaScript `\s`.
pub fn is_whitespace_char(c: char) -> bool {
    in_table(tables::JS_SPACE, c)
}

/// JavaScript `String.prototype.trimEnd`.
pub fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_whitespace_char)
}

fn js_is_blank(s: &str) -> bool {
    s.chars().all(is_whitespace_char)
}

pub fn is_cjk_break(c: char) -> bool {
    in_table(tables::CJK_BREAK, c)
}

pub fn is_cjk_punctuation(c: char) -> bool {
    in_table(tables::CJK_PUNCT, c)
}

/// East Asian Width as reported by get-east-asian-width (2 for wide/fullwidth, else 1).
fn east_asian_width(c: char) -> usize {
    if in_table(tables::WIDE, c) { 2 } else { 1 }
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

fn could_be_emoji(segment: &str, first: char) -> bool {
    let cp = first as u32;
    (0x1f000..=0x1fbff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
        || segment.contains('\u{FE0F}')
        || segment.encode_utf16().count() > 2
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// `^\p{RGI_Emoji}$`: exact for single code points, VS16 and skin-tone forms (tables generated from Node);
/// keycaps, flags, tag and ZWJ sequences are recognised structurally.
fn is_rgi_emoji(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    match chars.as_slice() {
        [] => false,
        [c] => in_table(tables::RGI_SINGLE, *c),
        [c, '\u{FE0F}'] => in_table(tables::RGI_WITH_VS16, *c),
        [c, m] if is_skin_tone(*m) => in_table(tables::RGI_MODIFIER_BASE, *c),
        [c, '\u{FE0F}', '\u{20E3}'] => matches!(c, '#' | '*' | '0'..='9'),
        [a, b] if is_regional(*a) && is_regional(*b) => true,
        ['\u{1F3F4}', rest @ ..] if rest.last() == Some(&'\u{E007F}') => rest[..rest.len() - 1]
            .iter()
            .all(|c| ('\u{E0020}'..='\u{E007E}').contains(c)),
        _ if chars.contains(&'\u{200D}') => segment.split('\u{200D}').all(|part| {
            let p: Vec<char> = part.chars().collect();
            match p.as_slice() {
                [c] => in_table(tables::EXTENDED_PICTOGRAPHIC, *c) || in_table(tables::RGI_SINGLE, *c),
                [c, '\u{FE0F}'] => in_table(tables::EXTENDED_PICTOGRAPHIC, *c),
                [c, m] if is_skin_tone(*m) => in_table(tables::RGI_MODIFIER_BASE, *c),
                _ => false,
            }
        }),
        _ => false,
    }
}

/// Terminal width of one grapheme cluster.
pub(crate) fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }
    let Some(first) = segment.chars().next() else {
        return 0;
    };
    if segment.chars().all(|c| in_table(tables::TERMINAL_SPACING_MARK, c)) {
        return segment.chars().count();
    }
    if segment.chars().all(|c| in_table(tables::ZERO_WIDTH, c)) {
        return 0;
    }
    if could_be_emoji(segment, first) && is_rgi_emoji(segment) {
        return 2;
    }
    let base = segment.trim_start_matches(|c| in_table(tables::NON_PRINTING, c));
    let mut chars = base.chars();
    let Some(cp) = chars.next() else {
        return 0;
    };
    if is_regional(cp) {
        return 2;
    }
    let mut width = east_asian_width(cp);
    let mut follows_mark = false;
    for c in chars {
        if in_table(tables::TERMINAL_SPACING_MARK, c) {
            width += 1;
            follows_mark = false;
        } else if in_table(tables::MARK, c) {
            follows_mark = true;
        } else if !in_table(tables::NON_PRINTING, c) {
            if follows_mark || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
                width += east_asian_width(c);
            } else if c == '\u{0E33}' || c == '\u{0EB3}' {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

/// Visible width of a string in terminal columns (tabs count 3, escape sequences 0).
pub fn visible_width(s: &str) -> usize {
    if is_printable_ascii(s) {
        return s.len();
    }
    let clean = strip_terminal_sequences(&s.replace('\t', "   "));
    clean.graphemes(true).map(grapheme_width).sum()
}

/// Remove ANSI, OSC and APC sequences while preserving visible text.
pub fn strip_terminal_sequences(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = ansi_len(s, i) {
            i += len;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or('\0');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Expand Thai/Lao AM vowels and tabs the way terminals draw them (`normalizeTerminalOutput`).
pub fn normalize_terminal_output(s: &str) -> String {
    let s = s
        .replace('\u{0E33}', "\u{0E4D}\u{0E32}")
        .replace('\u{0EB3}', "\u{0ECD}\u{0EB2}");
    if !s.contains('\t') {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = ansi_len(&s, i) {
            out.push_str(&s[i..i + len]);
            i += len;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or('\0');
        if c == '\t' {
            out.push_str("   ");
        } else {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

/// Byte length of the CSI/OSC/APC sequence starting at `pos`, if any (`extractAnsiCode`).
pub fn ansi_len(s: &str, pos: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(pos) != Some(&0x1b) {
        return None;
    }
    match b.get(pos + 1) {
        Some(b'[') => {
            let end = b[pos + 2..].iter().position(|c| b"mGKHJ".contains(c))?;
            Some(end + 3)
        }
        Some(b']') | Some(b'_') => {
            let mut j = pos + 2;
            while j < b.len() {
                if b[j] == 0x07 {
                    return Some(j + 1 - pos);
                }
                if b[j] == 0x1b && b.get(j + 1) == Some(&b'\\') {
                    return Some(j + 2 - pos);
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

/// `extractAnsiCode`: the escape sequence at `pos`.
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<&str> {
    ansi_len(s, pos).map(|len| &s[pos..pos + len])
}

/// End (byte offset) of the plain-text run starting at `i`.
fn text_run_end(s: &str, i: usize, stop_at_tab: bool) -> usize {
    let b = s.as_bytes();
    let mut end = i;
    while end < b.len() {
        if (stop_at_tab && b[end] == b'\t') || ansi_len(s, end).is_some() {
            break;
        }
        end += 1;
    }
    end
}

#[derive(Clone, PartialEq)]
struct Hyperlink {
    params: String,
    url: String,
    bel: bool,
}

enum Osc8 {
    Other,
    Close,
    Open(Hyperlink),
}

fn parse_osc8(code: &str) -> Osc8 {
    let Some(rest) = code.strip_prefix("\x1b]8;") else {
        return Osc8::Other;
    };
    let bel = code.ends_with('\x07');
    let body = if bel {
        &rest[..rest.len().saturating_sub(1)]
    } else {
        &rest[..rest.len().saturating_sub(2)]
    };
    let Some(sep) = body.find(';') else {
        return Osc8::Other;
    };
    let url = &body[sep + 1..];
    if url.is_empty() {
        return Osc8::Close;
    }
    Osc8::Open(Hyperlink {
        params: body[..sep].to_string(),
        url: url.to_string(),
        bel,
    })
}

fn osc8_close(bel: bool) -> &'static str {
    if bel { "\x1b]8;;\x07" } else { "\x1b]8;;\x1b\\" }
}

fn active_osc8_close(prefix: &str) -> &'static str {
    if !prefix.contains("\x1b]8;") {
        return "";
    }
    let mut active: Option<Hyperlink> = None;
    let mut i = 0;
    while i < prefix.len() {
        if let Some(len) = ansi_len(prefix, i) {
            match parse_osc8(&prefix[i..i + len]) {
                Osc8::Open(h) => active = Some(h),
                Osc8::Close => active = None,
                Osc8::Other => {}
            }
            i += len;
        } else {
            i += 1;
        }
    }
    active.map_or("", |h| osc8_close(h.bel))
}

/// Tracks active SGR attributes and OSC 8 links so styles survive line breaks.
#[derive(Default)]
pub(crate) struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg: Option<String>,
    bg: Option<String>,
    link: Option<Hyperlink>,
}

impl AnsiCodeTracker {
    pub(crate) fn process(&mut self, code: &str) {
        match parse_osc8(code) {
            Osc8::Open(h) => {
                self.link = Some(h);
                return;
            }
            Osc8::Close => {
                self.link = None;
                return;
            }
            Osc8::Other => {}
        }
        if !code.ends_with('m') {
            return;
        }
        let Some(params) = first_sgr_params(code) else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset();
            return;
        }
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let code = parse_int(parts[i]);
            if code == Some(38) || code == Some(48) {
                let color = if parts.get(i + 1) == Some(&"5") && i + 2 < parts.len() {
                    Some((parts[i..i + 3].join(";"), 3))
                } else if parts.get(i + 1) == Some(&"2") && i + 4 < parts.len() {
                    Some((parts[i..i + 5].join(";"), 5))
                } else {
                    None
                };
                if let Some((color, used)) = color {
                    if code == Some(38) {
                        self.fg = Some(color);
                    } else {
                        self.bg = Some(color);
                    }
                    i += used;
                    continue;
                }
            }
            match code {
                Some(0) => self.reset(),
                Some(1) => self.bold = true,
                Some(2) => self.dim = true,
                Some(3) => self.italic = true,
                Some(4) => self.underline = true,
                Some(5) => self.blink = true,
                Some(7) => self.inverse = true,
                Some(8) => self.hidden = true,
                Some(9) => self.strikethrough = true,
                Some(21) => self.bold = false,
                Some(22) => {
                    self.bold = false;
                    self.dim = false;
                }
                Some(23) => self.italic = false,
                Some(24) => self.underline = false,
                Some(25) => self.blink = false,
                Some(27) => self.inverse = false,
                Some(28) => self.hidden = false,
                Some(29) => self.strikethrough = false,
                Some(39) => self.fg = None,
                Some(49) => self.bg = None,
                Some(c) if (30..=37).contains(&c) || (90..=97).contains(&c) => self.fg = Some(c.to_string()),
                Some(c) if (40..=47).contains(&c) || (100..=107).contains(&c) => self.bg = Some(c.to_string()),
                _ => {}
            }
            i += 1;
        }
    }

    fn reset(&mut self) {
        let link = self.link.take();
        *self = Self {
            link,
            ..Self::default()
        };
    }

    pub(crate) fn active_codes(&self) -> String {
        let mut codes: Vec<&str> = Vec::new();
        for (on, c) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.hidden, "8"),
            (self.strikethrough, "9"),
        ] {
            if on {
                codes.push(c);
            }
        }
        if let Some(fg) = &self.fg {
            codes.push(fg);
        }
        if let Some(bg) = &self.bg {
            codes.push(bg);
        }
        let mut out = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some(h) = &self.link {
            out.push_str(&format!(
                "\x1b]8;{};{}{}",
                h.params,
                h.url,
                if h.bel { "\x07" } else { "\x1b\\" }
            ));
        }
        out
    }

    pub(crate) fn line_end_reset(&self) -> String {
        let mut out = String::new();
        if self.underline {
            out.push_str("\x1b[24m");
        }
        if let Some(h) = &self.link {
            out.push_str(osc8_close(h.bel));
        }
        out
    }

    fn update_from_text(&mut self, text: &str) {
        let mut i = 0;
        while i < text.len() {
            if let Some(len) = ansi_len(text, i) {
                self.process(&text[i..i + len]);
                i += len;
            } else {
                i += 1;
            }
        }
    }
}

/// `/\x1b\[([\d;]*)m/`: parameters of the first SGR sequence in `code`.
fn first_sgr_params(code: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(off) = code[from..].find("\x1b[") {
        let start = from + off + 2;
        let rest = &code[start..];
        let n = rest.bytes().take_while(|b| b.is_ascii_digit() || *b == b';').count();
        if rest.as_bytes().get(n) == Some(&b'm') {
            return Some(&rest[..n]);
        }
        from = start;
    }
    None
}

/// JavaScript `Number.parseInt(s, 10)` for SGR parameter strings.
fn parse_int(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Background color active at the end of an ANSI-styled string.
pub fn get_active_background_ansi(text: &str) -> String {
    let mut t = AnsiCodeTracker::default();
    t.update_from_text(text);
    t.bg.map_or(String::new(), |bg| format!("\x1b[{bg}m"))
}

fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending = String::new();
    let mut current_is_space: Option<bool> = None;
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        let end = text_run_end(text, i, false);
        for seg in text[i..end].graphemes(true) {
            let is_space = seg == " ";
            if !is_space && seg.chars().any(is_cjk_break) {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                current_is_space = None;
                tokens.push(std::mem::take(&mut pending) + seg);
                continue;
            }
            if !current.is_empty() && current_is_space != Some(is_space) {
                tokens.push(std::mem::take(&mut current));
            }
            current.push_str(&std::mem::take(&mut pending));
            current_is_space = Some(is_space);
            current.push_str(seg);
        }
        i = end;
    }
    if !pending.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending);
        } else {
            current = pending;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Word-wrap text to `width` columns, carrying active ANSI styles across breaks. No padding.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();
    for input_line in split_lines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.active_codes()
        };
        result.extend(wrap_single_line(&(prefix + input_line), width));
        tracker.update_from_text(input_line);
    }
    result
}

/// `text.split(/\r\n|\r|\n/)`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\r' => {
                out.push(&text[start..i]);
                i += if b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                start = i;
            }
            b'\n' => {
                out.push(&text[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push(&text[start..]);
    out
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }
    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();
    let mut current = String::new();
    let mut current_width = 0;
    for token in split_into_tokens_with_ansi(line) {
        let token_width = visible_width(&token);
        let is_ws = js_is_blank(&token);
        if token_width > width && !is_ws {
            if !current.is_empty() {
                current.push_str(&tracker.line_end_reset());
                wrapped.push(std::mem::take(&mut current));
            }
            let mut broken = break_long_word(&token, width, &mut tracker);
            current = broken.pop().unwrap_or_default();
            wrapped.extend(broken);
            current_width = visible_width(&current);
            continue;
        }
        if current_width + token_width > width && current_width > 0 {
            let mut line_to_wrap = js_trim_end(&current).to_string();
            line_to_wrap.push_str(&tracker.line_end_reset());
            wrapped.push(line_to_wrap);
            if is_ws {
                current = tracker.active_codes();
                current_width = 0;
            } else {
                current = tracker.active_codes() + &token;
                current_width = token_width;
            }
        } else {
            current.push_str(&token);
            current_width += token_width;
        }
        tracker.update_from_text(&token);
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        return vec![String::new()];
    }
    wrapped.iter().map(|l| js_trim_end(l).to_string()).collect()
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = tracker.active_codes();
    let mut current_width = 0;
    let mut i = 0;
    while i < word.len() {
        if let Some(len) = ansi_len(word, i) {
            let code = &word[i..i + len];
            current.push_str(code);
            tracker.process(code);
            i += len;
            continue;
        }
        let end = text_run_end(word, i, false);
        for g in word[i..end].graphemes(true) {
            let w = visible_width(g);
            if current_width + w > width {
                current.push_str(&tracker.line_end_reset());
                lines.push(std::mem::replace(&mut current, tracker.active_codes()));
                current_width = 0;
            }
            current.push_str(g);
            current_width += w;
        }
        i = end;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() { vec![String::new()] } else { lines }
}

/// Pad `line` to `width` and wrap it with a background function.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let pad = width.saturating_sub(visible_width(line));
    bg(&format!("{line}{}", " ".repeat(pad)))
}

fn truncate_fragment(text: &str, max_width: usize) -> (String, usize) {
    if max_width == 0 || text.is_empty() {
        return (String::new(), 0);
    }
    if is_printable_ascii(text) {
        let clipped = &text[..text.len().min(max_width)];
        return (clipped.to_string(), clipped.len());
    }
    let mut result = String::new();
    let mut width = 0;
    let mut pending = String::new();
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if width + 3 > max_width {
                break;
            }
            result.push_str(&std::mem::take(&mut pending));
            result.push('\t');
            width += 3;
            i += 1;
            continue;
        }
        let end = text_run_end(text, i, true);
        for g in text[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if width + w > max_width {
                return (result, width);
            }
            result.push_str(&std::mem::take(&mut pending));
            result.push_str(g);
            width += w;
        }
        i = end;
    }
    (result, width)
}

fn finalize_truncated(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max: usize,
    pad: bool,
) -> String {
    let close = active_osc8_close(prefix);
    let mut out = if ellipsis.is_empty() {
        format!("{prefix}{close}\x1b[0m")
    } else {
        format!("{prefix}{close}\x1b[0m{ellipsis}\x1b[0m")
    };
    if pad {
        out.push_str(&" ".repeat(max.saturating_sub(prefix_width + ellipsis_width)));
    }
    out
}

/// Truncate to `max_width` columns, appending `ellipsis` when cut; optionally pad to exactly `max_width`.
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    let padded = |s: &str, w: usize| {
        if pad {
            format!("{s}{}", " ".repeat(max_width.saturating_sub(w)))
        } else {
            s.to_string()
        }
    };
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return padded("", 0);
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return padded(text, text_width);
        }
        let (clipped, w) = truncate_fragment(ellipsis, max_width);
        if w == 0 {
            return padded("", 0);
        }
        return finalize_truncated("", 0, &clipped, w, max_width, pad);
    }
    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return padded(text, text.len());
        }
        let target = max_width - ellipsis_width;
        return finalize_truncated(&text[..target], target, ellipsis, ellipsis_width, max_width, pad);
    }
    let target = max_width - ellipsis_width;
    let mut result = String::new();
    let mut pending = String::new();
    let mut seen = 0;
    let mut kept = 0;
    let mut keep = true;
    let mut overflowed = false;
    let mut i = 0;
    'outer: while i < text.len() {
        if let Some(len) = ansi_len(text, i) {
            pending.push_str(&text[i..i + len]);
            i += len;
            continue;
        }
        if text.as_bytes()[i] == b'\t' {
            if keep && kept + 3 <= target {
                result.push_str(&std::mem::take(&mut pending));
                result.push('\t');
                kept += 3;
            } else {
                keep = false;
                pending.clear();
            }
            seen += 3;
            if seen > max_width {
                overflowed = true;
                break;
            }
            i += 1;
            continue;
        }
        let end = text_run_end(text, i, true);
        for g in text[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if keep && kept + w <= target {
                result.push_str(&std::mem::take(&mut pending));
                result.push_str(g);
                kept += w;
            } else {
                keep = false;
                pending.clear();
            }
            seen += w;
            if seen > max_width {
                overflowed = true;
                break 'outer;
            }
        }
        i = end;
    }
    if !overflowed {
        return padded(text, seen);
    }
    finalize_truncated(&result, kept, ellipsis, ellipsis_width, max_width, pad)
}

/// Visible columns `[start, start+len)` of a line, keeping escape codes; `strict` drops wide chars crossing the end.
pub fn slice_with_width(line: &str, start: usize, length: usize, strict: bool) -> (String, usize) {
    if length == 0 {
        return (String::new(), 0);
    }
    let end_col = start + length;
    let (mut result, mut result_width, mut col, mut i) = (String::new(), 0, 0, 0);
    let mut pending = String::new();
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            let code = &line[i..i + len];
            if col >= start && col < end_col {
                result.push_str(code);
            } else if col < start {
                pending.push_str(code);
            }
            i += len;
            continue;
        }
        let end = text_run_end(line, i, false);
        for g in line[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if col >= start && col < end_col && (!strict || col + w <= end_col) {
                result.push_str(&std::mem::take(&mut pending));
                result.push_str(g);
                result_width += w;
            }
            col += w;
            if col >= end_col {
                break;
            }
        }
        i = end;
        if col >= end_col {
            break;
        }
    }
    (result, result_width)
}
