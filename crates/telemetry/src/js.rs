//! Small JavaScript semantics the ported payloads depend on: truthiness, `\s`/`trim`, `decodeURI`, default string
//! sort order, `Date.prototype.toISOString` and the ISO subset of `Date.parse`.

use serde_json::Value;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// JavaScript truthiness of a JSON value.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// The `\s` class (WhiteSpace and LineTerminator), which is also what `String.prototype.trim` removes.
pub(crate) fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

pub(crate) fn trim(value: &str) -> &str {
    value.trim_matches(is_space)
}

/// `Array.prototype.sort()` without a comparator: UTF-16 code unit order.
pub(crate) fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `decodeURI`: decodes escapes except those of reserved characters; `None` where JavaScript throws `URIError`.
pub(crate) fn decode_uri(value: &str) -> Option<String> {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = value.as_bytes();
    let hex = |at: usize| -> Option<u8> {
        if bytes.get(at) != Some(&b'%') {
            return None;
        }
        let digits = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
        u8::from_str_radix(digits, 16)
            .ok()
            .filter(|_| digits.bytes().all(|d| d.is_ascii_hexdigit()))
    };
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let c = value[i..].chars().next()?;
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let lead = hex(i)?;
        if lead < 0x80 {
            if RESERVED.contains(&lead) {
                out.push_str(&value[i..i + 3]);
            } else {
                out.push(char::from(lead));
            }
            i += 3;
            continue;
        }
        let len = match lead.leading_ones() {
            2 => 2,
            3 => 3,
            4 => 4,
            _ => return None,
        };
        let mut sequence = vec![lead];
        for k in 1..len {
            let next = hex(i + 3 * k)?;
            if next & 0xC0 != 0x80 {
                return None;
            }
            sequence.push(next);
        }
        out.push_str(std::str::from_utf8(&sequence).ok()?);
        i += 3 * len;
    }
    Some(out)
}

const MS_PER_DAY: i64 = 86_400_000;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month 1-12, day 1-31) for days since the Unix epoch.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        month,
        day,
    )
}

/// Milliseconds since the Unix epoch (negative before it).
pub fn epoch_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// `new Date(ms).toISOString()` for years 0-9999.
pub fn iso_timestamp(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let rem = ms.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// `Date.parse` for the ECMAScript date-time string format (`YYYY[-MM[-DD]][THH:mm[:ss[.sss]][Z|±HH:mm]]`).
/// Date-time forms without an offset are read as UTC (JavaScript reads them as local time); anything else is `None`
/// (JavaScript's NaN, except for the legacy formats its fallback parser accepts).
pub fn parse_iso_millis(value: &str) -> Option<i64> {
    let b = value.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize, n: usize| -> Option<i64> {
        let part = b.get(*i..*i + n)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *i += n;
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    let year = match b.first() {
        Some(sign @ (b'+' | b'-')) => {
            i = 1;
            let y = digits(&mut i, 6)?;
            if *sign == b'-' {
                if y == 0 {
                    return None;
                }
                -y
            } else {
                y
            }
        }
        _ => digits(&mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = digits(&mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = digits(&mut i, 2)?;
        }
    }
    // V8 accepts days 1-31 in every month and rolls over (2026-02-30 is March 2).
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * MS_PER_DAY;
    if i == b.len() {
        return Some(ms);
    }
    if b.get(i) != Some(&b'T') {
        return None;
    }
    i += 1;
    let hour = digits(&mut i, 2)?;
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let minute = digits(&mut i, 2)?;
    let (mut second, mut millis) = (0, 0);
    if b.get(i) == Some(&b':') {
        i += 1;
        second = digits(&mut i, 2)?;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            let fraction = value.get(start..i)?;
            if fraction.is_empty() {
                return None;
            }
            let padded = format!("{:0<3}", &fraction[..fraction.len().min(3)]);
            millis = padded.parse().ok()?;
        }
    }
    let valid_time =
        (hour < 24 && minute < 60 && second < 60) || (hour == 24 && minute == 0 && second == 0 && millis == 0);
    if !valid_time {
        return None;
    }
    ms += ((hour * 60 + minute) * 60 + second) * 1000 + millis;
    match b.get(i) {
        None => Some(ms),
        Some(b'Z') if i + 1 == b.len() => Some(ms),
        Some(sign @ (b'+' | b'-')) => {
            i += 1;
            let offset_hours = digits(&mut i, 2)?;
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            let offset_minutes = digits(&mut i, 2)?;
            if i != b.len() || offset_hours > 23 || offset_minutes > 59 {
                return None;
            }
            let offset = (offset_hours * 60 + offset_minutes) * 60_000;
            Some(if *sign == b'+' { ms - offset } else { ms + offset })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_js_iso_round_trip_and_parse_subset() {
        // Literal values from node: new Date(ms).toISOString() and Date.parse(s).
        assert_eq!(iso_timestamp(1_775_398_029_321), "2026-04-05T14:07:09.321Z");
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(parse_iso_millis("2026-04-05T14:07:09.321Z"), Some(1_775_398_029_321));
        assert_eq!(parse_iso_millis("2026-04-05"), Some(1_775_347_200_000));
        assert_eq!(
            parse_iso_millis("2026-04-05T11:00:00.000+02:00"),
            Some(1_775_379_600_000)
        );
        assert_eq!(parse_iso_millis("2026-04-05T10:00:00Z"), Some(1_775_383_200_000));
        assert_eq!(parse_iso_millis("2026-04-05T10:00:00.5Z"), Some(1_775_383_200_500));
        assert_eq!(parse_iso_millis("2026-02-30"), Some(1_772_409_600_000));
        assert_eq!(parse_iso_millis("2026-02-32"), None);
        assert_eq!(parse_iso_millis("+002026-04-05"), Some(1_775_347_200_000));
        assert_eq!(parse_iso_millis("2026-04-05T24:00:00Z"), Some(1_775_433_600_000));
        assert_eq!(
            parse_iso_millis("2026-04-05T10:00:00.1234567Z"),
            Some(1_775_383_200_123)
        );
        assert_eq!(parse_iso_millis("garbage"), None);
        assert_eq!(parse_iso_millis("2026-04-05T25:00Z"), None);
    }

    #[test]
    fn telemetry_js_decode_uri_matches_node() {
        // Literal values from node's decodeURI.
        assert_eq!(decode_uri("local%20memory").as_deref(), Some("local memory"));
        assert_eq!(decode_uri("a%2Fb%23c%3A%41").as_deref(), Some("a%2Fb%23c%3AA"));
        assert_eq!(decode_uri("%C3%A9%e2%82%ac").as_deref(), Some("é€"));
        assert_eq!(decode_uri("100%"), None);
        assert_eq!(decode_uri("%E0%A4%A"), None);
        assert_eq!(decode_uri("%C3%41"), None);
        assert_eq!(decode_uri("%FF"), None);
        assert_eq!(decode_uri("%ED%A0%80"), None);
    }
}
