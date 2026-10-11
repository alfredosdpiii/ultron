//! Generic file-write hooks for the RLM REPL (ultron/file-hooks.ts): the kernel's `edit()`/`write()` send each
//! proposed write to the host before touching the file (`files.before_write`), and shell commands that may write a
//! file go to the guards before they run. A guard that blocks makes `write()` raise ValueError with its diagnostic
//! (runtime.py `_check_write`) or the host refuse the command ("bash command was not run: ..."). A guard that fails
//! lets the write proceed with a visible note: it fails open, never silently.

use crate::Guard;
use fancy_regex::Regex;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

/// A guard's answer: block with a reason, or allow with an optional note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Block(String),
    Allow(Option<String>),
}

/// One registered guard (Loki, extensions). `Err` means the guard could not check (the write proceeds unchecked).
pub trait FileWriteGuard: Send + Sync {
    /// Shown in notes ("Loki").
    fn name(&self) -> &str;
    /// A proposed write: the absolute, normalized path and the complete new text; `cwd` is the lane's directory.
    fn before_write(&self, path: &Path, content: &str, cwd: &Path) -> Result<Verdict, String>;
    /// A shell command that may write files ([`may_write_files`]), before it runs in `cwd`.
    fn before_shell(&self, _command: &str, _cwd: &Path) -> Result<Verdict, String> {
        Ok(Verdict::Allow(None))
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

// JavaScript's `\b` after `null` is "no ASCII word character next"; `\d` is ASCII.
static DEV_NULL: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>>?\s*/dev/null(?![A-Za-z0-9_])"));
static FD_DUP: LazyLock<Regex> = LazyLock::new(|| re(r"[0-9]*>&[0-9]+"));
static WRITERS: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        re(r"(^|[^<>-])>{1,2}\|?(?!&)"),
        re(r"(^|[\s;&|(])(tee|cp|mv|install)\s"),
        re(r"(^|[\s;&|(])sed\s+(?:[^|;&]*\s)?(-i|--in-place)"),
        re(r"(^|[\s;&|(])perl\s+(?:[^|;&]*\s)?-[a-zA-Z]*i"),
        re(r"(^|[\s;&|(])dd\s[^|;&]*\bof="),
    ]
});

/// `mayWriteFiles`: whether a command may write a file, cheaply and generously (a redirect, `tee`, `sed -i`,
/// `perl -i`, `cp`/`mv`/`install`, `dd of=`). Redirects to `/dev/null` and fd duplications do not count.
pub fn may_write_files(command: &str) -> bool {
    let text = DEV_NULL.replace_all(command, " ");
    let text = FD_DUP.replace_all(&text, " ");
    WRITERS
        .iter()
        .any(|pattern| pattern.is_match(text.as_ref()).unwrap_or(false))
}

/// `path.resolve(cwd, path)`: lexical, `..` and `.` removed.
pub fn resolve_lexical(cwd: &Path, path: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in cwd.join(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            _ => {}
        }
    }
    out
}

/// `path.relative(from, to)` when `to` is inside `from`, as a `/` path; None for `from` itself or outside it.
pub fn inside(from: &Path, to: &Path) -> Option<String> {
    let relative = to.strip_prefix(from).ok()?.to_string_lossy().into_owned();
    (!relative.is_empty() && !relative.starts_with("..")).then_some(relative)
}

/// The guards of one session, answering the host's `files.before_write` and shell checks (`FileHooks`).
pub struct FileHooks {
    cwd: PathBuf,
    guards: Vec<Box<dyn FileWriteGuard>>,
}

impl FileHooks {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        FileHooks {
            cwd: cwd.into(),
            guards: Vec::new(),
        }
    }

    pub fn add(&mut self, guard: Box<dyn FileWriteGuard>) {
        self.guards.push(guard);
    }

    /// A path for notes: relative to the session directory when inside it.
    fn display(&self, path: &Path) -> String {
        inside(&self.cwd, path).unwrap_or_else(|| path.to_string_lossy().into_owned())
    }
}

impl Guard for FileHooks {
    fn before_shell(&self, command: &str) -> Option<String> {
        if self.guards.is_empty() || !may_write_files(command) {
            return None;
        }
        // Notes (a guard that could not check, advise-only refusals) are not shown for shell commands.
        let reasons: Vec<String> = self
            .guards
            .iter()
            .filter_map(|guard| match guard.before_shell(command, &self.cwd) {
                Ok(Verdict::Block(reason)) => Some(format!("[{}] {reason}", guard.name())),
                _ => None,
            })
            .collect();
        (!reasons.is_empty()).then(|| reasons.join("\n"))
    }

    fn before_write(&self, path: &str, content: &str) -> Value {
        let path = resolve_lexical(&self.cwd, path);
        let mut reasons = Vec::new();
        let mut notes = Vec::new();
        for guard in &self.guards {
            let name = guard.name();
            match guard.before_write(&path, content, &self.cwd) {
                Ok(Verdict::Block(reason)) => reasons.push(format!("[{name}] {reason}")),
                Ok(Verdict::Allow(Some(message))) if !message.is_empty() => notes.push(format!("[{name}] {message}")),
                Ok(Verdict::Allow(_)) => {}
                Err(error) => notes.push(format!(
                    "[{name}] {name} did not check this write to {} ({error}); it was written unchecked.",
                    self.display(&path)
                )),
            }
        }
        if reasons.is_empty() {
            json!({ "blocked": false, "notes": notes })
        } else {
            json!({ "blocked": true, "reason": reasons.join("\n"), "notes": notes })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_inside_matches_path_relative() {
        let root = Path::new("/r");
        assert_eq!(inside(root, Path::new("/r/a/b.txt")).as_deref(), Some("a/b.txt"));
        assert_eq!(inside(root, Path::new("/r")), None);
        assert_eq!(inside(root, Path::new("/rx/a")), None);
        assert_eq!(inside(root, Path::new("/r/..x")), None);
        assert_eq!(resolve_lexical(root, "a/../b/./c"), Path::new("/r/b/c"));
        assert_eq!(resolve_lexical(root, "/abs/../x"), Path::new("/x"));
    }
}
