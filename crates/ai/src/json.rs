//! JSON helpers ported from `utils/json-parse.ts`, `utils/hash.ts`, and the `partial-json` 0.1.7 parser upstream uses
//! for streamed tool arguments.

use serde_json::{Map, Number, Value};

/// Parse JSON the way `JSON.parse` yields numbers: integral values become integers, so re-serializing matches
/// `JSON.stringify` (`-3e2` is `-300`, not `-300.0`).
pub fn parse_json(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<Value>(text).map(normalize_numbers)
}

/// Integral floats within the exact range become integers; `-0` becomes `0`.
pub fn normalize_numbers(value: Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap_or_default();
            if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                // ponytail: i64 covers the exact-integer range of an f64.
                Value::Number(Number::from(f as i64))
            } else {
                Value::Number(n)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, normalize_numbers(v))).collect()),
        other => other,
    }
}

/// `JSON.stringify` of a parsed value.
pub fn stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

/// Escapes raw control characters inside strings and doubles backslashes before invalid escapes.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }
        if c == '"' {
            repaired.push(c);
            in_string = false;
            index += 1;
            continue;
        }
        if c == '\\' {
            let Some(&next) = chars.get(index + 1) else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };
            if next == 'u' {
                let digits: String = chars.iter().skip(index + 2).take(4).collect();
                if digits.len() == 4 && digits.chars().all(|d| d.is_ascii_hexdigit()) {
                    repaired.push_str("\\u");
                    repaired.push_str(&digits);
                    index += 6;
                    continue;
                }
            }
            if VALID_JSON_ESCAPES.contains(&next) {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }
            repaired.push_str("\\\\");
            index += 1;
            continue;
        }
        if (c as u32) <= 0x1f {
            match c {
                '\u{8}' => repaired.push_str("\\b"),
                '\u{c}' => repaired.push_str("\\f"),
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                other => repaired.push_str(&format!("\\u{:04x}", other as u32)),
            }
        } else {
            repaired.push(c);
        }
        index += 1;
    }
    repaired
}

pub fn parse_json_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    match parse_json(json) {
        Ok(value) => Ok(value),
        Err(error) => {
            let repaired = repair_json(json);
            if repaired != json {
                parse_json(&repaired)
            } else {
                Err(error)
            }
        }
    }
}

/// Parse possibly incomplete streamed JSON; never fails (an empty object when nothing parses).
pub fn parse_streaming_json(partial: &str) -> Value {
    if partial.trim().is_empty() {
        return Value::Object(Map::new());
    }
    if let Ok(value) = parse_json_with_repair(partial) {
        return value;
    }
    let or_empty = |v: Value| if v.is_null() { Value::Object(Map::new()) } else { v };
    if let Some(value) = partial_parse(partial) {
        return or_empty(value);
    }
    if let Some(value) = partial_parse(&repair_json(partial)) {
        return or_empty(value);
    }
    Value::Object(Map::new())
}

/// `partial-json`'s `parse(text, Allow.ALL)`; `None` where it throws. `Infinity`/`NaN` become `null`, as their
/// `JSON.stringify` does.
pub fn partial_parse(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parser = Partial {
        s: trimmed.chars().collect(),
        i: 0,
    };
    parser.any()
}

struct Partial {
    s: Vec<char>,
    i: usize,
}

/// JavaScript `substring`: clamps to the length and swaps reversed bounds.
fn substring(s: &[char], a: isize, b: isize) -> String {
    let len = s.len() as isize;
    let (a, b) = (a.clamp(0, len) as usize, b.clamp(0, len) as usize);
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    s[start..end].iter().collect()
}

fn last_index_of(s: &[char], c: char) -> isize {
    s.iter().rposition(|&x| x == c).map_or(-1, |p| p as isize)
}

fn js_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => stringify(other),
    }
}

impl Partial {
    fn at(&self, i: usize) -> Option<char> {
        self.s.get(i).copied()
    }

    fn sub(&self, a: usize, b: usize) -> String {
        substring(&self.s, a as isize, b as isize)
    }

    fn literal(&mut self, word: &str, min_rest: usize) -> bool {
        let n = word.chars().count();
        let rest = self.s.len() - self.i;
        let ahead = self.sub(self.i, self.i + n);
        if ahead == word || (rest < n && rest > min_rest && word.starts_with(&self.sub(self.i, self.s.len()))) {
            self.i += n;
            return true;
        }
        false
    }

    fn any(&mut self) -> Option<Value> {
        self.skip_blank();
        if self.i >= self.s.len() {
            return None;
        }
        match self.s[self.i] {
            '"' => return self.string(),
            '{' => return self.object(),
            '[' => return self.array(),
            _ => {}
        }
        if self.literal("null", 0) {
            return Some(Value::Null);
        }
        if self.literal("true", 0) {
            return Some(Value::Bool(true));
        }
        if self.literal("false", 0) {
            return Some(Value::Bool(false));
        }
        if self.literal("Infinity", 0) || self.literal("-Infinity", 1) || self.literal("NaN", 0) {
            return Some(Value::Null);
        }
        self.number()
    }

    fn string(&mut self) -> Option<Value> {
        let start = self.i;
        let mut escape = false;
        self.i += 1;
        while self.i < self.s.len() && (self.s[self.i] != '"' || (escape && self.s[self.i - 1] == '\\')) {
            escape = if self.s[self.i] == '\\' { !escape } else { false };
            self.i += 1;
        }
        if self.at(self.i) == Some('"') {
            self.i += 1;
            return parse_json(&self.sub(start, self.i - usize::from(escape))).ok();
        }
        let head = self.sub(start, self.i - usize::from(escape));
        match parse_json(&format!("{head}\"")) {
            Ok(value) => Some(value),
            Err(_) => {
                let cut = substring(&self.s, start as isize, last_index_of(&self.s, '\\'));
                parse_json(&format!("{cut}\"")).ok()
            }
        }
    }

    fn object(&mut self) -> Option<Value> {
        self.i += 1;
        self.skip_blank();
        let mut object = Map::new();
        while self.at(self.i) != Some('}') {
            self.skip_blank();
            if self.i >= self.s.len() {
                return Some(Value::Object(object));
            }
            let Some(key) = self.string() else {
                return Some(Value::Object(object));
            };
            self.skip_blank();
            self.i += 1;
            match self.any() {
                Some(value) => {
                    object.insert(js_key(&key), value);
                }
                None => return Some(Value::Object(object)),
            }
            self.skip_blank();
            if self.at(self.i) == Some(',') {
                self.i += 1;
            }
        }
        self.i += 1;
        Some(Value::Object(object))
    }

    fn array(&mut self) -> Option<Value> {
        self.i += 1;
        let mut items = Vec::new();
        while self.at(self.i) != Some(']') {
            match self.any() {
                Some(value) => items.push(value),
                None => return Some(Value::Array(items)),
            }
            self.skip_blank();
            if self.at(self.i) == Some(',') {
                self.i += 1;
            }
        }
        self.i += 1;
        Some(Value::Array(items))
    }

    fn number(&mut self) -> Option<Value> {
        let last_e = last_index_of(&self.s, 'e');
        if self.i == 0 {
            let whole: String = self.s.iter().collect();
            if whole == "-" {
                return None;
            }
            return parse_json(&whole)
                .ok()
                .or_else(|| parse_json(&substring(&self.s, 0, last_e)).ok());
        }
        let start = self.i;
        if self.at(self.i) == Some('-') {
            self.i += 1;
        }
        while let Some(c) = self.at(self.i) {
            if ",]}".contains(c) {
                break;
            }
            self.i += 1;
        }
        let literal = self.sub(start, self.i);
        if let Ok(value) = parse_json(&literal) {
            return Some(value);
        }
        if literal == "-" {
            return None;
        }
        parse_json(&substring(&self.s, start as isize, last_e)).ok()
    }

    fn skip_blank(&mut self) {
        while self.i < self.s.len() && " \n\r\t".contains(self.s[self.i]) {
            self.i += 1;
        }
    }
}

/// `shortHash`: a fast deterministic 53-bit hash of the UTF-16 code units, in base 36.
pub fn short_hash(text: &str) -> String {
    let mut h1: u32 = 0xdeadbeef;
    let mut h2: u32 = 0x41c6ce57;
    for unit in text.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2654435761);
        h2 = (h2 ^ ch).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut n: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit(n % 36, 36).unwrap_or('0'));
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// Number of UTF-16 code units, JavaScript's `length`.
pub(crate) fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JavaScript `text.slice(0, n)` over UTF-16 code units, never splitting a surrogate pair.
pub(crate) fn js_prefix(text: &str, n: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        units += c.len_utf16();
        if units > n {
            return &text[..index];
        }
    }
    text
}
