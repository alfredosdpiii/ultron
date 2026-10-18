//! Ultron's built-in Loki guardrails (ultron/loki.ts `LokiGuard`), running the Rust Loki engine in-process instead of
//! `python loki.py`:
//! - before `edit()`/`write()` change a file: `protect --file <path> --preview ultron` on the proposed content; a
//!   finding blocks the write (with `advise`, it is only reported);
//! - before a shell command that may write files: `shell-writes`, which refuses a write into source or configuration
//!   that would skip the before-write check;
//! - after a cell, on the files it changed by other means (`hook --file ... --format json`): new findings to fix,
//!   findings shown earlier and still open as one line, advisory lines once, at most 40 files per check.
//!
//! The policy is the repository's `.loki/loki.json` under the root (the Git work tree, else the session directory).

use crate::file_hooks::{AfterCellReport, AfterOutcome, CellChanges, FileWriteGuard, Verdict, inside};
use fancy_regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

/// Files one after-cell check takes; the rest are named as not checked.
const MAX_HOOK_FILES: usize = 40;
static PROTECTED_LOKI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\.loki/\S*: protected content changed$").expect("static pattern"));
static FINDING_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^:\s][^:\n]*?)(?::\d+)?: ").expect("static pattern"));
static ADVISORY_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(: loki/slop-[\w-]+: |\[Warning/|^oxlint advisory \(not blocking\))").expect("static pattern")
});
static ENGINE_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^loki: (invalid hook input|guard error)").expect("static pattern"));

/// A check's lines by tier (`LokiReport`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LokiReport {
    pub blocking: Vec<String>,
    pub advisory: Vec<String>,
    pub not_checked: Vec<String>,
    pub error: Option<String>,
}

fn texts(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            Value::Object(map) => map.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            _ => String::new(),
        })
        .filter(|text| !text.trim().is_empty())
        .collect()
}

/// `parseLokiReport`: the `--format json` document, or none when stdout is not one.
pub fn parse_loki_report(stdout: &str) -> Option<LokiReport> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    let map = parsed.as_object()?;
    if !map.contains_key("status") || !map.contains_key("blocking") {
        return None;
    }
    Some(LokiReport {
        blocking: texts(&map["blocking"]),
        advisory: texts(map.get("advisory").unwrap_or(&Value::Null)),
        not_checked: texts(map.get("not_checked").unwrap_or(&Value::Null)),
        error: map.get("error").and_then(Value::as_str).map(str::to_string),
    })
}

/// `classifyLokiText`: the tiers from the text an engine prints on stderr: `NOT CHECKED` lines, slop and analyzer
/// warnings as advisory, and when the check failed everything else (after the advisory label: advisory) blocking.
pub fn classify_loki_text(stderr: &str, failed: bool) -> LokiReport {
    let mut report = LokiReport::default();
    let mut labelled = false;
    for raw in stderr.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("loki: post-write check failed") {
            continue;
        }
        if failed && ENGINE_ERROR.is_match(line).unwrap_or(false) {
            return LokiReport {
                error: Some(line.to_string()),
                ..LokiReport::default()
            };
        }
        if line == "loki: advisory (not blocking):" {
            labelled = true;
        } else if line.starts_with("NOT CHECKED") {
            report.not_checked.push(line.to_string());
        } else if !failed || labelled || ADVISORY_LINE.is_match(line).unwrap_or(false) {
            report.advisory.push(line.to_string());
        } else {
            report.blocking.push(line.to_string());
        }
    }
    report
}

/// `findingKey`: a finding's file and its identity without the line number, which moves as the file is edited.
/// The engine's own `loki: ...` lines name no file at their start (`loki` is no path): a protected-file line is
/// followed by the file it names, others cannot be followed. Keyed by `loki`, they stayed "still open in loki"
/// for the rest of the session, since no check ever clears a file named `loki`.
fn finding_key(line: &str) -> (String, String) {
    if let Some(rest) = line.strip_prefix("loki: ") {
        return match rest.strip_prefix("protected file: ") {
            Some(named) => {
                let path = named.split(" (set ").next().unwrap_or(named).to_string();
                (path, line.to_string())
            }
            None => (String::new(), line.to_string()),
        };
    }
    match FINDING_PATH.captures(line).ok().flatten() {
        Some(found) => {
            let path = found[1].to_string();
            let rest = &line[found[0].len()..];
            (path.clone(), format!("{path}: {rest}"))
        }
        None => (String::new(), line.to_string()),
    }
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// A `.loki/` file whose content is the committed one: a `git checkout`, `switch` or `commit` in the cell brought
/// it there, so it is no change to the policy and not reported as one (a branch switch otherwise reported
/// `protected file: .loki/loki.json` on the cell). Untracked or modified `.loki/` files are still checked.
fn loki_file_as_committed(root: &Path, relative: &str) -> bool {
    if !relative.starts_with(".loki/") {
        return false;
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    git(&["ls-files", "--error-unmatch", "--", relative]) && git(&["diff", "--quiet", "HEAD", "--", relative])
}

fn first_line(text: &str) -> String {
    text.split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LokiMode {
    On,
    Advise,
    Off,
}

const OFF_VALUES: [&str; 5] = ["off", "0", "false", "no", "none"];

/// `lokiTimeoutMs`: ULTRON_LOKI_TIMEOUT_MS, how long a before-write (or before-shell) check may take before the write
/// proceeds unchecked, when it is a positive safe integer. Unset or invalid: None, no limit (TypeScript: 5 s), so a
/// check slowed by machine load still decides the write instead of letting it through unchecked.
pub fn loki_timeout(value: Option<&str>) -> Option<std::time::Duration> {
    // `Number(value)`: surrounding whitespace is ignored, and integral forms like "250.0" or "1e3" are integers.
    let n: f64 = value?.trim().parse().ok()?;
    (n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0).then(|| std::time::Duration::from_millis(n as u64))
}

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
    /// Findings already shown and not fixed since, per tier and file, so a later cell is not told again in full.
    open: Mutex<HashMap<&'static str, HashMap<String, HashSet<String>>>>,
    paused_shown: AtomicBool,
    /// Runs `hook --file ... --format json` in `root` (default: the Rust engine in-process).
    hook: HookRunner,
    /// Post-write checks run (`postWrite`): they compare against a base revision, so only in a repository.
    post_write: bool,
    /// How long a before-write check may take (`timeoutMs`); None: no limit.
    timeout: Option<std::time::Duration>,
}

/// Runs Loki's post-write `hook` on repository-relative files: its exit status, stdout and stderr.
pub type HookRunner = std::sync::Arc<dyn Fn(&Path, &[String]) -> loki::run::Outcome + Send + Sync>;

/// One tier's lines not yet shown for their file, how many shown earlier are still there, and their files.
struct Unseen {
    fresh: Vec<String>,
    earlier: usize,
    paths: Vec<String>,
}

impl LokiGuard {
    /// `root`: the repository (or working directory) Loki checks; `mode` is on or advise.
    pub fn new(root: impl Into<PathBuf>, mode: LokiMode) -> Self {
        LokiGuard {
            root: root.into(),
            mode,
            shown_notices: Mutex::new(Vec::new()),
            open: Mutex::new(HashMap::new()),
            paused_shown: AtomicBool::new(false),
            hook: std::sync::Arc::new(|root: &Path, files: &[String]| loki::run::hook(root, files, true)),
            post_write: true,
            timeout: None,
        }
    }

    /// The limit on each before-write and before-shell check (see [`loki_timeout`]); None: no limit.
    pub fn with_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether the files each cell changed are checked after it (`postWrite`; default on). Outside a Git
    /// repository only before-write checks run.
    pub fn with_post_write(mut self, post_write: bool) -> Self {
        self.post_write = post_write;
        self
    }

    /// Post-write checks ask `python` (the project's interpreter, `findProjectPython`; loki.ts passes it to the
    /// engine as `LOKI_PYTHON`) whether newly imported modules exist.
    pub fn with_project_python(mut self, python: Option<String>) -> Self {
        self.hook = std::sync::Arc::new(move |root: &Path, files: &[String]| {
            loki::run::hook_with_python(root, files, true, python.as_deref())
        });
        self
    }

    /// The same guard with another post-write engine (tests: a stand-in that answers scripted reports).
    pub fn with_hook(mut self, hook: HookRunner) -> Self {
        self.hook = hook;
        self
    }

    /// `#unseen`: the lines of one tier not yet shown for their file; a finding gone from a file checked now was
    /// fixed, and is new again if it comes back.
    fn unseen(&self, tier: &'static str, lines: &[String], checked: &[String]) -> Unseen {
        let mut open = self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let memory = open.entry(tier).or_default();
        let mut now: Vec<(String, HashSet<String>)> = Vec::new();
        let mut fresh = Vec::new();
        let mut stale: Vec<String> = Vec::new();
        for line in lines {
            let (path, key) = finding_key(line);
            if !path.is_empty() && memory.get(&path).is_some_and(|keys| keys.contains(&key)) {
                if !stale.contains(&path) {
                    stale.push(path.clone());
                }
            } else {
                fresh.push(line.clone());
            }
            // A line that names no file cannot be followed from cell to cell.
            if !path.is_empty() {
                match now.iter_mut().find(|(p, _)| *p == path) {
                    Some((_, keys)) => {
                        keys.insert(key);
                    }
                    None => now.push((path, HashSet::from([key]))),
                }
            }
        }
        for path in checked {
            memory.remove(path);
        }
        for (path, keys) in now {
            memory.insert(path, keys);
        }
        Unseen {
            earlier: lines.len() - fresh.len(),
            fresh,
            paths: stale,
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

    /// ULTRON_LOKI_TIMEOUT_MS when set (see [`loki_timeout`]); otherwise the in-process engine has no per-call
    /// limit (its tool runs have their own).
    fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout
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

    fn watches_cells(&self) -> bool {
        self.post_write && self.mode != LokiMode::Off
    }

    /// `afterCellChanges`: `hook` on the files a cell changed inside the repository (at most 40).
    fn after_cell_changes(
        &self,
        changes: &CellChanges,
        _cwd: &Path,
        _lane: &str,
    ) -> Result<Option<AfterCellReport>, String> {
        let mut targets: Vec<String> = Vec::new();
        for path in changes.files.iter().chain(&changes.checked) {
            if let Some(relative) = inside(&self.root, path)
                && !targets.contains(&relative)
                && !loki_file_as_committed(&self.root, &relative)
            {
                targets.push(relative);
            }
        }
        if targets.is_empty() {
            return Ok(None);
        }
        let checked: Vec<String> = targets.iter().take(MAX_HOOK_FILES).cloned().collect();
        let outcome = (self.hook)(&self.root, &checked);
        let failed = outcome.status != 0;
        let report = parse_loki_report(&outcome.stdout).unwrap_or_else(|| classify_loki_text(&outcome.stderr, failed));
        if let Some(error) = report.error {
            return Err(error);
        }
        if failed && report.blocking.is_empty() {
            let line = first_line(&outcome.stderr);
            return Err(if line.is_empty() {
                format!("the checker failed (exit status {})", outcome.status)
            } else {
                line
            });
        }
        // Loki compares against the committed policy: while .loki/ itself has uncommitted changes every post-write
        // check reports that and stops. Say so once.
        if !report.blocking.is_empty()
            && report
                .blocking
                .iter()
                .all(|line| PROTECTED_LOKI.is_match(line.trim()).unwrap_or(false))
        {
            if self.paused_shown.swap(true, Ordering::SeqCst) {
                return Ok(None);
            }
            return Ok(Some(AfterCellReport {
                outcome: AfterOutcome::Clean,
                message: Some("post-write checks are paused: .loki/ has uncommitted changes, and Loki checks against the committed policy. Before-write checks of edit() and write() still run. Commit .loki/ to resume them (git add .loki && git commit -m 'Add Loki guardrails' -- .loki).".into()),
            }));
        }
        let blocking = self.unseen("blocking", &report.blocking, &checked);
        let advisory = self.unseen("advisory", &report.advisory, &checked);
        let mut sections = Vec::new();
        if !blocking.fresh.is_empty() {
            sections.push(format!(
                "{} from this cell's changes; fix these:\n{}",
                count(blocking.fresh.len(), "new finding"),
                blocking.fresh.join("\n")
            ));
        }
        if blocking.earlier > 0 {
            sections.push(format!(
                "{} reported earlier {} still open in {}.",
                count(blocking.earlier, "finding"),
                if blocking.earlier == 1 { "is" } else { "are" },
                blocking.paths.join(", ")
            ));
        }
        if !advisory.fresh.is_empty() {
            sections.push(format!(
                "FYI (advisory, not blocking; no change required):\n{}",
                advisory.fresh.join("\n")
            ));
        }
        sections.extend(self.notice(&report.not_checked));
        if targets.len() > checked.len() {
            sections.push(format!(
                "({} more changed files were not checked)",
                targets.len() - checked.len()
            ));
        }
        let outcome = if !report.blocking.is_empty() {
            AfterOutcome::Findings
        } else if !advisory.fresh.is_empty() {
            AfterOutcome::Advisory
        } else {
            AfterOutcome::Clean
        };
        Ok((!sections.is_empty()).then(|| AfterCellReport {
            outcome,
            message: Some(sections.join("\n")),
        }))
    }

    fn before_shell(&self, command: &str, cwd: &Path) -> Result<Verdict, String> {
        let (status, stderr) =
            loki::shell::run_shell_writes(&self.root, &json!({"command": command, "cwd": cwd.to_string_lossy()}));
        if status == 0 {
            return Ok(Verdict::Allow(None));
        }
        Ok(self.refused(&stderr, status, "would refuse this command (advise-only mode)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #5: `loki: protected file: X` was keyed by the file `loki`, which no check clears, so it stayed
    /// "still open in loki" all session. It is followed by the file it names; other `loki:` lines by nothing.
    #[test]
    fn host_loki_engine_lines_are_keyed_by_the_file_they_name() {
        let protected = "loki: protected file: .loki/loki.json (set LOKI_ALLOW_PROTECTED=1 to override)";
        assert_eq!(
            finding_key(protected),
            (".loki/loki.json".to_string(), protected.to_string())
        );
        let other = "loki: unsupported changed file type: a";
        assert_eq!(finding_key(other), (String::new(), other.to_string()));
        assert_eq!(
            finding_key("src/a.py:3: loki/x: y"),
            ("src/a.py".to_string(), "src/a.py: loki/x: y".to_string())
        );
    }
}
