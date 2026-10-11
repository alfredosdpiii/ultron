//! Small JavaScript-compatible helpers shared by the modules.

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the epoch (`Date.now()`); injected so fixtures can fix it.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0)
    })
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `crypto.randomUUID()`.
pub fn uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("operating system random source");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `new Date(ms).toISOString()`.
pub fn iso(ms: i64) -> String {
    session_file::js::iso_string(ms)
}

/// `JSON.stringify(value)`.
pub fn stringify(value: &Value) -> String {
    session_file::js::stringify(value)
}

/// `JSON.stringify(value, null, "\t")`.
pub fn stringify_tabs(value: &Value) -> String {
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    let _ = value.serialize(&mut serializer);
    String::from_utf8(out).unwrap_or_default()
}

/// Refuses payload keys outside `allowed`: "Unknown payload field: <key>".
pub fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

/// JavaScript `String.prototype.trim` whitespace.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `text.length`: UTF-16 code units.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.slice(0, units)` in UTF-16 code units (a split surrogate pair is dropped).
pub fn utf16_prefix(text: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += ch.len_utf16();
        if used > units {
            return &text[..index];
        }
    }
    text
}

/// A finite JSON number that is a safe integer (`Number.isSafeInteger`).
pub fn safe_integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0).then_some(number as i64)
}

fn collation_class(ch: char) -> u8 {
    if ch.is_ascii_digit() {
        1
    } else if ch.is_alphabetic() {
        2
    } else {
        0
    }
}

/// `a.localeCompare(b)` for the ASCII names these modules sort: punctuation before digits before letters,
/// letters case-insensitively first, then lowercase before uppercase.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let key = |text: &str| -> Vec<(u8, char)> {
        text.chars()
            .map(|ch| (collation_class(ch), ch.to_lowercase().next().unwrap_or(ch)))
            .collect()
    };
    key(a).cmp(&key(b)).then_with(|| {
        let tertiary = |text: &str| -> Vec<bool> { text.chars().map(char::is_uppercase).collect() };
        tertiary(a).cmp(&tertiary(b))
    })
}

pub(crate) fn object(entries: Vec<(&str, Value)>) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}
