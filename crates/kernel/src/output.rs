//! Bounded output: per-cell stream limits, raw fd 1/fd 2 capture, output from outside a cell, and the stderr tail.
//! Lengths are JavaScript string lengths (UTF-16 code units), as in kernel.ts; a cut never splits a character.

use std::collections::VecDeque;

/// Protocol frames are bounded per frame (wire bytes, including JSON overhead).
pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// stdout or stderr kept for one request (cell), in UTF-16 code units.
pub(crate) const MAX_REQUEST_STREAM_CHARS: usize = 4 * MAX_FRAME_BYTES;
const REQUEST_STREAM_MARKER: &str =
    "\n[... further output dropped: over the RLM kernel's 4 MiB per-cell output limit ...]\n";
/// Raw stderr of the kernel process kept as a tail for diagnostics only.
pub(crate) const STDERR_TAIL_CHARS: usize = 8000;
/// Raw output from outside any cell, kept for the next cell.
pub(crate) const MAX_ORPHAN_CHARS: usize = 64 * 1024;

pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Byte index of the last character boundary at or before `units` UTF-16 code units into `text`.
fn utf16_index(text: &str, units: usize) -> usize {
    let mut counted = 0;
    for (index, character) in text.char_indices() {
        counted += character.len_utf16();
        if counted > units {
            return index;
        }
    }
    text.len()
}

/// The first `units` UTF-16 code units of `text` (fewer when that would split a character).
pub(crate) fn utf16_head(text: &str, units: usize) -> &str {
    &text[..utf16_index(text, units)]
}

/// The last `units` UTF-16 code units of `text` (fewer when that would split a character).
pub(crate) fn utf16_tail(text: &str, units: usize) -> &str {
    let length = utf16_len(text);
    if length <= units {
        return text;
    }
    let skip = length - units;
    let mut counted = 0;
    for (index, character) in text.char_indices() {
        if counted >= skip {
            return &text[index..];
        }
        counted += character.len_utf16();
    }
    ""
}

/// `current` + `text` within the per-cell stream limit; the marker is added once, the first time it cuts.
pub(crate) fn append_bounded(current: &mut String, text: &str) {
    let used = utf16_len(current);
    if used > MAX_REQUEST_STREAM_CHARS {
        return;
    }
    let room = MAX_REQUEST_STREAM_CHARS - used;
    if utf16_len(text) <= room {
        current.push_str(text);
        return;
    }
    current.push_str(&text[..utf16_index(text, room)]);
    current.push_str(REQUEST_STREAM_MARKER);
}

/// Appends the last STDERR_TAIL_CHARS of `text` to `tail` and keeps the result within that bound.
pub(crate) fn keep_tail(tail: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    tail.push_str(utf16_tail(text, STDERR_TAIL_CHARS));
    let kept = utf16_tail(tail, STDERR_TAIL_CHARS).len();
    tail.drain(..tail.len() - kept);
}

/// Streaming UTF-8 decoding that replaces invalid sequences with U+FFFD (as `TextDecoder` with `stream: true`).
#[derive(Debug, Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub(crate) fn decode(&mut self, bytes: &[u8]) -> String {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = &data[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{fffd}');
                            rest = &after[length..];
                        }
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        text
    }
}

/// Raw fd 1 or fd 2 output of one cell: head and tail within the per-cell limit, with the middle counted.
#[derive(Debug, Default)]
pub(crate) struct RawCapture {
    head: String,
    head_len: usize,
    tail: VecDeque<(String, usize)>,
    tail_len: usize,
    dropped: usize,
    pub(crate) marker_seen: bool,
}

impl RawCapture {
    /// Keep the head and tail of a cell's raw output, each half the per-cell stream limit.
    pub(crate) fn push(&mut self, mut text: &str) {
        let half = MAX_REQUEST_STREAM_CHARS / 2;
        if self.head_len < half {
            let split = utf16_index(text, half - self.head_len);
            let taken = &text[..split];
            self.head.push_str(taken);
            self.head_len += utf16_len(taken);
            text = &text[split..];
        }
        if text.is_empty() {
            return;
        }
        let length = utf16_len(text);
        self.tail.push_back((text.to_string(), length));
        self.tail_len += length;
        while self.tail.len() > 1 && self.tail_len - self.tail[0].1 >= half {
            if let Some((_, first)) = self.tail.pop_front() {
                self.tail_len -= first;
                self.dropped += first;
            }
        }
    }

    pub(crate) fn text(&self, fd: u8) -> String {
        let half = MAX_REQUEST_STREAM_CHARS / 2;
        let joined: String = self.tail.iter().map(|(text, _)| text.as_str()).collect();
        let mut dropped = self.dropped;
        let mut tail = joined.as_str();
        if self.tail_len > half {
            tail = utf16_tail(tail, half);
            dropped += self.tail_len - utf16_len(tail);
        }
        if dropped == 0 {
            return format!("{}{tail}", self.head);
        }
        format!(
            "{}\n[... {dropped} characters of raw fd {fd} output cut from the middle: over the RLM kernel's 4 MiB per-cell output limit ...]\n{tail}",
            self.head
        )
    }
}

/// Output that arrived while no cell owned a stream, as reported at the start of the next cell.
pub(crate) fn orphan_text(text: &str, dropped: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let cut = if dropped {
        format!(
            "\n[... further output dropped: over the {} KiB limit ...]",
            MAX_ORPHAN_CHARS / 1024
        )
    } else {
        String::new()
    };
    let end = if !cut.is_empty() || !text.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!(
        "[kernel output written outside a cell, before this one (a background thread or process):]\n{text}{cut}{end}[end of output from outside a cell]\n"
    )
}

/// The marker runtime.py writes to fd 1 and fd 2 after a cell's output.
pub(crate) fn flush_marker(request_id: &str) -> Vec<u8> {
    format!("\x1eultron-rlm-flush:{request_id}\x1e").into_bytes()
}

/// Length of the longest tail of `data` that is a proper prefix of `marker`.
pub(crate) fn marker_prefix_len(data: &[u8], marker: &[u8]) -> usize {
    let from = data.len().saturating_sub(marker.len() - 1);
    (from..data.len())
        .find(|&start| marker.starts_with(&data[start..]))
        .map_or(0, |start| data.len() - start)
}

pub(crate) fn find(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_utf8_stream_holds_split_characters_and_replaces_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        let euro = "€".as_bytes();
        assert_eq!(stream.decode(&euro[..1]), "");
        assert_eq!(stream.decode(&euro[1..]), "€");
        assert_eq!(stream.decode(b"a\xffb\xe2\x82"), "a\u{fffd}b");
        assert_eq!(stream.decode(b"x"), "\u{fffd}x");
    }

    #[test]
    fn kernel_marker_prefix_finds_the_longest_partial_marker_at_the_end() {
        let marker = b"\x1eab\x1e";
        assert_eq!(marker_prefix_len(b"xx\x1ea", marker), 2);
        assert_eq!(marker_prefix_len(b"xx\x1eab", marker), 3);
        // The closing byte of a whole marker can open the next one.
        assert_eq!(marker_prefix_len(b"xx\x1eab\x1e", marker), 1);
        assert_eq!(marker_prefix_len(b"xx\x1eab\x1ey", marker), 0);
        assert_eq!(marker_prefix_len(b"xx\x1e", marker), 1);
        assert_eq!(marker_prefix_len(b"xxa", marker), 0);
    }

    #[test]
    fn kernel_append_bounded_cuts_once_with_a_marker() {
        let mut current = "a".repeat(MAX_REQUEST_STREAM_CHARS - 2);
        append_bounded(&mut current, "🎉🎉");
        assert!(current.ends_with(&format!("{}🎉{REQUEST_STREAM_MARKER}", "a")));
        let before = current.clone();
        append_bounded(&mut current, "more");
        assert_eq!(current, before);
    }

    #[test]
    fn kernel_stderr_tail_keeps_the_last_8000_code_units() {
        let mut tail = String::new();
        keep_tail(&mut tail, &"a".repeat(7990));
        keep_tail(&mut tail, &"b".repeat(20));
        assert_eq!(tail, format!("{}{}", "a".repeat(7980), "b".repeat(20)));
        keep_tail(&mut tail, &format!("{}c", "d".repeat(9000)));
        assert_eq!(tail, format!("{}c", "d".repeat(7999)));
    }

    #[test]
    fn kernel_raw_capture_keeps_head_and_tail_and_counts_the_cut() {
        let half = MAX_REQUEST_STREAM_CHARS / 2;
        let mut capture = RawCapture::default();
        capture.push(&"h".repeat(half + 10));
        assert_eq!(capture.text(1), format!("{}{}", "h".repeat(half), "h".repeat(10)));
        capture.push(&"t".repeat(half));
        let text = capture.text(1);
        assert!(text.starts_with(&"h".repeat(half)));
        assert!(text.contains("\n[... 10 characters of raw fd 1 output cut from the middle"));
        assert!(text.ends_with(&format!("...]\n{}", "t".repeat(half))));
    }

    #[test]
    fn kernel_orphan_text_wraps_and_marks_dropped_output() {
        assert_eq!(orphan_text("", true), "");
        assert_eq!(
            orphan_text("late\n", false),
            "[kernel output written outside a cell, before this one (a background thread or process):]\nlate\n[end of output from outside a cell]\n"
        );
        assert_eq!(
            orphan_text("late", true),
            "[kernel output written outside a cell, before this one (a background thread or process):]\nlate\n[... further output dropped: over the 64 KiB limit ...]\n[end of output from outside a cell]\n"
        );
    }
}
