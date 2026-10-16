//! The parts of node-semver 7 the package manager uses, without options (`loose` and `includePrerelease` off):
//! `valid`, `validRange`, `satisfies`, `maxSatisfying`, `gt` and `rcompare`. Ranges cover `||` sets, hyphen ranges,
//! x-ranges (`1`, `1.x`, `*`), tilde (`~`, `~>`), caret and primitive comparators, with node-semver's prerelease rule
//! (a prerelease version satisfies a set only when a comparator of that set names the same `major.minor.patch` with
//! a prerelease).
//!
//! Evidence: `crates/cli/fixtures/packages.mjs` records node-semver's answers over a version and range matrix;
//! `tests/packages_units.rs` replays them.

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

/// `gt(a, b)`; None when either is invalid (node-semver throws).
pub fn gt(a: &str, b: &str) -> Option<bool> {
    compare(a, b).map(|o| o == Ordering::Greater)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(Clone, Debug)]
enum Comparator {
    Any,
    Cmp(Op, Version),
}

impl Comparator {
    fn test(&self, v: &Version) -> bool {
        match self {
            Comparator::Any => true,
            Comparator::Cmp(op, c) => {
                let o = v.cmp(c);
                match op {
                    Op::Lt => o == Ordering::Less,
                    Op::Le => o != Ordering::Greater,
                    Op::Gt => o == Ordering::Greater,
                    Op::Ge => o != Ordering::Less,
                    Op::Eq => o == Ordering::Equal,
                }
            }
        }
    }
}

/// A parsed range (`new Range(r)`): comparator sets joined by `||`.
#[derive(Clone, Debug)]
pub struct Range(Vec<Vec<Comparator>>);

/// One `XRANGEPLAIN`: `[v=\s]*` then up to three parts (`x`, `X`, `*` or a number), prerelease and build only
/// after a third part. `None` parts are wildcards.
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<Ident>,
}

fn partial(text: &str) -> Option<Partial> {
    let text = text.trim_start_matches(['v', '=', ' ']);
    let part = |p: &str| -> Option<Option<u64>> {
        if matches!(p, "x" | "X" | "*") {
            Some(None)
        } else {
            numeric(p).map(Some)
        }
    };
    let mut pieces = text.splitn(3, '.');
    let major = part(pieces.next()?)?;
    let minor = match pieces.next() {
        Some(p) => part(p)?,
        None => {
            return Some(Partial {
                major,
                minor: None,
                patch: None,
                pre: Vec::new(),
            });
        }
    };
    let (patch, pre) = match pieces.next() {
        Some(last) => {
            let end = last.find(['-', '+']).unwrap_or(last.len());
            (part(&last[..end])?, parse_tail(&last[end..])?)
        }
        None => (None, Vec::new()),
    };
    // node-semver rejects a number after a wildcard (`x.1.2`).
    if (major.is_none() && (minor.is_some() || patch.is_some())) || (minor.is_none() && patch.is_some()) {
        return None;
    }
    Some(Partial {
        major,
        minor,
        patch,
        pre,
    })
}

fn zero_pre() -> Vec<Ident> {
    vec![Ident::Num(0)]
}

fn cmp(op: Op, major: u64, minor: u64, patch: u64, pre: Vec<Ident>) -> Comparator {
    Comparator::Cmp(op, Version::new(major, minor, patch, pre))
}

/// `replaceXRange` for one operator (`""` is an x-range) and partial.
fn xrange(op: &str, p: Partial) -> Option<Vec<Comparator>> {
    let op = if op == "=" && (p.major.is_none() || p.minor.is_none() || p.patch.is_none()) {
        ""
    } else {
        op
    };
    let Some(major) = p.major else {
        return Some(if op == ">" || op == "<" {
            vec![cmp(Op::Lt, 0, 0, 0, zero_pre())]
        } else {
            vec![Comparator::Any]
        });
    };
    if p.minor.is_some() && p.patch.is_some() {
        let v = Version::new(major, p.minor.unwrap_or(0), p.patch.unwrap_or(0), p.pre);
        let op = match op {
            "" | "=" => Op::Eq,
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            _ => return None,
        };
        return Some(vec![Comparator::Cmp(op, v)]);
    }
    let (mut major, mut minor, mut patch) = (major, p.minor.unwrap_or(0), 0);
    if !op.is_empty() {
        let mut op = op;
        if op == ">" {
            op = ">=";
            if p.minor.is_none() {
                major += 1;
                minor = 0;
            } else {
                minor += 1;
            }
            patch = 0;
        } else if op == "<=" {
            op = "<";
            if p.minor.is_none() {
                major += 1;
            } else {
                minor += 1;
            }
        }
        let pre = if op == "<" { zero_pre() } else { Vec::new() };
        let op = match op {
            "<" => Op::Lt,
            ">=" => Op::Ge,
            _ => return None,
        };
        return Some(vec![cmp(op, major, minor, patch, pre)]);
    }
    Some(match p.minor {
        None => vec![
            cmp(Op::Ge, major, 0, 0, Vec::new()),
            cmp(Op::Lt, major + 1, 0, 0, zero_pre()),
        ],
        Some(minor) => vec![
            cmp(Op::Ge, major, minor, 0, Vec::new()),
            cmp(Op::Lt, major, minor + 1, 0, zero_pre()),
        ],
    })
}

/// `replaceTilde`.
fn tilde(p: Partial) -> Vec<Comparator> {
    let Some(major) = p.major else {
        return vec![Comparator::Any];
    };
    match (p.minor, p.patch) {
        (None, _) => vec![
            cmp(Op::Ge, major, 0, 0, Vec::new()),
            cmp(Op::Lt, major + 1, 0, 0, zero_pre()),
        ],
        (Some(minor), None) => vec![
            cmp(Op::Ge, major, minor, 0, Vec::new()),
            cmp(Op::Lt, major, minor + 1, 0, zero_pre()),
        ],
        (Some(minor), Some(patch)) => vec![
            cmp(Op::Ge, major, minor, patch, p.pre),
            cmp(Op::Lt, major, minor + 1, 0, zero_pre()),
        ],
    }
}

/// `replaceCaret`.
fn caret(p: Partial) -> Vec<Comparator> {
    let Some(major) = p.major else {
        return vec![Comparator::Any];
    };
    let upper = |minor: u64, patch: u64| {
        if major != 0 {
            cmp(Op::Lt, major + 1, 0, 0, zero_pre())
        } else if minor != 0 {
            cmp(Op::Lt, 0, minor + 1, 0, zero_pre())
        } else {
            cmp(Op::Lt, 0, 0, patch + 1, zero_pre())
        }
    };
    match (p.minor, p.patch) {
        (None, _) => vec![
            cmp(Op::Ge, major, 0, 0, Vec::new()),
            cmp(Op::Lt, major + 1, 0, 0, zero_pre()),
        ],
        (Some(minor), None) => vec![
            cmp(Op::Ge, major, minor, 0, Vec::new()),
            if major == 0 {
                cmp(Op::Lt, 0, minor + 1, 0, zero_pre())
            } else {
                cmp(Op::Lt, major + 1, 0, 0, zero_pre())
            },
        ],
        (Some(minor), Some(patch)) => vec![cmp(Op::Ge, major, minor, patch, p.pre), upper(minor, patch)],
    }
}

/// `hyphenReplace`: `a - b`.
fn hyphen(from: Partial, to: Partial) -> Vec<Comparator> {
    let mut out = Vec::new();
    match from.major {
        None => {}
        Some(major) => out.push(cmp(
            Op::Ge,
            major,
            from.minor.unwrap_or(0),
            if from.minor.is_some() {
                from.patch.unwrap_or(0)
            } else {
                0
            },
            if from.minor.is_some() && from.patch.is_some() {
                from.pre
            } else {
                Vec::new()
            },
        )),
    }
    match (to.major, to.minor, to.patch) {
        (None, _, _) => {}
        (Some(major), None, _) => out.push(cmp(Op::Lt, major + 1, 0, 0, zero_pre())),
        (Some(major), Some(minor), None) => out.push(cmp(Op::Lt, major, minor + 1, 0, zero_pre())),
        (Some(major), Some(minor), Some(patch)) => out.push(cmp(Op::Le, major, minor, patch, to.pre)),
    }
    if out.is_empty() {
        out.push(Comparator::Any);
    }
    out
}

/// One comparator set (between `||`).
fn parse_set(text: &str) -> Option<Vec<Comparator>> {
    let text = text.trim();
    if let Some((from, to)) = text.split_once(" - ")
        && !from.contains(' ')
        && !to.trim().contains(' ')
    {
        return Some(hyphen(partial(from)?, partial(to.trim())?));
    }
    // Operators may be followed by spaces (`> 1.2`, `~ 1`, `^ 1`): glue each to its version.
    let tokens: Vec<&str> = text.split(' ').filter(|t| !t.is_empty()).collect();
    let mut glued: Vec<String> = Vec::new();
    let mut pending = String::new();
    for token in tokens {
        if matches!(token, "<" | ">" | "<=" | ">=" | "=" | "~" | "~>" | "^") {
            pending.push_str(token);
            continue;
        }
        glued.push(format!("{pending}{token}"));
        pending.clear();
    }
    if !pending.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for token in glued {
        let comparators = if let Some(rest) = token.strip_prefix("~>").or_else(|| token.strip_prefix('~')) {
            tilde(partial(rest)?)
        } else if let Some(rest) = token.strip_prefix('^') {
            caret(partial(rest)?)
        } else {
            let op_len = token.find(|c| !matches!(c, '<' | '>' | '=')).unwrap_or(token.len());
            let (op, rest) = token.split_at(op_len);
            if !matches!(op, "" | "<" | ">" | "<=" | ">=" | "=") {
                return None;
            }
            xrange(op, partial(rest)?)?
        };
        out.extend(comparators);
    }
    if out.is_empty() {
        out.push(Comparator::Any);
    }
    Some(out)
}

/// `new Range(r)`; None when node-semver throws (`validRange(r) === null`).
pub fn parse_range(range: &str) -> Option<Range> {
    let collapsed = range.split_whitespace().collect::<Vec<_>>().join(" ");
    let sets: Vec<Vec<Comparator>> = collapsed.split("||").map(parse_set).collect::<Option<_>>()?;
    Some(Range(sets))
}

/// `validRange(r) !== null`.
pub fn valid_range(range: &str) -> bool {
    parse_range(range).is_some()
}

impl Range {
    /// `range.test(v)`.
    pub fn test(&self, version: &Version) -> bool {
        self.0.iter().any(|set| {
            if !set.iter().all(|c| c.test(version)) {
                return false;
            }
            if version.pre.is_empty() {
                return true;
            }
            set.iter().any(|c| match c {
                Comparator::Cmp(_, v) => {
                    !v.pre.is_empty() && (v.major, v.minor, v.patch) == (version.major, version.minor, version.patch)
                }
                Comparator::Any => false,
            })
        })
    }
}

/// `satisfies(version, range)`.
pub fn satisfies(version: &str, range: &str) -> bool {
    match (parse(version), parse_range(range)) {
        (Some(v), Some(r)) => r.test(&v),
        _ => false,
    }
}

/// `maxSatisfying(versions, range)`: the highest satisfying version, as given.
pub fn max_satisfying<'a>(versions: &[&'a str], range: &str) -> Option<&'a str> {
    let range = parse_range(range)?;
    let mut best: Option<(&str, Version)> = None;
    for v in versions {
        let Some(parsed) = parse(v) else { continue };
        if range.test(&parsed) && best.as_ref().is_none_or(|(_, b)| parsed > *b) {
            best = Some((v, parsed));
        }
    }
    best.map(|(v, _)| v)
}

/// `[...versions].sort(rcompare)[0]`: the highest version; None when one is invalid (node-semver throws).
pub fn highest<'a>(versions: &[&'a str]) -> Option<&'a str> {
    let mut parsed: Vec<(&str, Version)> = Vec::new();
    for v in versions {
        parsed.push((v, parse(v)?));
    }
    let mut best: Option<(&str, Version)> = None;
    for (v, version) in parsed {
        if best.as_ref().is_none_or(|(_, b)| version > *b) {
            best = Some((v, version));
        }
    }
    best.map(|(v, _)| v)
}
