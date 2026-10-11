//! Splits raw terminal input into complete key/escape sequences and bracketed pastes (port of
//! packages/tui/src/stdin-buffer.ts). Sans-IO: the caller owns the flush timer.

use std::time::Duration;

const ESC: &str = "\x1b";
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// One unit of input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Data(String),
    Paste(String),
}

#[derive(PartialEq)]
enum Status {
    Complete,
    Incomplete,
}

fn is_complete(data: &str) -> Status {
    use Status::*;
    let after = &data[1..];
    if after.is_empty() {
        return Incomplete;
    }
    let ends_st = data.ends_with("\x1b\\");
    match after.as_bytes()[0] {
        b'[' if after.starts_with("[M") => {
            if data.chars().count() >= 6 {
                Complete
            } else {
                Incomplete
            }
        }
        b'[' => is_complete_csi(data),
        b']' if ends_st || data.ends_with('\x07') => Complete,
        b'P' | b'_' if ends_st => Complete,
        b']' | b'P' | b'_' => Incomplete,
        b'O' if after.chars().count() >= 2 => Complete,
        b'O' => Incomplete,
        _ => Complete,
    }
}

fn is_complete_csi(data: &str) -> Status {
    let payload = &data[2..];
    let Some(last) = payload.chars().last() else {
        return Status::Incomplete;
    };
    if !('\x40'..='\x7e').contains(&last) {
        return Status::Incomplete;
    }
    if let Some(mouse) = payload.strip_prefix('<') {
        let parts: Vec<&str> = mouse[..mouse.len() - 1].split(';').collect();
        let ok = (last == 'M' || last == 'm')
            && parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        return if ok { Status::Complete } else { Status::Incomplete };
    }
    Status::Complete
}

/// `^\x1b\[(\d+)(?::\d*)?(?::\d+)?u$` with codepoint >= 32.
fn unmodified_kitty_printable(seq: &str) -> Option<u32> {
    let body = seq.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let mut parts = body.split(':');
    let cp = parts.next()?;
    let shifted = parts.next();
    let base = parts.next();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some() || !digits(cp) || shifted.is_some_and(|s| !s.is_empty() && !digits(s)) {
        return None;
    }
    if base.is_some_and(|b| !digits(b)) {
        return None;
    }
    let cp: u32 = cp.parse().ok()?;
    (cp >= 32).then_some(cp)
}

fn extract_complete(buffer: &str) -> (Vec<String>, String) {
    let mut seqs = Vec::new();
    let mut pos = 0;
    while pos < buffer.len() {
        let remaining = &buffer[pos..];
        if !remaining.starts_with(ESC) {
            let c = remaining.chars().next().unwrap_or('\0');
            seqs.push(c.to_string());
            pos += c.len_utf8();
            continue;
        }
        let mut found = false;
        for (end, c) in remaining.char_indices().skip(1).map(|(i, c)| (i + c.len_utf8(), c)) {
            let _ = c;
            let candidate = &remaining[..end];
            if is_complete(candidate) == Status::Complete {
                if candidate == "\x1b\x1b" && remaining[end..].starts_with(['[', ']', 'O', 'P', '_']) {
                    seqs.push(ESC.to_string());
                    pos += 1;
                } else {
                    seqs.push(candidate.to_string());
                    pos += end;
                }
                found = true;
                break;
            }
        }
        if !found {
            return (seqs, remaining.to_string());
        }
    }
    (seqs, String::new())
}

/// Buffers partial escape sequences until they complete.
#[derive(Default)]
pub struct StdinBuffer {
    buffer: String,
    paste: Option<String>,
    pending_kitty_printable: Option<u32>,
}

impl StdinBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed input; returns the complete events. Check [`StdinBuffer::flush_after`] for a pending partial sequence.
    pub fn process(&mut self, data: &str) -> Vec<InputEvent> {
        let mut out = Vec::new();
        self.process_into(data, &mut out);
        out
    }

    /// Feed raw bytes: a lone high byte is the legacy meta encoding of ESC + (byte - 128).
    pub fn process_bytes(&mut self, data: &[u8]) -> Vec<InputEvent> {
        if let [b] = data
            && *b > 127
        {
            return self.process(&format!("\x1b{}", char::from(b - 128)));
        }
        self.process(&String::from_utf8_lossy(data))
    }

    fn process_into(&mut self, data: &str, out: &mut Vec<InputEvent>) {
        if data.is_empty() && self.buffer.is_empty() {
            self.emit("", out);
            return;
        }
        self.buffer.push_str(data);
        if let Some(paste) = self.paste.as_mut() {
            paste.push_str(&std::mem::take(&mut self.buffer));
            self.finish_paste(out);
            return;
        }
        if let Some(start) = self.buffer.find(PASTE_START) {
            if start > 0 {
                let (seqs, _) = extract_complete(&self.buffer[..start]);
                for s in seqs {
                    self.emit(&s, out);
                }
            }
            self.pending_kitty_printable = None;
            self.paste = Some(self.buffer[start + PASTE_START.len()..].to_string());
            self.buffer.clear();
            self.finish_paste(out);
            return;
        }
        let (seqs, remainder) = extract_complete(&self.buffer);
        self.buffer = remainder;
        for s in seqs {
            self.emit(&s, out);
        }
    }

    fn finish_paste(&mut self, out: &mut Vec<InputEvent>) {
        let Some(paste) = self.paste.as_ref() else {
            return;
        };
        let Some(end) = paste.find(PASTE_END) else {
            return;
        };
        let content = paste[..end].to_string();
        let remaining = paste[end + PASTE_END.len()..].to_string();
        self.paste = None;
        self.pending_kitty_printable = None;
        out.push(InputEvent::Paste(content));
        if !remaining.is_empty() {
            self.process_into(&remaining, out);
        }
    }

    fn emit(&mut self, seq: &str, out: &mut Vec<InputEvent>) {
        let mut chars = seq.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && Some(c as u32) == self.pending_kitty_printable
        {
            self.pending_kitty_printable = None;
            return;
        }
        self.pending_kitty_printable = unmodified_kitty_printable(seq);
        out.push(InputEvent::Data(seq.to_string()));
    }

    /// How long to wait before calling [`StdinBuffer::flush`] (10 ms for a lone ESC, 50 ms otherwise).
    pub fn flush_after(&self) -> Option<Duration> {
        match self.buffer.as_str() {
            "" => None,
            ESC => Some(Duration::from_millis(10)),
            _ => Some(Duration::from_millis(50)),
        }
    }

    /// Emit whatever is buffered as one sequence (timeout path).
    pub fn flush(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        self.pending_kitty_printable = None;
        vec![std::mem::take(&mut self.buffer)]
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn buffered(&self) -> &str {
        &self.buffer
    }
}
