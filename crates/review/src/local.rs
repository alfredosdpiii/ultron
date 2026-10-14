//! What autoreview takes from the user's own machine for a review (`autoreview/local.ts`): an existing local
//! checkout of the reviewed repository (only its prepared environment directories are ever used, read-only, for
//! sandboxed test runs) and private review guides (given to the review's frames, never to appear in what is posted).
//!
//! Known differences, outside recorded behaviour: `expandPath` resolves a relative path against the process's
//! current directory as Node did, but a current directory that cannot be read leaves the path relative.

use std::path::Path;

use regex::Regex;

use crate::github::PullRef;
use crate::js::{is_space, len16, path_join, trim};
use crate::plan::{PlannedComment, ReviewPlan};
use crate::runner::{Env, RunOptions, Runner, argv};

/// `~` and relative paths made absolute.
pub fn expand_path(path: &str, home: &str) -> String {
    let text = trim(path);
    if text == "~" {
        return home.into();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return path_join(&[home, rest]);
    }
    if text.starts_with('/') {
        return text.into();
    }
    let cwd = std::env::current_dir().map_or_else(|_| String::new(), |dir| dir.to_string_lossy().into_owned());
    // path.resolve: joined and normalized, without a trailing slash.
    let resolved = path_join(&[&cwd, text]);
    match resolved.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped.into(),
        _ => resolved,
    }
}

/// The user's home directory (`os.homedir()`: `HOME`).
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Whether a git remote URL names `owner/repo` on `host` (https, ssh or scp-like syntax; `.git` optional).
pub fn remote_matches(url: &str, host: &str, owner: &str, repo: &str) -> bool {
    let url = trim(url);
    // JavaScript's `.` stops at line terminators.
    let full = Regex::new(
        r"^(?:https?://|ssh://|git://)(?:[^@/]+@)?([^/:]+)(?::[0-9]+)?/([^\n\r\u{2028}\u{2029}]+?)(?:\.git)?/?$",
    )
    .expect("static pattern");
    let scp = Regex::new(r"^(?:[^@/]+@)?([^/:]+):([^\n\r\u{2028}\u{2029}]+?)(?:\.git)?/?$").expect("static pattern");
    let Some(found) = full.captures(url).or_else(|| scp.captures(url)) else {
        return false;
    };
    found[1].to_lowercase() == host.to_lowercase()
        && found[2].to_lowercase() == format!("{owner}/{repo}").to_lowercase()
}

/// An existing local checkout of the repository under one of `roots` (`<root>/<repo>`), accepted only when one of
/// its git remotes is that repository.
pub fn find_checkout(runner: &dyn Runner, roots: &[String], r: &PullRef) -> Option<String> {
    let safe = Regex::new(r"^[A-Za-z0-9._-]+$").expect("static pattern");
    if !safe.is_match(&r.repo) || r.repo == "." || r.repo == ".." {
        return None;
    }
    let home = home_dir();
    for root in roots {
        let dir = path_join(&[&expand_path(root, &home), &r.repo]);
        if !Path::new(&dir).join(".git").exists() {
            continue;
        }
        let mut env = Env::new();
        env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
        let result = runner.run(
            &argv(&["git", "-C", &dir, "config", "--get-regexp", r"^remote\..*\.url$"]),
            &RunOptions {
                env,
                timeout_ms: Some(20_000),
                ..RunOptions::default()
            },
        );
        if result.code != 0 {
            continue;
        }
        let urls = result.stdout.split('\n').filter_map(|line| {
            // line.trim().split(/\s+/)[1]
            let line = trim(line);
            let mut parts = line.split(is_space).filter(|part| !part.is_empty());
            if line.is_empty() {
                return None;
            }
            parts.nth(1)
        });
        if urls
            .collect::<Vec<_>>()
            .iter()
            .any(|url| remote_matches(url, &r.host, &r.owner, &r.repo))
        {
            return Some(dir);
        }
    }
    None
}

/// The guide names (file names and paths) that appear in `text`.
pub fn guide_mentions(text: &str, guide_names: &[String]) -> Vec<String> {
    let lower = text.to_lowercase();
    guide_names
        .iter()
        .filter(|name| {
            let needle = name.to_lowercase();
            if len16(&needle) < 4 {
                return false;
            }
            // A file name must stand alone; a path is matched anywhere.
            if needle.contains('/') {
                return lower.contains(&needle);
            }
            let pattern = format!("(?:^|[^a-z0-9_.-]){}(?:$|[^a-z0-9_-])", regex::escape(&needle));
            Regex::new(&pattern).is_ok_and(|pattern| pattern.is_match(&lower))
        })
        .cloned()
        .collect()
}

/// `text.split(/(?<=[.!?])\s+/)`.
fn sentences(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if is_space(c) && matches!(previous, Some('.' | '!' | '?')) {
            parts.push(&text[start..index]);
            let mut end = index + c.len_utf8();
            while let Some(&(next, n)) = chars.peek() {
                if !is_space(n) {
                    break;
                }
                end = next + n.len_utf8();
                chars.next();
            }
            start = end;
            previous = None;
            continue;
        }
        previous = Some(c);
    }
    parts.push(&text[start..]);
    parts
}

/// `path.basename`.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// The plan with nothing that names a private review guide: an inline comment that mentions one is not posted,
/// and a sentence of the body that does is removed. Returns the plan and what was withheld.
pub fn without_guide_mentions(plan: &ReviewPlan, guide_names: &[String]) -> (ReviewPlan, Vec<String>) {
    if guide_names.is_empty() {
        return (plan.clone(), Vec::new());
    }
    let mut withheld = Vec::new();
    let mut comments: Vec<PlannedComment> = Vec::new();
    let mut dropped = Vec::new();
    for comment in &plan.comments {
        let found = guide_mentions(&comment.body, guide_names);
        if found.is_empty() {
            comments.push(comment.clone());
        } else {
            dropped.push(comment.finding);
            let names: Vec<&str> = found.iter().map(|name| basename(name)).collect();
            withheld.push(format!(
                "an inline comment on {}:{} ({})",
                comment.path,
                comment.line,
                names.join(", ")
            ));
        }
    }
    let mut body = plan.body.clone();
    if !guide_mentions(&body, guide_names).is_empty() {
        body = body
            .split('\n')
            .map(|line| {
                if guide_mentions(line, guide_names).is_empty() {
                    line.to_string()
                } else {
                    sentences(line)
                        .into_iter()
                        .filter(|part| guide_mentions(part, guide_names).is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        withheld.push("part of the review body".into());
    }
    let mut over_cap = plan.over_cap.clone();
    over_cap.extend(dropped);
    (
        ReviewPlan {
            body,
            comments,
            over_cap,
            ..plan.clone()
        },
        withheld,
    )
}
