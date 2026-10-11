//! JavaScript semantics the session format depends on: `JSON.stringify` output, `Date` parsing and
//! `toISOString`, `String.prototype.trim`, truthiness, and number coercion.

use serde_json::{Map, Number, Value};

/// Serializes like `JSON.stringify(value)`: JavaScript number formatting, integer-like object keys
/// first in ascending order, and only the escapes `JSON.stringify` emits.
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_to_string(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in object_entries(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JavaScript number as `Number.prototype.toString` prints it. Integers beyond 2^53 are rounded
/// to the nearest double first, as `JSON.parse` does.
pub fn number_to_string(n: &Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(u) = n.as_u64() {
        if u <= SAFE {
            return u.to_string();
        }
    } else if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= SAFE
    {
        return i.to_string();
    }
    format_f64(n.as_f64().unwrap_or(f64::NAN))
}

/// `Number.prototype.toString()` for a double; non-finite values become `null` as in JSON.
pub fn format_f64(f: f64) -> String {
    if !f.is_finite() {
        return "null".to_string();
    }
    if f == 0.0 {
        return "0".to_string();
    }
    let sign = if f < 0.0 { "-" } else { "" };
    // Rust's `{:e}` prints the shortest round-tripping digits, like ECMAScript's Number::toString.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64;
    let n = exponent.parse::<i64>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e_sign = if e < 0 { "-" } else { "+" };
        if k == 1 {
            format!("{digits}e{e_sign}{}", e.abs())
        } else {
            format!("{}.{}e{e_sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

fn is_array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u32>().ok().filter(|n| *n != u32::MAX)
}

/// Object entries in JavaScript property order: array-index keys ascending, then insertion order.
pub fn object_entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut indexed: Vec<(u32, &String, &Value)> = map
        .iter()
        .filter_map(|(k, v)| is_array_index(k).map(|i| (i, k, v)))
        .collect();
    indexed.sort_by_key(|(i, _, _)| *i);
    let mut out: Vec<(&String, &Value)> = indexed.into_iter().map(|(_, k, v)| (k, v)).collect();
    out.extend(map.iter().filter(|(k, _)| is_array_index(k).is_none()));
    out
}

/// JavaScript truthiness of a JSON value; `None` is `undefined`.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `undefined` or `null`, the values `??` and `== null` treat as missing.
pub fn nullish(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `Number(value)` for JSON primitives; objects and arrays are `NaN`.
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => string_to_number(s),
        Value::Array(_) | Value::Object(_) => f64::NAN,
    }
}

fn string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() || !rest.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return rest.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(int) && digits(frac) && !(int.is_empty() && frac.is_empty());
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    if mantissa_ok && exponent_ok {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `String(value)` as `Array.prototype.join` applies it: `null` and `undefined` become "".
pub fn join_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => number_to_string(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|v| join_string(Some(v))).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

const MAX_TIME_MS: f64 = 8.64e15;

/// ECMAScript TimeClip: `None` is an invalid date.
pub fn time_clip(ms: f64) -> Option<i64> {
    if !ms.is_finite() || ms.abs() > MAX_TIME_MS {
        return None;
    }
    Some(ms.trunc() as i64)
}

/// `new Date(value).getTime()` for a JSON value; `None` is `NaN` (an invalid date).
pub fn date_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::String(s) => parse_date(s),
        Value::Array(_) | Value::Object(_) => None,
        other => time_clip(to_number(other)),
    }
}

/// A timestamp as a JSON number, or `null` for an invalid date (as `JSON.stringify(NaN)` writes it).
pub fn date_json(value: Option<&Value>) -> Value {
    date_value(value).map_or(Value::Null, Value::from)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `new Date(ms).toISOString()`.
pub fn iso_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else {
        format!("{}{:06}", if y < 0 { '-' } else { '+' }, y.abs())
    };
    format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn digits(&mut self, min: usize, max: usize) -> Option<i64> {
        let start = self.i;
        while self.i < self.s.len() && self.i - start < max && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        if self.i - start < min {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
    fn eat(&mut self, c: u8) -> bool {
        let hit = self.s.get(self.i) == Some(&c);
        self.i += usize::from(hit);
        hit
    }
    fn peek_any(&self, set: &[u8]) -> Option<u8> {
        self.s.get(self.i).copied().filter(|c| set.contains(c))
    }
}

/// `Date.parse` for the ISO-8601 forms V8 accepts (`YYYY[-MM[-DD]][THH:mm[:ss[.fff…]]][Z|±HH:mm]`,
/// `±YYYYYY` years, a space or lowercase `t`/`z`, one-digit month/day, day overflow up to 31).
/// A date-time without an offset is read as UTC. Other formats are invalid (`None`).
pub fn parse_date(input: &str) -> Option<i64> {
    let mut c = Cursor {
        s: trim(input).as_bytes(),
        i: 0,
    };
    let year = match c.peek_any(b"+-") {
        Some(sign) => {
            c.i += 1;
            let y = c.digits(6, 6)?;
            if sign == b'-' && y == 0 {
                return None;
            }
            if sign == b'-' { -y } else { y }
        }
        None => c.digits(4, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if c.eat(b'-') {
        month = c.digits(1, 2)?;
        if c.eat(b'-') {
            day = c.digits(1, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time_ms = 0;
    let mut offset_min = 0;
    if c.peek_any(b"Tt ").is_some() {
        c.i += 1;
        let hour = c.digits(2, 2)?;
        if !c.eat(b':') {
            return None;
        }
        let minute = c.digits(2, 2)?;
        let (mut second, mut millis) = (0, 0);
        if c.eat(b':') {
            second = c.digits(2, 2)?;
            if c.eat(b'.') {
                let start = c.i;
                c.digits(1, usize::MAX)?;
                let frac = std::str::from_utf8(&c.s[start..c.i]).ok()?;
                millis = format!("{frac:0<3}")[..3].parse::<i64>().ok()?;
            }
        }
        if hour > 24 || minute > 59 || second > 59 || (hour == 24 && (minute, second, millis) != (0, 0, 0)) {
            return None;
        }
        time_ms = ((hour * 60 + minute) * 60 + second) * 1000 + millis;
        if c.peek_any(b"Zz").is_some() {
            c.i += 1;
        } else if let Some(sign) = c.peek_any(b"+-") {
            c.i += 1;
            let oh = c.digits(2, 2)?;
            c.eat(b':');
            let om = c.digits(2, 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            offset_min = (oh * 60 + om) * if sign == b'+' { 1 } else { -1 };
        }
    }
    if c.i != c.s.len() {
        return None;
    }
    let ms = days_from_civil(year, month, 1) * 86_400_000 + (day - 1) * 86_400_000 + time_ms - offset_min * 60_000;
    time_clip(ms as f64)
}

/// `Array.prototype.sort` with a comparator, as V8 runs it on arrays under 64 elements (one natural
/// run, then binary insertion). A `NaN` comparison counts as equal. Stable; never panics on an
/// inconsistent comparator.
// ponytail: V8 merges runs for 64+ elements; for consistent comparators any stable sort agrees.
pub fn sort_by<T>(items: &mut [T], compare: impl Fn(&T, &T) -> f64) {
    let less = |a: &T, b: &T| compare(a, b) < 0.0;
    let n = items.len();
    if n < 2 {
        return;
    }
    let mut run = 2;
    let descending = less(&items[1], &items[0]);
    while run < n {
        let order_less = less(&items[run], &items[run - 1]);
        let equal_or_greater = !order_less;
        if (descending && equal_or_greater) || (!descending && order_less) {
            break;
        }
        run += 1;
    }
    if descending {
        items[..run].reverse();
    }
    for start in run..n {
        let (mut left, mut right) = (0, start);
        while left < right {
            let mid = left + (right - left) / 2;
            if less(&items[start], &items[mid]) {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        items[left..=start].rotate_right(1);
    }
}
