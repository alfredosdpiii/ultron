//! The bash approval decision, ported from oh-my-pi (fa5ff4a) `tools/bash.ts` (`CRITICAL_BASH_PATTERNS`,
//! `BashTool.approval`, the `bash.patterns` rule matching) and `tools/shell-tokenize.ts` (`tokenizeShellSegments`,
//! `extractLiteralAndChainSegments`).
//!
//! The critical patterns are kept as omp's JavaScript regular-expression sources and flags and translated once to the
//! `regex` crate's syntax with JavaScript's (non-unicode) semantics: `\b` is an ASCII word boundary, `\s` is
//! ECMAScript's whitespace set, `.` excludes line terminators, and `i` folds ASCII letters only.

use crate::approval::{Decision, Policy, Tier};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// omp's `CRITICAL_BASH_PATTERNS`: (JavaScript source, flags), verbatim.
pub const CRITICAL_PATTERN_SOURCES: &[(&str, &str)] = &[
    (
        r"\brm\s+(?:-\S+\s+)*(?:-[a-z]*[rRfF][a-z]*|--recursive|--force)\s+(?:-\S+\s+)*\/",
        "i",
    ),
    (r"\brm\s+(?:-\S+\s+)*--no-preserve-root\b", "i"),
    (r"\bsudo\s+rm\b", "i"),
    (r"\bchmod\s+-R\s+[0-7]+\s+\/", "i"),
    (r"\bchmod\s+-R\s+[ugoa+\-=rwxXst,]+\s+\/", ""),
    (r"\bchown\s+-R\s+\S+\s+\/", "i"),
    (r":\(\)\s*\{\s*:\s*\|\s*:", "i"),
    (r">\s*\/dev\/sd[a-z]", "i"),
    (r"\bmkfs(\.|\b)", "i"),
    (r"\bdd\s+if=.+of=\/dev\/", "i"),
    (r"\bshred\s+\/dev\/", "i"),
    (r"\bcryptsetup\b", "i"),
    (r">\s*\/etc\/(?:passwd|shadow|sudoers)\b", "i"),
    (r"\btee\s+(?:-a\s+)?\/etc\/(?:passwd|shadow|sudoers)\b", "i"),
    (r"\b(?:curl|wget|fetch)\b[^|]*\|\s*(?:bash|sh|zsh|fish)\b", "i"),
    (
        r"(?:^|[\s;&|(])(?:bash|sh|zsh|source|\.)\s+<\(\s*(?:curl|wget|fetch)\b",
        "i",
    ),
    (
        r#"\beval\s+["'`]?\$\(\s*(?:curl|wget|fetch)\b|\beval\s+`\s*(?:curl|wget|fetch)\b"#,
        "i",
    ),
    (r"\bkill\s+-9\s+1\b", ""),
    (r"(?:^|[\s;&|(])(?:shutdown|poweroff|reboot|halt)(?:\s|$|[;|&])", "i"),
    (r"(?:^|[\s;&|(])init\s+0\b", "i"),
    (r"\bnc\b[^|;]*\s-[a-zA-Z]*[ec][a-zA-Z]*\s", "i"),
];

/// ECMAScript WhiteSpace and LineTerminator code points (what `\s` matches).
const JS_SPACE: &str =
    r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

/// Translate a JavaScript (non-unicode) regular expression source to `regex` syntax. Covers the constructs the
/// critical table uses; anything else is passed through.
pub fn translate_js_regex(source: &str, ignore_case: bool) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            i += 2;
            match (next, in_class) {
                ('s', false) => out.push_str(&format!("[{JS_SPACE}]")),
                ('S', false) => out.push_str(&format!("[^{JS_SPACE}]")),
                ('s', true) => out.push_str(JS_SPACE),
                ('b', false) => out.push_str(r"(?-u:\b)"),
                ('/', _) => out.push('/'),
                _ => {
                    out.push('\\');
                    out.push(next);
                }
            }
            continue;
        }
        i += 1;
        if in_class {
            match ch {
                ']' => {
                    in_class = false;
                    out.push(']');
                }
                c if ignore_case && c.is_ascii_alphabetic() => {
                    // A range `a-z` gains its other case `A-Z`; a single letter both cases.
                    if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(char::is_ascii_alphabetic) {
                        let end = chars[i + 1];
                        i += 2;
                        out.push_str(&format!("{c}-{end}"));
                        out.push_str(&format!("{}-{}", swap_case(c), swap_case(end)));
                    } else {
                        out.push(c);
                        out.push(swap_case(c));
                    }
                }
                '&' | '~' => {
                    out.push('\\');
                    out.push(ch);
                }
                c => out.push(c),
            }
            continue;
        }
        match ch {
            '[' => {
                in_class = true;
                out.push('[');
                if chars.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
            }
            '.' => out.push_str(r"[^\n\r\x{2028}\x{2029}]"),
            c if ignore_case && c.is_ascii_alphabetic() => out.push_str(&format!("[{c}{}]", swap_case(c))),
            c => out.push(c),
        }
    }
    out
}

fn swap_case(c: char) -> char {
    if c.is_ascii_lowercase() {
        c.to_ascii_uppercase()
    } else {
        c.to_ascii_lowercase()
    }
}

static CRITICAL: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    CRITICAL_PATTERN_SOURCES
        .iter()
        .map(|(source, flags)| {
            Regex::new(&translate_js_regex(source, flags.contains('i'))).expect("critical pattern compiles")
        })
        .collect()
});

pub fn is_critical(command: &str) -> bool {
    CRITICAL.iter().any(|regex| regex.is_match(command))
}

// --- Tokenizers (shell-tokenize.ts) ---------------------------------------------------------------------------

/// `tokenizeShellSegments`: command segments split at `;`, `&`, `|`, `(`, `)` and newlines, each a list of words
/// with quotes and escapes removed. Not a full parser (no heredocs, substitutions).
pub fn tokenize_shell_segments(command: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut value = String::new();
    let mut in_word = false;
    let mut separator = String::new();
    let (mut single, mut double) = (false, false);
    let end_separator = |separator: &mut String, current: &mut Vec<String>, segments: &mut Vec<Vec<String>>| {
        if !separator.is_empty() {
            if separator.chars().any(|c| "\n;&|()".contains(c)) && !current.is_empty() {
                segments.push(std::mem::take(current));
            }
            separator.clear();
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if single {
            if ch == '\'' {
                single = false;
            } else {
                value.push(ch);
            }
            i += 1;
            continue;
        }
        if double {
            if ch == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '"' | '\\' | '$' | '`') {
                value.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if ch == '"' {
                double = false;
            } else {
                value.push(ch);
            }
            i += 1;
            continue;
        }
        if matches!(ch, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')') {
            if in_word {
                if !value.is_empty() {
                    current.push(std::mem::take(&mut value));
                }
                in_word = false;
            }
            separator.push(ch);
            i += 1;
            continue;
        }
        if !in_word {
            end_separator(&mut separator, &mut current, &mut segments);
            in_word = true;
        }
        match ch {
            '\'' => single = true,
            '"' => double = true,
            '\\' if i + 1 < chars.len() => {
                value.push(chars[i + 1]);
                i += 1;
            }
            c => value.push(c),
        }
        i += 1;
    }
    if in_word && !value.is_empty() {
        current.push(value);
    }
    end_separator(&mut separator, &mut current, &mut segments);
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// One command of a flat `&&` chain: its source text (trimmed) and literal argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSegment {
    pub text: String,
    pub argv: Vec<String>,
}

const STATEFUL_COMMANDS: &[&str] = &[
    "!",
    ".",
    "test",
    "[",
    "[[",
    "-",
    "autoload",
    "bindkey",
    "bye",
    "chdir",
    "disable",
    "emulate",
    "functions",
    "limit",
    "print",
    "rehash",
    "sched",
    "setopt",
    "unfunction",
    "unhash",
    "unlimit",
    "unsetopt",
    "vared",
    "zle",
    "zmodload",
    "zparseopts",
    "zstyle",
    "alias",
    "bg",
    "break",
    "builtin",
    "case",
    "cd",
    "command",
    "coproc",
    "continue",
    "declare",
    "dirs",
    "disown",
    "do",
    "done",
    "elif",
    "else",
    "enable",
    "esac",
    "eval",
    "exec",
    "exit",
    "export",
    "fc",
    "fg",
    "fi",
    "for",
    "function",
    "getopts",
    "hash",
    "history",
    "if",
    "in",
    "jobs",
    "let",
    "logout",
    "mapfile",
    "local",
    "popd",
    "printf",
    "pushd",
    "readonly",
    "read",
    "readarray",
    "return",
    "select",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "then",
    "time",
    "times",
    "trap",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "until",
    "wait",
    "while",
];
const INTERPRETER_COMMANDS: &[&str] = &[
    "ash",
    "bash",
    "busybox",
    "csh",
    "dash",
    "env",
    "nocorrect",
    "noglob",
    "fish",
    "ksh",
    "sh",
    "sudo",
    "tcsh",
    "zsh",
];
static SHELL_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*\+?=").unwrap());
static REINTERPRET_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:-[^-]*[ce]|--(?:command|eval)(?:=.*)?)$").unwrap());

/// `extractLiteralAndChainSegments`: two or more literal argv commands joined only by `&&`, or None for anything
/// needing more shell interpretation than quote removal.
pub fn literal_and_chain(command: &str) -> Option<Vec<ChainSegment>> {
    if command.chars().any(|c| (c < '\x20' && c != '\t') || c == '\x7f') {
        return None;
    }
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut argv: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut segment_start = 0usize;
    let push_token = |argv: &mut Vec<String>, token: &mut String, started: &mut bool| {
        if *started {
            argv.push(std::mem::take(token));
            *started = false;
        }
    };
    let push_segment = |end: usize,
                        start: usize,
                        argv: &mut Vec<String>,
                        token: &mut String,
                        started: &mut bool,
                        segments: &mut Vec<ChainSegment>|
     -> bool {
        push_token(argv, token, started);
        let Some(executable) = argv.first() else { return false };
        if executable.is_empty() {
            return false;
        }
        let name = executable.rsplit(['/', '\\']).next().unwrap_or(executable);
        if argv.iter().any(|a| SHELL_ASSIGNMENT.is_match(a))
            || argv.iter().skip(1).any(|a| REINTERPRET_OPTION.is_match(a))
            || STATEFUL_COMMANDS.contains(&name)
            || INTERPRETER_COMMANDS.contains(&name)
        {
            return false;
        }
        let text: String = chars[start..end].iter().collect();
        segments.push(ChainSegment {
            text: js_trim(&text).to_owned(),
            argv: std::mem::take(argv),
        });
        true
    };
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    token.push(ch);
                }
                i += 1;
                continue;
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                    i += 1;
                    continue;
                }
                if matches!(ch, '$' | '`' | '\n' | '\r') {
                    return None;
                }
                if ch == '\\' {
                    let next = chars.get(i + 1).copied();
                    match next {
                        None | Some('\n' | '\r') => return None,
                        Some(n @ ('"' | '\\' | '$' | '`')) => {
                            token.push(n);
                            i += 2;
                        }
                        Some(_) => {
                            token.push(ch);
                            i += 1;
                        }
                    }
                    continue;
                }
                token.push(ch);
                i += 1;
                continue;
            }
            None => {}
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
            i += 1;
            continue;
        }
        if ch == '\\' {
            match chars.get(i + 1).copied() {
                None | Some('\n' | '\r') => return None,
                Some(next) => token.push(next),
            }
            started = true;
            i += 2;
            continue;
        }
        if ch == ' ' || ch == '\t' {
            push_token(&mut argv, &mut token, &mut started);
            i += 1;
            continue;
        }
        if ch == '&' && chars.get(i + 1) == Some(&'&') {
            if !push_segment(i, segment_start, &mut argv, &mut token, &mut started, &mut segments) {
                return None;
            }
            i += 2;
            segment_start = i;
            continue;
        }
        if "\n\r&|;<>`$()*?[]{}~".contains(ch) || (ch == '#' && !started) {
            return None;
        }
        token.push(ch);
        started = true;
        i += 1;
    }
    if quote.is_some()
        || !push_segment(
            chars.len(),
            segment_start,
            &mut argv,
            &mut token,
            &mut started,
            &mut segments,
        )
        || segments.len() < 2
    {
        return None;
    }
    Some(segments)
}

/// JavaScript `String.prototype.trim` (ECMAScript whitespace and line terminators).
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_space)
}

fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

// --- Pattern rules --------------------------------------------------------------------------------------------

const CONTROL: &[char] = &['\n', '\r', ';', '&', '|', '<', '>', '`', '$', '(', ')'];
static REINTERPRETED_ARGUMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ \t])(?:-[^-]*[ce]|--(?:command|eval))(?:[= \t]|$)").unwrap());

/// `hasBashApprovalShellControl`: whether the command has shell syntax that could smuggle a second command past an
/// `allow` rule (operators, expansions, or quoted operators an `-c`/`-e` option reinterprets).
pub fn has_shell_control(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut quote: Option<char> = None;
    let mut reinterpretable = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else if CONTROL.contains(&ch) {
                reinterpretable = true;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            if chars.get(i + 1).is_some_and(|next| CONTROL.contains(next)) {
                reinterpretable = true;
            }
            i += 2;
            continue;
        }
        if quote == Some('"') {
            if ch == '"' {
                quote = None;
            } else if ch == '`' || ch == '$' {
                return true;
            } else if CONTROL.contains(&ch) {
                reinterpretable = true;
            }
            i += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if CONTROL.contains(&ch) {
            return true;
        }
        i += 1;
    }
    reinterpretable && REINTERPRETED_ARGUMENT.is_match(command)
}

/// A `approval.bash.patterns` rule: a glob (`*` any text) over the whitespace-normalized command.
#[derive(Clone, Debug)]
pub struct PatternRule {
    pub matcher: String,
    pub approval: Policy,
    regex: Regex,
}

impl PatternRule {
    pub fn new(matcher: &str, approval: Policy) -> Option<Self> {
        let matcher = normalize(matcher);
        if matcher.is_empty() {
            return None;
        }
        let escaped: Vec<String> = matcher.split('*').map(regex::escape).collect();
        let regex = Regex::new(&format!("^{}$", escaped.join("(?s:.*)"))).ok()?;
        Some(PatternRule {
            matcher,
            approval,
            regex,
        })
    }

    fn whole(&self, command: &str) -> bool {
        let normalized = normalize(command);
        !normalized.is_empty() && self.regex.is_match(&normalized)
    }

    /// `deny`/`prompt` matching: the whole command or any single segment.
    fn any_segment(&self, command: &str) -> bool {
        let normalized = normalize(command);
        if normalized.is_empty() {
            return false;
        }
        self.regex.is_match(&normalized)
            || tokenize_shell_segments(command)
                .iter()
                .map(|segment| segment.join(" "))
                .filter(|segment| !segment.is_empty())
                .any(|segment| self.regex.is_match(&segment))
    }

    /// `allow` must vouch for the entire command and never rides a compound line.
    fn applies(&self, command: &str) -> bool {
        if self.approval == Policy::Allow {
            !has_shell_control(command) && self.whole(command)
        } else {
            self.any_segment(command)
        }
    }
}

/// `getBashApprovalPatternRules`: well-formed `{match, approval}` entries of a settings array, in order.
pub fn pattern_rules(value: Option<&Value>) -> Vec<PatternRule> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let record = item.as_object()?;
            let matcher = record.get("match")?.as_str()?;
            let approval = Policy::parse(record.get("approval")?)?;
            PatternRule::new(matcher, approval)
        })
        .collect()
}

fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in js_trim(text).chars() {
        if is_js_space(c) {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

fn find_rule<'a>(command: &str, rules: &'a [PatternRule]) -> Option<&'a PatternRule> {
    rules.iter().find(|rule| rule.applies(command))
}

fn pattern_decision(policy: Policy, rule: &PatternRule) -> Decision {
    let reason = match policy {
        Policy::Deny => format!("Blocked by bash pattern: {}", rule.matcher),
        _ => format!("Prompt required by bash pattern: {}", rule.matcher),
    };
    Decision::object(Tier::Exec, true, Some(policy), Some(reason))
}

fn critical_decision() -> Decision {
    Decision::object(Tier::Exec, true, None, Some("Critical pattern detected".into()))
}

/// `BashTool.approval`: the tier and policy of running `command`. `allow_compound` is omp's
/// `bash.allowCompoundCommands` (the shell is always bash here, a POSIX shell).
pub fn bash_decision(command: &str, rules: &[PatternRule], allow_compound: bool) -> Decision {
    let chain = if allow_compound {
        literal_and_chain(command)
    } else {
        None
    };
    let mut pattern_rule: Option<&PatternRule> = None;
    if let Some(chain) = &chain {
        for rule in rules {
            if rule.approval != Policy::Allow
                && rule.whole(command)
                && !chain.iter().any(|segment| rule.any_segment(&segment.text))
            {
                if rule.approval == Policy::Deny {
                    pattern_rule = Some(rule);
                    break;
                }
                pattern_rule = pattern_rule.or(Some(rule));
            }
        }
    } else {
        pattern_rule = find_rule(command, rules);
    }
    if let Some(rule) = pattern_rule.filter(|rule| rule.approval == Policy::Deny) {
        return pattern_decision(Policy::Deny, rule);
    }
    let critical = !command.is_empty() && is_critical(command);
    let Some(chain) = chain else {
        if critical {
            return critical_decision();
        }
        return match pattern_rule {
            Some(rule) if rule.approval == Policy::Allow => {
                Decision::object(Tier::Write, false, Some(Policy::Allow), None)
            }
            Some(rule) if rule.approval == Policy::Prompt => pattern_decision(Policy::Prompt, rule),
            _ => Decision::tier(Tier::Exec),
        };
    };
    let mut prompt_rule = pattern_rule;
    let mut unmatched = false;
    for segment in &chain {
        match find_rule(&segment.text, rules) {
            Some(rule) if rule.approval == Policy::Deny => return pattern_decision(Policy::Deny, rule),
            Some(rule) if rule.approval == Policy::Prompt => prompt_rule = prompt_rule.or(Some(rule)),
            Some(_) => {}
            None => unmatched = true,
        }
    }
    if let Some(rule) = prompt_rule {
        return pattern_decision(Policy::Prompt, rule);
    }
    if critical || chain.iter().any(|segment| is_critical(&segment.argv.join(" "))) {
        return critical_decision();
    }
    if unmatched {
        Decision::tier(Tier::Exec)
    } else {
        Decision::object(Tier::Write, false, Some(Policy::Allow), None)
    }
}
