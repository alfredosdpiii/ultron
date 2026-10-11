//! Ultron's built-in Loki guardrails (ultron/loki.ts `LokiGuard`), running the Rust Loki engine in-process instead of
//! `python loki.py`:
//! - before `edit()`/`write()` change a file: `protect --file <path> --preview ultron` on the proposed content; a
//!   finding blocks the write (with `advise`, it is only reported);
//! - before a shell command that may write files: `shell-writes`, which refuses a write into source or configuration
//!   that would skip the before-write check.
//!
//! The policy is the repository's `.loki/loki.json` under the root (the Git work tree, else the session directory).

use crate::file_hooks::{FileWriteGuard, Verdict, inside};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LokiMode {
    On,
    Advise,
    Off,
}

const OFF_VALUES: [&str; 5] = ["off", "0", "false", "no", "none"];

/// `lokiMode`: ULTRON_LOKI (off | advise | on), else the `loki.mode` setting, else on.
pub fn loki_mode(env: Option<&str>, settings: &Map<String, Value>) -> LokiMode {
    let raw = env.map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty());
    match raw.as_deref() {
        Some(v) if OFF_VALUES.contains(&v) => LokiMode::Off,
        Some("advise") => LokiMode::Advise,
        Some(_) => LokiMode::On,
        None => match settings.get("mode").and_then(Value::as_str) {
            Some("off") => LokiMode::Off,
            Some("advise") => LokiMode::Advise,
            _ => LokiMode::On,
        },
    }
}

/// `gitRoot`: the Git work tree containing `cwd`, or None outside one.
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !root.is_empty()).then(|| PathBuf::from(root))
}

/// `classifyLokiText` for a check that passed: `NOT CHECKED` lines apart, every other line advisory.
fn classify_passed(stderr: &str) -> (Vec<String>, Vec<String>) {
    let (mut advisory, mut not_checked) = (Vec::new(), Vec::new());
    for line in stderr.split('\n').map(str::trim) {
        if line.is_empty()
            || line.starts_with("loki: post-write check failed")
            || line == "loki: advisory (not blocking):"
        {
            continue;
        }
        if line.starts_with("NOT CHECKED") {
            not_checked.push(line.to_string());
        } else {
            advisory.push(line.to_string());
        }
    }
    (advisory, not_checked)
}

const NOT_PORTED: &str = "loki: guard error: NotPorted: ";

pub struct LokiGuard {
    root: PathBuf,
    mode: LokiMode,
    /// NOT CHECKED lines are shown once per session, not after every write.
    shown_notices: Mutex<Vec<String>>,
}

impl LokiGuard {
    /// `root`: the repository (or working directory) Loki checks; `mode` is on or advise.
    pub fn new(root: impl Into<PathBuf>, mode: LokiMode) -> Self {
        LokiGuard {
            root: root.into(),
            mode,
            shown_notices: Mutex::new(Vec::new()),
        }
    }

    /// NOT CHECKED notes not yet shown this session, as one short line.
    fn notice(&self, lines: &[String]) -> Option<String> {
        let mut shown = self
            .shown_notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut fresh = Vec::new();
        for line in lines {
            if !shown.contains(line) {
                shown.push(line.clone());
                fresh.push(
                    line.strip_prefix("NOT CHECKED")
                        .unwrap_or(line)
                        .trim_start()
                        .to_string(),
                );
            }
        }
        (!fresh.is_empty()).then(|| format!("not checked in this session (said once): {}", fresh.join("; ")))
    }

    fn refused(&self, stderr: &str, status: u8, advise: &str) -> Verdict {
        let text = stderr.trim();
        let reason = if text.is_empty() {
            format!("the checker failed (exit status {status})")
        } else {
            text.to_string()
        };
        if self.mode == LokiMode::Advise {
            Verdict::Allow(Some(format!("{advise}:\n{reason}")))
        } else {
            Verdict::Block(reason)
        }
    }
}

impl FileWriteGuard for LokiGuard {
    fn name(&self) -> &str {
        "Loki"
    }

    fn before_write(&self, path: &Path, content: &str, cwd: &Path) -> Result<Verdict, String> {
        let Some(relative) = inside(&self.root, path) else {
            return Ok(Verdict::Allow(None));
        };
        let envelope = json!({
            "cwd": cwd.to_string_lossy(),
            "tool_name": "write",
            "tool_input": {"path": path.to_string_lossy(), "content": content},
        });
        let outcome = loki::run::protect(&self.root, &[relative], Some("ultron"), false, || Ok(envelope));
        // A check the Rust engine does not have yet (a language rule pack): the write proceeds, with a note.
        if let Some(error) = outcome.stderr.trim().strip_prefix(NOT_PORTED) {
            return Err(error.to_string());
        }
        if outcome.status == 0 {
            let (mut lines, not_checked) = classify_passed(&outcome.stderr);
            lines.extend(self.notice(&not_checked));
            return Ok(Verdict::Allow((!lines.is_empty()).then(|| lines.join("\n"))));
        }
        Ok(self.refused(
            &outcome.stderr,
            outcome.status,
            "would block this write (advise-only mode)",
        ))
    }

    fn before_shell(&self, command: &str, cwd: &Path) -> Result<Verdict, String> {
        let (status, stderr) =
            loki::shell_writes::run(&self.root, &json!({"command": command, "cwd": cwd.to_string_lossy()}));
        if status == 0 {
            return Ok(Verdict::Allow(None));
        }
        Ok(self.refused(&stderr, status, "would refuse this command (advise-only mode)"))
    }
}
