//! The parts of node-semver 7 the self-update check uses, without options (`loose` and `includePrerelease` off):
//! `valid` and `compare` (crate::package_cli `latest_release`, `is_newer`).
//!
//! Evidence: `crates/cli/fixtures/packages.mjs` records node-semver's answers over a version matrix;
//! `tests/semver_units.rs` replays them.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Text(String),
}

/// A parsed version (`new SemVer(v)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Ident>,
}

impl Version {
    fn new(major: u64, minor: u64, patch: u64, pre: Vec<Ident>) -> Self {
        Self {
            major,
            minor,
            patch,
            pre,
        }
    }

    /// `major.minor.patch[-pre]`, as `valid()` returns it.
    pub fn format(&self) -> String {
        let mut out = format!("{}.{}.{}", self.major, self.minor, self.patch);
        if !self.pre.is_empty() {
            let pre: Vec<String> = self
                .pre
                .iter()
                .map(|p| match p {
                    Ident::Num(n) => n.to_string(),
                    Ident::Text(t) => t.clone(),
                })
                .collect();
            out.push('-');
            out.push_str(&pre.join("."));
        }
        out
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| compare_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(a: &[Ident], b: &[Ident]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    for i in 0.. {
        match (a.get(i), b.get(i)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let order = match (x, y) {
                    (Ident::Num(x), Ident::Num(y)) => x.cmp(y),
                    (Ident::Num(_), Ident::Text(_)) => Ordering::Less,
                    (Ident::Text(_), Ident::Num(_)) => Ordering::Greater,
                    (Ident::Text(x), Ident::Text(y)) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
    Ordering::Equal
}

/// `NUMERICIDENTIFIER`: `0|[1-9]\d*`, within `Number.MAX_SAFE_INTEGER`.
fn numeric(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    text.parse::<u64>().ok().filter(|n| *n <= 9_007_199_254_740_991)
}

fn letter_dash_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `PRERELEASEIDENTIFIER`: a numeric identifier, or one with a letter or dash.
fn pre_ident(text: &str) -> Option<Ident> {
    if let Some(n) = numeric(text) {
        return Some(Ident::Num(n));
    }
    (letter_dash_number(text) && text.bytes().any(|b| !b.is_ascii_digit())).then(|| Ident::Text(text.to_string()))
}

/// `-pre` and `+build` after the patch number.
fn parse_tail(tail: &str) -> Option<Vec<Ident>> {
    let (rest, build) = match tail.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (tail, None),
    };
    if let Some(build) = build
        && !build.split('.').all(letter_dash_number)
    {
        return None;
    }
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let pre = rest.strip_prefix('-')?;
    pre.split('.').map(pre_ident).collect()
}

/// `new SemVer(v)` (`FULL`): `v?` then `major.minor.patch[-pre][+build]`, trimmed.
pub fn parse(version: &str) -> Option<Version> {
    let text = version.trim();
    if text.len() > 256 {
        return None;
    }
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.splitn(3, '.');
    let major = numeric(parts.next()?)?;
    let minor = numeric(parts.next()?)?;
    let last = parts.next()?;
    let end = last.find(['-', '+']).unwrap_or(last.len());
    let patch = numeric(&last[..end])?;
    let pre = parse_tail(&last[end..])?;
    Some(Version::new(major, minor, patch, pre))
}

/// `valid(v)`: the normalized version, or None.
pub fn valid(version: &str) -> Option<String> {
    parse(version).map(|v| v.format())
}

/// `compare(a, b)` for two valid versions; None when either is invalid (node-semver throws).
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(parse(a)?.cmp(&parse(b)?))
}
