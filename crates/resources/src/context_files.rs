//! AGENTS.md / CLAUDE.md discovery (`loadProjectContextFiles` in core/resource-loader.ts).

use crate::paths::{basename, canonicalize, dirname, join, resolve, resolve_path, strip_bom};
use std::fs;
use std::path::Path;

/// A loaded context file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

const CANDIDATES: [&str; 5] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

fn load_from_dir(dir: &str) -> Option<ContextFile> {
    CANDIDATES.iter().find_map(|name| {
        let path = join(dir, name);
        if !fs::metadata(&path).ok()?.is_file() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        Some(ContextFile {
            content: strip_bom(&content).to_string(),
            path,
        })
    })
}

/// (repo dir, common git dir) of the repository containing `cwd` (`findGitPaths`).
fn find_git_paths(cwd: &str) -> Option<(String, String)> {
    let mut dir = cwd.to_string();
    loop {
        let git = join(&dir, ".git");
        if let Ok(meta) = fs::metadata(&git) {
            if meta.is_file() {
                let content = fs::read_to_string(&git).ok()?;
                if let Some(target) = content.trim().strip_prefix("gitdir: ") {
                    let git_dir = resolve(&dir, target.trim());
                    if !Path::new(&join(&git_dir, "HEAD")).exists() {
                        return None;
                    }
                    let common = match fs::read_to_string(join(&git_dir, "commondir")) {
                        Ok(common) => resolve(&git_dir, common.trim()),
                        Err(_) => git_dir,
                    };
                    return Some((dir, common));
                }
            } else if meta.is_dir() {
                if !Path::new(&join(&git, "HEAD")).exists() {
                    return None;
                }
                return Some((dir, git));
            }
        }
        let parent = dirname(&dir);
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

/// The main repo's context file that a nested linked worktree's own copy shadows (canonical path).
fn find_shadowed(cwd: &str) -> Option<String> {
    let (repo_dir, common_git_dir) = find_git_paths(cwd)?;
    let common = canonicalize(&common_git_dir);
    let worktree_root = canonicalize(&repo_dir);
    let main_root = dirname(&common);
    if !worktree_root.starts_with(&format!("{main_root}/")) {
        return None;
    }
    if canonicalize(&join(&main_root, ".git")) != common {
        return None;
    }
    let own = load_from_dir(&worktree_root)?;
    Some(join(&main_root, basename(&own.path)))
}

/// The global context file from `agent_dir`, then one per ancestor of `cwd`, outermost first.
pub fn load_project_context_files(cwd: &str, agent_dir: &str) -> Vec<ContextFile> {
    let cwd = resolve_path(cwd, "/", false);
    let agent_dir = resolve_path(agent_dir, &cwd, false);
    let mut files = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(global) = load_from_dir(&agent_dir) {
        seen.push(global.path.clone());
        files.push(global);
    }
    let shadowed = find_shadowed(&cwd);
    let mut ancestors = Vec::new();
    let mut dir = cwd;
    loop {
        if let Some(file) = load_from_dir(&dir) {
            let is_shadowed = shadowed.as_ref().is_some_and(|s| &canonicalize(&file.path) == s);
            if !is_shadowed && !seen.contains(&file.path) {
                seen.push(file.path.clone());
                ancestors.insert(0, file);
            }
        }
        let parent = dirname(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }
    files.extend(ancestors);
    files
}
