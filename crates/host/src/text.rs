//! Output text handling shared by the blocking bash and shell jobs, ported from utils/ansi.ts (stripAnsi),
//! utils/shell.ts (sanitizeBinaryOutput), core/tools/truncate.ts (truncateTail) and the streaming TextDecoder.

const ESC: char = '\u{1b}';
const CSI: char = '\u{9b}';

/// Streaming UTF-8 decoder with WHATWG replacement (one U+FFFD per maximal invalid subpart).
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}

impl Decoder {
    pub fn decode(&mut self, data: &[u8], stream: bool) -> String {
        self.pending.extend_from_slice(data);
        let bytes = std::mem::take(&mut self.pending);
        let mut out = String::new();
        let mut rest = &bytes[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{fffd}');
                            rest = &after[len..];
                        }
                        None if stream => {
                            self.pending = after.to_vec();
                            break;
                        }
                        None => {
                            out.push('\u{fffd}');
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Chunk text as both TS paths write it: `sanitizeBinaryOutput(stripAnsi(text)).replace(/\r/g, "")`.
pub fn clean(text: &str) -> String {
    strip_ansi(text)
        .chars()
        .filter(|&c| {
            let code = c as u32;
            !(code <= 0x1f && !matches!(code, 0x09 | 0x0a) || (0xfff9..=0xfffb).contains(&code))
        })
        .collect()
}

pub fn strip_ansi(text: &str) -> String {
    if !text.contains(ESC) && !text.contains(CSI) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match osc_end(&chars, i).or_else(|| csi_end(&chars, i)) {
            Some(end) => i = end,
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// `\u001B\][\s\S]*?(?:\u0007|\u001B\|\u009C)`
fn osc_end(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&ESC) || chars.get(i + 1) != Some(&']') {
        return None;
    }
    let mut j = i + 2;
    while j < chars.len() {
        match chars[j] {
            '\u{7}' | '\u{9c}' => return Some(j + 1),
            ESC if chars.get(j + 1) == Some(&'\\') => return Some(j + 2),
            _ => j += 1,
        }
    }
    None
}

/// `[\u001B\u009B][[\]()#;?]*(?:\d{1,4}(?:[;:]\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]`, with the regex's backtracking.
fn csi_end(chars: &[char], i: usize) -> Option<usize> {
    if !matches!(chars.get(i), Some(&ESC) | Some(&CSI)) {
        return None;
    }
    let start = i + 1;
    let mut most = 0;
    while chars.get(start + most).is_some_and(|c| "[]()#;?".contains(*c)) {
        most += 1;
    }
    for intermediates in (0..=most).rev() {
        let p = start + intermediates;
        let mut ends = Vec::new();
        param_ends(chars, p, &mut ends);
        ends.push(p);
        for end in ends {
            if chars.get(end).is_some_and(|&c| is_final(c)) {
                return Some(end + 1);
            }
        }
    }
    None
}

fn digits(chars: &[char], at: usize, max: usize) -> usize {
    let mut n = 0;
    while n < max && chars.get(at + n).is_some_and(char::is_ascii_digit) {
        n += 1;
    }
    n
}

/// End positions of `\d{1,4}(?:[;:]\d{0,4})*` at `p`, most preferred (greedy) first.
fn param_ends(chars: &[char], p: usize, ends: &mut Vec<usize>) {
    for n in (1..=digits(chars, p, 4)).rev() {
        separators(chars, p + n, ends);
    }
}

fn separators(chars: &[char], at: usize, ends: &mut Vec<usize>) {
    if chars.get(at).is_some_and(|&c| c == ';' || c == ':') {
        for n in (0..=digits(chars, at + 1, 4)).rev() {
            separators(chars, at + 1 + n, ends);
        }
    }
    ends.push(at);
}

fn is_final(c: char) -> bool {
    c.is_ascii_digit()
        || ('A'..='P').contains(&c)
        || ('R'..='T').contains(&c)
        || c == 'Z'
        || c == 'c'
        || ('f'..='n').contains(&c)
        || ('q'..='u').contains(&c)
        || matches!(c, 'y' | '=' | '>' | '<' | '~')
}

pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_LINES: usize = 2000;

/// truncateTail with the default limits: the last lines that fit, else the end of the last line. `None` when the text fits.
pub fn truncate_tail(content: &str) -> Option<String> {
    let mut lines: Vec<&str> = if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    };
    if content.ends_with('\n') {
        lines.pop();
    }
    if lines.len() <= DEFAULT_MAX_LINES && content.len() <= DEFAULT_MAX_BYTES {
        return None;
    }
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() >= DEFAULT_MAX_LINES {
            break;
        }
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > DEFAULT_MAX_BYTES {
            if kept.is_empty() {
                kept.push(tail_bytes(line, DEFAULT_MAX_BYTES).to_owned());
            }
            break;
        }
        kept.push((*line).to_owned());
        bytes += size;
    }
    kept.reverse();
    Some(kept.join("\n"))
}

/// The last `max` bytes of `text`, starting at a character boundary.
pub fn tail_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
