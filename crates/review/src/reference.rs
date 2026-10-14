//! The structural reference of the code a change touches (`autoreview_reference.py`), built by the host before any
//! model runs: definitions with their line ranges and signatures, imports resolved to files of the repository, call
//! sites resolved to the definitions they reach, test functions and what they call, literal families, and the
//! GitHub Actions workflow and Terraform facts. The repository stays data (RLM): the model gets a bounded view with
//! ids ([`Reference::block`]), exact lookups the host serves on request ([`serve`]: `symbol`, `callers`, `callees`,
//! `tests_of`), and host-observed facts ([`Reference::facts`], [`Reference::structural_findings`]) that go through
//! the verifier like any other finding.
//!
//! Only what the change can reach is indexed, bounded in files and bytes: the changed files, the files that import
//! them, the modules they import, the test files that name the changed symbols, and the Terraform siblings of a
//! changed `.tf` file. The repository is read through [`Repo`] (`git ls-tree`, `git show`, `git grep` at the reviewed
//! commit; [`GitRepo`] over a [`Runner`]).
//!
//! TypeScript/JavaScript (`reference_ts`), YAML (`reference_yaml`), Terraform (`reference_tf`), the facts
//! (`reference_facts`) and everything here are Rust. Python files are indexed by Python's `ast`: the host runs the
//! kernel's interpreter once per build ([`ProcessPython`], `-I`, the embedded `reference_py_index.py`, the source's
//! visitor verbatim) and reads back their records; the batches go to the one process over a line protocol. Without
//! an interpreter the Python files are left out of the index, counted in `Stats::python_unindexed` and named in
//! [`Reference::notes`].
//!
//! Known differences from the source:
//! - `build` no longer sets `repo.reference` (Python attached the reference to the deep pass's `Repo` object); the
//!   deep pass keeps the [`Reference`] it built.
//! - Python's `_module_map` iterated a `set` of paths, so which file a colliding dotted name maps to was
//!   hash-order dependent; here the tracked files are taken in `git ls-tree` order (the first wins, as `setdefault`).
//! - The base versions of changed Python files are indexed in the first interpreter request, with the changed files
//!   (the source indexed them last); the result is the same.
//! - `exports` and Terraform `refs` are sorted sets (Python sets, used for membership only).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::ops::Deref;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::FileDiff;
use crate::py;
use crate::reference_facts::{terraform_facts, workflow_facts};
use crate::reference_tf::{TfBlock, parse_terraform};
use crate::reference_ts::index_ts;
use crate::reference_yaml::{Yaml, YamlNode, parse_yaml};
use crate::scope::{GIT_TIMEOUT_S, Runner, join, normpath};

/// Bounds of the index: files read, bytes parsed, call sites and families kept.
pub const MAX_FILES: usize = 80;
pub const MAX_BYTES: usize = 3_000_000;
pub const MAX_IMPORTERS: usize = 30;
pub const MAX_TEST_FILES: usize = 20;
pub const MAX_CALLS_PER_FILE: usize = 2_000;
pub const MAX_FAMILIES_PER_FILE: usize = 60;
/// Bounds of what is shown.
pub const BLOCK_CHARS: usize = 12_000;
pub const MAX_CALLERS_SHOWN: usize = 8;
pub const MAX_TESTS_SHOWN: usize = 5;
pub const MAX_BODY_LINES: u64 = 60;
/// A literal family is indexed from two members; drift is reported from three.
pub const MIN_FAMILY_ITEMS: usize = 2;
pub const MIN_FAMILY_REPORTED: usize = 3;
pub const MAX_LINE_CHARS: usize = 160;
/// The deep pass's `Repo` bounds: a file larger than this (in characters) is not read; grep hits by default.
pub const MAX_FILE_BYTES: usize = 2_000_000;
/// Hang guard of one Python indexer request.
pub const PYTHON_TIMEOUT_S: u64 = 120;

/// The Python indexer the host runs (`reference_py_index.py`).
pub const PY_INDEX_SCRIPT: &str = include_str!("reference_py_index.py");

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

pub(crate) static PY_FILE: LazyLock<Regex> = LazyLock::new(|| regex(r"\.py$"));
pub(crate) static TS_FILE: LazyLock<Regex> = LazyLock::new(|| regex(r"\.(ts|tsx|js|jsx|mjs|cjs|mts|cts)$"));
pub(crate) static WORKFLOW: LazyLock<Regex> = LazyLock::new(|| regex(r"(^|/)\.github/workflows/[^/]+\.ya?ml$"));
pub(crate) static TF_FILE: LazyLock<Regex> = LazyLock::new(|| regex(r"\.tf$"));
pub(crate) static TEST_PATH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$"));
static IDENT: LazyLock<Regex> = LazyLock::new(|| regex(r"^[A-Za-z_][A-Za-z0-9_]*$"));

/// `_clip`: stripped, and cut to `limit` characters with an ellipsis.
pub fn clip(text: &str, limit: usize) -> String {
    py::clip(py::strip(text), limit)
}

/// `str.isupper()`: some cased character, and no lowercase one.
pub fn is_upper(text: &str) -> bool {
    text.chars().any(char::is_uppercase) && !text.chars().any(char::is_lowercase)
}

/// `re.escape` (Python 3.7+): the pattern sent to `git grep -E` is the source's, character for character.
pub fn py_re_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{0b}\u{0c}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `posixpath.dirname`.
pub fn dirname(path: &str) -> String {
    match path.rfind('/') {
        None => String::new(),
        Some(index) => {
            let head = &path[..=index];
            if head.chars().all(|c| c == '/') {
                head.to_string()
            } else {
                head.trim_end_matches('/').to_string()
            }
        }
    }
}

/// `posixpath.basename`.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `posixpath.splitext(name)[0]` of a base name.
fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if name[..dot].chars().any(|c| c != '.') => &name[..dot],
        _ => name,
    }
}

// --- Data -------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub path: String,
    pub name: String,
    pub qualname: String,
    /// function | method | class | constant | variable | interface | type | enum
    pub kind: String,
    pub line: u64,
    pub end: u64,
    pub signature: String,
    pub parent: Option<String>,
    pub bases: Vec<String>,
    pub decorators: Vec<String>,
    pub doc: String,
    pub exported: bool,
}

impl Symbol {
    pub fn key(&self) -> String {
        format!("{}::{}", self.path, self.qualname)
    }

    fn defines(&self) -> bool {
        matches!(self.kind.as_str(), "function" | "method" | "class")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub path: String,
    pub line: u64,
    pub callee: String,
    pub text: String,
    pub enclosing: Option<String>,
    /// Keys of the definitions the call resolves to (empty: unresolved, matched by name only).
    pub targets: Vec<String>,
}

/// A literal list, tuple, set or array of strings: the registries and member lists changes drift on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Family {
    pub path: String,
    pub line: u64,
    pub name: Option<String>,
    pub items: Vec<String>,
    pub enclosing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIndex {
    pub path: String,
    pub lang: String,
    pub symbols: Vec<Symbol>,
    /// Local name -> (module path in the repository or None, imported name or `*` for a module).
    pub imports: IndexMap<String, (Option<String>, String)>,
    pub calls: Vec<CallSite>,
    pub families: Vec<Family>,
    /// Names the file exports (TS/JS), or every top-level name (Python).
    pub exports: BTreeSet<String>,
    pub parsed: bool,
}

impl FileIndex {
    pub fn new(path: &str, lang: &str) -> Self {
        Self {
            path: path.to_string(),
            lang: lang.to_string(),
            symbols: Vec::new(),
            imports: IndexMap::new(),
            calls: Vec::new(),
            families: Vec::new(),
            exports: BTreeSet::new(),
            parsed: true,
        }
    }
}

// --- The repository ---------------------------------------------------------------------------------------------

/// A repository at one commit, read-only. Shared between threads (the deep pass serves lookups through the same
/// reference), so implementations are `Sync`.
pub trait Repo: Sync {
    /// Tracked paths.
    fn files(&self) -> Vec<String>;
    /// The lines of a tracked text file; `None` when it is binary, too large or unreadable.
    fn lines(&self, path: &str) -> Option<Vec<String>>;
    /// `(path, line, text)` of lines matching a POSIX extended `pattern`, at most `limit`; `Err` when grep failed.
    fn grep(&self, pattern: &str, limit: usize) -> Result<Vec<(String, u64, String)>, String>;
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a [`Reference`] holds its repository: borrowed for its lifetime, or shared (the deep pass keeps the
/// reference beside the repository it reads, so it cannot borrow it).
#[derive(Clone)]
pub enum RepoHandle<'a> {
    Borrowed(&'a dyn Repo),
    Shared(Arc<dyn Repo + Send + 'a>),
}

impl<'a> Deref for RepoHandle<'a> {
    type Target = dyn Repo + 'a;

    fn deref(&self) -> &Self::Target {
        match self {
            RepoHandle::Borrowed(repo) => *repo,
            RepoHandle::Shared(repo) => &**repo,
        }
    }
}

/// [`Repo`] through git (`ls-tree`, `show`, `grep` at `rev`), as the deep pass's `Repo`; reads are cached.
pub struct GitRepo<'r> {
    pub root: String,
    pub rev: String,
    runner: &'r (dyn Runner + Sync),
    files: Mutex<Option<Vec<String>>>,
    cache: Mutex<HashMap<String, Option<Vec<String>>>>,
}

impl<'r> GitRepo<'r> {
    pub fn new(root: &str, rev: &str, runner: &'r (dyn Runner + Sync)) -> Self {
        Self {
            root: root.to_string(),
            rev: rev.to_string(),
            runner,
            files: Mutex::new(None),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn git(&self, args: &[&str]) -> crate::scope::Output {
        let argv: Vec<String> = std::iter::once("git")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        self.runner.run(&argv, &self.root, GIT_TIMEOUT_S)
    }
}

impl Repo for GitRepo<'_> {
    fn files(&self) -> Vec<String> {
        if let Some(files) = locked(&self.files).as_ref() {
            return files.clone();
        }
        let output = self.git(&["ls-tree", "-r", "--name-only", "-z", &self.rev]);
        let files: Vec<String> = if output.code == 0 {
            output
                .stdout
                .split('\0')
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };
        *locked(&self.files) = Some(files.clone());
        files
    }

    fn lines(&self, path: &str) -> Option<Vec<String>> {
        if let Some(cached) = locked(&self.cache).get(path) {
            return cached.clone();
        }
        let output = self.git(&["show", &format!("{}:{path}", self.rev)]);
        let head: String = output.stdout.chars().take(8192).collect();
        let ok = output.code == 0 && !head.contains('\0') && output.stdout.chars().count() <= MAX_FILE_BYTES;
        let lines = ok.then(|| py::splitlines(&output.stdout).into_iter().map(String::from).collect());
        locked(&self.cache).insert(path.to_string(), lines.clone());
        lines
    }

    fn grep(&self, pattern: &str, limit: usize) -> Result<Vec<(String, u64, String)>, String> {
        let output = self.git(&[
            "grep",
            "-n",
            "-I",
            "--no-color",
            "-E",
            "-e",
            pattern,
            &self.rev,
            "--",
            ".",
        ]);
        if output.code != 0 && output.code != 1 {
            return Err(format!("grep failed: {}", clip(&output.stderr, 120)));
        }
        let prefix = format!("{}:", self.rev);
        let mut hits = Vec::new();
        for raw in py::splitlines(&output.stdout) {
            let raw = raw.strip_prefix(&prefix).unwrap_or(raw);
            let (path, rest) = raw.split_once(':').unwrap_or((raw, ""));
            let (number, text) = rest.split_once(':').unwrap_or((rest, ""));
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                hits.push((path.to_string(), number.parse().unwrap_or(0), text.to_string()));
            }
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }
}

// --- The Python indexer -----------------------------------------------------------------------------------------

/// Indexes Python files: `Some` records (one per file, in order) or `None` when no interpreter answered.
pub trait PythonIndex {
    fn index(&mut self, files: &[(String, String)]) -> Option<Vec<FileIndex>>;
}

/// No interpreter: every Python file is left out.
pub struct NoPython;

impl PythonIndex for NoPython {
    fn index(&mut self, _files: &[(String, String)]) -> Option<Vec<FileIndex>> {
        None
    }
}

struct Running {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<String>,
}

/// The kernel's interpreter running `reference_py_index.py` (`-I`), started on the first request and kept for the
/// build's later ones; a failed or silent interpreter (past [`PYTHON_TIMEOUT_S`]) is killed and not asked again.
pub struct ProcessPython {
    pub program: String,
    running: Option<Running>,
    broken: bool,
}

impl ProcessPython {
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            running: None,
            broken: false,
        }
    }

    /// `ULTRON_PYTHON`, else `/usr/bin/python3`.
    pub fn from_env() -> Self {
        Self::new(&std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".to_string()))
    }

    fn start(&mut self) -> Option<&mut Running> {
        if self.running.is_none() && !self.broken {
            let spawned = Command::new(&self.program)
                .args(["-I", "-c", PY_INDEX_SCRIPT])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = spawned else {
                self.broken = true;
                return None;
            };
            let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                let _ = child.kill();
                let _ = child.wait();
                self.broken = true;
                return None;
            };
            let (send, replies) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if send.send(line).is_err() {
                        break;
                    }
                }
            });
            self.running = Some(Running { child, stdin, replies });
        }
        self.running.as_mut()
    }

    fn stop(&mut self) {
        if let Some(mut running) = self.running.take() {
            drop(running.stdin);
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
}

impl PythonIndex for ProcessPython {
    fn index(&mut self, files: &[(String, String)]) -> Option<Vec<FileIndex>> {
        let request = serde_json::to_string(files).ok()?;
        let running = self.start()?;
        let sent = running
            .stdin
            .write_all(request.as_bytes())
            .and_then(|()| running.stdin.write_all(b"\n"))
            .and_then(|()| running.stdin.flush());
        let reply = match sent {
            Ok(()) => running.replies.recv_timeout(Duration::from_secs(PYTHON_TIMEOUT_S)),
            Err(_) => Err(RecvTimeoutError::Disconnected),
        };
        let records = reply
            .ok()
            .and_then(|line| serde_json::from_str::<Option<Vec<FileIndex>>>(&line).ok())
            .flatten()
            .filter(|records| records.len() == files.len());
        if records.is_none() {
            self.broken = true;
            self.stop();
        }
        records
    }
}

impl Drop for ProcessPython {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- The reference ----------------------------------------------------------------------------------------------

/// What a build read and kept (`Reference.stats`), plus the Python files no interpreter indexed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub files: usize,
    pub bytes: usize,
    pub symbols: usize,
    pub calls: usize,
    pub families: usize,
    pub workflows: usize,
    pub terraform: usize,
    pub ms: i64,
    pub skipped: usize,
    pub python_unindexed: usize,
}

/// The index and its views. `build` reads what the change can reach; the queries never touch the repository again
/// except through the [`Repo`] given (bodies, citations, workflow siblings).
pub struct Reference<'a> {
    pub repo: RepoHandle<'a>,
    pub base_repo: Option<RepoHandle<'a>>,
    /// The changed files (binary ones dropped).
    pub files: Vec<FileDiff>,
    pub max_files: usize,
    pub max_bytes: usize,
    pub index: IndexMap<String, FileIndex>,
    pub workflows: IndexMap<String, YamlNode>,
    pub terraform: IndexMap<String, Vec<TfBlock>>,
    pub changed_lines: IndexMap<String, BTreeSet<u64>>,
    pub removed_text: IndexMap<String, Vec<String>>,
    pub base_index: IndexMap<String, FileIndex>,
    pub stats: Stats,
    /// What the build could not do (no Python interpreter).
    pub notes: Vec<String>,
    modules: HashMap<String, String>,
    tracked_paths: HashSet<String>,
    ids: IndexMap<String, String>,
    facts: Option<Vec<Value>>,
    pending: Vec<(String, String)>,
    pending_base: Vec<(String, String)>,
    unindexed: Vec<String>,
}

/// `[(path, line, how)]` of the families sharing most of one's items.
pub type FamilyMatch<'f> = (&'f Family, String);

impl<'a> Reference<'a> {
    pub fn new(repo: &'a dyn Repo, files: &[FileDiff], base_repo: Option<&'a dyn Repo>) -> Self {
        Self::with(RepoHandle::Borrowed(repo), files, base_repo.map(RepoHandle::Borrowed))
    }

    /// A reference that holds its repositories (the deep pass's: it lives beside the repository it reads).
    pub fn shared(
        repo: Arc<dyn Repo + Send + 'a>,
        files: &[FileDiff],
        base_repo: Option<Arc<dyn Repo + Send + 'a>>,
    ) -> Self {
        Self::with(RepoHandle::Shared(repo), files, base_repo.map(RepoHandle::Shared))
    }

    fn with(repo: RepoHandle<'a>, files: &[FileDiff], base_repo: Option<RepoHandle<'a>>) -> Self {
        Self {
            repo,
            base_repo,
            files: files.iter().filter(|item| !item.binary).cloned().collect(),
            max_files: MAX_FILES,
            max_bytes: MAX_BYTES,
            index: IndexMap::new(),
            workflows: IndexMap::new(),
            terraform: IndexMap::new(),
            changed_lines: IndexMap::new(),
            removed_text: IndexMap::new(),
            base_index: IndexMap::new(),
            stats: Stats::default(),
            notes: Vec::new(),
            modules: HashMap::new(),
            tracked_paths: HashSet::new(),
            ids: IndexMap::new(),
            facts: None,
            pending: Vec::new(),
            pending_base: Vec::new(),
            unindexed: Vec::new(),
        }
    }

    // -- building

    /// Read and index what the change can reach. `clock` gives seconds (monotonic) for `stats.ms`.
    pub fn build(mut self, python: &mut dyn PythonIndex, clock: &dyn Fn() -> f64) -> Self {
        let began = clock();
        let listed = self.repo.files();
        let tracked: HashSet<String> = listed.iter().cloned().collect();
        let changed: Vec<String> = self
            .files
            .iter()
            .filter(|item| tracked.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        for item in &self.files {
            let lines = item.hunks.iter().flat_map(|hunk| &hunk.lines);
            let added = lines
                .clone()
                .filter(|line| line.kind == '+')
                .filter_map(|line| line.new)
                .filter(|n| *n != 0);
            self.changed_lines.insert(item.path.clone(), added.collect());
            let removed = lines
                .filter(|line| line.kind == '-')
                .map(|line| line.text.clone())
                .collect();
            self.removed_text.insert(item.path.clone(), removed);
        }
        self.modules = module_map(&listed);
        self.tracked_paths = tracked.clone();
        let mut wanted = changed.clone();
        wanted.extend(self.importers(&changed, &tracked));
        for path in &changed {
            self.read(path);
        }
        // The base versions of changed code files, for signature changes (Python ones join the first request).
        if let Some(base) = self.base_repo.clone() {
            for path in &changed {
                let python_file = PY_FILE.is_match(path);
                if !python_file && !TS_FILE.is_match(path) {
                    continue;
                }
                let Some(lines) = base.lines(path).filter(|lines| !lines.is_empty()) else {
                    continue;
                };
                let text = lines.join("\n");
                if python_file {
                    self.base_index.insert(path.clone(), FileIndex::new(path, "python"));
                    self.pending_base.push((path.clone(), text));
                } else {
                    self.base_index.insert(path.clone(), index_ts(path, &text));
                }
            }
        }
        self.flush_python(python);
        for path in wanted.clone() {
            if let Some(index) = self.index.get(&path) {
                for module_path in self.imported_paths(index) {
                    if !wanted.contains(&module_path) {
                        wanted.push(module_path);
                    }
                }
            }
        }
        wanted.extend(self.test_files(&changed, &tracked));
        // Terraform references cross files of one directory: the siblings of a changed .tf file are read too.
        let mut sorted: Vec<&String> = tracked.iter().collect();
        sorted.sort();
        for path in self.changed_lines.keys() {
            if TF_FILE.is_match(path) {
                let directory = dirname(path);
                let siblings: Vec<String> = sorted
                    .iter()
                    .filter(|other| TF_FILE.is_match(other) && dirname(other) == directory && !wanted.contains(other))
                    .take(20)
                    .map(|other| other.to_string())
                    .collect();
                wanted.extend(siblings);
            }
        }
        for path in &wanted {
            if self.index.len() + self.workflows.len() + self.terraform.len() >= self.max_files
                || self.stats.bytes >= self.max_bytes
            {
                self.stats.skipped += 1;
                continue;
            }
            self.read(path);
        }
        self.flush_python(python);
        // Imports resolve to paths once every file is read.
        let resolutions: Vec<(String, String, Option<String>, String)> = self
            .index
            .values()
            .flat_map(|index| {
                index
                    .imports
                    .iter()
                    .filter(|(_, (module_path, _))| module_path.is_none())
                    .map(|(local, (_, name))| {
                        let leaf = name.rsplit(':').next().unwrap_or("").to_string();
                        (
                            index.path.clone(),
                            local.clone(),
                            self.resolve_module(name, &index.path),
                            leaf,
                        )
                    })
            })
            .collect();
        for (path, local, module_path, leaf) in resolutions {
            if let Some(index) = self.index.get_mut(&path) {
                index.imports.insert(local, (module_path, leaf));
            }
        }
        let paths: Vec<String> = self.index.keys().cloned().collect();
        for path in paths {
            self.resolve_calls(&path);
        }
        let stats = &mut self.stats;
        stats.files = self.index.len() + self.workflows.len() + self.terraform.len();
        stats.symbols = self.index.values().map(|index| index.symbols.len()).sum();
        stats.calls = self.index.values().map(|index| index.calls.len()).sum();
        stats.families = self.index.values().map(|index| index.families.len()).sum();
        stats.workflows = self.workflows.len();
        stats.terraform = self.terraform.len();
        stats.ms = ((clock() - began) * 1000.0) as i64;
        stats.python_unindexed = self.unindexed.len();
        if !self.unindexed.is_empty() {
            self.notes.push(format!(
                "no Python interpreter answered: {} Python file(s) not indexed ({})",
                self.unindexed.len(),
                self.unindexed.join(", ")
            ));
        }
        self
    }

    fn read(&mut self, path: &str) {
        if self.index.contains_key(path)
            || self.workflows.contains_key(path)
            || self.terraform.contains_key(path)
            || self.unindexed.iter().any(|done| done == path)
        {
            return;
        }
        let Some(lines) = self.repo.lines(path) else {
            return;
        };
        let text = lines.join("\n");
        self.stats.bytes += text.chars().count();
        if PY_FILE.is_match(path) {
            self.index.insert(path.to_string(), FileIndex::new(path, "python"));
            self.pending.push((path.to_string(), text));
        } else if TS_FILE.is_match(path) {
            self.index.insert(path.to_string(), index_ts(path, &text));
        } else if WORKFLOW.is_match(path) {
            match parse_yaml(&text) {
                Ok(Some(node)) if matches!(node.value, Yaml::Map(_)) => {
                    self.workflows.insert(path.to_string(), node);
                }
                Ok(_) => {}
                Err(_) => self.stats.skipped += 1, // one unreadable file never costs the others
            }
        } else if TF_FILE.is_match(path) {
            self.terraform.insert(path.to_string(), parse_terraform(path, &text));
        }
    }

    /// Index the Python files read since the last request, in one request to the interpreter.
    fn flush_python(&mut self, python: &mut dyn PythonIndex) {
        if self.pending.is_empty() && self.pending_base.is_empty() {
            return;
        }
        let heads = std::mem::take(&mut self.pending);
        let bases = std::mem::take(&mut self.pending_base);
        let request: Vec<(String, String)> = heads.iter().chain(&bases).cloned().collect();
        match python.index(&request) {
            Some(mut records) => {
                let base_records = records.split_off(heads.len());
                for record in records {
                    self.index.insert(record.path.clone(), record);
                }
                for record in base_records {
                    self.base_index.insert(record.path.clone(), record);
                }
            }
            None => {
                for (path, _) in &heads {
                    self.index.shift_remove(path);
                }
                for (path, _) in &bases {
                    self.base_index.shift_remove(path);
                }
                self.unindexed.extend(heads.into_iter().map(|(path, _)| path));
            }
        }
    }

    /// Files that import a changed module, by a grep of its module name or relative path.
    fn importers(&self, changed: &[String], tracked: &HashSet<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for path in changed {
            if !(PY_FILE.is_match(path) || TS_FILE.is_match(path)) {
                continue;
            }
            let mut stem = stem_of(basename(path)).to_string();
            if stem == "__init__" || stem == "index" {
                stem = basename(&dirname(path)).to_string();
            }
            if stem.is_empty() || !IDENT.is_match(&stem.replace('-', "_")) {
                continue;
            }
            // `git grep -E` is POSIX: no \s, \w or \b.
            let name = py_re_escape(&stem);
            let pattern = format!(
                "(from|import)[ \\t]+[A-Za-z0-9_.]*{name}([^A-Za-z0-9_]|$)|require\\(['\"][^'\"]*{name}['\"]|from[ \\t]+['\"][^'\"]*{name}(\\.[a-z]+)?['\"]"
            );
            let Ok(hits) = self.repo.grep(&pattern, MAX_IMPORTERS * 2) else {
                continue;
            };
            for (hit_path, _, _) in hits {
                if &hit_path != path
                    && tracked.contains(&hit_path)
                    && (PY_FILE.is_match(&hit_path) || TS_FILE.is_match(&hit_path))
                    && !out.contains(&hit_path)
                {
                    out.push(hit_path);
                }
                if out.len() >= MAX_IMPORTERS {
                    break;
                }
            }
        }
        out
    }

    fn test_files(&self, changed: &[String], tracked: &HashSet<String>) -> Vec<String> {
        let names: Vec<&str> = changed
            .iter()
            .filter_map(|path| self.index.get(path))
            .flat_map(|index| &index.symbols)
            .filter(|symbol| symbol.defines() && symbol.exported)
            .map(|symbol| symbol.name.as_str())
            .take(12)
            .collect();
        if names.is_empty() {
            return Vec::new();
        }
        let pattern = names
            .iter()
            .map(|name| py_re_escape(name))
            .collect::<Vec<_>>()
            .join("|");
        let Ok(hits) = self
            .repo
            .grep(&format!("(^|[^A-Za-z0-9_])({pattern})([^A-Za-z0-9_]|$)"), 200)
        else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for (hit_path, _, _) in hits {
            if TEST_PATH.is_match(&hit_path)
                && tracked.contains(&hit_path)
                && !out.contains(&hit_path)
                && (PY_FILE.is_match(&hit_path) || TS_FILE.is_match(&hit_path))
            {
                out.push(hit_path);
            }
            if out.len() >= MAX_TEST_FILES {
                break;
            }
        }
        out
    }

    fn imported_paths(&self, index: &FileIndex) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (module_path, name) in index.imports.values() {
            let resolved = module_path.clone().or_else(|| self.resolve_module(name, &index.path));
            if let Some(resolved) = resolved
                && !out.contains(&resolved)
            {
                out.push(resolved);
            }
        }
        out
    }

    /// The repository file a Python module (`pkg.mod`, `.rel:name`) or a TS specifier (`./x`) names.
    pub fn resolve_module(&self, spec: &str, from_path: &str) -> Option<String> {
        let known = |candidate: &str| self.tracked_paths.contains(candidate);
        let from_ts = self.index.get(from_path).is_some_and(|index| index.lang == "ts");
        if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') || from_ts {
            let spec = spec.split(':').next().unwrap_or("");
            if !spec.starts_with('.') {
                return None;
            }
            let base = normpath(&join(&dirname(from_path), spec));
            let candidates = [
                base.clone(),
                format!("{base}.ts"),
                format!("{base}.tsx"),
                format!("{base}.js"),
                format!("{base}.jsx"),
                format!("{base}.mjs"),
                format!("{base}/index.ts"),
                format!("{base}/index.tsx"),
                format!("{base}/index.js"),
            ];
            return candidates.into_iter().find(|candidate| known(candidate));
        }
        let (module, name) = spec.split_once(':').unwrap_or((spec, ""));
        if module.starts_with('.') {
            let rest = module.trim_start_matches('.');
            let dots = module.len() - rest.len();
            let mut directory = dirname(from_path);
            for _ in 1..dots {
                directory = dirname(&directory);
            }
            let base = if rest.is_empty() {
                directory
            } else {
                join(&directory, &rest.replace('.', "/"))
            };
            for candidate in [format!("{base}.py"), format!("{base}/__init__.py")] {
                if known(&candidate) {
                    return Some(candidate);
                }
            }
            // `from . import name`: the name may be a module.
            if !name.is_empty() && name != "*" {
                for candidate in [format!("{base}/{name}.py"), format!("{base}/{name}/__init__.py")] {
                    if known(&candidate) {
                        return Some(candidate);
                    }
                }
            }
            return None;
        }
        self.modules
            .get(module)
            .filter(|found| !found.is_empty())
            .or_else(|| {
                if name.is_empty() {
                    None
                } else {
                    self.modules.get(&format!("{module}.{name}"))
                }
            })
            .cloned()
    }

    fn resolve_calls(&mut self, path: &str) {
        let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        for other in self.index.values() {
            for symbol in other.symbols.iter().filter(|symbol| symbol.defines()) {
                by_name.entry(symbol.name.as_str()).or_default().push(symbol);
            }
        }
        let Some(index) = self.index.get(path) else {
            return;
        };
        let mut local: HashMap<&str, &Symbol> = HashMap::new();
        let mut methods: HashMap<&str, Vec<&Symbol>> = HashMap::new();
        for symbol in &index.symbols {
            if matches!(symbol.kind.as_str(), "function" | "class") {
                local.insert(symbol.name.as_str(), symbol);
            }
            if symbol.kind == "method" {
                methods.entry(symbol.name.as_str()).or_default().push(symbol);
            }
        }
        let symbols_of = |module_path: &Option<String>| -> Option<&Vec<Symbol>> {
            module_path
                .as_ref()
                .and_then(|module_path| self.index.get(module_path))
                .map(|index| &index.symbols)
        };
        let mut resolved: Vec<Vec<String>> = Vec::new();
        for call in &index.calls {
            let parts: Vec<&str> = call.callee.split('.').collect();
            let leaf = parts[parts.len() - 1];
            let mut targets: Vec<&Symbol> = Vec::new();
            if parts.len() == 1 {
                if let Some(symbol) = local.get(leaf) {
                    targets = vec![symbol];
                } else if let Some((module_path, name)) = index.imports.get(leaf)
                    && let Some(symbols) = symbols_of(module_path)
                {
                    targets = symbols
                        .iter()
                        .filter(|s| &s.name == name && matches!(s.kind.as_str(), "function" | "class"))
                        .collect();
                }
            } else {
                let head = parts[0];
                if matches!(head, "self" | "cls" | "this") && methods.contains_key(leaf) {
                    let own = call.enclosing.as_ref().map(|name| name.split('.').next().unwrap_or(""));
                    let all = &methods[leaf];
                    targets = all.iter().copied().filter(|s| s.parent.as_deref() == own).collect();
                    if targets.is_empty() {
                        targets = all.clone();
                    }
                } else if let Some((module_path, name)) = index.imports.get(head) {
                    if let Some(symbols) = symbols_of(module_path) {
                        targets = if name == "*" || name == "default" {
                            symbols.iter().filter(|s| s.name == leaf && s.defines()).collect()
                        } else {
                            symbols
                                .iter()
                                .filter(|s| s.parent.as_ref() == Some(name) && s.name == leaf)
                                .collect()
                        };
                    }
                } else if local.get(head).is_some_and(|symbol| symbol.kind == "class") {
                    targets = index
                        .symbols
                        .iter()
                        .filter(|s| s.parent.as_deref() == Some(head) && s.name == leaf)
                        .collect();
                }
            }
            if targets.is_empty()
                && parts.len() > 1
                && let Some(named) = by_name.get(leaf)
                && named.len() <= 3
            {
                targets = named.clone(); // a method of a known class, reached through an object of unknown type
            }
            resolved.push(targets.iter().take(5).map(|symbol| symbol.key()).collect());
        }
        if let Some(index) = self.index.get_mut(path) {
            for (call, targets) in index.calls.iter_mut().zip(resolved) {
                call.targets = targets;
            }
        }
    }

    // -- queries

    /// Definitions of `name` (a qualname or a leaf), the exact qualname first, then those in changed files.
    pub fn symbols_named(&self, name: &str) -> Vec<&Symbol> {
        let leaf = name.rsplit('.').next().unwrap_or(name);
        let mut out: Vec<&Symbol> = self
            .index
            .values()
            .flat_map(|index| &index.symbols)
            .filter(|symbol| symbol.qualname == name || symbol.name == leaf)
            .collect();
        out.sort_by(|a, b| {
            let key = |s: &Symbol| (s.qualname != name, !self.changed_lines.contains_key(&s.path));
            key(a)
                .cmp(&key(b))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        out
    }

    /// The innermost definition around a line.
    pub fn enclosing(&self, path: &str, line: u64) -> Option<&Symbol> {
        let index = self.index.get(path)?;
        let mut best: Option<&Symbol> = None;
        for symbol in &index.symbols {
            if symbol.defines()
                && symbol.line <= line
                && line <= symbol.end
                && best.is_none_or(|best| symbol.line >= best.line)
            {
                best = Some(symbol);
            }
        }
        best
    }

    /// Call sites that reach a definition of `name` (resolved), then those that name it without resolution.
    pub fn callers(&self, name: &str, outside_changes: bool) -> Vec<&CallSite> {
        let wanted: HashSet<String> = self.symbols_named(name).iter().map(|symbol| symbol.key()).collect();
        let leaf = name.rsplit('.').next().unwrap_or(name);
        let mut resolved = Vec::new();
        let mut by_name = Vec::new();
        for call in self.index.values().flat_map(|index| &index.calls) {
            if outside_changes
                && self
                    .changed_lines
                    .get(&call.path)
                    .is_some_and(|lines| lines.contains(&call.line))
            {
                continue;
            }
            if !wanted.is_empty() && call.targets.iter().any(|target| wanted.contains(target)) {
                resolved.push(call);
            } else if call.targets.is_empty() && call.callee.rsplit('.').next() == Some(leaf) {
                by_name.push(call);
            }
        }
        resolved.extend(by_name);
        resolved
    }

    /// The calls inside the first definition of `name`.
    pub fn callees(&self, name: &str) -> Vec<&CallSite> {
        for symbol in self.symbols_named(name) {
            let Some(index) = self.index.get(&symbol.path) else {
                continue;
            };
            return index
                .calls
                .iter()
                .filter(|call| symbol.line < call.line && call.line <= symbol.end)
                .collect();
        }
        Vec::new()
    }

    /// `(path, line, test name)` of test functions that call `name`, directly or through a resolved import.
    pub fn tests_of(&self, name: &str) -> Vec<(String, u64, String)> {
        let mut out: Vec<(String, u64, String)> = Vec::new();
        for call in self.callers(name, false) {
            if !TEST_PATH.is_match(&call.path) {
                continue;
            }
            let entry = (call.path.clone(), call.line, call.enclosing.clone().unwrap_or_default());
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        out
    }

    /// Methods of the same name in other classes of the index (overrides, or siblings in a family).
    pub fn overrides(&self, symbol: &Symbol) -> Vec<&Symbol> {
        if symbol.kind != "method" {
            return Vec::new();
        }
        let key = symbol.key();
        self.index
            .values()
            .flat_map(|index| &index.symbols)
            .filter(|other| other.kind == "method" && other.name == symbol.name && other.key() != key)
            .collect()
    }

    /// Families elsewhere that share most of this one's items: `(family, "same" | "missing 'a'" | "extra 'c'")`.
    pub fn family_matches(&self, family: &Family) -> Vec<FamilyMatch<'_>> {
        let mut out = Vec::new();
        let mine: BTreeSet<&String> = family.items.iter().collect();
        for other in self.index.values().flat_map(|index| &index.families) {
            if other.path == family.path && other.line == family.line {
                continue;
            }
            let theirs: BTreeSet<&String> = other.items.iter().collect();
            let shared = mine.intersection(&theirs).count();
            if shared < MIN_FAMILY_ITEMS || (shared as f64) < 0.6 * mine.len().min(theirs.len()) as f64 {
                continue;
            }
            let missing: Vec<&String> = mine.difference(&theirs).copied().collect();
            let extra: Vec<&String> = theirs.difference(&mine).copied().collect();
            if missing.is_empty() && extra.is_empty() {
                out.push((other, "same".to_string()));
                continue;
            }
            let listed = |items: &[&String]| {
                items
                    .iter()
                    .take(4)
                    .map(|item| py::repr(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mut parts = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("missing {}", listed(&missing)));
            }
            if !extra.is_empty() {
                parts.push(format!("extra {}", listed(&extra)));
            }
            out.push((other, parts.join("; ")));
        }
        out
    }

    /// Definitions that hold an added line.
    pub fn changed_symbols(&self) -> Vec<&Symbol> {
        let mut out = Vec::new();
        for (path, lines) in &self.changed_lines {
            let Some(index) = self.index.get(path) else {
                continue;
            };
            for symbol in &index.symbols {
                if symbol.defines() && lines.iter().any(|line| symbol.line <= *line && *line <= symbol.end) {
                    out.push(symbol);
                }
            }
        }
        out
    }

    /// The signature at the base commit: `None` without a base index of the file, `Some("")` when it is new.
    pub fn signature_before(&self, symbol: &Symbol) -> Option<String> {
        let base = self.base_index.get(&symbol.path)?;
        Some(
            base.symbols
                .iter()
                .find(|other| other.qualname == symbol.qualname)
                .map_or(String::new(), |other| other.signature.clone()),
        )
    }

    // -- views

    pub(crate) fn id(&mut self, prefix: &str, key: &str) -> String {
        if let Some(id) = self.ids.get(key) {
            return id.clone();
        }
        let number = self.ids.values().filter(|value| value.starts_with(prefix)).count() + 1;
        let id = format!("{prefix}{number}");
        self.ids.insert(key.to_string(), id.clone());
        id
    }

    /// One definition: where, signature (and what it was at the base commit), class, overrides, callers, tests;
    /// with `body`, its first [`MAX_BODY_LINES`] numbered lines.
    pub fn symbol_view(&self, symbol: &Symbol, body: bool) -> String {
        let change = match self.signature_before(symbol) {
            Some(before) if before.is_empty() => " [new in this change]".to_string(),
            Some(before) if before != symbol.signature => format!(" [was: {before}]"),
            _ => String::new(),
        };
        let callers = self.callers(&symbol.qualname, true);
        let tests = self.tests_of(&symbol.qualname);
        let overrides = self.overrides(symbol);
        let title = if symbol.signature.is_empty() {
            &symbol.name
        } else {
            &symbol.signature
        };
        let mut lines = vec![format!(
            "{}:{}-{} {title}{change}",
            symbol.path, symbol.line, symbol.end
        )];
        if !symbol.doc.is_empty() {
            lines.push(format!("  doc: {}", symbol.doc));
        }
        if let Some(parent) = &symbol.parent {
            lines.push(format!("  in class {parent}"));
        }
        if !overrides.is_empty() {
            let listed: Vec<String> = overrides
                .iter()
                .take(4)
                .map(|other| format!("{}:{} {}", other.path, other.line, other.qualname))
                .collect();
            lines.push(format!("  same-named methods: {}", listed.join("; ")));
        }
        let shown = &callers[..callers.len().min(MAX_CALLERS_SHOWN)];
        if callers.is_empty() {
            lines.push("  callers outside the change: none found in the indexed files".to_string());
        } else {
            let listed: Vec<String> = shown
                .iter()
                .map(|call| {
                    let inside = call
                        .enclosing
                        .as_ref()
                        .map_or(String::new(), |name| format!(" in {name}"));
                    format!("{}:{}{inside}: {}", call.path, call.line, clip(&call.text, 90))
                })
                .collect();
            let more = if callers.len() > shown.len() {
                format!("; ... {} more", callers.len() - shown.len())
            } else {
                String::new()
            };
            lines.push(format!(
                "  callers outside the change ({}): {}{more}",
                callers.len(),
                listed.join("; ")
            ));
        }
        if !tests.is_empty() {
            let listed: Vec<String> = tests
                .iter()
                .take(MAX_TESTS_SHOWN)
                .map(|(path, line, name)| {
                    if name.is_empty() {
                        format!("{path}:{line}")
                    } else {
                        format!("{path}:{line} {name}")
                    }
                })
                .collect();
            lines.push(format!("  tests calling it: {}", listed.join("; ")));
        }
        if body {
            let source = self.repo.lines(&symbol.path).unwrap_or_default();
            let end = symbol
                .end
                .min(symbol.line + MAX_BODY_LINES - 1)
                .min(source.len() as u64);
            let numbered: Vec<String> = (symbol.line..=end)
                .filter_map(|number| {
                    let text = source.get((number as usize).checked_sub(1)?)?;
                    Some(format!("{number:>5} | {}", clip(text, 300)))
                })
                .collect();
            lines.push(numbered.join("\n"));
            if end < symbol.end {
                lines.push(format!("      ... ({} more lines)", symbol.end - end));
            }
        }
        lines.join("\n")
    }

    /// The reference view the frames read: the changed definitions with their callers and tests, the enclosing
    /// definition of each hunk, and the facts; bounded to `limit` characters.
    pub fn block(&mut self, limit: usize) -> String {
        let mut sections: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut definitions: Vec<Symbol> = Vec::new();
        for symbol in self.changed_symbols().into_iter().take(12) {
            if seen.insert(symbol.key()) {
                definitions.push(symbol.clone());
            }
        }
        for item in &self.files {
            if !self.index.contains_key(&item.path) {
                continue;
            }
            for hunk in &item.hunks {
                if let Some(inner) = self.enclosing(&item.path, hunk.new_first())
                    && seen.insert(inner.key())
                {
                    definitions.push(inner.clone());
                }
            }
        }
        for symbol in &definitions {
            let id = self.id("D", &symbol.key());
            sections.push(format!("[{id}] {}", self.symbol_view(symbol, false)));
        }
        for fact in self.facts() {
            sections.push(format!("[{}] {}", py::str_of(&fact["id"]), py::str_of(&fact["text"])));
        }
        if sections.is_empty() {
            return String::new();
        }
        let mut text = String::from(
            "Reference, built by the host from the repository at the reviewed commit (definitions resolved, not \
             grepped; untrusted repository data). Ask {\"symbol\": {\"name\": ...}} for a definition with its body, \
             {\"callers\": {\"symbol\": ...}}, {\"callees\": {\"symbol\": ...}} and {\"tests_of\": {\"symbol\": ...}} for more.",
        );
        let mut length = text.chars().count();
        for section in sections {
            let size = section.chars().count();
            if length + size + 2 > limit {
                text.push_str("\n... (reference cut at its size limit)");
                break;
            }
            text.push_str("\n\n");
            text.push_str(&section);
            length += size + 2;
        }
        text
    }

    // -- facts

    /// Host-observed facts about the change, each `{id, kind, shape, path, line, ...}`: literal families the change
    /// touches that are listed elsewhere, signature changes with their callers, and the workflow and Terraform
    /// observations (with a `finding` and its `cite`). Computed once.
    pub fn facts(&mut self) -> Vec<Value> {
        if let Some(facts) = &self.facts {
            return facts.clone();
        }
        let mut facts: Vec<Value> = Vec::new();
        let mut pending: Vec<(String, Value, String)> = Vec::new(); // (prefix, fact without id, id key)
        for item in &self.files {
            let Some(index) = self.index.get(&item.path) else {
                continue;
            };
            let empty = BTreeSet::new();
            let lines = self.changed_lines.get(&item.path).unwrap_or(&empty);
            for family in &index.families {
                let span = family.line..family.line + family.items.len().max(1) as u64;
                let touched = span.into_iter().any(|line| lines.contains(&line)) || lines.contains(&family.line);
                if !touched || family.items.len() < MIN_FAMILY_REPORTED {
                    continue;
                }
                let matches = self.family_matches(family);
                if matches.is_empty() {
                    continue;
                }
                let label = family
                    .name
                    .as_ref()
                    .map_or("a literal list".to_string(), |name| format!("`{name}`"));
                let place = |other: &Family, how: &str| {
                    let named = other.name.as_ref().map_or(String::new(), |name| format!(" (`{name}`)"));
                    let how = if how == "same" {
                        String::new()
                    } else {
                        format!(": {how}")
                    };
                    format!("{}:{}{named}{how}", other.path, other.line)
                };
                let places: Vec<String> = matches.iter().take(6).map(|(other, how)| place(other, how)).collect();
                let differing = matches
                    .iter()
                    .any(|(other, how)| how != "same" && !self.changed_lines.contains_key(&other.path));
                let shown: Vec<String> = family.items.iter().take(6).map(|item| py::repr(item)).collect();
                let text = format!(
                    "The change edits the literal family {label} at {}:{} ({}{}); the same family is listed at {}{}",
                    family.path,
                    family.line,
                    shown.join(", "),
                    if family.items.len() > 6 { ", ..." } else { "" },
                    places.join("; "),
                    if differing {
                        ". A file not in this change lists it differently."
                    } else {
                        "."
                    }
                );
                let fact = serde_json::json!({
                    "kind": "family", "shape": "family", "path": family.path, "line": family.line,
                    "items": family.items,
                    "matches": matches.iter().map(|(other, how)| serde_json::json!([other.path, other.line, how])).collect::<Vec<_>>(),
                    "text": text,
                });
                pending.push(("F".to_string(), fact, format!("{}:{}", family.path, family.line)));
            }
        }
        for symbol in self.changed_symbols() {
            let Some(before) = self.signature_before(symbol) else {
                continue;
            };
            if before.is_empty() || before == symbol.signature {
                continue;
            }
            let callers = self.callers(&symbol.qualname, true);
            let tail = if callers.is_empty() {
                "no caller outside the change in the indexed files".to_string()
            } else {
                let listed: Vec<String> = callers
                    .iter()
                    .take(8)
                    .map(|call| format!("{}:{}", call.path, call.line))
                    .collect();
                format!("{} caller(s) outside the change: {}", callers.len(), listed.join("; "))
            };
            let text = format!(
                "The signature of {} changed: was `{before}`, now `{}`; {tail}",
                symbol.qualname, symbol.signature
            );
            let fact = serde_json::json!({
                "kind": "signature", "shape": "signature", "path": symbol.path, "line": symbol.line, "text": text,
            });
            pending.push(("F".to_string(), fact, format!("sig:{}", symbol.key())));
        }
        for (prefix, fact, key) in pending {
            facts.push(with_id(self.id(&prefix, &key), fact));
        }
        facts.extend(self.workflow_facts());
        facts.extend(terraform_facts(self));
        self.facts = Some(facts.clone());
        facts
    }

    /// The facts of the changed workflows.
    pub fn workflow_facts(&mut self) -> Vec<Value> {
        let paths: Vec<String> = self
            .workflows
            .keys()
            .filter(|path| self.changed_lines.contains_key(*path))
            .cloned()
            .collect();
        let mut out = Vec::new();
        for path in paths {
            out.extend(workflow_facts(self, &path));
        }
        out
    }

    /// The facts that are findings by themselves, in the deep pass's finding shape, each with citations the host
    /// checked (a quote at its line). They are candidates: the verifier judges them like any other.
    pub fn structural_findings(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        for fact in self.facts() {
            let Some(Value::Object(finding)) = fact.get("finding") else {
                continue;
            };
            if finding.is_empty() {
                continue;
            }
            let mut citations: Vec<Value> = Vec::new();
            for cite in fact.get("cite").and_then(Value::as_array).into_iter().flatten() {
                let (Some(path), Some(line)) = (cite[0].as_str(), cite[1].as_u64()) else {
                    continue;
                };
                let source = self.repo.lines(path).unwrap_or_default();
                if line >= 1 && line as usize <= source.len() {
                    let quote = py::strip(&source[line as usize - 1]).to_string();
                    citations.push(serde_json::json!({"path": path, "line": line, "quote": quote}));
                }
            }
            if citations.is_empty() {
                continue;
            }
            let fact_path = py::str_of(&fact["path"]);
            let beyond_diff = citations.iter().any(|cite| {
                let path = cite["path"].as_str().unwrap_or("");
                let line = cite["line"].as_u64().unwrap_or(0);
                path != fact_path || !self.changed_lines.get(path).is_some_and(|lines| lines.contains(&line))
            });
            let mut record = finding.clone();
            let confidence = finding.get("confidence").cloned().unwrap_or(serde_json::json!(0.7));
            record.insert("file".to_string(), fact["path"].clone());
            record.insert("line".to_string(), fact["line"].clone());
            record.insert("citations".to_string(), Value::Array(citations));
            record.insert(
                "shape".to_string(),
                fact.get("shape").cloned().unwrap_or(Value::String(String::new())),
            );
            record.insert("source".to_string(), Value::String("deep:structure".to_string()));
            record.insert("reviewers".to_string(), serde_json::json!(["deep:structure"]));
            record.insert("beyond_diff".to_string(), Value::Bool(beyond_diff));
            record.insert("confidence".to_string(), confidence);
            out.push(Value::Object(record));
        }
        out
    }
}

/// `{"id": id, **fact}`.
pub(crate) fn with_id(id: String, fact: Value) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("id".to_string(), Value::String(id));
    if let Value::Object(fields) = fact {
        out.extend(fields);
    }
    Value::Object(out)
}

/// Dotted module name -> path, for every Python file under every plausible source root (the first file wins a
/// collision), plus every path by itself for TS resolution.
pub fn module_map(tracked: &[String]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for path in tracked {
        out.insert(path.clone(), path.clone());
        if !PY_FILE.is_match(path) {
            continue;
        }
        let mut parts: Vec<&str> = path[..path.len() - 3].split('/').collect();
        if parts.last() == Some(&"__init__") {
            parts.pop();
        }
        for start in 0..parts.len() {
            let dotted = parts[start..].join(".");
            if !dotted.is_empty() {
                out.entry(dotted).or_insert_with(|| path.clone());
            }
        }
    }
    out
}

// --- Serving requests -------------------------------------------------------------------------------------------

/// Python truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Answer a `symbol`, `callers`, `callees` or `tests_of` request from the index: `(title, body)`, or `None` when the
/// index does not know the name (the caller falls back to a text search).
pub fn serve(reference: &Reference<'_>, kind: &str, args: &Value) -> Option<(String, String)> {
    let name = if kind == "symbol" {
        args.get("name")
            .filter(|name| truthy(name))
            .or_else(|| args.get("symbol"))
    } else {
        args.get("symbol")
    };
    let name = py::strip(name?.as_str()?);
    if name.is_empty() {
        return None;
    }
    match kind {
        "symbol" => {
            let symbols = reference.symbols_named(name);
            if symbols.is_empty() {
                return None;
            }
            let parts: Vec<String> = symbols
                .iter()
                .take(3)
                .map(|symbol| reference.symbol_view(symbol, true))
                .collect();
            let more = if symbols.len() > 3 {
                format!(
                    "\n... {} more definitions of {}",
                    symbols.len() - 3,
                    name.rsplit('.').next().unwrap_or(name)
                )
            } else {
                String::new()
            };
            Some((
                format!("symbol {name} -> {} definition(s)", symbols.len()),
                parts.join("\n\n") + &more,
            ))
        }
        "callers" => {
            let calls = reference.callers(name, false);
            if calls.is_empty() {
                return None; // a defined name nothing calls: the text search finds its uses
            }
            let shown = &calls[..calls.len().min(MAX_CALLERS_SHOWN * 3)];
            let body: Vec<String> = shown
                .iter()
                .map(|call| {
                    let inside = call
                        .enclosing
                        .as_ref()
                        .map_or(String::new(), |name| format!(" in {name}"));
                    let how = if call.targets.is_empty() {
                        " (by name)"
                    } else {
                        " (resolved)"
                    };
                    format!("{}:{}{inside}{how}: {}", call.path, call.line, clip(&call.text, 140))
                })
                .collect();
            let more = if calls.len() > shown.len() {
                " (more not shown)"
            } else {
                ""
            };
            Some((
                format!("callers {name} -> {} call sites{more}", calls.len()),
                body.join("\n"),
            ))
        }
        "callees" => {
            let symbols = reference.symbols_named(name);
            let first = symbols.first()?;
            let calls = reference.callees(name);
            let body: Vec<String> = calls
                .iter()
                .take(40)
                .map(|call| {
                    let targets = if call.targets.is_empty() {
                        " (unresolved)".to_string()
                    } else {
                        format!(
                            " -> {}",
                            call.targets.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                        )
                    };
                    format!("{}:{}: {}(...){targets}", call.path, call.line, call.callee)
                })
                .collect();
            let body = if body.is_empty() {
                "no calls inside it".to_string()
            } else {
                body.join("\n")
            };
            Some((format!("callees of {} -> {} calls", first.qualname, calls.len()), body))
        }
        "tests_of" => {
            if reference.symbols_named(name).is_empty() && reference.callers(name, false).is_empty() {
                return None;
            }
            let tests = reference.tests_of(name);
            let body: Vec<String> = tests
                .iter()
                .take(30)
                .map(|(path, line, test)| {
                    if test.is_empty() {
                        format!("{path}:{line}")
                    } else {
                        format!("{path}:{line} {test}")
                    }
                })
                .collect();
            let body = if body.is_empty() {
                "no test calls it in the indexed files".to_string()
            } else {
                body.join("\n")
            };
            Some((format!("tests_of {name} -> {} test(s)", tests.len()), body))
        }
        _ => None,
    }
}
