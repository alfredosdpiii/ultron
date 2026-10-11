//! JavaScript semantics the ported subcommands print with: `toFixed`, UTF-16 lengths and padding, `String(value)`,
//! numbers that serialize like `JSON.stringify`, `String.prototype.replace` with a string pattern, and base64.

use serde_json::{Number, Value};

/// `x.toFixed(digits)`: the nearest decimal; an exact tie picks the larger magnitude (Rust's formatting picks the
/// even digit).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if x < 0.0 {
        return format!("-{}", to_fixed(-x, digits));
    }
    let exact = format!("{:.*}", digits + 30, x);
    let (kept, rest) = exact.split_at(exact.len() - 30);
    if !(rest.starts_with('5') && rest[1..].bytes().all(|b| b == b'0')) {
        return format!("{x:.digits$}");
    }
    let kept = kept.strip_suffix('.').unwrap_or(kept);
    let mut bytes: Vec<u8> = kept.bytes().collect();
    let mut index = bytes.len();
    loop {
        if index == 0 {
            bytes.insert(0, b'1');
            break;
        }
        index -= 1;
        match bytes[index] {
            b'.' => continue,
            b'9' => bytes[index] = b'0',
            digit => {
                bytes[index] = digit + 1;
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap_or_default()
}

/// `text.length`: UTF-16 code units.
pub fn len16(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.padEnd(width)`.
pub fn pad_end(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(len16(text))))
}

/// `text.padStart(width)`.
pub fn pad_start(text: &str, width: usize) -> String {
    format!("{}{text}", " ".repeat(width.saturating_sub(len16(text))))
}

/// `text.slice(start)` in UTF-16 code units (a cut inside a surrogate pair keeps the pair out).
pub fn tail16(text: &str, start: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units >= start {
            return &text[index..];
        }
        units += c.len_utf16();
    }
    ""
}

/// `String(x)` for a finite number.
pub fn number(x: f64) -> String {
    session_file::js::format_f64(x)
}

/// A number as `JSON.stringify` writes it: integral values without a fraction.
pub fn num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(x as i64))
    } else {
        Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

/// `String(value)` for a JSON value.
pub fn string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => session_file::js::number_to_string(n),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `haystack.replace(pattern, replacement)` with a string pattern: the first occurrence only, and the replacement's
/// `$$`, `$&`, `` $` `` and `$'` patterns expanded.
pub fn replace_first(haystack: &str, pattern: &str, replacement: &str) -> String {
    let Some(at) = haystack.find(pattern) else {
        return haystack.to_string();
    };
    let before = &haystack[..at];
    let after = &haystack[at + pattern.len()..];
    let mut out = String::with_capacity(haystack.len() + replacement.len());
    out.push_str(before);
    let mut chars = replacement.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().map(|(_, n)| *n) {
            Some('$') => {
                out.push('$');
                chars.next();
            }
            Some('&') => {
                out.push_str(pattern);
                chars.next();
            }
            Some('`') => {
                out.push_str(before);
                chars.next();
            }
            Some('\'') => {
                out.push_str(after);
                chars.next();
            }
            _ => out.push('$'),
        }
    }
    out.push_str(after);
    out
}

/// Standard base64 with padding (`Buffer.toString("base64")`).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `Math.round(x)`: halves round toward positive infinity.
pub fn round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// A character JavaScript's `\s` and `trim` treat as whitespace.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `text.slice(0, end)` in UTF-16 code units; a cut inside a surrogate pair keeps the pair out.
pub fn head16(text: &str, end: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        units += c.len_utf16();
        if units > end {
            return &text[..index];
        }
    }
    text
}

/// `text.trim()`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Decode standard base64 (padding optional); None for other characters.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}
