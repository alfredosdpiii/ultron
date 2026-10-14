//! The deep pass's read-only repository and its request serving (`autoreview_deep.py` `Repo`, `Rejected`,
//! `_numbered`, `_hits`, `_definition_pattern`, `serve_request`, `serve_blocks`, `serve`). An investigator frame
//! asks for lookups; the host validates each against a closed set, answers it from the head commit through a fixed
//! set of read-only git subcommands (`grep`, `show`, `ls-tree`, `log`), and never runs the reviewed code.
//!
//! The structural reference (`autoreview_reference.py`) and the test session (`autoreview_tests.py`) are ported
//! elsewhere: they reach this module through the [`Reference`] and [`TestSession`] traits, implemented here for
//! [`Structural`] (the reference built through a [`Repo::reader`] of this repository, sharing its cache) and for
//! `RefCell<review_tests::TestSession>`. A [`Repo::owned`] repository owns its runner, so the host can keep it,
//! its reference and its session across requests.
//!
//! Known differences from the Python, all outside recorded behaviour:
//! - A grep pattern is validated with the `regex` crate where the Python used `re.compile`: the text after
//!   "bad regular expression: " is the Rust error, not Python's. Look-around and backreferences, which `re` accepts
//!   and the `regex` crate does not, are let through to `git grep -E` as the Python let them through. Patterns only
//!   one of the two engines accepts (`\p{L}` is Rust-only, `\Z` Python-only) differ in whether they are rejected
//!   here or by git ("grep failed: ...").
//! - `git grep` line numbers are read as ASCII digits (`str.isdigit` also takes other Unicode digits; git never
//!   writes them).
//! - `Repo::git` refuses a subcommand outside the list with an error instead of raising `AssertionError`.
//! - A test session's export or process failure (Python's `RuntimeError`/`OSError`, which propagated out of
//!   `serve_request`) is refused with "the test session failed (<Type>: <message>)".
//! - The reference's stats carry `python_unindexed` when no interpreter indexed the Python files (Python always had
//!   its own `ast`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use regex::Regex;
use serde_json::{Map, Value};

use crate::diff::FileDiff;
use crate::findings::text;
use crate::levels::truthy;
use crate::py;
use crate::reference::{self as structural, PythonIndex};
use crate::review_tests;
pub use crate::review_tests::summarize_value as summarize;
use crate::scope::{Output, Runner, normpath};

pub const GIT_TIMEOUT_S: u64 = 30;
/// The only git subcommands the deep pass runs. All of them only read the object database.
pub const ALLOWED_GIT: [&str; 4] = ["grep", "show", "ls-tree", "log"];
pub const MAX_REQUESTS: usize = 8;
pub const ROUND_CHARS: usize = 24_000;
pub const MAX_READ_LINES: i64 = 200;
pub const MAX_GREP_HITS: i64 = 50;
pub const DEFAULT_GREP_HITS: i64 = 20;
/// Hits fetched for a scoped or count-only grep: its count is exact up to this many.
pub const MAX_COUNT_HITS: usize = 1_000;
pub const MAX_REFERENCES: usize = 30;
pub const MAX_LIST: usize = 200;
pub const MAX_PATTERN_CHARS: usize = 200;
pub const MAX_FILE_BYTES: usize = 2_000_000;
pub const MAX_HISTORY: i64 = 20;
pub const MAX_BLAME_LINES: i64 = 60;

/// A request the host will not serve; the message goes back to the investigator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

fn rejected<T>(message: impl Into<String>) -> Result<T, Rejected> {
    Err(Rejected(message.into()))
}

/// One grep hit: (path, line, text).
pub type Hit = (String, i64, String);

/// A definition the structural reference knows (`autoreview_reference.Symbol`, as far as this module reads it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolAt {
    /// The symbol's key in the reference (`Symbol.key`), for [`Reference::symbol_view`].
    pub key: String,
    pub path: String,
    pub line: i64,
}

/// A call site the structural reference resolved (`autoreview_reference.CallSite`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAt {
    pub path: String,
    pub line: i64,
    pub text: String,
    pub enclosing: Option<String>,
}

/// The structural reference of the change (`autoreview_reference.Reference` and its module-level `serve`).
pub trait Reference: Send + Sync {
    /// A `symbol`, `callers`, `callees` or `tests_of` request answered from the index, or `None` when it does not
    /// know the name.
    fn serve(&self, kind: &str, args: &Map<String, Value>) -> Option<(String, String)>;
    fn symbols_named(&self, name: &str) -> Vec<SymbolAt>;
    /// `symbol_view(symbol, body=True)`.
    fn symbol_view(&self, symbol: &SymbolAt) -> String;
    fn callers(&self, name: &str, outside_changes: bool) -> Vec<CallAt>;
    /// Whether the reference read (indexed) this file.
    fn indexed(&self, path: &str) -> bool;
    /// The reference's view for the frames (`block()`).
    fn block(&self) -> String;
    fn stats(&self) -> Map<String, Value>;
    /// Host-observed facts (`facts()`: families, workflow and Terraform facts), for the compiled mode's coverage.
    fn facts(&self) -> Vec<Value> {
        Vec::new()
    }
    /// Findings the reference observed by itself (`structural_findings()`), for the verifier.
    fn structural_findings(&self) -> Vec<Value> {
        Vec::new()
    }
}

/// The review's sandboxed test session (`autoreview_tests.TestSession`), as the deep pass uses it. Records are the
/// session's run records (JSON objects with `n`, `kind`, `rev`, `command`, `status`, `tests`, `output`, `paths`, ...).
/// Not `Sync`: a session runs one execution at a time (its executor and exports are its own), as the Python did.
pub trait TestSession {
    /// `run(paths(raw_paths), select)`: one run of the named tests; `Err` is a `TestsRejected` message.
    fn run(&self, paths: Option<&Value>, select: Option<&Value>) -> Result<Value, String>;
    /// `mutation(path, line, replacement, tests)`: the tests run with one line replaced.
    fn mutation(
        &self,
        path: Option<&Value>,
        line: Option<&Value>,
        replacement: Option<&Value>,
        tests: Option<&Value>,
    ) -> Result<Value, String>;
    fn records(&self) -> Vec<Value>;
    fn limit(&self) -> usize;
    fn tracked(&self, path: &str) -> bool;
    /// Whether a runner is recognized for these test files (`plan(paths)` is non-empty).
    fn plan(&self, paths: &[String]) -> bool;
    /// `compare(paths)`: `{"runs": [...], "regressions": [{"id", "head", "base", "detail"}]}`.
    fn compare(&self, paths: &[String]) -> Result<Value, String>;
}

type Lines = Option<Arc<Vec<String>>>;

/// The process runner a [`Repo`] runs git through: borrowed, or owned (a repository the host keeps across
/// requests, with its reference and test session beside it).
#[derive(Clone)]
enum RunnerRef<'r> {
    Borrowed(&'r (dyn Runner + Sync)),
    Owned(Arc<dyn Runner + Send + Sync>),
}

impl RunnerRef<'_> {
    fn get(&self) -> &(dyn Runner + Sync) {
        match self {
            RunnerRef::Borrowed(runner) => *runner,
            RunnerRef::Owned(runner) => &**runner,
        }
    }
}

/// What every handle on one commit shares: the tracked files, the file cache and the git call count.
#[derive(Default)]
struct Reads {
    files: OnceLock<(Vec<String>, HashSet<String>)>,
    cache: Mutex<HashMap<String, Lines>>,
    calls: AtomicUsize,
}

/// A repository at one commit, through a fixed set of read-only git subcommands.
pub struct Repo<'r> {
    pub root: String,
    pub rev: String,
    runner: RunnerRef<'r>,
    reads: Arc<Reads>,
    reference: Mutex<Option<Arc<dyn Reference>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Repo<'static> {
    /// A repository that owns its runner (the host's, kept across requests).
    pub fn owned(root: &str, rev: &str, runner: Arc<dyn Runner + Send + Sync>) -> Self {
        Self::with(root, rev, RunnerRef::Owned(runner))
    }
}

impl<'r> Repo<'r> {
    pub fn new(root: &str, rev: &str, runner: &'r (dyn Runner + Sync)) -> Self {
        Self::with(root, rev, RunnerRef::Borrowed(runner))
    }

    fn with(root: &str, rev: &str, runner: RunnerRef<'r>) -> Self {
        Self {
            root: root.to_string(),
            rev: rev.to_string(),
            runner,
            reads: Arc::default(),
            reference: Mutex::new(None),
        }
    }

    /// Another handle on the same commit sharing this one's reads (files, cache, call count) but no reference: what
    /// the structural reference reads through (Python's reference held the deep pass's `Repo` itself).
    pub fn reader(&self) -> Repo<'r> {
        Repo {
            root: self.root.clone(),
            rev: self.rev.clone(),
            runner: self.runner.clone(),
            reads: self.reads.clone(),
            reference: Mutex::new(None),
        }
    }

    /// The same repository at another commit, with reads of its own (`Repo(repo.root, rev, repo._runner)`).
    pub fn at(&self, rev: &str) -> Repo<'r> {
        Self::with(&self.root, rev, self.runner.clone())
    }

    /// Git calls made, for the record.
    pub fn calls(&self) -> usize {
        self.reads.calls.load(Ordering::Relaxed)
    }

    /// The structural reference of the change, once the map built it.
    pub fn reference(&self) -> Option<Arc<dyn Reference>> {
        lock(&self.reference).clone()
    }

    pub fn set_reference(&self, reference: Option<Arc<dyn Reference>>) {
        *lock(&self.reference) = reference;
    }

    /// Runs `git <args>` when the subcommand is one of [`ALLOWED_GIT`].
    pub fn git(&self, args: &[&str]) -> Result<Output, String> {
        match args.first() {
            Some(first) if ALLOWED_GIT.contains(first) => {}
            first => {
                let shown = first.map_or("()".to_string(), |name| format!("({},)", py::repr(name)));
                return Err(format!("the deep pass does not run git {shown}"));
            }
        }
        self.reads.calls.fetch_add(1, Ordering::Relaxed);
        let argv: Vec<String> = std::iter::once("git")
            .chain(args.iter().copied())
            .map(str::to_string)
            .collect();
        Ok(self.runner.get().run(&argv, &self.root, GIT_TIMEOUT_S))
    }

    fn call(&self, args: &[&str]) -> Output {
        self.git(args).unwrap_or_else(|stderr| Output {
            code: -1,
            stdout: String::new(),
            stderr,
        })
    }

    fn listing(&self) -> &(Vec<String>, HashSet<String>) {
        self.reads.files.get_or_init(|| {
            let out = self.call(&["ls-tree", "-r", "--name-only", "-z", self.rev.as_str()]);
            let files: Vec<String> = if out.code == 0 {
                out.stdout
                    .split('\0')
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect()
            } else {
                Vec::new()
            };
            let tracked = files.iter().cloned().collect();
            (files, tracked)
        })
    }

    /// The tracked files of the commit.
    pub fn files(&self) -> &[String] {
        &self.listing().0
    }

    pub fn is_tracked(&self, path: &str) -> bool {
        self.listing().1.contains(path)
    }

    /// `raw` as a tracked path of the commit, or Rejected.
    pub fn path(&self, raw: Option<&Value>) -> Result<String, Rejected> {
        let raw = match raw {
            Some(Value::String(raw)) if !py::strip(raw).is_empty() && !raw.contains('\0') && !raw.contains('\n') => raw,
            _ => return rejected("path must be a string"),
        };
        let mut text = py::strip(raw);
        if text.starts_with(['/', '~', ':', '-']) || text.contains('\\') {
            return rejected(format!("{} is not a path inside the repository", py::repr(text)));
        }
        if let Some(rest) = text.strip_prefix("./") {
            text = rest;
        }
        if text.split('/').any(|part| part == "..") {
            return rejected(format!("{} leaves the repository", py::repr(text)));
        }
        let normal = normpath(text);
        if !self.is_tracked(&normal) {
            return rejected(format!("{normal} is not a tracked file at the reviewed commit"));
        }
        Ok(normal)
    }

    /// The lines of a tracked text file at the commit; `None` when it is binary, too large or unreadable.
    pub fn lines(&self, path: &str) -> Lines {
        if let Some(found) = lock(&self.reads.cache).get(path) {
            return found.clone();
        }
        let out = self.call(&["show", &format!("{}:{}", self.rev, path)]);
        let ok = out.code == 0
            && !out.stdout.chars().take(8192).any(|c| c == '\0')
            && out.stdout.chars().count() <= MAX_FILE_BYTES;
        let lines = ok.then(|| Arc::new(py::splitlines(&out.stdout).into_iter().map(str::to_string).collect()));
        lock(&self.reads.cache).insert(path.to_string(), lines.clone());
        lines
    }

    /// (path, line, text) of matching lines in tracked text files, at most `limit`.
    pub fn grep(
        &self,
        pattern: &str,
        fixed: bool,
        word: bool,
        pathspec: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Hit>, Rejected> {
        let mut args = vec!["grep", "-n", "-I", "--no-color", if fixed { "-F" } else { "-E" }];
        if word {
            args.push("-w");
        }
        args.extend([
            "-e",
            pattern,
            self.rev.as_str(),
            "--",
            pathspec.filter(|spec| !spec.is_empty()).unwrap_or("."),
        ]);
        let out = self.call(&args);
        if out.code != 0 && out.code != 1 {
            return rejected(format!("grep failed: {}", text(Some(&Value::String(out.stderr)), 120)));
        }
        let prefix = format!("{}:", self.rev);
        let mut hits = Vec::new();
        for raw in py::splitlines(&out.stdout) {
            let raw = raw.strip_prefix(prefix.as_str()).unwrap_or(raw);
            let (path, rest) = raw.split_once(':').unwrap_or((raw, ""));
            let (number, body) = rest.split_once(':').unwrap_or((rest, ""));
            if !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
                && let Ok(line) = number.parse()
            {
                hits.push((path.to_string(), line, body.to_string()));
            }
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    fn log_lines(&self, args: &[&str]) -> Vec<String> {
        let out = self.call(args);
        if out.code != 0 {
            return Vec::new();
        }
        py::splitlines(&out.stdout).into_iter().map(str::to_string).collect()
    }

    /// The last commits that touched a file: short hash, date, subject.
    pub fn history(&self, path: &str, count: i64) -> Vec<String> {
        let count = count.to_string();
        self.log_lines(&[
            "log",
            "-n",
            count.as_str(),
            "--format=%h %ad %s",
            "--date=short",
            self.rev.as_str(),
            "--",
            path,
        ])
    }

    /// The commits that last changed lines start..end of a file.
    pub fn line_history(&self, path: &str, start: i64, end: i64, count: i64) -> Vec<String> {
        let range = format!("-L{start},{end}:{path}");
        let count = count.to_string();
        let args = [
            "log",
            range.as_str(),
            "-s",
            "-n",
            count.as_str(),
            "--format=%h %ad %s",
            "--date=short",
            self.rev.as_str(),
        ];
        self.log_lines(&args)
            .into_iter()
            .filter(|line| !py::strip(line).is_empty())
            .collect()
    }

    /// The commits that added or removed a string.
    pub fn pickaxe(&self, needle: &str, count: i64) -> Vec<String> {
        let needle = format!("-S{needle}");
        let count = count.to_string();
        self.log_lines(&[
            "log",
            needle.as_str(),
            "-n",
            count.as_str(),
            "--format=%h %ad %s",
            "--date=short",
            self.rev.as_str(),
        ])
    }

    /// Subjects and first body lines of the commits under review.
    pub fn messages(&self, base: Option<&str>, count: usize) -> Vec<String> {
        let Some(base) = base.filter(|base| !base.is_empty()) else {
            return Vec::new();
        };
        let count = count.to_string();
        let range = format!("{base}..{}", self.rev);
        let out = self.call(&["log", "-n", count.as_str(), "--format=%s%n%b%x00", range.as_str()]);
        if out.code != 0 {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for entry in out.stdout.split('\0') {
            let kept: Vec<String> = py::splitlines(py::strip(entry))
                .into_iter()
                .map(py::strip)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            lines.extend(kept.into_iter().take(3));
        }
        lines
    }
}

static SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_.$-]{0,79}\n?\z").expect("pattern"));
static GLOB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./*?\[\]{}, -]{1,160}\n?\z").expect("pattern"));

/// `"{number:>5} | {line}"` for lines start..=end (1-based), each clipped to 300 characters.
pub fn numbered(lines: &[String], start: i64, end: i64) -> String {
    (start..=end)
        .filter_map(|number| {
            let index = usize::try_from(number - 1).ok()?;
            Some(format!("{number:>5} | {}", py::clip(lines.get(index)?, 300)))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Grep hits as `path:line: text` lines, or "0 matches".
pub fn hits_text(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "0 matches".to_string();
    }
    hits.iter()
        .map(|(path, line, body)| format!("{path}:{line}: {}", py::clip(py::strip(body), 200)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Python's `re.escape`.
pub fn re_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{0b}\u{0c}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The `git grep -E` pattern of a definition of `symbol` (its last dotted part).
pub fn definition_pattern(symbol: &str) -> String {
    let leaf = symbol.rsplit('.').next().unwrap_or(symbol);
    let name = re_escape(leaf);
    format!(
        "(^|[^A-Za-z0-9_])(def|class|function|func|fn|interface|type|struct|enum|const|let|var)[ \\t]+{name}([^A-Za-z0-9_]|$)|^[ \\t]*{name}[ \\t]*[:=][^=]"
    )
}

/// Python's `int(value)`: the integer, or the `TypeError`/`ValueError` message.
pub fn py_int(value: Option<&Value>) -> Result<i64, String> {
    if let Some(number) = crate::findings::int_of(value) {
        return Ok(number);
    }
    let kind = match value {
        None | Some(Value::Null) => "NoneType",
        Some(Value::Array(_)) => "list",
        Some(Value::Object(_)) => "dict",
        Some(Value::String(raw)) => return Err(format!("invalid literal for int() with base 10: {}", py::repr(raw))),
        Some(Value::Bool(_)) | Some(Value::Number(_)) => return Err("cannot convert to integer".to_string()),
    };
    Err(format!(
        "int() argument must be a string, a bytes-like object or a real number, not '{kind}'"
    ))
}

/// `_int(value, default)`: `int(value)`, or `default` when it does not convert.
pub fn int_or(value: Option<&Value>, default: i64) -> i64 {
    py_int(value).unwrap_or(default)
}

/// `int(value or default)`.
fn int_truthy(value: Option<&Value>, default: i64) -> Result<i64, String> {
    if truthy(value) { py_int(value) } else { Ok(default) }
}

/// Python's `json.dumps(value)` (ASCII only, `", "` and `": "` separators), or with `indent` as `json.dumps(value,
/// indent=n)`.
pub fn dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    dump(value, indent, 0, &mut out);
    out
}

fn dump_str(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn dump(value: &Value, indent: Option<usize>, depth: usize, out: &mut String) {
    let (open_sep, item_sep, close_sep) = match indent {
        Some(width) => (
            format!("\n{}", " ".repeat(width * (depth + 1))),
            format!(",\n{}", " ".repeat(width * (depth + 1))),
            format!("\n{}", " ".repeat(width * depth)),
        ),
        None => (String::new(), ", ".to_string(), String::new()),
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => out.push_str(&py::float_repr(float)),
            _ => out.push_str(&number.to_string()),
        },
        Value::String(text) => dump_str(text, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            out.push_str(&open_sep);
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(&item_sep);
                }
                dump(item, indent, depth + 1, out);
            }
            out.push_str(&close_sep);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            out.push_str(&open_sep);
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(&item_sep);
                }
                dump_str(key, out);
                out.push_str(": ");
                dump(item, indent, depth + 1, out);
            }
            out.push_str(&close_sep);
            out.push('}');
        }
    }
}

/// Whether Python's `re.compile` would take the pattern, as far as the `regex` crate can tell (look-around and
/// backreferences, which it does not support, are let through).
fn compile_error(pattern: &str) -> Option<String> {
    let error = Regex::new(pattern).err()?.to_string();
    if error.contains("look-around") || error.contains("backreferences") {
        return None;
    }
    let last = error
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(&error);
    Some(last.trim().trim_start_matches("error: ").to_string())
}

/// Answer one investigator request from the head commit: (title, body). Rejected for anything outside the closed
/// set: the read-only lookups and, when the review may run tests, two sandboxed test executions.
pub fn serve_request(
    repo: &Repo<'_>,
    request: &Value,
    tests: Option<&dyn TestSession>,
) -> Result<(String, String), Rejected> {
    let Some((kind, args)) = request
        .as_object()
        .filter(|map| map.len() == 1)
        .and_then(|map| map.iter().next())
    else {
        return rejected(
            "a request is an object with exactly one of read, grep, list, definition, references, symbol, callers, \
             callees, tests_of, history, blame_range, pickaxe",
        );
    };
    let Some(args) = args.as_object() else {
        return rejected(format!("{kind} takes an object"));
    };
    let reference = repo.reference();
    let mut kind = kind.as_str();
    let mut fallback = Map::new();
    let mut args = args;
    if matches!(kind, "symbol" | "callers" | "callees" | "tests_of") {
        let key = if kind == "symbol" { "name" } else { "symbol" };
        let name = match args.get(key) {
            Some(Value::String(name)) if SYMBOL.is_match(py::strip(name)) => py::strip(name).to_string(),
            _ => return rejected(format!("{kind} takes one identifier in {key}")),
        };
        if let Some(answer) = reference.as_ref().and_then(|reference| reference.serve(kind, args)) {
            return Ok(answer);
        }
        if kind == "callees" {
            return rejected(format!(
                "no definition of {name} in the reference; ask for definition or grep instead"
            ));
        }
        kind = if kind == "symbol" { "definition" } else { "references" };
        fallback.insert("symbol".into(), Value::String(name));
        args = &fallback;
    }
    let get = |key: &str| args.get(key);
    match kind {
        "read" => {
            let path = repo.path(get("path"))?;
            let Some(lines) = repo.lines(&path) else {
                return rejected(format!("{path} is binary or too large to read"));
            };
            let bad = || Rejected("read takes line numbers in start and end".to_string());
            let start = int_truthy(get("start"), 1).map_err(|_| bad())?.max(1);
            let end = int_truthy(get("end"), start + MAX_READ_LINES - 1).map_err(|_| bad())?;
            let total = lines.len() as i64;
            let end = end.min(total).min(start + MAX_READ_LINES - 1);
            if start > total {
                return rejected(format!("{path} has {total} lines"));
            }
            Ok((
                format!("read {path}:{start}-{end} (of {total} lines)"),
                numbered(&lines, start, end),
            ))
        }
        "grep" => serve_grep(repo, args),
        "list" => {
            let raw = match get("dir") {
                Some(Value::String(raw))
                    if !raw.starts_with(['/', '~', '-', ':'])
                        && !raw.split('/').any(|part| part == "..")
                        && !raw.contains('\0') =>
                {
                    raw
                }
                _ => return rejected("dir must be a directory inside the repository"),
            };
            let stripped = py::strip(raw);
            let directory = normpath(if stripped.is_empty() { "." } else { stripped });
            let prefix = if directory == "." {
                String::new()
            } else {
                format!("{directory}/")
            };
            let mut names: Vec<String> = Vec::new();
            let mut seen = HashSet::new();
            for path in repo.files() {
                let Some(rest) = path.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                let entry = match rest.split_once('/') {
                    Some((head, tail)) if !tail.is_empty() => format!("{head}/"),
                    Some((head, _)) => head.to_string(),
                    None => rest.to_string(),
                };
                if seen.insert(entry.clone()) {
                    names.push(entry);
                }
            }
            if names.is_empty() {
                return rejected(format!("{directory} is not a directory with tracked files"));
            }
            let mut body = names.iter().take(MAX_LIST).cloned().collect::<Vec<_>>().join("\n");
            if names.len() > MAX_LIST {
                body.push_str(&format!("\n... {} more", names.len() - MAX_LIST));
            }
            Ok((format!("list {directory} ({} entries)", names.len()), body))
        }
        "definition" | "references" => {
            let symbol = match get("symbol") {
                Some(Value::String(symbol)) if SYMBOL.is_match(symbol) => symbol.as_str(),
                _ => return rejected("symbol must be one identifier"),
            };
            if kind == "references" {
                let leaf = symbol.rsplit('.').next().unwrap_or(symbol);
                let hits = repo.grep(leaf, true, true, None, MAX_REFERENCES)?;
                let mut body = hits_text(&hits);
                if let Some(reference) = reference.as_ref()
                    && !reference.symbols_named(symbol).is_empty()
                {
                    let calls = reference.callers(symbol, false);
                    if !calls.is_empty() {
                        let shown = calls
                            .iter()
                            .take(MAX_REFERENCES)
                            .map(|call| {
                                let within = call
                                    .enclosing
                                    .as_deref()
                                    .filter(|name| !name.is_empty())
                                    .map(|name| format!(" in {name}"))
                                    .unwrap_or_default();
                                format!("{}:{}{within}: {}", call.path, call.line, py::clip(&call.text, 140))
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        body = format!("Call sites resolved by the reference:\n{shown}\n\nText matches:\n{body}");
                    }
                }
                let more = if hits.len() >= MAX_REFERENCES {
                    " (more not shown)"
                } else {
                    ""
                };
                return Ok((format!("references {symbol} -> {} matches{more}", hits.len()), body));
            }
            let mut hits = repo.grep(&definition_pattern(symbol), false, false, None, 6)?;
            let mut parts = Vec::new();
            if let Some(reference) = reference.as_ref() {
                let known = reference.symbols_named(symbol);
                if !known.is_empty() {
                    parts = known.iter().take(2).map(|item| reference.symbol_view(item)).collect();
                    hits.retain(|hit| {
                        !known
                            .iter()
                            .any(|item| hit.0 == item.path && (hit.1 - item.line).abs() <= 1)
                    });
                }
            }
            for (path, line, _) in hits.iter().take(3) {
                if let Some(lines) = repo.lines(path).filter(|lines| !lines.is_empty()) {
                    let end = (lines.len() as i64).min(line + 24);
                    parts.push(format!("{path}:\n{}", numbered(&lines, *line, end)));
                }
            }
            if hits.len() > 3 {
                parts.push(hits_text(&hits[3..]));
            }
            let body = if parts.is_empty() {
                "0 matches".to_string()
            } else {
                parts.join("\n\n")
            };
            Ok((format!("definition {symbol} -> {} matches", hits.len()), body))
        }
        "history" => {
            let path = repo.path(get("path"))?;
            let count = int_or(get("n"), 10).clamp(1, MAX_HISTORY);
            let lines = repo.history(&path, count);
            let body = if lines.is_empty() {
                "no commits found".to_string()
            } else {
                lines.join("\n")
            };
            Ok((format!("history {path} -> {} commits", lines.len()), body))
        }
        "blame_range" => {
            let path = repo.path(get("path"))?;
            let total = repo.lines(&path).map_or(0, |lines| lines.len() as i64);
            let start = int_or(get("start"), 1).max(1);
            let end = int_or(get("end"), start).min(total).min(start + MAX_BLAME_LINES - 1);
            if start > total || end < start {
                return rejected(format!("{path} has {total} lines"));
            }
            let lines = repo.line_history(&path, start, end, 5);
            let body = if lines.is_empty() {
                "no history available for these lines".to_string()
            } else {
                lines.join("\n")
            };
            Ok((
                format!("blame_range {path}:{start}-{end} -> {} commits", lines.len()),
                body,
            ))
        }
        "pickaxe" => {
            let needle = match get("string") {
                Some(Value::String(needle))
                    if (3..=120).contains(&needle.chars().count())
                        && !needle.contains('\n')
                        && !needle.contains('\0') =>
                {
                    needle
                }
                _ => return rejected("pickaxe takes a one-line string of 3 to 120 characters"),
            };
            let lines = repo.pickaxe(needle, int_or(get("n"), 5).clamp(1, MAX_HISTORY));
            let body = if lines.is_empty() {
                "no commit added or removed it".to_string()
            } else {
                lines.join("\n")
            };
            Ok((format!("pickaxe {} -> {} commits", py::repr(needle), lines.len()), body))
        }
        "run_tests" | "mutation_check" => {
            let Some(tests) = tests else {
                return rejected("tests are not run in this review");
            };
            let record = if kind == "run_tests" {
                tests.run(get("paths"), get("select"))
            } else {
                tests.mutation(get("path"), get("line"), get("replacement"), get("tests"))
            }
            .map_err(Rejected)?;
            Ok((format!("test run {}", py::fstr(record.get("n"))), summarize(&record)))
        }
        other => rejected(format!(
            "unknown request {}: use read, grep, list, definition, references, history, blame_range or pickaxe",
            py::repr(other)
        )),
    }
}

fn serve_grep(repo: &Repo<'_>, args: &Map<String, Value>) -> Result<(String, String), Rejected> {
    let get = |key: &str| args.get(key);
    let pattern = match get("pattern") {
        Some(Value::String(pattern))
            if !pattern.is_empty()
                && pattern.chars().count() <= MAX_PATTERN_CHARS
                && !pattern.contains('\n')
                && !pattern.contains('\0') =>
        {
            pattern.as_str()
        }
        _ => {
            return rejected(format!(
                "grep takes a one-line pattern of at most {MAX_PATTERN_CHARS} characters"
            ));
        }
    };
    if let Some(error) = compile_error(pattern) {
        return rejected(format!("bad regular expression: {error}"));
    }
    let glob = match get("path_glob") {
        None | Some(Value::Null) => None,
        Some(Value::String(glob))
            if GLOB.is_match(glob)
                && !glob.starts_with(['/', '-', ':'])
                && !glob.split('/').any(|part| part == "..") =>
        {
            Some(glob.as_str())
        }
        Some(_) => return rejected("path_glob must be a relative glob inside the repository"),
    };
    let pathspec = glob.map(|glob| format!(":(glob){glob}"));
    let limit = int_truthy(get("max"), DEFAULT_GREP_HITS).map_or(DEFAULT_GREP_HITS, |max| max.clamp(1, MAX_GREP_HITS));
    let limit = limit as usize;
    let start = int_or(get("start"), 0).max(0);
    let end = int_or(get("end"), 0).max(0);
    let scoped = start > 0 || end > 0;
    let count_only = get("count_only") == Some(&Value::Bool(true));
    let fetch = if scoped || count_only { MAX_COUNT_HITS } else { limit };
    let raw_hits = repo.grep(pattern, false, false, pathspec.as_deref(), fetch)?;
    let low = if start > 0 { start } else { 1 };
    let high = if end > 0 { end } else { 1_000_000_000 };
    let hits: Vec<Hit> = if scoped {
        raw_hits
            .iter()
            .filter(|hit| low <= hit.1 && hit.1 <= high)
            .cloned()
            .collect()
    } else {
        raw_hits.clone()
    };
    let capped = raw_hits.len() >= fetch;
    let shown = &hits[..hits.len().min(limit)];
    let mut place = glob.map(|glob| format!(" in {glob}")).unwrap_or_default();
    if scoped {
        let last = if end > 0 { end.to_string() } else { "end".to_string() };
        place.push_str(&format!(" lines {low}-{last}"));
    }
    let note = if capped {
        " (more not shown)".to_string()
    } else if shown.len() < hits.len() {
        format!(" ({} shown)", shown.len())
    } else {
        String::new()
    };
    Ok((
        format!("grep {}{place} -> {} matches{note}", py::repr(pattern), hits.len()),
        hits_text(shown),
    ))
}

/// What one round served: the (title, block) pairs, how many requests were served and how many rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub blocks: Vec<(String, String)>,
    pub served: usize,
    pub rejected: usize,
}

/// Answer a round of requests within the round's size limit.
pub fn serve_blocks(repo: &Repo<'_>, requests: &Value, limit: usize, tests: Option<&dyn TestSession>) -> Served {
    let mut out = Served {
        blocks: Vec::new(),
        served: 0,
        rejected: 0,
    };
    let empty = Vec::new();
    let items = requests.as_array().unwrap_or(&empty);
    let mut size = 0;
    for request in items.iter().take(MAX_REQUESTS) {
        let (title, body) = match serve_request(repo, request, tests) {
            Ok(answer) => {
                out.served += 1;
                answer
            }
            Err(Rejected(message)) => {
                out.rejected += 1;
                let shown: String = dumps(request, None).chars().take(160).collect();
                (format!("rejected {shown}"), message)
            }
        };
        let mut block = format!("## {title}\n{body}");
        let length = block.chars().count();
        if size + length > limit {
            block = block.chars().take(limit.saturating_sub(size)).collect::<String>()
                + "\n... cut: the round's size limit is reached; ask for less at a time";
        }
        size += block.chars().count();
        out.blocks.push((title, block));
        if size >= limit {
            break;
        }
    }
    if items.len() > MAX_REQUESTS {
        out.blocks.push((
            "not served".to_string(),
            format!(
                "## not served\n{} requests beyond the {MAX_REQUESTS} allowed per round",
                items.len() - MAX_REQUESTS
            ),
        ));
    }
    out
}

/// Answer a round of requests within the round's size limit: (text, served, rejected).
pub fn serve(
    repo: &Repo<'_>,
    requests: &Value,
    limit: usize,
    tests: Option<&dyn TestSession>,
) -> (String, usize, usize) {
    let round = serve_blocks(repo, requests, limit, tests);
    let text = round
        .blocks
        .iter()
        .map(|(_, block)| block.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (text, round.served, round.rejected)
}

// --- The glue: the ported reference and test session behind this module's traits ---------------------------------

impl structural::Repo for Repo<'_> {
    fn files(&self) -> Vec<String> {
        Repo::files(self).to_vec()
    }

    fn lines(&self, path: &str) -> Option<Vec<String>> {
        Repo::lines(self, path).map(|lines| lines.as_ref().clone())
    }

    fn grep(&self, pattern: &str, limit: usize) -> Result<Vec<(String, u64, String)>, String> {
        let hits = Repo::grep(self, pattern, false, false, None, limit).map_err(|error| error.0)?;
        Ok(hits
            .into_iter()
            .filter_map(|(path, line, body)| Some((path, u64::try_from(line).ok()?, body)))
            .collect())
    }
}

/// The structural reference of the change ([`crate::reference::Reference`]) as the deep pass holds it: built from
/// the head commit through a [`Repo::reader`] of the deep pass's repository (so both share one cache, as the
/// Python's reference read through the same `Repo`), and the base commit through a repository of its own.
pub struct Structural<'r> {
    inner: Mutex<structural::Reference<'r>>,
}

impl<'r> Structural<'r> {
    /// `Reference(repo, files, base_repo=Repo(repo.root, base, repo._runner) if base else None).build()`.
    pub fn build(
        repo: &Repo<'r>,
        files: &[FileDiff],
        base: Option<&str>,
        python: &mut dyn PythonIndex,
        clock: &dyn Fn() -> f64,
    ) -> Self {
        let head: Arc<dyn structural::Repo + Send + 'r> = Arc::new(repo.reader());
        let base_repo = base
            .filter(|rev| !rev.is_empty())
            .map(|rev| Arc::new(repo.at(rev)) as Arc<dyn structural::Repo + Send + 'r>);
        let reference = structural::Reference::shared(head, files, base_repo).build(python, clock);
        Self {
            inner: Mutex::new(reference),
        }
    }

    /// The reference itself, for what the deep pass's view does not cover.
    pub fn with<T>(&self, f: impl FnOnce(&mut structural::Reference<'r>) -> T) -> T {
        f(&mut lock(&self.inner))
    }
}

/// `dict(reference.stats)`: Python's keys; `python_unindexed` only when files went unindexed (Python always had
/// its own `ast`).
fn stats_map(stats: &structural::Stats) -> Map<String, Value> {
    let mut map = match serde_json::to_value(stats) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    if stats.python_unindexed == 0 {
        map.remove("python_unindexed");
    }
    map
}

impl Reference for Structural<'_> {
    fn serve(&self, kind: &str, args: &Map<String, Value>) -> Option<(String, String)> {
        structural::serve(&lock(&self.inner), kind, &Value::Object(args.clone()))
    }

    fn symbols_named(&self, name: &str) -> Vec<SymbolAt> {
        lock(&self.inner)
            .symbols_named(name)
            .into_iter()
            .map(|symbol| SymbolAt {
                key: symbol.key(),
                path: symbol.path.clone(),
                line: i64::try_from(symbol.line).unwrap_or(i64::MAX),
            })
            .collect()
    }

    fn symbol_view(&self, symbol: &SymbolAt) -> String {
        let reference = lock(&self.inner);
        let found = reference
            .index
            .values()
            .flat_map(|index| &index.symbols)
            .find(|item| item.key() == symbol.key && i64::try_from(item.line).ok() == Some(symbol.line));
        found.map_or_else(String::new, |item| reference.symbol_view(item, true))
    }

    fn callers(&self, name: &str, outside_changes: bool) -> Vec<CallAt> {
        lock(&self.inner)
            .callers(name, outside_changes)
            .into_iter()
            .map(|call| CallAt {
                path: call.path.clone(),
                line: i64::try_from(call.line).unwrap_or(i64::MAX),
                text: call.text.clone(),
                enclosing: call.enclosing.clone(),
            })
            .collect()
    }

    fn indexed(&self, path: &str) -> bool {
        lock(&self.inner).index.contains_key(path)
    }

    fn block(&self) -> String {
        lock(&self.inner).block(structural::BLOCK_CHARS)
    }

    fn stats(&self) -> Map<String, Value> {
        stats_map(&lock(&self.inner).stats)
    }

    fn facts(&self) -> Vec<Value> {
        lock(&self.inner).facts()
    }

    fn structural_findings(&self) -> Vec<Value> {
        lock(&self.inner).structural_findings()
    }
}

/// The review's test session ([`review_tests::TestSession`]) behind [`TestSession`]: records as JSON objects,
/// refusals as their messages. An export or process failure (Python's `RuntimeError`/`OSError`, which propagated
/// out of `serve_request`) comes back as a message too: "the test session failed (<Type>: <message>)".
impl TestSession for RefCell<review_tests::TestSession> {
    fn run(&self, paths: Option<&Value>, select: Option<&Value>) -> Result<Value, String> {
        let mut args = Map::new();
        if let Some(paths) = paths {
            args.insert("paths".into(), paths.clone());
        }
        if let Some(select) = select {
            args.insert("select".into(), select.clone());
        }
        let record = self
            .borrow_mut()
            .run_tests(&Value::Object(args))
            .map_err(session_error)?;
        Ok(serde_json::to_value(record).unwrap_or(Value::Null))
    }

    fn mutation(
        &self,
        path: Option<&Value>,
        line: Option<&Value>,
        replacement: Option<&Value>,
        tests: Option<&Value>,
    ) -> Result<Value, String> {
        let record = self
            .borrow_mut()
            .mutation(path, line, replacement, tests)
            .map_err(session_error)?;
        Ok(serde_json::to_value(record).unwrap_or(Value::Null))
    }

    fn records(&self) -> Vec<Value> {
        self.borrow()
            .records
            .iter()
            .map(|record| serde_json::to_value(record).unwrap_or(Value::Null))
            .collect()
    }

    fn limit(&self) -> usize {
        self.borrow().limit
    }

    fn tracked(&self, path: &str) -> bool {
        self.borrow().tracked.contains(path)
    }

    fn plan(&self, paths: &[String]) -> bool {
        !self.borrow().plan(paths).is_empty()
    }

    fn compare(&self, paths: &[String]) -> Result<Value, String> {
        let compared = self.borrow_mut().compare(paths).map_err(session_error)?;
        Ok(serde_json::to_value(compared).unwrap_or(Value::Null))
    }
}

fn session_error(error: review_tests::TestsError) -> String {
    match error {
        review_tests::TestsError::Rejected(rejected) => rejected.0,
        other => format!("the test session failed ({}: {other})", other.python_type()),
    }
}
