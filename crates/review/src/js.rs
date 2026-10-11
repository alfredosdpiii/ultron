//! JavaScript semantics the ported autoreview code depends on: `\s` and `trim`, UTF-16 string lengths, `toFixed`,
//! `Math.round`, `String(number)`, Node's `path.join`, and numbers that serialize like JSON.stringify writes them.

use serde::{Deserialize, Serialize, Serializer};

/// A character JavaScript's `\s` and `String.prototype.trim` treat as whitespace: WhiteSpace (with U+FEFF and the
/// Zs category) and LineTerminator. Unlike Unicode White_Space, U+0085 is not one.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `text.trim()`.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `text.replace(/\s+/g, " ").trim()`.
pub fn flatten(text: &str) -> String {
    text.split(is_space)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text.length`: UTF-16 code units.
pub fn len16(text: &str) -> usize {
    text.encode_utf16().count()
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

/// JavaScript's default `Array.prototype.sort` order for strings: UTF-16 code units.
pub fn cmp16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Math.round(x)`: halves round toward positive infinity.
pub fn round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// `String(x)` for a number in the range where JavaScript does not switch to exponent notation.
pub fn number(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e21 {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// `x.toFixed(digits)`: the nearest decimal; an exact tie picks the larger magnitude (Rust's formatting picks the
/// even digit).
pub fn to_fixed(x: f64, digits: usize) -> String {
    if x < 0.0 {
        return format!("-{}", to_fixed(-x, digits));
    }
    let exact = format!("{:.*}", digits + 30, x);
    let cut = exact.len() - 30;
    let (kept, rest) = exact.split_at(cut);
    let kept = kept.strip_suffix('.').unwrap_or(kept);
    if !(rest.starts_with('5') && rest[1..].bytes().all(|b| b == b'0')) {
        return format!("{:.*}", digits, x);
    }
    // A tie: increment the kept digits.
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

/// Node's `path.posix.join`: the non-empty parts joined with `/`, then normalized (`.`, `..` and repeated slashes
/// resolved; a trailing slash kept); `.` when nothing is left.
pub fn path_join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return ".".into();
    }
    let absolute = joined.starts_with('/');
    let trailing = joined.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let mut path = out.join("/");
    if path.is_empty() && !absolute {
        path = ".".into();
    }
    if trailing && !path.is_empty() && !(absolute && out.is_empty()) {
        path.push('/');
    }
    if absolute { format!("/{path}") } else { path }
}

/// A JavaScript number: written by JSON.stringify as an integer when it is one.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Deserialize)]
#[serde(transparent)]
pub struct Num(pub f64);

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.fract() == 0.0 && self.0.abs() < 9_007_199_254_740_992.0 {
            serializer.serialize_i64(self.0 as i64)
        } else {
            serializer.serialize_f64(self.0)
        }
    }
}

/// `Math.min(max, Math.max(min, value))`.
pub fn clamp(value: f64, min: f64, max: f64) -> f64 {
    max.min(min.max(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_js_to_fixed_rounds_ties_up_like_javascript() {
        // Literal values from node: (2.25).toFixed(1), (0.125).toFixed(2), (1.005).toFixed(2), (9.95).toFixed(1),
        // (0.31).toFixed(2), (9.995).toFixed(2), (0.5).toFixed(0), (-2.25).toFixed(1).
        assert_eq!(to_fixed(2.25, 1), "2.3");
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(9.95, 1), "9.9");
        assert_eq!(to_fixed(0.31, 2), "0.31");
        assert_eq!(to_fixed(9.995, 2), "9.99");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(9.5, 0), "10");
        assert_eq!(to_fixed(-2.25, 1), "-2.3");
    }

    #[test]
    fn review_js_path_join_normalizes_like_node() {
        // Literal values from node's path.posix.join.
        assert_eq!(path_join(&["/agent/", "autoreview"]), "/agent/autoreview");
        assert_eq!(path_join(&["/a/../b", "x"]), "/b/x");
        assert_eq!(path_join(&["", "mise"]), "mise");
        assert_eq!(path_join(&[".", "mise"]), "mise");
        assert_eq!(path_join(&["a/../..", "b"]), "../b");
        assert_eq!(path_join(&["/..", ".."]), "/");
        assert_eq!(path_join(&["a", ".."]), ".");
        assert_eq!(path_join(&["a/", ""]), "a/");
        assert_eq!(path_join(&["/", ""]), "/");
        assert_eq!(path_join(&["", ""]), ".");
        assert_eq!(path_join(&["x/./", "./"]), "x/");
    }

    #[test]
    fn review_js_whitespace_is_javascripts_not_unicodes() {
        assert_eq!(flatten(" a\u{85}b\u{feff} \u{2028}c\t"), "a\u{85}b c");
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(0.49999999999999994), 0.0);
        assert_eq!(head16("a\u{1F600}b", 2), "a");
        assert_eq!(head16("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(len16("é\u{1F600}"), 3);
    }
}
