//! Model patterns as the TypeScript resolver reads them (`coding-agent/src/core/model-resolver.ts`):
//! `findExactModelReferenceMatch`, `parseModelPattern` (exact, then partial id/name matching preferring aliases,
//! `:<thinking>` suffixes peeled off the end) and `resolveModelScopeFromModels` (`--models` / `enabledModels`: globs
//! through minimatch, fuzzy patterns otherwise), over any model record ([`ScopeModel`]). The provider layer
//! (`ai::model_resolver`) and the TUI's Ctrl+P cycling share them.
//!
//! Evidence: `crates/ai/fixtures/generate-model-resolver.mjs` runs the TypeScript resolver;
//! `crates/ai/tests/model_resolver.rs` and `crates/interactive/tests/next_model.rs` replay it.

pub mod glob;

use glob::minimatch;
use std::cmp::Ordering;

/// What the resolver reads of a model.
pub trait ScopeModel {
    fn provider(&self) -> &str;
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    /// `isClaudeCodeModel`: Claude Code CLI models answer only to their full `claude-code/<id>` reference.
    fn is_claude_code(&self) -> bool;
}

/// `isValidThinkingLevel` (cli/args.ts).
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn is_valid_thinking_level(level: &str) -> bool {
    THINKING_LEVELS.contains(&level)
}

/// JavaScript's whitespace and line terminators (`String.prototype.trim`).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String.prototype.localeCompare` (ICU root collation) for catalog ids and references: punctuation sorts before
/// digits, digits before letters, letters case-insensitively, with lowercase first as the tie-break.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    let key = |c: char| -> (u8, u32) {
        if let Some(i) = PUNCT.find(c) {
            (0, i as u32)
        } else if c.is_ascii_digit() {
            (1, c as u32)
        } else {
            (2, c.to_ascii_lowercase() as u32)
        }
    };
    let primary = a.chars().map(key).cmp(b.chars().map(key));
    primary.then_with(|| {
        let tertiary = |c: char| u8::from(c.is_ascii_uppercase());
        a.chars().map(tertiary).cmp(b.chars().map(tertiary))
    })
}

/// `modelsAreEqual`: same provider and id.
pub fn same<M: ScopeModel>(a: &M, b: &M) -> bool {
    a.id() == b.id() && a.provider() == b.provider()
}

fn reference<M: ScopeModel>(model: &M) -> String {
    format!("{}/{}", model.provider(), model.id())
}

/// `findExactModelReferenceMatch`: `provider/id` (case-insensitive) or a bare id that only one model has.
pub fn find_exact_model_reference_match<'a, M: ScopeModel>(reference_: &str, models: &'a [M]) -> Option<&'a M> {
    let trimmed = reference_.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_lowercase();
    let canonical: Vec<&M> = models
        .iter()
        .filter(|m| reference(*m).to_lowercase() == normalized)
        .collect();
    match canonical.len() {
        1 => return Some(canonical[0]),
        0 => {}
        _ => return None,
    }
    if let Some((provider, id)) = trimmed.split_once('/') {
        let (provider, id) = (
            provider.trim_matches(is_js_whitespace),
            id.trim_matches(is_js_whitespace),
        );
        if !provider.is_empty() && !id.is_empty() {
            let matches: Vec<&M> = models
                .iter()
                .filter(|m| {
                    m.provider().to_lowercase() == provider.to_lowercase() && m.id().to_lowercase() == id.to_lowercase()
                })
                .collect();
            match matches.len() {
                1 => return Some(matches[0]),
                0 => {}
                _ => return None,
            }
        }
    }
    let ids: Vec<&M> = models
        .iter()
        .filter(|m| m.id().to_lowercase() == normalized && !m.is_claude_code())
        .collect();
    (ids.len() == 1).then(|| ids[0])
}

/// `isAlias`: no `-YYYYMMDD` date suffix (or a `-latest` one).
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    let dated =
        bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit);
    !dated
}

/// The first model of a stable descending sort by id (`localeCompare`).
fn highest<'a, M: ScopeModel>(models: impl Iterator<Item = &'a M>) -> Option<&'a M> {
    let mut best: Option<&M> = None;
    for model in models {
        if best.is_none_or(|b| locale_compare(model.id(), b.id()) == Ordering::Greater) {
            best = Some(model);
        }
    }
    best
}

/// `tryMatchModel`: an exact reference, else the best partial id/name match (an alias over dated ids).
fn try_match_model<'a, M: ScopeModel>(pattern: &str, models: &'a [M]) -> Option<&'a M> {
    if let Some(exact) = find_exact_model_reference_match(pattern, models) {
        return Some(exact);
    }
    let needle = pattern.to_lowercase();
    let matches: Vec<&M> = models
        .iter()
        .filter(|m| {
            !m.is_claude_code()
                && (m.id().to_lowercase().contains(&needle) || m.name().to_lowercase().contains(&needle))
        })
        .collect();
    highest(matches.iter().copied().filter(|m| is_alias(m.id())))
        .or_else(|| highest(matches.iter().copied().filter(|m| !is_alias(m.id()))))
}

/// `ParsedModelResult`.
#[derive(Debug, PartialEq)]
pub struct Parsed<'a, M> {
    pub model: Option<&'a M>,
    pub thinking_level: Option<String>,
    pub warning: Option<String>,
}

/// `parseModelPattern`: the whole pattern as a model, else the part before the last `:` with that suffix as the
/// thinking level. An invalid suffix warns and is dropped when `allow_invalid_fallback` (scope mode), otherwise it
/// is part of the id and the pattern fails (strict CLI mode).
pub fn parse_model_pattern<'a, M: ScopeModel>(
    pattern: &str,
    models: &'a [M],
    allow_invalid_fallback: bool,
) -> Parsed<'a, M> {
    if let Some(model) = try_match_model(pattern, models) {
        return Parsed {
            model: Some(model),
            thinking_level: None,
            warning: None,
        };
    }
    let none = Parsed {
        model: None,
        thinking_level: None,
        warning: None,
    };
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return none;
    };
    if is_valid_thinking_level(suffix) {
        let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
        if result.model.is_some() {
            let thinking_level = result.warning.is_none().then(|| suffix.to_string());
            return Parsed {
                thinking_level,
                ..result
            };
        }
        return result;
    }
    if !allow_invalid_fallback {
        return none;
    }
    let result = parse_model_pattern(prefix, models, allow_invalid_fallback);
    if result.model.is_some() {
        return Parsed {
            model: result.model,
            thinking_level: None,
            warning: Some(format!(
                "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
            )),
        };
    }
    result
}

/// A model a pattern selected, with the thinking level the pattern named.
#[derive(Debug, PartialEq)]
pub struct Scoped<'a, M> {
    pub model: &'a M,
    pub thinking_level: Option<String>,
}

/// `ModelScopeDiagnostic`: a warning about one pattern (`code` is `no-match` or `invalid-thinking-level`).
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub pattern: String,
}

/// `resolveModelScopeFromModels`: the models `patterns` select, in pattern order without duplicates, and the
/// warnings for patterns that select nothing or name an invalid thinking level.
pub fn resolve_model_scope<'a, M: ScopeModel>(
    patterns: &[String],
    models: &'a [M],
) -> (Vec<Scoped<'a, M>>, Vec<ScopeDiagnostic>) {
    let mut scoped: Vec<Scoped<'a, M>> = Vec::new();
    let mut diagnostics = Vec::new();
    let push = |scoped: &mut Vec<Scoped<'a, M>>, model: &'a M, thinking: Option<String>| {
        if !scoped.iter().any(|s| same(s.model, model)) {
            scoped.push(Scoped {
                model,
                thinking_level: thinking,
            });
        }
    };
    let no_match = |pattern: &str| ScopeDiagnostic {
        code: "no-match",
        message: format!("No models match pattern \"{pattern}\""),
        pattern: pattern.to_string(),
    };
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let (glob, thinking) = match pattern.rsplit_once(':') {
                Some((head, suffix)) if is_valid_thinking_level(suffix) => (head, Some(suffix.to_string())),
                _ => (pattern.as_str(), None),
            };
            if let Some(exact) = find_exact_model_reference_match(glob, models) {
                push(&mut scoped, exact, thinking);
                continue;
            }
            let matching: Vec<&M> = models
                .iter()
                .filter(|m| minimatch(&reference(*m), glob) || (!m.is_claude_code() && minimatch(m.id(), glob)))
                .collect();
            if matching.is_empty() {
                diagnostics.push(no_match(pattern));
                continue;
            }
            for model in matching {
                push(&mut scoped, model, thinking.clone());
            }
            continue;
        }
        let parsed = parse_model_pattern(pattern, models, true);
        if let Some(warning) = parsed.warning {
            diagnostics.push(ScopeDiagnostic {
                code: "invalid-thinking-level",
                message: warning,
                pattern: pattern.clone(),
            });
        }
        match parsed.model {
            Some(model) => push(&mut scoped, model, parsed.thinking_level),
            None => diagnostics.push(no_match(pattern)),
        }
    }
    (scoped, diagnostics)
}
