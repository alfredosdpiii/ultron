//! Local time for printed dates (`Date#getHours` and friends): the zone named by `TZ` (a zoneinfo name or a POSIX
//! rule), else `/etc/localtime`, read from TZif files (RFC 8536) with the footer rule for times after the last
//! transition. Anything unreadable is UTC.
//!
//! A copy of `crates/cli/src/localtime.rs` for the crates below the CLI (session-core cannot depend on cli).

use std::path::{Path, PathBuf};

/// Calendar fields of a time: year, month (1-12), day, hour, minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fields {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The calendar fields of `ms` (milliseconds since the epoch) shifted by `offset_seconds`.
pub fn fields(ms: f64, offset_seconds: i64) -> Fields {
    let total = (ms / 1000.0).floor() as i64 + offset_seconds;
    let days = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Fields {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        minute: ((secs % 3600) / 60) as u32,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Day {
    /// `Jn`: 1-365, February 29 never counted.
    Julian(u32),
    /// `n`: 0-365, February 29 counted.
    Zero(u32),
    /// `Mm.w.d`.
    Month(u32, u32, u32),
}

/// A transition rule's day and its time of day in seconds.
type RuleTime = (Day, i64);

#[derive(Clone, Debug, PartialEq)]
struct PosixRule {
    std_offset: i64,
    dst: Option<(i64, RuleTime, RuleTime)>,
}

fn parse_name(s: &str) -> Option<&str> {
    if let Some(rest) = s.strip_prefix('<') {
        let end = rest.find('>')?;
        return Some(&rest[end + 1..]);
    }
    let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
    (end >= 3).then(|| &s[end..])
}

/// `[+-]hh[:mm[:ss]]` in seconds, and the rest.
fn parse_time(s: &str) -> Option<(i64, &str)> {
    let (sign, s) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let mut total = 0;
    let mut rest = s;
    for (i, scale) in [3600, 60, 1].into_iter().enumerate() {
        if i > 0 {
            match rest.strip_prefix(':') {
                Some(r) => rest = r,
                None => break,
            }
        }
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        total += rest[..end].parse::<i64>().ok()? * scale;
        rest = &rest[end..];
    }
    Some((sign * total, rest))
}

fn parse_date(s: &str) -> Option<((Day, i64), &str)> {
    let number = |s: &str| -> Option<(u32, usize)> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        Some((s[..end].parse().ok()?, end))
    };
    let (day, mut rest) = if let Some(r) = s.strip_prefix('M') {
        let (m, a) = number(r)?;
        let r = r[a..].strip_prefix('.')?;
        let (w, b) = number(r)?;
        let r = r[b..].strip_prefix('.')?;
        let (d, c) = number(r)?;
        (Day::Month(m, w, d), &r[c..])
    } else if let Some(r) = s.strip_prefix('J') {
        let (n, a) = number(r)?;
        (Day::Julian(n), &r[a..])
    } else {
        let (n, a) = number(s)?;
        (Day::Zero(n), &s[a..])
    };
    let mut time = 7200;
    if let Some(r) = rest.strip_prefix('/') {
        let (t, r) = parse_time(r)?;
        time = t;
        rest = r;
    }
    Some(((day, time), rest))
}

fn parse_posix(s: &str) -> Option<PosixRule> {
    let rest = parse_name(s)?;
    let (std_west, rest) = parse_time(rest)?;
    if rest.is_empty() {
        return Some(PosixRule {
            std_offset: -std_west,
            dst: None,
        });
    }
    let rest = parse_name(rest)?;
    let (dst_west, rest) = if rest.is_empty() || rest.starts_with(',') {
        (std_west - 3600, rest)
    } else {
        parse_time(rest)?
    };
    let rules = if rest.is_empty() { ",M3.2.0,M11.1.0" } else { rest };
    let (start, rest) = parse_date(rules.strip_prefix(',')?)?;
    let (end, rest) = parse_date(rest.strip_prefix(',')?)?;
    rest.is_empty().then_some(PosixRule {
        std_offset: -std_west,
        dst: Some((-dst_west, start, end)),
    })
}

/// Seconds since the epoch of the rule's day in `year`, at local midnight (as UTC).
fn rule_day(year: i64, day: Day) -> i64 {
    let jan1 = days_from_civil(year, 1, 1);
    let days = match day {
        Day::Julian(n) => jan1 + i64::from(n) - 1 + i64::from(is_leap(year) && n >= 60),
        Day::Zero(n) => jan1 + i64::from(n),
        Day::Month(m, w, d) => {
            let first = days_from_civil(year, m, 1);
            let weekday = (first + 4).rem_euclid(7) as u32;
            let mut day = 1 + (d + 7 - weekday) % 7 + (w.max(1) - 1) * 7;
            while day > days_in_month(year, m) {
                day -= 7;
            }
            first + i64::from(day) - 1
        }
    };
    days * 86_400
}

impl PosixRule {
    fn offset(&self, t: i64) -> i64 {
        let Some((dst_offset, (start_day, start_time), (end_day, end_time))) = self.dst else {
            return self.std_offset;
        };
        let year = fields((t + self.std_offset) as f64 * 1000.0, 0).year;
        let start = rule_day(year, start_day) + start_time - self.std_offset;
        let end = rule_day(year, end_day) + end_time - dst_offset;
        let dst = if start < end {
            start <= t && t < end
        } else {
            !(end <= t && t < start)
        };
        if dst { dst_offset } else { self.std_offset }
    }
}

/// A time zone: TZif transitions and types, and the rule for later times.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Zone {
    transitions: Vec<(i64, usize)>,
    offsets: Vec<i64>,
    footer: Option<PosixRule>,
}

fn be(bytes: &[u8], at: usize, len: usize) -> Option<i64> {
    let slice = bytes.get(at..at + len)?;
    let mut value: i64 = 0;
    for b in slice {
        value = (value << 8) | i64::from(*b);
    }
    // Sign-extend.
    let shift = 64 - 8 * len as u32;
    Some((value << shift) >> shift)
}

impl Zone {
    /// UTC.
    pub fn utc() -> Zone {
        Zone::default()
    }

    /// Parse a TZif file.
    pub fn from_tzif(bytes: &[u8]) -> Option<Zone> {
        if bytes.get(..4)? != b"TZif" {
            return None;
        }
        let counts = |at: usize| -> Option<[usize; 6]> {
            let mut c = [0; 6];
            for (i, slot) in c.iter_mut().enumerate() {
                *slot = be(bytes, at + 20 + 4 * i, 4)? as usize;
            }
            Some(c)
        };
        let [isut, isstd, leap, time, types, chars] = counts(0)?;
        let (base, size) = if bytes[4] >= b'2' {
            let v1 = 44 + time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
            (v1, 8)
        } else {
            (0, 4)
        };
        let [isut, isstd, leap, time, types, chars] = counts(base)?;
        let data = base + 44;
        let mut zone = Zone::default();
        for i in 0..time {
            let at = be(bytes, data + i * size, size)?;
            let kind = *bytes.get(data + time * size + i)? as usize;
            zone.transitions.push((at, kind));
        }
        let types_at = data + time * (size + 1);
        for i in 0..types {
            zone.offsets.push(be(bytes, types_at + i * 6, 4)?);
        }
        if size == 8 {
            let end = types_at + types * 6 + chars + leap * 12 + isstd + isut;
            let footer = bytes.get(end..)?;
            let text = std::str::from_utf8(footer).ok()?.trim_matches('\n');
            zone.footer = parse_posix(text);
        }
        Some(zone)
    }

    /// The zone of a POSIX TZ string such as `EST5EDT,M3.2.0,M11.1.0`.
    pub fn from_posix(text: &str) -> Option<Zone> {
        Some(Zone {
            footer: Some(parse_posix(text)?),
            ..Zone::default()
        })
    }

    /// The zone the process runs in: `TZ` (a zoneinfo name or a POSIX rule), else `/etc/localtime`.
    pub fn from_env(tz: Option<&str>) -> Zone {
        let read = |path: &Path| std::fs::read(path).ok().and_then(|b| Zone::from_tzif(&b));
        match tz {
            None => read(Path::new("/etc/localtime")).unwrap_or_default(),
            Some(tz) => {
                let name = tz.strip_prefix(':').unwrap_or(tz);
                if name.is_empty() {
                    return Zone::utc();
                }
                let path = if name.starts_with('/') {
                    PathBuf::from(name)
                } else {
                    Path::new("/usr/share/zoneinfo").join(name)
                };
                (!name.contains(".."))
                    .then(|| read(&path))
                    .flatten()
                    .or_else(|| Zone::from_posix(name))
                    .unwrap_or_default()
            }
        }
    }

    /// The offset from UTC, in seconds, at `ms` since the epoch.
    pub fn offset(&self, ms: f64) -> i64 {
        let t = (ms / 1000.0).floor() as i64;
        if self.transitions.is_empty()
            && let Some(footer) = &self.footer
        {
            return footer.offset(t);
        }
        match self.transitions.iter().rposition(|(at, _)| *at <= t) {
            None => self.offsets.first().copied().unwrap_or(0),
            Some(i) if i == self.transitions.len() - 1 && self.footer.is_some() => {
                self.footer.as_ref().map_or(0, |f| f.offset(t))
            }
            Some(i) => self.offsets.get(self.transitions[i].1).copied().unwrap_or(0),
        }
    }

    /// The local calendar fields of `ms`.
    pub fn fields(&self, ms: f64) -> Fields {
        fields(ms, self.offset(ms))
    }
}
