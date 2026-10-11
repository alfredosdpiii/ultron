//! Small JavaScript runtime semantics the ported providers depend on: `TextDecoder` streaming UTF-8 decoding, `atob` /
//! `btoa`, and `String.prototype.trim`.

/// `new TextDecoder()` used with `{ stream: true }`: invalid sequences become U+FFFD, an incomplete sequence at the end
/// of a chunk waits for the next one.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &after[len..];
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }

    /// `decoder.decode()`: flush what is left, lossily.
    pub fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `btoa` over bytes.
pub fn btoa(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `atob` (WHATWG forgiving-base64 decode): ASCII whitespace is ignored, padding is optional; `None` where `atob`
/// throws `InvalidCharacterError`.
pub fn atob(text: &str) -> Option<Vec<u8>> {
    let mut data: Vec<u8> = text
        .bytes()
        .filter(|b| !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .collect();
    if !text.is_ascii() {
        return None;
    }
    if data.len().is_multiple_of(4) {
        if data.ends_with(b"==") {
            data.truncate(data.len() - 2);
        } else if data.ends_with(b"=") {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in data {
        let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// JavaScript's whitespace and line terminators (`String.prototype.trim`).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// The Latin-1 "binary string" `atob` returns, as JavaScript sees it.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// The `SyntaxError` message V8's `JSON.parse` throws for `text`, or `None` when it parses. Covers the messages of
/// V8's JSON parser: end of input, unexpected token with source context, and the positional messages
/// (`... in JSON at position N (line L column C)`).
pub fn json_parse_error(text: &str) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut parser = V8Json { units: &units, pos: 0 };
    let result = parser.value().and_then(|()| {
        parser.skip_ws();
        if parser.pos < units.len() {
            Err(JsonError::At(
                "Unexpected non-whitespace character after JSON",
                parser.pos,
            ))
        } else {
            Ok(())
        }
    });
    let error = result.err()?;
    Some(match error {
        JsonError::Eos => "Unexpected end of JSON input".into(),
        JsonError::Token(pos) => unexpected_token(&units, pos),
        JsonError::At(message, pos) => {
            let (line, column) = line_column(&units, pos);
            let place = if message.ends_with("after JSON") {
                ""
            } else {
                " in JSON"
            };
            format!("{message}{place} at position {pos} (line {line} column {column})")
        }
    })
}

enum JsonError {
    Eos,
    Token(usize),
    At(&'static str, usize),
}

struct V8Json<'a> {
    units: &'a [u16],
    pos: usize,
}

fn is_ws(unit: u16) -> bool {
    matches!(unit, 0x20 | 0x09 | 0x0A | 0x0D)
}

impl V8Json<'_> {
    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(is_ws) {
            self.pos += 1;
        }
    }

    fn unexpected(&self) -> JsonError {
        if self.pos >= self.units.len() {
            JsonError::Eos
        } else {
            JsonError::Token(self.pos)
        }
    }

    fn value(&mut self) -> Result<(), JsonError> {
        self.skip_ws();
        match self
            .peek()
            .map(|u| u as u8 as char)
            .filter(|_| self.peek().is_some_and(|u| u < 0x80))
        {
            None => Err(self.unexpected()),
            Some('"') => self.string(),
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('t') => self.literal("true"),
            Some('f') => self.literal("false"),
            Some('n') => self.literal("null"),
            Some('-' | '0'..='9') => self.number(),
            Some(_) => Err(self.unexpected()),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), JsonError> {
        for expected in word.encode_utf16() {
            match self.peek() {
                None => return Err(JsonError::Eos),
                Some(unit) if unit == expected => self.pos += 1,
                Some(_) => return Err(JsonError::Token(self.pos)),
            }
        }
        Ok(())
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while self.peek().is_some_and(|u| (0x30..=0x39).contains(&u)) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<(), JsonError> {
        if self.peek() == Some(u16::from(b'-')) {
            self.pos += 1;
            if !self.peek().is_some_and(|u| (0x30..=0x39).contains(&u)) {
                return Err(JsonError::At("No number after minus sign", self.pos));
            }
        }
        if self.peek() == Some(u16::from(b'0')) {
            self.pos += 1;
        } else {
            self.digits();
        }
        if self.peek() == Some(u16::from(b'.')) {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(JsonError::At("Unterminated fractional number", self.pos));
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x2B | 0x2D)) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(JsonError::At("Exponent part is missing a number", self.pos));
            }
        }
        Ok(())
    }

    fn string(&mut self) -> Result<(), JsonError> {
        self.pos += 1;
        loop {
            match self.peek() {
                None => return Err(JsonError::At("Unterminated string", self.pos)),
                Some(0x22) => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(0x5C) => {
                    self.pos += 1;
                    match self.peek() {
                        None => return Err(JsonError::At("Unterminated string", self.pos)),
                        Some(0x75) => {
                            for _ in 0..4 {
                                self.pos += 1;
                                if !self
                                    .peek()
                                    .is_some_and(|u| (u as u8 as char).is_ascii_hexdigit() && u < 0x80)
                                {
                                    return Err(JsonError::At("Bad Unicode escape", self.pos));
                                }
                            }
                            self.pos += 1;
                        }
                        Some(0x22 | 0x5C | 0x2F | 0x62 | 0x66 | 0x6E | 0x72 | 0x74) => self.pos += 1,
                        Some(_) => return Err(JsonError::At("Bad escaped character", self.pos)),
                    }
                }
                Some(unit) if unit < 0x20 => {
                    return Err(JsonError::At("Bad control character in string literal", self.pos));
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn object(&mut self) -> Result<(), JsonError> {
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(0x7D) {
            self.pos += 1;
            return Ok(());
        }
        loop {
            if self.peek() != Some(0x22) {
                return Err(JsonError::At("Expected property name or '}'", self.pos));
            }
            self.string()?;
            self.skip_ws();
            if self.peek() != Some(0x3A) {
                return Err(JsonError::At("Expected ':' after property name", self.pos));
            }
            self.pos += 1;
            self.value()?;
            self.skip_ws();
            match self.peek() {
                Some(0x2C) => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(0x7D) => {
                    self.pos += 1;
                    return Ok(());
                }
                _ => return Err(JsonError::At("Expected ',' or '}' after property value", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<(), JsonError> {
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(0x5D) {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.value()?;
            self.skip_ws();
            match self.peek() {
                Some(0x2C) => self.pos += 1,
                Some(0x5D) => {
                    self.pos += 1;
                    return Ok(());
                }
                _ => return Err(JsonError::At("Expected ',' or ']' after array element", self.pos)),
            }
        }
    }
}

fn line_column(units: &[u16], pos: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for &unit in &units[..pos.min(units.len())] {
        if unit == 0x0A {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// V8's `Unexpected token` message with up to ten units of context on each side.
fn unexpected_token(units: &[u16], pos: usize) -> String {
    const CONTEXT: usize = 10;
    let token = String::from_utf16_lossy(&units[pos..pos + 1]);
    let length = units.len();
    if length <= CONTEXT * 2 + 1 {
        return format!(
            "Unexpected token '{token}', \"{}\" is not valid JSON",
            String::from_utf16_lossy(units)
        );
    }
    let start = pos.saturating_sub(CONTEXT);
    let end = (pos + CONTEXT).min(length);
    let snippet = String::from_utf16_lossy(&units[start..end]);
    let prefix = if start == 0 { "" } else { "..." };
    let suffix = if end == length { "" } else { "..." };
    format!("Unexpected token '{token}', {prefix}\"{snippet}\"{suffix} is not valid JSON")
}

/// `/first.?second/i`: `first`, at most one character, then `second`, case-insensitively.
pub fn loose_match(text: &str, first: &str, second: &str) -> bool {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let first: Vec<char> = first.chars().collect();
    let second: Vec<char> = second.chars().collect();
    let matches_at = |i: usize, pattern: &[char]| lower.get(i..i + pattern.len()) == Some(pattern);
    (0..lower.len()).any(|i| {
        matches_at(i, &first) && (matches_at(i + first.len(), &second) || matches_at(i + first.len() + 1, &second))
    })
}
