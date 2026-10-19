//! The note a failed command's result gets when its output shows the sandbox refused it, so the model learns the rule
//! instead of retrying blindly (wave 38; Mirror's `SANDBOX_ERROR_PATTERNS` in tools/bash_tool.rs, made precise).
//!
//! A refusal is claimed only when the sandbox is the reason:
//! - a write error (`Read-only file system`, `Permission denied`, `Operation not permitted`) on a path the policy
//!   keeps read-only, which this process (outside the sandbox) could write: the sandbox made it read-only. A path
//!   that is read-only on the host too (`/usr`) gets no note: the command would fail the same way unsandboxed;
//! - any of those, or `No such file or directory`, on a path inside a hidden one (credentials) that exists on the
//!   host: the sandbox hid it;
//! - an unreachable network or failed name lookup while the policy has the network off.
//!
//! The first refusal found, in output order, is the one reported (one note per result).

use crate::launcher::Policy;
use std::path::{Path, PathBuf};

/// What the sandbox refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A write to a path the sandbox keeps read-only.
    ReadOnly(PathBuf),
    /// A path inside one the sandbox hides.
    Hidden(PathBuf),
    /// Network access while the network is off.
    Network,
}

/// Write errors, lowercased (Node prints `EROFS: read-only file system, open '<path>'`).
const WRITE_ERRORS: &[&str] = &["read-only file system", "permission denied", "operation not permitted"];
const MISSING: &str = "no such file or directory";

/// Messages of a connection or name lookup that cannot work without a network, lowercased.
const NETWORK_ERRORS: &[&str] = &[
    "network is unreachable",
    "could not resolve host",
    "could not resolve hostname",
    "temporary failure in name resolution",
    "name or service not known",
    "unable to resolve host address",
    "no address associated with hostname",
    "getaddrinfo eai_again",
    "getaddrinfo enotfound",
];

/// What the host can tell about a path outside the sandbox.
pub trait HostPaths {
    /// Whether this process could write `path` (the file, or the nearest existing directory above it).
    fn writable(&self, path: &Path) -> bool;
    /// Whether `path` exists (not following a final symlink).
    fn exists(&self, path: &Path) -> bool;
}

/// The real file system, as this (unsandboxed) process sees it.
pub struct RealHostPaths;

impl HostPaths for RealHostPaths {
    fn writable(&self, path: &Path) -> bool {
        let mut current = path;
        loop {
            if std::fs::symlink_metadata(current).is_ok() {
                return rustix::fs::access(current, rustix::fs::Access::WRITE_OK).is_ok();
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }
}

/// The first refusal `output` shows under `policy`; `paths` answers for the host outside the sandbox.
pub fn detect(policy: &Policy, output: &str, paths: &dyn HostPaths) -> Option<Refusal> {
    for line in output.lines() {
        let lower = line.to_lowercase();
        if !policy.network && NETWORK_ERRORS.iter().any(|error| lower.contains(error)) {
            return Some(Refusal::Network);
        }
        let write = WRITE_ERRORS
            .iter()
            .find_map(|error| lower.find(error).map(|at| (at, error.len())));
        let missing = lower.find(MISSING).map(|at| (at, MISSING.len()));
        let Some((at, len)) = write.or(missing) else {
            continue;
        };
        let Some(path) = error_path(line, at, len) else {
            continue;
        };
        let path = absolute(policy, &path);
        if hidden(policy, &path) && paths.exists(&path) {
            return Some(Refusal::Hidden(path));
        }
        if write.is_some() && !private_tmp(&path) && !policy.may_write(&path) && paths.writable(&path) {
            return Some(Refusal::ReadOnly(path));
        }
    }
    None
}

/// The note line for `refusal`.
pub fn note(policy: &Policy, refusal: &Refusal) -> String {
    match refusal {
        Refusal::ReadOnly(path) => {
            let roots: Vec<String> = policy.writable.iter().map(|p| p.display().to_string()).collect();
            format!(
                "[sandbox: writing {} was blocked: writes outside the project are blocked by the sandbox. Writable: {}, /tmp; the sandbox.writable setting adds paths]",
                path.display(),
                roots.join(", ")
            )
        }
        Refusal::Hidden(path) => format!(
            "[sandbox: {} is hidden by the sandbox (credentials and sandbox.hidden paths are invisible to commands); it cannot be read or written from here]",
            path.display()
        ),
        Refusal::Network => {
            "[sandbox: commands have no network access (sandbox.network is off); retrying will not help]".into()
        }
    }
}

/// [`detect`] and [`note`] over the real file system.
pub fn refusal_note(policy: &Policy, output: &str) -> Option<String> {
    detect(policy, output, &RealHostPaths).map(|refusal| note(policy, &refusal))
}

/// The path an error message at `line[at..at + len]` is about:
/// - quoted right before it (`touch: cannot touch '/x': Read-only file system`, also with `‘’` or `"`);
/// - the last `: ` field before it (`bash: line 1: /x: Permission denied`), or that field's last word when it is
///   a path (`sh: 1: cannot create /x: Read-only file system`, `could not lock config file /x: ...`);
/// - quoted after it (`[Errno 30] Read-only file system: '/x'`, `EROFS: read-only file system, open '/x'`).
fn error_path(line: &str, at: usize, len: usize) -> Option<String> {
    let before = line[..at].trim_end().trim_end_matches(':').trim_end();
    if let Some(path) = quoted_at_end(before) {
        return Some(path);
    }
    let field = before.rsplit(": ").next().unwrap_or(before).trim();
    if looks_like_path(field) {
        return Some(field.to_string());
    }
    if let Some(word) = field.split_whitespace().last()
        && word.starts_with('/')
    {
        return Some(word.to_string());
    }
    first_quoted(&line[at + len..])
}

const QUOTES: &[(char, char)] = &[('\'', '\''), ('‘', '’'), ('"', '"'), ('`', '\'')];

fn quoted_at_end(text: &str) -> Option<String> {
    let close = text.chars().last()?;
    let (open, _) = QUOTES.iter().find(|(_, c)| *c == close)?;
    let inner = &text[..text.len() - close.len_utf8()];
    let start = inner.rfind(*open)?;
    let path = &inner[start + open.len_utf8()..];
    (!path.is_empty()).then(|| path.to_string())
}

fn first_quoted(text: &str) -> Option<String> {
    for (index, character) in text.char_indices() {
        if let Some((open, close)) = QUOTES.iter().find(|(open, _)| *open == character) {
            let rest = &text[index + open.len_utf8()..];
            let end = rest.find(*close)?;
            let path = &rest[..end];
            return (!path.is_empty()).then(|| path.to_string());
        }
    }
    None
}

/// A single word naming a file: absolute, or relative with a `/` or a dot in it (not `line 1`, `touch`).
fn looks_like_path(field: &str) -> bool {
    !field.is_empty()
        && !field.contains(char::is_whitespace)
        && (field.starts_with('/') || field.starts_with("./") || field.starts_with("../") || field.contains('/'))
}

fn absolute(policy: &Policy, path: &str) -> PathBuf {
    let path = match path.strip_prefix("~/") {
        Some(rest) => policy.home.join(rest),
        None => policy.cwd.join(path),
    };
    crate::settings::normalize(&path)
}

/// The sandbox's own /tmp (and the other private temp dirs): writes there never reach the host.
fn private_tmp(path: &Path) -> bool {
    ["/tmp", "/var/tmp", "/dev/shm"].iter().any(|dir| path.starts_with(dir))
}

/// Whether `path` is inside a hidden path, the most specific rule winning (a writable or readable path nested in a
/// hidden one is mounted again on top of it).
fn hidden(policy: &Policy, path: &Path) -> bool {
    let deepest = |roots: &[PathBuf]| {
        roots
            .iter()
            .filter(|root| path.starts_with(root))
            .map(|root| root.components().count())
            .max()
    };
    let Some(hidden) = deepest(&policy.hidden) else {
        return false;
    };
    let shown = deepest(&policy.writable).max(deepest(&policy.readable));
    shown.is_none_or(|shown| hidden >= shown)
}
