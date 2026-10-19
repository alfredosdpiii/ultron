//! Result conventions of the model's `bash` (wave 38, from Mirror's tools/bash_tool.rs):
//!
//! - exit code 1 from a read-only comparison command (`grep`, `rg`, `diff`, `cmp`, `test`, ...) reports a result,
//!   not a failure. When the command is a simple pipeline whose last command is one of those, the result carries an
//!   `exit_note` ("No matches found"); runtime.py then shows `[No matches found (exit code 1)]` instead of the
//!   failure line `[exit code 1]`. `exit_code` stays 1 (and `.ok`, a clean exit 0, stays false);
//! - a failed command whose output shows the sandbox refused it carries a `sandbox_note` (`sandbox::refusal`), a
//!   line runtime.py prints after the output, saying what was blocked and what is allowed.
//!
//! Both are fields the TypeScript host never sends, so runtime.py under it is unchanged.

use serde_json::Value;

/// Commands whose exit code 1 reports a result, and the note shown in place of the failure line.
pub const NON_ERROR_EXIT_1: &[(&str, &str)] = &[
    ("grep", "No matches found"),
    ("egrep", "No matches found"),
    ("fgrep", "No matches found"),
    ("rg", "No matches found"),
    ("diff", "Files differ"),
    ("cmp", "Files differ"),
    ("test", "Condition is false"),
    ("[", "Condition is false"),
];

/// The note for `exit_code` of `command`: exit code 1 of a simple pipeline (commands joined by `|` or `|&`, nothing
/// else) whose last command is one of [`NON_ERROR_EXIT_1`]. Anything else (`;`, `&&`, `||`, `&`, a newline, a
/// subshell or command substitution, a `!` negation) may have taken its status from another command: no note.
pub fn exit_note(command: &str, exit_code: Option<i32>) -> Option<&'static str> {
    if exit_code != Some(1) {
        return None;
    }
    let last = pipeline_commands(command)?.pop()?;
    let program = program_of(&last)?;
    let name = program.rsplit('/').next().unwrap_or(&program);
    NON_ERROR_EXIT_1
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, note)| *note)
}

/// The commands of a simple pipeline, split on `|` outside quotes; None when `command` is anything more.
fn pipeline_commands(command: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut in_single, mut in_double) = (false, false);
    let mut characters = command.trim().chars().peekable();
    while let Some(character) = characters.next() {
        if in_single {
            in_single = character != '\'';
            current.push(character);
            continue;
        }
        match character {
            '\\' => {
                current.push(character);
                current.push(characters.next()?);
            }
            '\'' if !in_double => {
                in_single = true;
                current.push(character);
            }
            '"' => {
                in_double = !in_double;
                current.push(character);
            }
            // Substitutions run commands of their own, inside quotes too.
            '`' => return None,
            '$' if characters.peek() == Some(&'(') => return None,
            _ if in_double => current.push(character),
            // `2>&1`, `>&2`, `&>file`: redirections, not a background `&` or `&&`.
            '&' if current.ends_with(['>', '<']) || characters.peek() == Some(&'>') => current.push(character),
            // A comment starts at a word's beginning; elsewhere `#` is part of the word.
            '#' if !(current.is_empty() || current.ends_with(char::is_whitespace)) => current.push(character),
            ';' | '&' | '\n' | '(' | ')' | '{' | '}' | '#' => return None,
            '|' => {
                match characters.peek() {
                    Some('|') => return None,
                    Some('&') => {
                        characters.next();
                    }
                    _ => {}
                }
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(character),
        }
    }
    if in_single || in_double {
        return None;
    }
    parts.push(current);
    parts.iter().all(|part| !part.trim().is_empty()).then_some(parts)
}

/// The program a simple command runs: its first word after `NAME=value` assignments, unquoted (a `!` negation,
/// where exit code 1 means the command succeeded, is a word of its own and no comparison command).
fn program_of(command: &str) -> Option<String> {
    let program = command.split_whitespace().find(|word| !is_assignment(word))?;
    Some(program.trim_matches(|c| c == '\'' || c == '"').to_string())
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Adds the notes to a `bash` result (`output`) or a job snapshot (`text`) of `command`: `exit_note` when the
/// command ended with a non-error exit 1, `sandbox_note` when it failed and `policy` (the sandbox in use, if any)
/// refused it. A result still running, stopped or without an exit code gets neither.
pub fn annotate(result: &mut Value, command: &str, policy: Option<&sandbox::Policy>) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let flag = |key: &str| object.get(key).and_then(Value::as_bool) == Some(true);
    if flag("running") || flag("cancelled") || flag("timed_out") {
        return;
    }
    if object
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status != "completed")
    {
        return;
    }
    let exit_code = object
        .get("exit_code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    if let Some(note) = exit_note(command, exit_code) {
        object.insert("exit_note".into(), note.into());
    }
    let Some(policy) = policy.filter(|_| exit_code.is_some_and(|code| code != 0)) else {
        return;
    };
    let output = ["output", "text"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or_default();
    if let Some(note) = sandbox::refusal::refusal_note(policy, output) {
        object.insert("sandbox_note".into(), note.into());
    }
}
