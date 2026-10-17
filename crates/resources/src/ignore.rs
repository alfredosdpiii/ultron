//! The `ignore` package (7.x) as the package manager uses it: `.gitignore`-style rules, matched case-insensitively
//! (its default), the last matching rule winning, and a path ignored when any of its parent directories is.
//! Each rule becomes a regular expression the way `ignore` builds one: a leading `/` or an inner slash anchors it,
//! `**/`, `/**/` and a trailing `/**` match directory runs, `*` and `?` stay inside one segment, a trailing `/`
//! matches directories only, and a pattern without one also matches a directory of that name.
//!
//! Evidence: `crates/cli/fixtures/packages.mjs` records `ignore().add(rules).ignores(path)` over a matrix;
//! `tests/packages_units.rs` replays it. The skill loader (crate::skills) and the package manager both use it.

use regex::Regex;

/// One compiled rule.
struct Rule {
    negative: bool,
    regex: Option<Regex>,
}

/// `ignore()`: rules added in order.
#[derive(Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

/// `checkPattern`: blank lines, comments and a lone trailing backslash are not rules.
fn is_rule(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.bytes().all(|b| b == b' ')
        && !pattern.starts_with('#')
        && !(pattern.ends_with('\\') && !pattern.ends_with("\\\\"))
}

/// Trailing spaces are dropped unless escaped (`a\ ` keeps one space).
fn trim_trailing_spaces(body: &str) -> String {
    let trimmed = body.trim_end_matches(' ');
    if trimmed.len() == body.len() {
        return body.to_string();
    }
    let backslashes = trimmed.chars().rev().take_while(|c| *c == '\\').count();
    if backslashes % 2 == 1 {
        format!("{} ", &trimmed[..trimmed.len() - 1])
    } else {
        trimmed.to_string()
    }
}

const POSIX: [(&str, &str); 12] = [
    ("alnum", "0-9A-Za-z"),
    ("alpha", "A-Za-z"),
    ("blank", r" \t"),
    ("cntrl", r"\x00-\x1f\x7f"),
    ("digit", "0-9"),
    ("graph", "!-.0-~"),
    ("lower", "a-z"),
    ("print", " -.0-~"),
    ("punct", r"!-.:-@\[-`{-~"),
    ("space", r" \t\n\r"),
    ("upper", "A-Z"),
    ("xdigit", "0-9A-Fa-f"),
];

fn class_member(c: char) -> String {
    if "\\]^-[&~".contains(c) {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

/// `scanBracket`: a `[...]` class at `start`, as a regex class that never matches `/`, and the index of its `]`.
/// None: it never closes (the rule then matches nothing).
fn scan_bracket(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut index = start + 1;
    let negated = matches!(chars.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut body = String::new();
    let mut prev: Option<char> = None;
    loop {
        let c = *chars.get(index)?;
        if c == '\\' {
            let escaped = *chars.get(index + 1)?;
            body.push_str(&class_member(escaped));
            prev = Some(escaped);
            index += 1;
        } else if c == '-' && prev.is_some() && index + 1 < chars.len() && chars[index + 1] != ']' {
            index += 1;
            let mut to = chars[index];
            if to == '\\' {
                index += 1;
                to = *chars.get(index)?;
            }
            if prev.is_some_and(|p| p <= to) {
                body.push('-');
                body.push_str(&class_member(to));
            }
            prev = None;
        } else if c == '[' && chars.get(index + 1) == Some(&':') {
            let name_start = index + 2;
            let mut end = name_start;
            while end < chars.len() && chars[end] != ']' {
                end += 1;
            }
            if end == chars.len() {
                return None;
            }
            if end > name_start && chars[end - 1] == ':' {
                let name: String = chars[name_start..end - 1].iter().collect();
                let expanded = POSIX.iter().find(|(n, _)| *n == name)?.1;
                body.push_str(expanded);
                prev = None;
                index = end;
            } else {
                body.push_str(&class_member('['));
                prev = Some('[');
                index = name_start - 2;
            }
        } else {
            body.push_str(&class_member(c));
            prev = Some(c);
        }
        index += 1;
        if chars.get(index) == Some(&']') {
            let source = if negated {
                format!("[^/{body}]")
            } else {
                format!("[{body}&&[^/]]")
            };
            return Some((source, index));
        }
    }
}

/// The regex of one segment's glob (no `/` inside). None: an unclosed class (never matches).
fn segment(glob: &[char], last_of_pattern: bool, after_slash: bool) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < glob.len() {
        let c = glob[i];
        match c {
            '\\' if i + 1 < glob.len() => {
                out.push_str(&regex::escape(&glob[i + 1].to_string()));
                i += 2;
                continue;
            }
            '*' => {
                let trailing = last_of_pattern && glob[i..].iter().all(|c| *c == '*');
                if trailing {
                    // `ignore`'s trailing wildcard: after a slash it needs one character (`abc/*` is not `abc/`).
                    out.push_str(if after_slash && i == 0 { "[^/]+" } else { "[^/]*" });
                    out.push_str("(?:$|/$)");
                    return Some(out);
                }
                out.push_str("[^/]*");
            }
            '?' => out.push_str("[^/]"),
            '[' => {
                let (class, end) = scan_bracket(glob, i)?;
                out.push_str(&class);
                i = end;
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
        i += 1;
    }
    Some(out)
}

/// `makeRegex` for one rule body (without its `!`).
fn compile(body: &str) -> Option<Regex> {
    let body = trim_trailing_spaces(body);
    let mut rest: &str = &body;
    let mut source = String::new();
    let inner_slash = rest.trim_end_matches('/').contains('/');
    if let Some(stripped) = rest.strip_prefix('/') {
        source.push('^');
        rest = stripped;
    } else if rest.starts_with("**/") {
        while let Some(stripped) = rest.strip_prefix("**/") {
            rest = stripped;
        }
        source.push_str("^(?:.*/)?");
    } else if inner_slash {
        source.push('^');
    } else {
        source.push_str("(?:^|/)");
    }
    let dir_only = rest.ends_with('/');
    let core = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = core.split('/').collect();
    let count = segments.len();
    for (index, seg) in segments.iter().enumerate() {
        let last = index + 1 == count;
        if *seg == "**" && index > 0 {
            if last {
                source.push_str(if dir_only { "(?:/[^/]+)+" } else { "/.+" });
                if !dir_only {
                    return Regex::new(&format!("(?i){source}")).ok();
                }
            } else {
                source.push_str("(?:/[^/]+)*");
            }
            continue;
        }
        if index > 0 {
            source.push('/');
        }
        let glob: Vec<char> = seg.chars().collect();
        match segment(&glob, last && !dir_only, index > 0) {
            Some(s) => source.push_str(&s),
            None => return Regex::new("[^\\s\\S]").ok(),
        }
    }
    if dir_only {
        source.push_str("/$");
    } else if !source.ends_with("(?:$|/$)") {
        source.push_str("(?:$|/$)");
    }
    Regex::new(&format!("(?i){source}")).ok()
}

impl Ignore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `add(patterns)`.
    pub fn add<S: AsRef<str>>(&mut self, patterns: &[S]) {
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if !is_rule(pattern) {
                continue;
            }
            let (negative, body) = match pattern.strip_prefix('!') {
                Some(body) => (true, body),
                None => (false, pattern),
            };
            let body = if let Some(rest) = body.strip_prefix("\\!") {
                format!("!{rest}")
            } else if let Some(rest) = body.strip_prefix("\\#") {
                format!("#{rest}")
            } else {
                body.to_string()
            };
            self.rules.push(Rule {
                negative,
                regex: compile(&body),
            });
        }
    }

    fn test(&self, path: &str) -> bool {
        let mut ignored = false;
        for rule in &self.rules {
            if rule.regex.as_ref().is_some_and(|r| r.is_match(path)) {
                ignored = !rule.negative;
            }
        }
        ignored
    }

    /// `ignores(path)` for a relative POSIX path (`dir/` for a directory).
    pub fn ignores(&self, path: &str) -> bool {
        let trimmed = path.strip_suffix('/').unwrap_or(path);
        let mut at = 0;
        while let Some(slash) = trimmed[at..].find('/') {
            let parent = &trimmed[..at + slash + 1];
            if self.test(parent) {
                return true;
            }
            at += slash + 1;
        }
        self.test(path)
    }
}

/// `prefixIgnorePattern`: a rule of an ignore file in a subdirectory, made relative to the walk's root.
pub fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || (trimmed.starts_with('#') && !trimmed.starts_with("\\#")) {
        return None;
    }
    let (negated, mut pattern) = if let Some(rest) = line.strip_prefix('!') {
        (true, rest)
    } else if line.starts_with("\\!") {
        (false, &line[1..])
    } else {
        (false, line)
    };
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
    }
    let prefixed = format!("{prefix}{pattern}");
    Some(if negated { format!("!{prefixed}") } else { prefixed })
}

/// `addIgnoreRules`: `.gitignore`, `.ignore` and `.fdignore` of `dir`, prefixed with its path under `root`.
pub fn add_ignore_rules(ig: &mut Ignore, dir: &str, root: &str) {
    let rel = crate::paths::relative(root, dir);
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    for name in [".gitignore", ".ignore", ".fdignore"] {
        let Ok(content) = std::fs::read_to_string(crate::paths::join(dir, name)) else {
            continue;
        };
        let patterns: Vec<String> = content
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            ig.add(&patterns);
        }
    }
}
