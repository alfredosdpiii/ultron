//! `ultron/refinement-validation.ts`: the content checks the worker's refinement service runs when a refinement is
//! activated (A32). A skill must be a loadable SKILL.md: YAML frontmatter with a kebab-case name and a description,
//! then a nonempty body. An instruction is bounded plain text. Capability requests are refused separately by the
//! refinement service itself.

use crate::local::{Validator, frontmatter};
use serde_json::Value;
use std::sync::Arc;

const MAX_SKILL_BYTES: usize = 64 * 1024;
const MAX_INSTRUCTION_BYTES: usize = 16 * 1024;

fn js_space(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

fn line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `new RegExp(`^${key}:\\s*(.+)$`, "m").exec(frontmatter)?.[1]?.trim()`.
fn field<'a>(frontmatter: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}:");
    let starts = std::iter::once(0).chain(
        frontmatter
            .char_indices()
            .filter(|(_, ch)| line_terminator(*ch))
            .map(|(at, ch)| at + ch.len_utf8()),
    );
    for start in starts {
        let Some(rest) = frontmatter[start..].strip_prefix(&prefix) else {
            continue;
        };
        // `\s*` is greedy and crosses lines; `(.+)` then takes the rest of a line.
        let skipped = rest.trim_start_matches(js_space);
        let line_end = skipped.find(line_terminator).unwrap_or(skipped.len());
        if line_end > 0 {
            return Some(skipped[..line_end].trim_matches(js_space));
        }
        // Nothing after the spaces: backtracking leaves `(.+)` a run of spaces when one exists on a line.
        let spaces = &rest[..rest.len() - skipped.len()];
        if spaces.chars().any(|ch| !line_terminator(ch)) {
            return Some("");
        }
    }
    None
}

/// `/^[a-z0-9]+(?:-[a-z0-9]+)*$/`.
fn kebab(name: &str) -> bool {
    name.split('-')
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// `validateRefinementContent(kind, content)`: `Ok(true)` for a valid skill or instruction, `Ok(true)` for the
/// kinds it does not check (TS `undefined`), an error naming the problem otherwise.
pub fn validate_refinement_content(kind: &str, content: &Value) -> Result<bool, String> {
    match kind {
        "skill" => {
            let text = content.as_str().ok_or("A skill refinement must be SKILL.md text")?;
            if text.len() > MAX_SKILL_BYTES {
                return Err("Skill content exceeds 64 KiB".into());
            }
            let (front, body) =
                frontmatter(text.trim_start_matches(js_space), true).ok_or("Skill content needs YAML frontmatter")?;
            if !field(front, "name").is_some_and(|name| !name.is_empty() && kebab(name)) {
                return Err("Skill frontmatter needs a kebab-case name".into());
            }
            if !field(front, "description").is_some_and(|description| !description.is_empty()) {
                return Err("Skill frontmatter needs a description".into());
            }
            if body.trim_matches(js_space).is_empty() {
                return Err("Skill body is empty".into());
            }
            Ok(true)
        }
        "instruction" => {
            let text = content
                .as_str()
                .filter(|text| !text.trim_matches(js_space).is_empty())
                .ok_or("An instruction refinement must be text")?;
            if text.len() > MAX_INSTRUCTION_BYTES {
                return Err("Instruction exceeds 16 KiB".into());
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// [`validate_refinement_content`] as a refinement service validator (the worker's).
pub fn worker_validator() -> Validator {
    Arc::new(validate_refinement_content)
}
