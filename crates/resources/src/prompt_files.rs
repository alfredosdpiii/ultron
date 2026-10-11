//! SYSTEM.md / APPEND_SYSTEM.md discovery and prompt inputs (DefaultResourceLoader in core/resource-loader.ts).

use crate::paths::{join, resolve_path, strip_bom};
use crate::skills::CONFIG_DIR_NAME;
use std::fs;
use std::path::Path;

/// A prompt input is a file path when that file exists (relative to the process cwd, like Node), else literal text.
/// An unreadable file falls back to the input text.
pub fn resolve_prompt_input(input: Option<&str>) -> Option<String> {
    let input = input.filter(|i| !i.is_empty())?;
    if Path::new(input).exists() {
        return Some(fs::read_to_string(input).map_or_else(|_| input.to_string(), |t| strip_bom(&t).to_string()));
    }
    Some(input.to_string())
}

/// `<cwd>/.pi/<name>` when the project is trusted and it exists, else `<agent_dir>/<name>` when it exists.
fn discover(cwd: &str, agent_dir: &str, name: &str, project_trusted: bool) -> Option<String> {
    let project = join(&join(cwd, CONFIG_DIR_NAME), name);
    if project_trusted && Path::new(&project).exists() {
        return Some(project);
    }
    let global = join(agent_dir, name);
    Path::new(&global).exists().then_some(global)
}

/// The resolved base system prompt and append prompts, with the source paths that exist.
#[derive(Debug, PartialEq, Eq)]
pub struct PromptFiles {
    pub system_prompt: Option<String>,
    pub system_prompt_source: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub append_sources: Vec<String>,
}

/// Explicit inputs win; otherwise SYSTEM.md and APPEND_SYSTEM.md are discovered.
pub fn load_prompt_files(
    cwd: &str,
    agent_dir: &str,
    project_trusted: bool,
    system_prompt: Option<&str>,
    append_system_prompt: Option<&[String]>,
) -> PromptFiles {
    let discovered = discover(cwd, agent_dir, "SYSTEM.md", project_trusted);
    let source = system_prompt.map(String::from).or(discovered);
    let appends: Vec<String> = match append_system_prompt {
        Some(list) => list.to_vec(),
        None => discover(cwd, agent_dir, "APPEND_SYSTEM.md", project_trusted)
            .into_iter()
            .collect(),
    };
    let existing = |s: &String| Path::new(s).exists().then(|| resolve_path(s, &process_cwd(), false));
    PromptFiles {
        system_prompt: resolve_prompt_input(source.as_deref()),
        system_prompt_source: source.as_ref().and_then(existing),
        append_system_prompt: appends.iter().filter_map(|s| resolve_prompt_input(Some(s))).collect(),
        append_sources: appends.iter().filter_map(existing).collect(),
    }
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}
