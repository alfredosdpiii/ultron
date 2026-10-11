//! Prompt templates: `/name args` expansion from markdown files (core/prompt-templates.ts).

use crate::paths::{basename, join, resolve, resolve_path};
use crate::skills::CONFIG_DIR_NAME;
use crate::{Diagnostic, frontmatter, js_space};
use std::fs;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    /// "user", "project" or "path".
    pub scope: &'static str,
    pub file_path: String,
}

#[derive(Debug, Default)]
pub struct LoadPromptTemplatesResult {
    pub templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Split arguments bash-style: whitespace separates, single or double quotes group.
pub fn parse_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in args.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if js_space(c) => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn digits(s: &str) -> usize {
    s.bytes().take_while(u8::is_ascii_digit).count()
}

/// JS `parseInt` of a digit run, saturating (an index past any argument list).
fn number(s: &str) -> usize {
    s.parse().unwrap_or(usize::MAX)
}

/// One placeholder at the start of `s` (which begins with `$`): (bytes consumed, replacement).
fn placeholder(s: &str, args: &[String], all: &str) -> Option<(usize, String)> {
    let arg = |n: usize| n.checked_sub(1).and_then(|i| args.get(i)).cloned();
    if let Some(inner) = s.strip_prefix("${") {
        // ${N:-default}, ${@:-default}, ${ARGUMENTS:-default}
        let target = match digits(inner) {
            0 if inner.starts_with("ARGUMENTS") => Some("ARGUMENTS"),
            0 if inner.starts_with('@') => Some("@"),
            0 => None,
            n => Some(&inner[..n]),
        };
        if let Some(target) = target
            && let Some(rest) = inner[target.len()..].strip_prefix(":-")
            && let Some(end) = rest.find('}')
        {
            let value = if target == "@" || target == "ARGUMENTS" {
                Some(all.to_string())
            } else {
                arg(number(target))
            };
            let replacement = value
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| rest[..end].to_string());
            return Some((2 + target.len() + 2 + end + 1, replacement));
        }
        // ${@:N} and ${@:N:L}
        let rest = inner.strip_prefix("@:")?;
        let n = digits(rest);
        if n == 0 {
            return None;
        }
        let start = number(&rest[..n]).saturating_sub(1);
        let after = &rest[n..];
        let (length, used) = match after.strip_prefix(':') {
            Some(l) if digits(l) > 0 && l[digits(l)..].starts_with('}') => {
                (Some(number(&l[..digits(l)])), 1 + digits(l))
            }
            _ if after.starts_with('}') => (None, 0),
            _ => return None,
        };
        let end = args.len().min(length.map_or(usize::MAX, |l| start.saturating_add(l)));
        let slice = args.get(start..end.max(start)).unwrap_or(&[]);
        return Some((2 + 2 + n + used + 1, slice.join(" ")));
    }
    let rest = &s[1..];
    if rest.starts_with("ARGUMENTS") {
        return Some((1 + 9, all.to_string()));
    }
    if rest.starts_with('@') {
        return Some((2, all.to_string()));
    }
    match digits(rest) {
        0 => None,
        n => Some((1 + n, arg(number(&rest[..n])).unwrap_or_default())),
    }
}

/// Replace `$1`, `$@`, `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, `${@:N}` and `${@:N:L}`; no recursion.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let mut out = String::new();
    let mut i = 0;
    while let Some(off) = content[i..].find('$') {
        let at = i + off;
        out.push_str(&content[i..at]);
        match placeholder(&content[at..], args, &all) {
            Some((used, replacement)) => {
                out.push_str(&replacement);
                i = at + used;
            }
            None => {
                out.push('$');
                i = at + 1;
            }
        }
    }
    out.push_str(&content[i..]);
    out
}

fn load_file(path: &str, scope: &'static str) -> (Option<PromptTemplate>, Vec<Diagnostic>) {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return (None, vec![Diagnostic::warning(error.to_string(), Some(path))]),
    };
    let parsed = match frontmatter::parse(&raw) {
        Ok(parsed) => parsed,
        Err(message) => return (None, vec![Diagnostic::warning(message, Some(path))]),
    };
    let file = basename(path);
    let name = file.strip_suffix(".md").unwrap_or(file).to_string();
    let mut description = frontmatter::string_field(&parsed.frontmatter, "description")
        .unwrap_or("")
        .to_string();
    if description.is_empty()
        && let Some(first) = parsed
            .body
            .split('\n')
            .find(|line| !line.trim_matches(js_space).is_empty())
    {
        description = first.chars().take(60).collect();
        if first.chars().count() > 60 {
            description.push_str("...");
        }
    }
    let argument_hint = frontmatter::string_field(&parsed.frontmatter, "argument-hint")
        .filter(|h| !h.is_empty())
        .map(String::from);
    let template = PromptTemplate {
        name,
        description,
        argument_hint,
        content: parsed.body,
        scope,
        file_path: path.to_string(),
    };
    (Some(template), Vec::new())
}

fn load_dir(dir: &str, scope_of: &dyn Fn(&str) -> &'static str, result: &mut LoadPromptTemplatesResult) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    for name in names {
        let full = join(dir, &name);
        if name.ends_with(".md") && fs::metadata(&full).is_ok_and(|m| m.is_file()) {
            let (template, diagnostics) = load_file(&full, scope_of(&full));
            result.templates.extend(template);
            result.diagnostics.extend(diagnostics);
        }
    }
}

/// Load templates from `agent_dir/prompts`, `cwd/.pi/prompts` (non-recursive) and explicit paths.
pub fn load_prompt_templates(
    cwd: &str,
    agent_dir: &str,
    prompt_paths: &[String],
    include_defaults: bool,
) -> LoadPromptTemplatesResult {
    let cwd = resolve_path(cwd, "/", false);
    let agent_dir = resolve_path(agent_dir, &cwd, false);
    let global = join(&agent_dir, "prompts");
    let project = resolve(&cwd, &format!("{CONFIG_DIR_NAME}/prompts"));
    let under = |target: &str, root: &str| target == root || target.starts_with(&format!("{root}/"));
    let scope_of = |path: &str| -> &'static str {
        if under(path, &global) {
            "user"
        } else if under(path, &project) {
            "project"
        } else {
            "path"
        }
    };
    let mut result = LoadPromptTemplatesResult::default();
    if include_defaults {
        load_dir(&global, &scope_of, &mut result);
        load_dir(&project, &scope_of, &mut result);
    }
    for raw in prompt_paths {
        let path = resolve_path(raw, &cwd, true);
        let Ok(meta) = fs::metadata(&path) else { continue };
        if meta.is_dir() {
            load_dir(&path, &scope_of, &mut result);
        } else if meta.is_file() && path.ends_with(".md") {
            let (template, diagnostics) = load_file(&path, scope_of(&path));
            result.templates.extend(template);
            result.diagnostics.extend(diagnostics);
        }
    }
    result
}

/// Expand `/name args` when `name` is a loaded template; any other text is returned unchanged.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(rest) = text.strip_prefix('/') else {
        return text.to_string();
    };
    let name_len = rest.find(js_space).unwrap_or(rest.len());
    if name_len == 0 {
        return text.to_string();
    }
    let (name, args) = rest.split_at(name_len);
    match templates.iter().find(|t| t.name == name) {
        Some(template) => substitute_args(
            &template.content,
            &parse_command_args(args.trim_start_matches(js_space)),
        ),
        None => text.to_string(),
    }
}
