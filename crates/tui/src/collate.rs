//! `String.prototype.localeCompare` (ICU root collation, the default locale of the TS runtime) for the strings the
//! TUI sorts: file names and theme names. Primary level: whitespace, then ASCII punctuation and symbols in CLDR
//! root order, then digits, then letters ignoring case; ties break on case, lowercase first. Characters outside
//! ASCII sort after ASCII letters by code point (no accent folding).

use std::cmp::Ordering;

/// CLDR root order of ASCII punctuation and symbols.
const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if c.is_ascii_digit() {
        return (2, c as u32);
    }
    if c.is_ascii_alphabetic() {
        return (3, c.to_ascii_lowercase() as u32);
    }
    if let Some(i) = PUNCTUATION.find(c) {
        return (1, i as u32);
    }
    if crate::utils::is_whitespace_char(c) {
        return (0, c as u32);
    }
    let lower = c.to_lowercase().next().unwrap_or(c);
    (4, lower as u32)
}

fn is_upper(c: char) -> bool {
    c.is_uppercase()
}

/// `a.localeCompare(b)`.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    a.chars()
        .map(primary)
        .cmp(b.chars().map(primary))
        .then_with(|| a.chars().map(is_upper).cmp(b.chars().map(is_upper)))
}
