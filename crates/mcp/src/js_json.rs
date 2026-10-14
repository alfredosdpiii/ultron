//! `JSON.parse` as V8 implements it, for the error messages the `mcp` gateway reports for an invalid `args` string
//! (`Error: Unexpected token 'x', "x" is not valid JSON`). Positions, lines and columns count UTF-16 units, as in
//! JavaScript; only `\n` starts a line. Numbers become what JavaScript holds (doubles), written as integers when
//! integral.

use serde_json::{Map, Number, Value};

/// Sources at least this long show the unexpected token in context (V8 `kMinOriginalSourceLengthForContext`).
const MIN_LENGTH_FOR_CONTEXT: usize = 21;
const CONTEXT_CHARS: usize = 10;

/// `JSON.parse(text)`, or V8's error message.
pub fn parse(text: &str) -> Result<Value, String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut parser = Parser { units: &units, pos: 0 };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos < units.len() {
        return Err(parser.located("Unexpected non-whitespace character after JSON", parser.pos));
    }
    Ok(value)
}

/// What a token at an unexpected place is.
enum Token {
    End,
    Number,
    Other,
}

struct Parser<'a> {
    units: &'a [u16],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0a | 0x0d)) {
            self.pos += 1;
        }
    }

    fn text(&self, from: usize, to: usize) -> String {
        String::from_utf16_lossy(&self.units[from.min(self.units.len())..to.min(self.units.len())])
    }

    /// `<message> in JSON at position P (line L column C)`.
    fn at(&self, message: &str, pos: usize) -> String {
        self.located(&format!("{message} in JSON"), pos)
    }

    /// `<message> at position P (line L column C)`.
    fn located(&self, message: &str, pos: usize) -> String {
        let before = &self.units[..pos.min(self.units.len())];
        let line = before.iter().filter(|&&u| u == 0x0a).count() + 1;
        let line_start = before.iter().rposition(|&u| u == 0x0a).map_or(0, |i| i + 1);
        format!(
            "{message} at position {pos} (line {line} column {})",
            pos - line_start + 1
        )
    }

    /// `ReportUnexpectedToken` without a specific message.
    fn unexpected(&self, token: Token) -> String {
        match token {
            Token::End => "Unexpected end of JSON input".into(),
            Token::Number => self.at("Unexpected number", self.pos),
            Token::Other => {
                let source = self.text(0, self.units.len());
                if ["NaN", "Infinity", "undefined", "[object Object]"].contains(&source.as_str()) {
                    return format!("\"{source}\" is not valid JSON");
                }
                let len = self.units.len();
                let pos = self.pos;
                let token = self.text(pos, pos + 1);
                let context = if len < MIN_LENGTH_FOR_CONTEXT {
                    format!("\"{source}\"")
                } else if pos < CONTEXT_CHARS {
                    format!("\"{}\"...", self.text(0, pos + CONTEXT_CHARS))
                } else if pos + CONTEXT_CHARS >= len {
                    format!("...\"{}\"", self.text(pos - CONTEXT_CHARS, len))
                } else {
                    format!("...\"{}\"...", self.text(pos - CONTEXT_CHARS, pos + CONTEXT_CHARS))
                };
                format!("Unexpected token '{token}', {context} is not valid JSON")
            }
        }
    }

    /// An error with V8's specific message for this place (also at the end of the input).
    fn expected(&self, message: &str) -> String {
        self.at(message, self.pos)
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            None => Err(self.unexpected(Token::End)),
            Some(0x22) => self.string().map(Value::String),
            Some(0x7b) => self.object(),
            Some(0x5b) => self.array(),
            Some(0x2d | 0x30..=0x39) => self.number(),
            Some(0x74) => self.literal("true", Value::Bool(true)),
            Some(0x66) => self.literal("false", Value::Bool(false)),
            Some(0x6e) => self.literal("null", Value::Null),
            Some(_) => Err(self.unexpected(Token::Other)),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        for expected in word.encode_utf16() {
            match self.peek() {
                None => return Err(self.unexpected(Token::End)),
                Some(unit) if unit == expected => self.pos += 1,
                Some(_) => return Err(self.unexpected(Token::Other)),
            }
        }
        Ok(value)
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(0x30..=0x39)) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(0x2d) {
            self.pos += 1;
            if !matches!(self.peek(), Some(0x30..=0x39)) {
                return Err(self.expected("No number after minus sign"));
            }
        }
        if self.peek() == Some(0x30) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x30..=0x39)) {
                return Err(self.unexpected(Token::Number));
            }
        } else {
            self.digits();
        }
        if self.peek() == Some(0x2e) {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.expected("Unterminated fractional number"));
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x2b | 0x2d)) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.expected("Exponent part is missing a number"));
            }
        }
        let value: f64 = self.text(start, self.pos).parse().unwrap_or(f64::NAN);
        Ok(js_number(value))
    }

    fn hex(&mut self) -> Result<u16, String> {
        let mut code = 0u16;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|u| char::from_u32(u32::from(u)))
                .and_then(|c| c.to_digit(16));
            let Some(digit) = digit else {
                return Err(self.expected("Bad Unicode escape"));
            };
            code = code * 16 + digit as u16;
            self.pos += 1;
        }
        Ok(code)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out: Vec<u16> = Vec::new();
        loop {
            let Some(unit) = self.peek() else {
                return Err(self.expected("Unterminated string"));
            };
            match unit {
                0x22 => {
                    self.pos += 1;
                    return Ok(String::from_utf16_lossy(&out));
                }
                0x5c => {
                    self.pos += 1;
                    let Some(escaped) = self.peek() else {
                        return Err(self.unexpected(Token::End));
                    };
                    let decoded = match escaped {
                        0x22 | 0x5c | 0x2f => escaped,
                        0x62 => 0x08,
                        0x66 => 0x0c,
                        0x6e => 0x0a,
                        0x72 => 0x0d,
                        0x74 => 0x09,
                        0x75 => {
                            self.pos += 1;
                            out.push(self.hex()?);
                            continue;
                        }
                        _ => return Err(self.expected("Bad escaped character")),
                    };
                    out.push(decoded);
                    self.pos += 1;
                }
                0..0x20 => return Err(self.expected("Bad control character in string literal")),
                _ => {
                    out.push(unit);
                    self.pos += 1;
                }
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut map = Map::new();
        self.skip_whitespace();
        if self.peek() == Some(0x7d) {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        if self.peek() != Some(0x22) {
            return Err(self.expected("Expected property name or '}'"));
        }
        loop {
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(0x3a) {
                return Err(self.expected("Expected ':' after property name"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value()?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(0x2c) => {
                    self.pos += 1;
                    self.skip_whitespace();
                    if self.peek() != Some(0x22) {
                        return Err(self.expected("Expected double-quoted property name"));
                    }
                }
                Some(0x7d) => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(self.expected("Expected ',' or '}' after property value")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(0x5d) {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(0x2c) => self.pos += 1,
                Some(0x5d) => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.expected("Expected ',' or ']' after array element")),
            }
        }
    }
}

/// A JavaScript number as JSON: integral values in the safe range as integers (so they serialize without a fraction).
pub fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}
