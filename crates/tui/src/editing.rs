//! Editing primitives shared by Input and Editor: kill ring, undo stack, word navigation
//! (ports of packages/tui/src/kill-ring.ts, undo-stack.ts, word-navigation.ts). Offsets are byte offsets.

use unicode_segmentation::UnicodeSegmentation;

/// Emacs-style kill ring; consecutive kills accumulate into one entry.
#[derive(Clone, Debug, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        match self.ring.last_mut() {
            Some(last) if accumulate => {
                if prepend {
                    last.insert_str(0, text);
                } else {
                    last.push_str(text);
                }
            }
            _ => self.ring.push(text.to_string()),
        }
    }

    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            self.ring.rotate_right(1);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// `PUNCTUATION_REGEX` from utils.ts.
pub fn is_punctuation(c: char) -> bool {
    "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".contains(c)
}

fn is_ws(s: &str) -> bool {
    // TS `/\s/.test(segment)`: any whitespace character.
    s.chars().any(char::is_whitespace)
}

/// Intl.Segmenter `isWordLike`: the segment holds letters or digits.
fn word_like(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

fn is_han(s: &str) -> bool {
    s.chars()
        .all(|c| matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x3FFFF))
}

/// UAX #29 word segments, with runs of Han ideographs merged into one word. ICU (Intl.Segmenter) splits Han
/// text with a dictionary; one word per run is the dictionary-free approximation.
fn word_segments(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0;
    for seg in text.split_word_bounds() {
        let end = start + seg.len();
        match out.last_mut() {
            Some(last) if is_han(last) && is_han(seg) => *last = &text[end - seg.len() - last.len()..end],
            _ => out.push(&text[start..end]),
        }
        start = end;
    }
    out
}

/// `findWordBackward`: skip whitespace, then a word (stopping after its last punctuation) or a punctuation run.
pub fn find_word_backward(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut segs = word_segments(&text[..cursor]);
    let mut c = cursor;
    while segs.last().is_some_and(|s| is_ws(s)) {
        c -= segs.pop().map_or(0, str::len);
    }
    let Some(last) = segs.last() else { return c };
    if word_like(last) {
        match last.rfind(is_punctuation) {
            Some(p) => c - (last.len() - p - last[p..].chars().next().map_or(1, char::len_utf8)),
            None => c - last.len(),
        }
    } else {
        while segs.last().is_some_and(|s| !word_like(s) && !is_ws(s)) {
            c -= segs.pop().map_or(0, str::len);
        }
        c
    }
}

/// `findWordForward`: skip whitespace, then a word (up to its first punctuation) or a punctuation run.
pub fn find_word_forward(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut segs = word_segments(&text[cursor..]).into_iter().peekable();
    let mut c = cursor;
    while let Some(s) = segs.next_if(|s| is_ws(s)) {
        c += s.len();
    }
    let Some(first) = segs.peek() else { return c };
    if word_like(first) {
        return c + first.find(is_punctuation).unwrap_or(first.len());
    }
    while let Some(s) = segs.next_if(|s| !word_like(s) && !is_ws(s)) {
        c += s.len();
    }
    c
}

/// Byte length of the grapheme before `cursor` (TS falls back to 1 code unit).
pub fn prev_grapheme_len(text: &str, cursor: usize) -> usize {
    text[..cursor].graphemes(true).next_back().map_or(0, str::len)
}

/// Byte length of the grapheme at `cursor`.
pub fn next_grapheme_len(text: &str, cursor: usize) -> usize {
    text[cursor..].graphemes(true).next().map_or(0, str::len)
}
