//! Unified diff parsing and file classification (`review_api.parse_diff`, `skip_reason`, `file_kind`, `render_hunk`).

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::py;

pub const MAX_LINE_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    /// `'+'`, `'-'` or `' '`.
    pub kind: char,
    pub old: Option<u64>,
    pub new: Option<u64>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hunk {
    pub old_start: u64,
    pub old_len: u64,
    pub new_start: u64,
    pub new_len: u64,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The first new-file line number in the hunk.
    pub fn new_first(&self) -> u64 {
        self.lines
            .iter()
            .find_map(|line| line.new)
            .unwrap_or(self.new_start.max(1))
    }

    /// The last new-file line number in the hunk.
    pub fn new_last(&self) -> u64 {
        self.lines
            .iter()
            .rev()
            .find_map(|line| line.new)
            .unwrap_or(self.new_start.max(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Added,
    Deleted,
    Renamed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn count(&self, kind: char) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == kind)
            .count()
    }

    pub fn added(&self) -> usize {
        self.count('+')
    }

    pub fn removed(&self) -> usize {
        self.count('-')
    }
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static HUNK: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^@@ -([0-9]+)(?:,([0-9]+))? \+([0-9]+)(?:,([0-9]+))? @@ ?(.*)$"));
static DIFF_GIT: LazyLock<Regex> = LazyLock::new(|| regex(r#"^diff --git "?[a-z]/(.*?)"? "?[a-z]/(.*?)"?$"#));
static PREFIX: LazyLock<Regex> = LazyLock::new(|| regex(r"^[a-z]/"));
static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(^|/)(package-lock\.json|npm-shrinkwrap\.json|yarn\.lock|pnpm-lock\.yaml|Cargo\.lock|poetry\.lock|Pipfile\.lock|composer\.lock|Gemfile\.lock|go\.sum|uv\.lock|bun\.lockb?)$|\.min\.(js|css)$|\.map$|\.snap$|\.generated\.|(^|/)(node_modules|vendor|dist|build)/",
    )
});
static BINARY_EXT: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)\.(png|jpe?g|gif|webp|ico|pdf|zip|gz|tgz|jar|wasm|woff2?|ttf|otf|mp[34]|mov|so|dylib|dll|exe)$")
});
static DOC: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\.(md|mdx|rst|txt|adoc)$"));
static TEST: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$"));

/// A quoted git path is a JSON-style string; one that does not parse keeps its text between the quotes.
fn unquote(path: &str) -> String {
    let path = py::strip(path);
    if path.starts_with('"') && path.ends_with('"') {
        return serde_json::from_str(path).unwrap_or_else(|_| path.get(1..path.len() - 1).unwrap_or("").to_string());
    }
    path.to_string()
}

fn number(text: Option<regex::Match<'_>>, default: u64) -> u64 {
    text.and_then(|found| found.as_str().parse().ok()).unwrap_or(default)
}

/// Parse unified git diff text (from `git diff` or `gh pr diff`) into files and hunks.
pub fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    let (mut old_left, mut new_left, mut old_no, mut new_no) = (0u64, 0u64, 0u64, 0u64);
    for raw in py::splitlines(text) {
        if in_hunk && (old_left > 0 || new_left > 0) {
            let hunk = files
                .last_mut()
                .and_then(|file| file.hunks.last_mut())
                .expect("an open hunk has a file");
            let mut chars = raw.chars();
            let marker = chars.next().unwrap_or(' ');
            let body = chars.as_str().to_string();
            let line = match marker {
                '\\' => continue,
                '+' => {
                    new_no += 1;
                    new_left = new_left.saturating_sub(1);
                    Some(DiffLine {
                        kind: '+',
                        old: None,
                        new: Some(new_no - 1),
                        text: body,
                    })
                }
                '-' => {
                    old_no += 1;
                    old_left = old_left.saturating_sub(1);
                    Some(DiffLine {
                        kind: '-',
                        old: Some(old_no - 1),
                        new: None,
                        text: body,
                    })
                }
                ' ' => {
                    old_no += 1;
                    new_no += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    Some(DiffLine {
                        kind: ' ',
                        old: Some(old_no - 1),
                        new: Some(new_no - 1),
                        text: body,
                    })
                }
                _ => None,
            };
            if let Some(line) = line {
                hunk.lines.push(line);
                continue;
            }
            in_hunk = false; // malformed: fall through to header parsing
        }
        if raw.starts_with("diff --git ") {
            let (old, new) = match DIFF_GIT.captures(raw) {
                Some(captures) => (Some(captures[1].to_string()), captures[2].to_string()),
                None => (None, raw.rsplit(" b/").next().unwrap_or(raw).to_string()),
            };
            files.push(FileDiff {
                path: new,
                old_path: old,
                status: Status::Modified,
                binary: false,
                hunks: Vec::new(),
            });
            in_hunk = false;
            continue;
        }
        let Some(current) = files.last_mut() else { continue };
        if raw.starts_with("new file mode") {
            current.status = Status::Added;
            current.old_path = None;
        } else if raw.starts_with("deleted file mode") {
            current.status = Status::Deleted;
        } else if let Some(rest) = raw.strip_prefix("rename from ") {
            current.old_path = Some(unquote(rest));
            current.status = Status::Renamed;
        } else if let Some(rest) = raw.strip_prefix("rename to ") {
            current.path = unquote(rest);
            current.status = Status::Renamed;
        } else if raw.starts_with("Binary files ") || raw.starts_with("GIT binary patch") {
            current.binary = true;
        } else if let Some(rest) = raw.strip_prefix("--- ") {
            let source = unquote(rest.split('\t').next().unwrap_or(""));
            if source != "/dev/null" && PREFIX.is_match(&source) && current.status != Status::Added {
                current.old_path = Some(source[2..].to_string());
            }
        } else if let Some(rest) = raw.strip_prefix("+++ ") {
            let dest = unquote(rest.split('\t').next().unwrap_or(""));
            if dest == "/dev/null" {
                current.status = Status::Deleted;
            } else if PREFIX.is_match(&dest) {
                current.path = dest[2..].to_string();
            }
        } else if let Some(captures) = HUNK.captures(raw) {
            let (old_start, old_len) = (number(captures.get(1), 0), number(captures.get(2), 1));
            let (new_start, new_len) = (number(captures.get(3), 0), number(captures.get(4), 1));
            current.hunks.push(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                header: py::strip(&captures[5]).to_string(),
                lines: Vec::new(),
            });
            in_hunk = true;
            (old_left, new_left, old_no, new_no) = (old_len, new_len, old_start, new_start);
        }
    }
    for item in &mut files {
        if item.status == Status::Modified
            && item
                .old_path
                .as_ref()
                .is_some_and(|old| !old.is_empty() && *old != item.path)
        {
            item.status = Status::Renamed;
        }
    }
    files
}

/// A synthetic diff for an untracked file: every line added.
pub fn added_file(path: &str, text: &str) -> FileDiff {
    let lines = py::splitlines(text);
    let hunk = Hunk {
        old_start: 0,
        old_len: 0,
        new_start: 1,
        new_len: lines.len() as u64,
        header: String::new(),
        lines: (1..)
            .zip(&lines)
            .map(|(number, body)| DiffLine {
                kind: '+',
                old: None,
                new: Some(number),
                text: body.to_string(),
            })
            .collect(),
    };
    let hunks = if lines.is_empty() { Vec::new() } else { vec![hunk] };
    FileDiff {
        path: path.to_string(),
        old_path: None,
        status: Status::Added,
        binary: false,
        hunks,
    }
}

/// Why a file is not reviewed, or `None` when it is.
pub fn skip_reason(item: &FileDiff) -> Option<&'static str> {
    if item.binary || BINARY_EXT.is_match(&item.path) {
        return Some("binary");
    }
    if item.status == Status::Deleted {
        return Some("deleted");
    }
    if GENERATED.is_match(&item.path) {
        return Some("generated, lockfile or vendored");
    }
    if item.added() == 0 {
        return Some("no added lines (rename, mode change or removal only)");
    }
    None
}

/// `"doc"`, `"test"` or `"code"`.
pub fn file_kind(path: &str) -> &'static str {
    if DOC.is_match(path) {
        "doc"
    } else if TEST.is_match(path) {
        "test"
    } else {
        "code"
    }
}

/// The hunk as gutter-numbered lines: new line number (right-aligned to 5), the marker, the clipped text.
pub fn render_hunk(hunk: &Hunk) -> Vec<String> {
    hunk.lines
        .iter()
        .map(|line| {
            let number = line.new.map(|number| number.to_string()).unwrap_or_default();
            format!("{number:>5} {} {}", line.kind, py::clip(&line.text, MAX_LINE_CHARS))
        })
        .collect()
}
