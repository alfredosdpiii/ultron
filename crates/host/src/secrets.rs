//! Secret masking of cell output, ported from ultron/secrets.ts (SecretDetector over rlm/secret-patterns.json),
//! ultron/rlm/output-secrets.ts (known secret values, `ULTRON_MASK_SECRETS`) and ultron/sensitive.ts.
//!
//! The rules stay data: the shipped `secret-patterns.json` is compiled in, so masker, runtime and release scan
//! still share one file. JS regex semantics are kept where they differ from Rust's: `\b`, `\d` and `\w` are
//! ASCII and `\s` is JavaScript's whitespace set (see [`js_regex`]).

use fancy_regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

const PATTERNS: &str = include_str!("../../../packages/coding-agent/src/ultron/rlm/secret-patterns.json");
const KNOWN_SECRET_MIN_LENGTH: usize = 8;
/// JavaScript's `\s` (WhiteSpace and LineTerminator), without the class brackets.
/// JavaScript's (ASCII) word character, for `\b` and `\B`: fancy-regex cannot switch Unicode mode off inline.
const W: &str = "[A-Za-z0-9_]";
const JS_SPACE: &str =
    r"\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

/// Compile a JavaScript pattern (flags `i`, `m` only) with JavaScript's ASCII `\b`, `\d`, `\w` and its `\s`.
pub fn js_regex(pattern: &str, flags: &str) -> Result<Regex, String> {
    let mut out = String::new();
    for flag in flags.chars() {
        match flag {
            'i' | 'm' => out.push_str(&format!("(?{flag})")),
            _ => return Err(format!("secret pattern flag {flag} is not portable")),
        }
    }
    let mut chars = pattern.chars();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let Some(next) = chars.next() else {
                    out.push('\\');
                    break;
                };
                match (next, in_class) {
                    ('b', false) => out.push_str(&format!("(?:(?<={W})(?!{W})|(?<!{W})(?={W}))")),
                    ('B', false) => out.push_str(&format!("(?:(?<={W})(?={W})|(?<!{W})(?!{W}))")),
                    ('d', false) => out.push_str("[0-9]"),
                    ('d', true) => out.push_str("0-9"),
                    ('D', false) => out.push_str("[^0-9]"),
                    ('w', false) => out.push_str("[A-Za-z0-9_]"),
                    ('w', true) => out.push_str("A-Za-z0-9_"),
                    ('W', false) => out.push_str("[^A-Za-z0-9_]"),
                    ('s', false) => out.push_str(&format!("[{JS_SPACE}]")),
                    ('S', false) => out.push_str(&format!("[^{JS_SPACE}]")),
                    _ => {
                        out.push('\\');
                        out.push(next);
                    }
                }
            }
            '[' if !in_class => {
                in_class = true;
                out.push(c);
            }
            ']' if in_class => {
                in_class = false;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Regex::new(&out).map_err(|error| format!("{pattern}: {error}"))
}

/// A finding: the rule id and the byte offsets of the value in the scanned text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretFinding {
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub preview: String,
}

struct Rule {
    id: String,
    regex: Regex,
    group: usize,
    mask: bool,
    scan: bool,
    keywords: Vec<String>,
    caseless: bool,
    min_entropy: f64,
    letter_and_digit: bool,
    reject: Option<Regex>,
    allow: Option<Regex>,
}

pub struct SecretDetector {
    rules: Vec<Rule>,
    safe: Vec<Regex>,
    test_value: Regex,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Bits of Shannon entropy per character (code points counted, divided by the UTF-16 length as JS does).
pub fn shannon_entropy(value: &str) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    let mut counts: Vec<(char, usize)> = Vec::new();
    for c in value.chars() {
        match counts.iter_mut().find(|(seen, _)| *seen == c) {
            Some((_, count)) => *count += 1,
            None => counts.push((c, 1)),
        }
    }
    let length = utf16_len(value) as f64;
    counts.iter().fold(0.0, |bits, (_, count)| {
        let p = *count as f64 / length;
        bits - p * p.log2()
    })
}

/// A redacted preview: up to 4 leading UTF-16 units (a quarter of the value at most) and the length.
pub fn preview_secret(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let keep = 4.min(units.len() / 4);
    format!("{}…({} chars)", String::from_utf16_lossy(&units[..keep]), units.len())
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

impl SecretDetector {
    /// The detector for a `secret-patterns.json` document.
    pub fn new(data: &Value) -> Result<Self, String> {
        let empty = Vec::new();
        let list = |key: &str| data.get(key).and_then(Value::as_array).unwrap_or(&empty).clone();
        let mut rules = Vec::new();
        for rule in list("rules") {
            let flags = string_field(&rule, "flags").unwrap_or_default();
            let caseless = flags.contains('i');
            let scopes: Vec<String> = rule
                .get("scopes")
                .and_then(Value::as_array)
                .map(|scopes| scopes.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
                .unwrap_or_else(|| vec!["mask".into(), "scan".into()]);
            let optional = |key: &str, flags_key: &str| -> Result<Option<Regex>, String> {
                string_field(&rule, key)
                    .map(|pattern| js_regex(pattern, string_field(&rule, flags_key).unwrap_or_default()))
                    .transpose()
            };
            rules.push(Rule {
                id: string_field(&rule, "id").unwrap_or_default().to_owned(),
                regex: js_regex(string_field(&rule, "regex").unwrap_or_default(), flags)?,
                group: rule.get("group").and_then(Value::as_u64).unwrap_or(0) as usize,
                mask: scopes.iter().any(|s| s == "mask"),
                scan: scopes.iter().any(|s| s == "scan"),
                keywords: rule
                    .get("keywords")
                    .and_then(Value::as_array)
                    .map(|words| {
                        words
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|word| if caseless { word.to_lowercase() } else { word.to_owned() })
                            .collect()
                    })
                    .unwrap_or_default(),
                caseless,
                min_entropy: rule.get("minEntropy").and_then(Value::as_f64).unwrap_or(0.0),
                letter_and_digit: rule.get("requireLetterAndDigit") == Some(&Value::Bool(true)),
                reject: optional("rejectValue", "rejectValueFlags")?,
                allow: optional("allowValue", "allowValueFlags")?,
            });
        }
        let safe = list("safeSpans")
            .iter()
            .map(|span| js_regex(string_field(span, "regex").unwrap_or_default(), ""))
            .collect::<Result<_, _>>()?;
        let test = data.get("testValue").cloned().unwrap_or(Value::Null);
        let test_value = js_regex(
            string_field(&test, "regex").unwrap_or_default(),
            string_field(&test, "flags").unwrap_or_default(),
        )?;
        Ok(SecretDetector {
            rules,
            safe,
            test_value,
        })
    }

    /// The detector for the shipped rules (rlm/secret-patterns.json), built once.
    pub fn shipped() -> &'static SecretDetector {
        static DETECTOR: OnceLock<SecretDetector> = OnceLock::new();
        DETECTOR.get_or_init(|| {
            let data: Value = serde_json::from_str(PATTERNS).expect("secret-patterns.json is JSON");
            SecretDetector::new(&data).expect("secret-patterns.json compiles")
        })
    }

    /// Rule ids, in priority order.
    pub fn kinds(&self) -> Vec<&str> {
        self.rules.iter().map(|rule| rule.id.as_str()).collect()
    }

    /// Non-overlapping findings in `text`, in order; `mask` selects the masking scope, else the scan scope.
    pub fn scan(&self, text: &str, mask: bool) -> Vec<SecretFinding> {
        if text.is_empty() {
            return Vec::new();
        }
        let lower = OnceLock::new();
        let safe = OnceLock::new();
        let mut found: Vec<(SecretFinding, usize)> = Vec::new();
        for (order, rule) in self.rules.iter().enumerate() {
            if !(if mask { rule.mask } else { rule.scan }) {
                continue;
            }
            if !rule.keywords.is_empty() {
                let haystack = if rule.caseless {
                    lower.get_or_init(|| text.to_lowercase()).as_str()
                } else {
                    text
                };
                if !rule.keywords.iter().any(|word| haystack.contains(word.as_str())) {
                    continue;
                }
            }
            // ponytail: a regex runtime error (backtrack limit) ends this rule's matches, like a non-match.
            for captures in rule.regex.captures_iter(text).map_while(Result::ok) {
                let Some(span) = captures.get(rule.group) else { continue };
                let (start, end) = (span.start(), span.end());
                let value = span.as_str();
                if !self.plausible(rule, value) {
                    continue;
                }
                let safe: &Vec<(usize, usize)> = safe.get_or_init(|| self.safe_spans(text));
                if safe.iter().any(|&(from, to)| from <= start && end <= to) {
                    continue;
                }
                let finding = SecretFinding {
                    kind: rule.id.clone(),
                    start,
                    end,
                    preview: preview_secret(value),
                };
                found.push((finding, order));
            }
        }
        // Earlier first; at the same start the longer match, then the earlier rule.
        found.sort_by(|(a, ao), (b, bo)| {
            a.start
                .cmp(&b.start)
                .then((b.end - b.start).cmp(&(a.end - a.start)))
                .then(ao.cmp(bo))
        });
        let mut kept: Vec<SecretFinding> = Vec::new();
        for (finding, _) in found {
            if kept.last().is_some_and(|last| finding.start < last.end) {
                continue;
            }
            kept.push(finding);
        }
        kept
    }

    /// `text` with every masking finding replaced by `[REDACTED:<kind>]`, and how many were replaced.
    pub fn redact_counted(&self, text: &str) -> (String, usize) {
        let findings = self.scan(text, true);
        let mut out = String::new();
        let mut cursor = 0;
        for finding in &findings {
            out.push_str(&text[cursor..finding.start]);
            out.push_str(&format!("[REDACTED:{}]", finding.kind));
            cursor = finding.end;
        }
        out.push_str(&text[cursor..]);
        (out, findings.len())
    }

    fn plausible(&self, rule: &Rule, value: &str) -> bool {
        let test = |regex: &Regex| regex.is_match(value).unwrap_or(false);
        if value.is_empty() {
            return false;
        }
        if rule.letter_and_digit
            && !(value.bytes().any(|b| b.is_ascii_alphabetic()) && value.bytes().any(|b| b.is_ascii_digit()))
        {
            return false;
        }
        if rule.min_entropy > 0.0 && shannon_entropy(value) < rule.min_entropy {
            return false;
        }
        if rule.reject.as_ref().is_some_and(test) || rule.allow.as_ref().is_some_and(test) {
            return false;
        }
        !test(&self.test_value)
    }

    fn safe_spans(&self, text: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        for regex in &self.safe {
            for found in regex.find_iter(text).map_while(Result::ok) {
                spans.push((found.start(), found.end()));
            }
        }
        spans
    }
}

/// Whether cell output is masked: `ULTRON_MASK_SECRETS` (passed as its value) set to off, 0, false or no turns it off.
pub fn mask_secrets_enabled(value: Option<&str>) -> bool {
    let value = value.map(|v| v.trim().to_lowercase()).unwrap_or_default();
    value.is_empty() || !["off", "0", "false", "no"].contains(&value.as_str())
}

/// Cell output masking: values known to be secret (MCP env values, headers, OAuth tokens) and the shipped rules.
#[derive(Default)]
pub struct OutputMasker {
    known: Mutex<Vec<String>>,
}

impl OutputMasker {
    /// Mask these exact values from now on; values shorter than 8 characters after trimming are skipped.
    pub fn register_secret_values(&self, values: &[Value]) {
        let mut known = self.known.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for value in values.iter().filter_map(Value::as_str) {
            let value = value.trim();
            if utf16_len(value) >= KNOWN_SECRET_MIN_LENGTH && !known.iter().any(|k| k == value) {
                known.push(value.to_owned());
            }
        }
    }

    /// `text` with secrets masked and how many were, unless `mask_secrets` (ULTRON_MASK_SECRETS) turns masking off.
    pub fn mask_counted(&self, text: &str, mask_secrets: Option<&str>) -> (String, usize) {
        if text.is_empty() || !mask_secrets_enabled(mask_secrets) {
            return (text.to_owned(), 0);
        }
        let mut out = text.to_owned();
        let mut masked = 0;
        for secret in self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            masked += out.matches(secret.as_str()).count();
            out = out.replace(secret.as_str(), "[REDACTED:known_secret]");
        }
        let (out, detected) = SecretDetector::shipped().redact_counted(&out);
        (out, masked + detected)
    }
}

fn any_match(patterns: &[(&str, &str)], text: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    patterns.iter().any(|(pattern, flags)| {
        cache
            .entry(format!("{flags}/{pattern}"))
            .or_insert_with(|| js_regex(pattern, flags).expect("sensitive pattern compiles"))
            .is_match(text)
            .unwrap_or(false)
    }) || !SecretDetector::shipped().scan(text, true).is_empty()
}

/// Whether memory text holds a secret or personal identifier (sensitive.ts containsSensitiveMemory).
pub fn contains_sensitive_memory(text: &str) -> bool {
    any_match(
        &[
            (r"\b(?:sk|ghp|github_pat|xox[baprs]|AIza|AKIA)[A-Za-z0-9_-]{8,}\b", "i"),
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "i"),
            (
                r"\b(?:api[_ -]?key|access[_ -]?token|auth[_ -]?token|password|passwd|secret|private[_ -]?key)\s*[:=]\s*\S+",
                "i",
            ),
            (r"\bbearer\s+[A-Za-z0-9._~+/=-]{16,}\b", "i"),
            (r"\b\d{3}-\d{2}-\d{4}\b", ""),
        ],
        text,
    )
}

/// Whether a code skill proposal holds a secret (sensitive.ts containsCodeSecret).
pub fn contains_code_secret(text: &str) -> bool {
    any_match(
        &[
            (
                r"\b(?:sk-(?:proj-|live-|test-)?|sk_live_|sk_test_|ghp_|gho_|github_pat_|xox[baprs]-|AIza|AKIA|ASIA)[A-Za-z0-9_-]{12,}",
                "",
            ),
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", ""),
            (
                r#"\b(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|passwd|secret|private[_-]?key)\s*[:=]\s*["'][^"'\s]{8,}["']"#,
                "i",
            ),
            (r"\bbearer\s+[A-Za-z0-9._~+/=-]{20,}", "i"),
            (r"\b\d{3}-\d{2}-\d{4}\b", ""),
        ],
        text,
    )
}
