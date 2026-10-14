//! Running the reviewed project's tests for `ultron autoreview`, sandboxed: a port of
//! `packages/coding-agent/src/ultron/rlm/autoreview_tests.py`.
//!
//! The deep pass may run tests: once automatically for the test files the map tied to the change (`compare`), and on
//! an investigator's `run_tests` / `mutation_check` request (`TestSession::run_tests`, `TestSession::mutation_check`,
//! or `TestSession::serve` for the deep pass's reply text). Models still have no shell: these are two more closed
//! request types the host serves. Every execution happens in a disposable export of the commit (`git archive`), inside
//! a sandbox (bubblewrap, rootless `unshare`, or Docker with a configured image), under a wall-clock limit with capped
//! output, at most `runs` times per review. Process execution goes through the `Executor` trait (`ProcessExecutor` runs
//! real processes; tests script it), exports through `Exporter` (`GitExport`), so nothing here needs a sandbox
//! installed to be tested.
//!
//! Known differences from the Python, all outside recorded behaviour:
//! - `Sandbox` probes the system directories (symlink or directory) and the user id when it is made (`Sandbox::new`),
//!   not on every `wrap`; tests give a recorded layout and user.
//! - Python raised for a `package.json` that is valid JSON but not an object (or whose `devDependencies`,
//!   `dependencies` or `scripts` is a truthy non-object); here such a value counts as empty.
//! - `self_check` reads a `SELFCHECK` line whose JSON is broken, or lacks a key, as an error / not ok (Python raised a
//!   `KeyError` or `JSONDecodeError`).
//! - `int(line)` in `mutation_check` accepts ASCII digits only in a string (Python also took other Unicode digits).
//! - `list_environments` visits subdirectories in `read_dir` order, which is the `os.scandir` order Python used on the
//!   same filesystem; a file name that is not UTF-8 is read lossily (Python kept it with surrogate escapes).
//! - `relpath` on paths that climb above the working directory (`..` first) is computed without it.
//! - `run_process` kills the process group through `kill -KILL -- -<pgid>` (std has no `killpg`), falling back to
//!   killing the child alone, as Python did when `killpg` failed. An empty argv is an error (Python: `IndexError`).
//!   The child gets its own process group (`process_group(0)`), not a new session (`setsid` needs `unsafe` here); the
//!   sandboxes start their own session anyway (`bwrap --new-session`). `detect_sandbox` reads an executor error as
//!   "does not work" (Python let it propagate).
//! - `TestSession` removes its exports when dropped, as well as on `close` (Python only on `close`).
//! - The launch environment and `which` used for a run are hooks (`SessionOptions::launch_env`, `which`); by default
//!   they read the process environment at each run, as Python did.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::levels::truthy as py_truthy;
use crate::py;

pub const DEFAULT_RUNS: i64 = 6;
pub const DEFAULT_TIMEOUT_S: f64 = 300.0;
pub const MAX_OUTPUT_BYTES: usize = 400_000;
pub const OUTPUT_TAIL_CHARS: usize = 2_000;
pub const MAX_PATHS: usize = 12;
pub const MAX_SELECT_CHARS: usize = 120;
pub const MAX_REPLACEMENT_CHARS: usize = 300;
pub const SANDBOX_HOME: &str = "/tmp/home";
pub const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// The only directories of a checkout that are ever bound: prepared environments.
pub const PYTHON_ENVS: [&str; 2] = [".venv", "venv"];
pub const NODE_ENV: &str = "node_modules";

const SYSTEM_DIRS: [&str; 6] = ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32"];
const ETC: [&str; 12] = [
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "passwd",
    "group",
    "nsswitch.conf",
    "localtime",
    "alternatives",
    "ssl",
    "ca-certificates",
    "mime.types",
    "os-release",
];
/// What `unshare` hides by mounting an empty tmpfs over it (bubblewrap starts from an empty root instead).
const HIDDEN: [&str; 7] = [
    "/home",
    "/root",
    "/run",
    "/media",
    "/srv",
    "/var/run",
    "/var/lib/docker",
];
const SYSTEM_PREFIXES: [&str; 6] = ["/usr/", "/bin/", "/sbin/", "/lib/", "/lib64/", "/lib32/"];
const PY_CONFIG: [&str; 5] = ["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg", "conftest.py"];

pub const UNSHARE_SCRIPT: &str = "set -e
work=\"$1\"; shift
mount --bind \"$work\" /mnt
mount -t tmpfs tmpfs /tmp
while [ \"$1\" != \"--\" ]; do if [ -d \"$1\" ]; then mount -t tmpfs tmpfs \"$1\"; fi; shift; done
shift
mkdir -p \"$work\" /tmp/home
mount --move /mnt \"$work\" 2>/dev/null || mount --bind /mnt \"$work\"
cd \"$work\"
exec \"$@\"
";

pub const SELF_CHECK: &str = r#"
import os, socket, sys
out = {}
try:
    socket.create_connection(("1.1.1.1", 53), 2).close()
    out["network"] = "reachable"
except OSError:
    out["network"] = "unreachable"
out["canary"] = "readable" if os.path.exists(sys.argv[1]) else "unreadable"
out["token"] = "present" if any("CANARY_TOKEN" in name for name in os.environ) else "absent"
leaked = [name for name in os.environ if any(word in name.upper() for word in ("TOKEN", "SECRET", "KEY", "PASSWORD", "AWS_", "SSH_", "GH_", "GITHUB"))]
out["credentialVariables"] = leaked
out["home"] = os.environ.get("HOME")
try:
    open("probe.txt", "w").write("x")
    out["workdir"] = "writable"
except OSError:
    out["workdir"] = "read-only"
try:
    open("/usr/ultron-autoreview-probe", "w").write("x")
    out["system"] = "writable"
except OSError:
    out["system"] = "read-only"
out["dockerSocket"] = "visible" if os.path.exists("/var/run/docker.sock") or os.path.exists("/run/docker.sock") else "hidden"
import json; print("SELFCHECK " + json.dumps(out))
"#;

/// An ordered environment (insertion order, as Python dicts keep it).
pub type Env = IndexMap<String, String>;

/// A test request the host will not run; the message goes back to the investigator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestsRejected(pub String);

impl fmt::Display for TestsRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a test session operation did not produce a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestsError {
    /// The request is refused (Python's `TestsRejected`).
    Rejected(TestsRejected),
    /// The commit could not be exported (Python's `RuntimeError`).
    Export(String),
    /// The executor or the file system failed (Python's `OSError`).
    Os(String),
}

impl TestsError {
    /// The Python exception type this stands for, for messages such as "the test session failed (OSError: ...)".
    pub fn python_type(&self) -> &'static str {
        match self {
            TestsError::Rejected(_) => "TestsRejected",
            TestsError::Export(_) => "RuntimeError",
            TestsError::Os(_) => "OSError",
        }
    }
}

impl fmt::Display for TestsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestsError::Rejected(error) => f.write_str(&error.0),
            TestsError::Export(message) | TestsError::Os(message) => f.write_str(message),
        }
    }
}

impl From<TestsRejected> for TestsError {
    fn from(error: TestsRejected) -> Self {
        TestsError::Rejected(error)
    }
}

fn rejected(message: impl Into<String>) -> TestsError {
    TestsError::Rejected(TestsRejected(message.into()))
}

/// Runs `argv` in `cwd` with exactly `env` and a timeout in seconds: (exit code, combined output); 124 on timeout,
/// 127 when not found. An `Err` is a failure to run at all (Python's `OSError`).
pub trait Executor {
    fn execute(&self, argv: &[String], cwd: &str, env: &Env, timeout_s: f64) -> Result<(i32, String), String>;
}

impl<F> Executor for F
where
    F: Fn(&[String], &str, &Env, f64) -> Result<(i32, String), String>,
{
    fn execute(&self, argv: &[String], cwd: &str, env: &Env, timeout_s: f64) -> Result<(i32, String), String> {
        self(argv, cwd, env, timeout_s)
    }
}

/// The real executor: `run_process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessExecutor;

impl Executor for ProcessExecutor {
    fn execute(&self, argv: &[String], cwd: &str, env: &Env, timeout_s: f64) -> Result<(i32, String), String> {
        run_process(argv, cwd, env, timeout_s)
    }
}

/// Makes a disposable copy of a commit's tree and returns its directory; `Err` carries the message.
pub trait Exporter {
    fn export(&self, root: &str, rev: &str) -> Result<String, String>;
}

impl<F> Exporter for F
where
    F: Fn(&str, &str) -> Result<String, String>,
{
    fn export(&self, root: &str, rev: &str) -> Result<String, String> {
        self(root, rev)
    }
}

/// The real exporter: `export_commit`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitExport;

impl Exporter for GitExport {
    fn export(&self, root: &str, rev: &str) -> Result<String, String> {
        export_commit(root, rev)
    }
}

// --- POSIX path semantics (Python's posixpath) --------------------------------------------------------------------

/// `os.path.join(a, b)`.
pub fn path_join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

fn join_all(parts: &[&str]) -> String {
    parts
        .iter()
        .skip(1)
        .fold(parts.first().map_or(String::new(), |p| p.to_string()), |acc, part| {
            path_join(&acc, part)
        })
}

/// `os.path.dirname`.
pub fn dirname(path: &str) -> String {
    let index = path.rfind('/').map_or(0, |i| i + 1);
    let head = &path[..index];
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// `os.path.basename`.
pub fn basename(path: &str) -> String {
    path[path.rfind('/').map_or(0, |i| i + 1)..].to_string()
}

/// `os.path.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(comp);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let out = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if out.is_empty() { ".".to_string() } else { out }
}

fn cwd() -> String {
    std::env::current_dir().map_or_else(|_| "/".to_string(), |p| p.to_string_lossy().into_owned())
}

/// `os.path.abspath`.
pub fn abspath(path: &str) -> String {
    if path.starts_with('/') {
        normpath(path)
    } else {
        normpath(&path_join(&cwd(), path))
    }
}

/// `os.path.relpath(path, start)`.
pub fn relpath(path: &str, start: &str) -> String {
    let anchor = |p: &str| {
        if p.starts_with('/') {
            normpath(p)
        } else {
            normpath(&format!("/{p}"))
        }
    };
    let (path, start) = (anchor(path), anchor(start));
    let split = |p: &str| {
        p.split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let (path, start) = (split(&path), split(&start));
    let common = path.iter().zip(&start).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); start.len() - common];
    parts.extend(path[common..].iter().cloned());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn split_path(path: &str) -> (String, String) {
    (dirname(path), basename(path))
}

/// `os.path.realpath` (not strict): symlinks resolved as far as the path exists, the rest kept.
pub fn realpath(path: &str) -> String {
    let absolute = if path.starts_with('/') {
        path.to_string()
    } else {
        path_join(&cwd(), path)
    };
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    let (resolved, _) = join_real(String::new(), &absolute, &mut seen);
    abspath(&resolved)
}

fn join_real(mut path: String, rest: &str, seen: &mut HashMap<String, Option<String>>) -> (String, bool) {
    let mut rest = rest.to_string();
    if rest.starts_with('/') {
        rest.remove(0);
        path = "/".to_string();
    }
    while !rest.is_empty() {
        let (name, tail) = match rest.find('/') {
            Some(index) => (rest[..index].to_string(), rest[index + 1..].to_string()),
            None => (rest.clone(), String::new()),
        };
        rest = tail;
        if name.is_empty() || name == "." {
            continue;
        }
        if name == ".." {
            if path.is_empty() {
                path = "..".to_string();
            } else {
                let (head, last) = split_path(&path);
                path = if last == ".." {
                    join_all(&[&head, "..", ".."])
                } else {
                    head
                };
            }
            continue;
        }
        let newpath = path_join(&path, &name);
        let is_link = fs::symlink_metadata(&newpath).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link {
            path = newpath;
            continue;
        }
        if let Some(known) = seen.get(&newpath) {
            match known {
                Some(target) => {
                    path = target.clone();
                    continue;
                }
                None => return (path_join(&newpath, &rest), false),
            }
        }
        seen.insert(newpath.clone(), None);
        let target = fs::read_link(&newpath).map_or_else(|_| String::new(), |t| t.to_string_lossy().into_owned());
        let (resolved, ok) = join_real(path, &target, seen);
        if !ok {
            return (path_join(&resolved, &rest), false);
        }
        path = resolved.clone();
        seen.insert(newpath, Some(resolved));
    }
    (path, true)
}

fn is_dir(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir())
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn exists(path: &str) -> bool {
    fs::metadata(path).is_ok()
}

fn lexists(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// `shutil.which(name)` over the process's `PATH`: an executable file, never a directory.
pub fn which(name: &str) -> Option<String> {
    let executable = |path: &str| fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if name.contains('/') {
        return executable(name).then(|| name.to_string());
    }
    let search = std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".to_string());
    search
        .split(':')
        .map(|dir| path_join(if dir.is_empty() { "." } else { dir }, name))
        .find(|path| executable(path))
}

/// `tempfile.mkdtemp(prefix=...)`: a new private directory under the temp directory.
fn random_hex(bytes: usize) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut buffer = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| "no randomness".to_string())?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn make_temp_dir(prefix: &str) -> Result<String, String> {
    let base = std::env::temp_dir();
    let mut last = String::from("no attempt");
    for _ in 0..100 {
        let name = random_hex(4)?;
        let path: PathBuf = base.join(format!("{prefix}{name}"));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path.to_string_lossy().into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = error.to_string(),
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(last)
}

fn remove_tree(path: &str) {
    let _ = fs::remove_dir_all(path);
}

fn tail_chars(text: &str, count: usize) -> &str {
    let total = text.chars().count();
    if total <= count {
        return text;
    }
    let skip = total - count;
    let index = text.char_indices().nth(skip).map_or(text.len(), |(i, _)| i);
    &text[index..]
}

fn head_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

// --- Sandbox -----------------------------------------------------------------------------------------------------

/// The whole environment of a sandboxed command, built from scratch: nothing of the caller's is inherited, so no
/// token, key, cloud or SSH variable can be in it.
pub fn sandbox_env(extra_path: Option<&str>) -> Env {
    let path = match extra_path {
        Some(extra) if !extra.is_empty() => format!("{extra}:{SANDBOX_PATH}"),
        _ => SANDBOX_PATH.to_string(),
    };
    [
        ("PATH", path.as_str()),
        ("HOME", SANDBOX_HOME),
        ("TMPDIR", "/tmp"),
        ("LANG", "C.UTF-8"),
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
        ("CI", "1"),
        ("PYTHONDONTWRITEBYTECODE", "1"),
        ("PYTHONHASHSEED", "0"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The environment of the sandbox launcher itself (bwrap, unshare, docker): a PATH and nothing else of ours (plus
/// `DOCKER_HOST` when set). `get` reads a variable of the caller's environment.
pub fn launch_env_from(get: &dyn Fn(&str) -> Option<String>) -> Env {
    let mut env = Env::new();
    env.insert(
        "PATH".to_string(),
        get("PATH").unwrap_or_else(|| SANDBOX_PATH.to_string()),
    );
    if let Some(host) = get("DOCKER_HOST") {
        env.insert("DOCKER_HOST".to_string(), host);
    }
    env
}

/// `launch_env_from` over the process environment.
pub fn launch_env() -> Env {
    launch_env_from(&|name| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mechanism {
    Bwrap,
    Unshare,
    Docker,
}

impl Mechanism {
    pub const ALL: [Mechanism; 3] = [Mechanism::Bwrap, Mechanism::Unshare, Mechanism::Docker];

    pub fn as_str(self) -> &'static str {
        match self {
            Mechanism::Bwrap => "bwrap",
            Mechanism::Unshare => "unshare",
            Mechanism::Docker => "docker",
        }
    }

    pub fn parse(name: &str) -> Option<Mechanism> {
        Mechanism::ALL.into_iter().find(|m| m.as_str() == name)
    }
}

/// What a system directory is on this machine, for bubblewrap's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemDir {
    /// A symlink, with its target (`os.readlink`).
    Link(String),
    Dir,
    Absent,
}

/// A directory made visible read-only inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// At the same place inside.
    Same(String),
    /// (source, destination).
    Pair(String, String),
}

impl Bind {
    fn pair(&self) -> Option<(String, String)> {
        match self {
            Bind::Same(path) if path.is_empty() => None,
            Bind::Same(path) => Some((path.clone(), path.clone())),
            Bind::Pair(source, target) => Some((source.clone(), target.clone())),
        }
    }

    fn is(&self, path: &str) -> bool {
        matches!(self, Bind::Same(own) if own == path)
    }
}

/// `_DOCKER_CLIENT`: the Docker client's stdin, a pipe nobody writes, open exactly as long as the client runs.
const DOCKER_CLIENT: &str = r#"exec "$@" < <(exec setpriv --pdeathsig KILL -- sleep infinity)"#;
/// `_DOCKER_CLIENT_POLLING`: the same without setpriv.
const DOCKER_CLIENT_POLLING: &str = r#"exec "$@" < <(while kill -0 $$ 2>/dev/null; do sleep 1; done)"#;
/// `_DOCKER_TIED`: inside the container, the watcher that kills the command when the client goes away.
const DOCKER_TIED: &str = r#"exec 3<&0; "$@" </dev/null 3<&- & c=$!; { cat <&3 >/dev/null; kill -KILL $c 2>/dev/null; } & w=$!; exec 3<&-; wait $c; s=$?; kill $w 2>/dev/null; exit $s"#;

/// How commands are isolated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub mechanism: Mechanism,
    pub image: Option<String>,
    /// The user's home, hidden by `unshare`.
    pub home: String,
    /// `uid:gid` for Docker's `--user`.
    pub user: String,
    /// The system directories as they are on this machine, in `SYSTEM_DIRS` order.
    pub system: Vec<(String, SystemDir)>,
    /// Whether `setpriv` is on PATH (`shutil.which("setpriv")`): Docker's client then dies with its parent.
    pub setpriv: bool,
}

/// The layout of the system directories bubblewrap binds, read from this machine.
pub fn probe_system_dirs() -> Vec<(String, SystemDir)> {
    SYSTEM_DIRS
        .iter()
        .map(|path| {
            let kind = if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                SystemDir::Link(
                    fs::read_link(path).map_or_else(|_| String::new(), |t| t.to_string_lossy().into_owned()),
                )
            } else if is_dir(path) {
                SystemDir::Dir
            } else {
                SystemDir::Absent
            };
            (path.to_string(), kind)
        })
        .collect()
}

/// `f"{os.getuid()}:{os.getgid()}"` (real ids, from /proc/self/status).
pub fn process_user() -> String {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
    };
    match (field("Uid:"), field("Gid:")) {
        (Some(uid), Some(gid)) => format!("{uid}:{gid}"),
        _ => fs::metadata("/proc/self").map_or_else(|_| "0:0".to_string(), |m| format!("{}:{}", m.uid(), m.gid())),
    }
}

impl Sandbox {
    /// A sandbox for this machine: the user's home, user ids and system directory layout read now.
    pub fn new(mechanism: Mechanism, image: Option<String>) -> Sandbox {
        Sandbox {
            mechanism,
            image,
            home: home_dir(),
            user: process_user(),
            system: probe_system_dirs(),
            setpriv: which("setpriv").is_some(),
        }
    }

    /// The full command line that runs `command` inside the sandbox, in `workdir` (the only writable directory) or
    /// its subdirectory `cwd`. `binds` are directories made visible read-only; `unshare` cannot place binds and
    /// ignores them.
    pub fn wrap(&self, command: &[String], workdir: &str, env: &Env, binds: &[Bind], cwd: &str) -> Vec<String> {
        let binds: Vec<(String, String)> = binds.iter().filter_map(Bind::pair).collect();
        let inside = if cwd.is_empty() {
            workdir.to_string()
        } else {
            path_join(workdir, cwd)
        };
        let mut command: Vec<String> = command.to_vec();
        if !cwd.is_empty() && self.mechanism != Mechanism::Bwrap {
            let mut wrapped: Vec<String> = ["sh", "-c", "cd \"$1\" && shift && exec \"$@\"", "cd"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            wrapped.push(inside.clone());
            wrapped.extend(command);
            command = wrapped;
        }
        let s = |text: &str| text.to_string();
        match self.mechanism {
            Mechanism::Bwrap => {
                let mut argv: Vec<String> = [
                    "bwrap",
                    "--unshare-all",
                    "--die-with-parent",
                    "--new-session",
                    "--cap-drop",
                    "ALL",
                    "--clearenv",
                ]
                .map(s)
                .to_vec();
                for (name, value) in env {
                    argv.extend([s("--setenv"), name.clone(), value.clone()]);
                }
                for (path, kind) in &self.system {
                    match kind {
                        SystemDir::Link(target) => argv.extend([s("--symlink"), target.clone(), path.clone()]),
                        SystemDir::Dir => argv.extend([s("--ro-bind"), path.clone(), path.clone()]),
                        SystemDir::Absent => {}
                    }
                }
                for name in ETC {
                    let path = format!("/etc/{name}");
                    argv.extend([s("--ro-bind-try"), path.clone(), path]);
                }
                argv.extend(
                    [
                        "--proc",
                        "/proc",
                        "--dev",
                        "/dev",
                        "--tmpfs",
                        "/tmp",
                        "--dir",
                        SANDBOX_HOME,
                    ]
                    .map(s),
                );
                argv.extend([s("--bind"), s(workdir), s(workdir)]);
                // After the export, so that an environment can sit inside it.
                for (source, target) in binds {
                    argv.extend([s("--ro-bind"), source, target]);
                }
                argv.extend([s("--chdir"), inside, s("--")]);
                argv.extend(command);
                argv
            }
            Mechanism::Unshare => {
                let mut hidden: Vec<String> = HIDDEN.map(s).to_vec();
                if !hidden.contains(&self.home) {
                    hidden.push(self.home.clone());
                }
                let mut argv: Vec<String> = [
                    "unshare",
                    "--user",
                    "--map-root-user",
                    "--mount",
                    "--net",
                    "--pid",
                    "--fork",
                    "--ipc",
                    "--uts",
                    "--kill-child",
                    "env",
                    "-i",
                ]
                .map(s)
                .to_vec();
                argv.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
                argv.extend(["sh", "-c", UNSHARE_SCRIPT, "sandbox", workdir].map(s));
                argv.extend(hidden);
                argv.push(s("--"));
                argv.extend(command);
                argv
            }
            Mechanism::Docker => {
                // A container is not a process of ours: killing the client (a timeout kills its process group)
                // leaves it running. It is tied to the client (DOCKER_CLIENT, DOCKER_TIED), the client to its parent
                // when setpriv is there, and the environment set inside with `env -i` (`-e` adds to the image's ENV).
                let mut argv: Vec<String> = Vec::new();
                if self.setpriv {
                    argv.extend(["setpriv", "--pdeathsig", "KILL", "--"].map(s));
                }
                let client = if self.setpriv {
                    DOCKER_CLIENT
                } else {
                    DOCKER_CLIENT_POLLING
                };
                argv.extend([s("bash"), s("-c"), s(client), s("ultron-docker")]);
                argv.extend(
                    [
                        "docker",
                        "run",
                        "--rm",
                        "--init",
                        "-i",
                        "--network",
                        "none",
                        "--cap-drop",
                        "ALL",
                        "--security-opt",
                        "no-new-privileges",
                        "--pids-limit",
                        "1024",
                        "--user",
                    ]
                    .map(s),
                );
                argv.push(self.user.clone());
                argv.extend(["--tmpfs", "/tmp", "-v"].map(s));
                argv.push(format!("{workdir}:{workdir}:rw"));
                argv.extend([s("-w"), s(workdir)]);
                for (source, target) in binds {
                    argv.extend([s("-v"), format!("{source}:{target}:ro")]);
                }
                argv.push(self.image.clone().unwrap_or_else(|| s("None")));
                argv.extend(["sh", "-c", DOCKER_TIED, "sh", "env", "-i"].map(s));
                argv.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
                argv.extend(command);
                argv
            }
        }
    }

    pub fn describe(&self) -> String {
        match self.mechanism {
            Mechanism::Bwrap => "bubblewrap: new user, mount, pid, ipc, uts, cgroup and network namespaces; an empty \
                                 root with the system directories read-only, a private /tmp and HOME, and the exported \
                                 commit as the only writable directory; all capabilities dropped; environment cleared"
                .to_string(),
            Mechanism::Unshare => "unshare: new user, mount, pid, ipc, uts and network namespaces; /home, /root, /run \
                                   and the user's home hidden under empty tmpfs mounts, a private /tmp; environment \
                                   cleared"
                .to_string(),
            Mechanism::Docker => format!(
                "docker ({}): no network, all capabilities dropped, no new privileges, only the exported commit \
                 mounted; environment cleared; the container ends with its client",
                self.image.as_deref().unwrap_or("None")
            ),
        }
    }
}

fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Run to completion in its own process group; kill the group on timeout; output (stdout and stderr together)
/// capped to its last `MAX_OUTPUT_BYTES` bytes and decoded leniently.
pub fn run_process(argv: &[String], cwd: &str, env: &Env, timeout: f64) -> Result<(i32, String), String> {
    let program = argv.first().ok_or("empty command")?;
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(|e| e.to_string())?)
        .stderr(writer)
        .process_group(0);
    let spawned = command.spawn();
    // The command holds the pipe's write ends: drop it so that the reader sees the end once the group exits.
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((127, format!("{program}: not found")));
        }
        Err(error) => return Err(error.to_string()),
    };
    let collector = std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 65_536];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    kept.extend_from_slice(&buffer[..n]);
                    if kept.len() > 2 * MAX_OUTPUT_BYTES {
                        kept.drain(..kept.len() - MAX_OUTPUT_BYTES);
                    }
                }
            }
        }
        if kept.len() > MAX_OUTPUT_BYTES {
            kept.drain(..kept.len() - MAX_OUTPUT_BYTES);
        }
        kept
    });
    let limit = if timeout.is_finite() && timeout > 0.0 {
        timeout.min(1e9)
    } else {
        0.0
    };
    let deadline = Instant::now() + Duration::from_secs_f64(limit);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let group = format!("-{}", child.id());
                let killed = Command::new("kill")
                    .args(["-KILL", "--", &group])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if !killed {
                    let _ = child.kill();
                }
                let _ = child.wait();
                break 124;
            }
        }
    };
    let output = collector.join().unwrap_or_default();
    Ok((code, String::from_utf8_lossy(&output).into_owned()))
}

fn works(sandbox: &Sandbox, executor: &dyn Executor) -> bool {
    let Ok(probe) = make_temp_dir("ultron-autoreview-probe-") else {
        return false;
    };
    let argv = sandbox.wrap(&["true".to_string()], &probe, &sandbox_env(None), &[], "");
    let ok = matches!(executor.execute(&argv, &probe, &launch_env(), 30.0), Ok((0, _)));
    remove_tree(&probe);
    ok
}

/// The strongest sandbox that works here: bubblewrap, then unshare, then Docker (with a local image). None when
/// there is none: tests are then not run. `make` builds the sandbox for a mechanism (`Sandbox::new` for this
/// machine).
pub fn detect_sandbox_with(
    image: Option<&str>,
    which: &dyn Fn(&str) -> Option<String>,
    executor: &dyn Executor,
    only: Option<Mechanism>,
    make: &dyn Fn(Mechanism, Option<String>) -> Sandbox,
) -> Option<Sandbox> {
    for mechanism in Mechanism::ALL {
        if only.is_some_and(|only| only != mechanism) {
            continue;
        }
        let no_image = image.is_none_or(str::is_empty);
        if which(mechanism.as_str()).is_none() || (mechanism == Mechanism::Docker && no_image) {
            continue;
        }
        let sandbox = make(
            mechanism,
            if mechanism == Mechanism::Docker {
                image.map(str::to_string)
            } else {
                None
            },
        );
        if works(&sandbox, executor) {
            return Some(sandbox);
        }
    }
    None
}

/// `detect_sandbox` with this machine's `which` and real processes.
pub fn detect_sandbox(image: Option<&str>, only: Option<Mechanism>) -> Option<Sandbox> {
    detect_sandbox_with(image, &which, &ProcessExecutor, only, &Sandbox::new)
}

/// Run the isolation checks inside the sandbox: the network must be unreachable, a canary file in the real `home`
/// unreadable, a token-like variable of the caller absent. The canary and the work directory are removed after.
pub fn self_check(sandbox: &Sandbox, executor: &dyn Executor, home: &str) -> Result<Value, String> {
    let suffix = random_hex(4)?;
    let canary = path_join(home, &format!(".ultron-autoreview-canary-{suffix}"));
    let workdir = make_temp_dir("ultron-autoreview-check-")?;
    let mut launch = launch_env();
    launch.insert(
        "ULTRON_AUTOREVIEW_CANARY_TOKEN".to_string(),
        "not-a-real-token".to_string(),
    );
    let result = (|| {
        fs::write(&canary, "canary\n").map_err(|e| e.to_string())?;
        let command: Vec<String> = ["python3", "-c", SELF_CHECK, &canary]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = sandbox.wrap(&command, &workdir, &sandbox_env(None), &[], "");
        let (code, output) = executor.execute(&argv, &workdir, &launch, 60.0)?;
        let Some(line) = py::splitlines(&output)
            .into_iter()
            .find(|line| line.starts_with("SELFCHECK "))
        else {
            return Ok(
                json!({"ok": false, "error": format!("the self-check did not run (exit {code}): {}",
                                                             tail_chars(&output, 300))}),
            );
        };
        let mut result: Map<String, Value> =
            serde_json::from_str(&line["SELFCHECK ".len()..]).map_err(|e| format!("the self-check reply: {e}"))?;
        let is = |key: &str, want: &str| result.get(key).and_then(Value::as_str) == Some(want);
        let ok = is("network", "unreachable")
            && is("canary", "unreadable")
            && is("token", "absent")
            && result.get("credentialVariables").is_some_and(|v| !truthy(v))
            && is("workdir", "writable")
            && is("system", "read-only")
            && is("dockerSocket", "hidden")
            && is("home", SANDBOX_HOME);
        result.insert("ok".to_string(), Value::Bool(ok));
        Ok(Value::Object(result))
    })();
    let _ = fs::remove_file(&canary);
    remove_tree(&workdir);
    result
}

/// The doctor report: the mechanism, what it isolates, and the self-check's result.
pub fn doctor_with(
    image: Option<&str>,
    detect: &dyn Fn(Option<&str>) -> Option<Sandbox>,
    check: &dyn Fn(&Sandbox) -> Result<Value, String>,
) -> Result<Value, String> {
    let Some(sandbox) = detect(image) else {
        return Ok(json!({"mechanism": null, "ok": false,
            "message": "tests are not run: no sandbox is available (install bubblewrap, or allow unprivileged user \
                        namespaces, or set autoreview.testImage for Docker)"}));
    };
    let result = check(&sandbox)?;
    let ok = result.get("ok").is_some_and(truthy);
    Ok(json!({"mechanism": sandbox.mechanism.as_str(), "isolation": sandbox.describe(), "selfCheck": result, "ok": ok}))
}

/// `doctor(image)` on this machine (the canary goes into the real home, as in Python).
pub fn doctor(image: Option<&str>) -> Result<Value, String> {
    let home = home_dir();
    doctor_with(image, &|image| detect_sandbox(image, None), &|sandbox| {
        self_check(sandbox, &ProcessExecutor, &home)
    })
}

// --- Environments from a local checkout ---------------------------------------------------------------------------

/// The install directory of an interpreter that lives outside the system directories (a uv-, pyenv- or
/// mise-managed Python under the home directory), found by resolving `binary`'s symlink chain; None when the
/// interpreter is a system one, or when no directory of its own can be told apart (never the home itself).
pub fn interpreter_home(binary: &str, home: Option<&str>) -> Option<String> {
    let real = realpath(binary);
    if !is_file(&real) || SYSTEM_PREFIXES.iter().any(|prefix| real.starts_with(prefix)) {
        return None;
    }
    let prefix = dirname(&dirname(&real));
    let home = realpath(
        &home
            .filter(|home| !home.is_empty())
            .map_or_else(home_dir, str::to_string),
    );
    if basename(&dirname(&real)) != "bin" || !is_dir(&path_join(&prefix, "lib")) {
        return None;
    }
    // The interpreter's own directory only: not the home, not a parent of it, not a top-level directory.
    if prefix == "/" || prefix == home || home.starts_with(&format!("{prefix}/")) || prefix.matches('/').count() < 3 {
        return None;
    }
    Some(prefix)
}

/// Whether `path`, symlinks resolved, is `root` or inside it.
pub fn inside(path: &str, root: &str) -> bool {
    let real = realpath(path);
    real == root || real.starts_with(&format!("{root}/"))
}

/// The prepared environments of a local checkout that serve tests run in a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Environment {
    /// (source, path relative to the repository).
    pub binds: Vec<(String, String)>,
    /// The relative path of the virtualenv's python.
    pub python: Option<String>,
    /// Directories outside the checkout an environment's python needs.
    pub interpreters: Vec<String>,
}

/// The prepared environments of a local checkout that serve tests run in `directory` (a path inside the
/// repository): the nearest Python virtualenv and every `node_modules` from that directory up to the root. Only
/// directories named .venv, venv or node_modules that really are inside the checkout qualify (a symlink leading
/// elsewhere does not), so the checkout's source, its .git and its .env files are never bound.
pub fn discover_environment(checkout: &str, directory: &str, home: Option<&str>) -> Environment {
    let mut out = Environment::default();
    let root = realpath(checkout);
    if !is_dir(&root) {
        return out;
    }
    let mut current = directory.trim_matches('/').to_string();
    loop {
        let base = if current.is_empty() {
            root.clone()
        } else {
            path_join(&root, &current)
        };
        let relative_of = |name: &str| {
            if current.is_empty() {
                name.to_string()
            } else {
                path_join(&current, name)
            }
        };
        if out.python.is_none() {
            for name in PYTHON_ENVS {
                let env = path_join(&base, name);
                let python = join_all(&[&env, "bin", "python"]);
                if is_dir(&env) && inside(&env, &root) && lexists(&python) {
                    let relative = relative_of(name);
                    out.binds.push((realpath(&env), relative.clone()));
                    out.python = Some(join_all(&[&relative, "bin", "python"]));
                    if let Some(prefix) = interpreter_home(&python, home)
                        && !inside(&prefix, &root)
                    {
                        out.interpreters.push(prefix);
                    }
                    break;
                }
            }
        }
        let modules = path_join(&base, NODE_ENV);
        if is_dir(&modules) && inside(&modules, &root) {
            out.binds.push((realpath(&modules), relative_of(NODE_ENV)));
        }
        if current.is_empty() {
            break;
        }
        current = dirname(&current);
    }
    out
}

/// Every prepared environment of a checkout down to `depth` directories, for `doctor --repo` and the
/// `environments` command: `{"path", "kind"}` plus `"interpreter"` (a directory, or "system") for a virtualenv.
pub fn list_environments(checkout: &str, depth: usize, home: Option<&str>) -> Vec<Value> {
    let root = realpath(checkout);
    let mut out = Vec::new();
    walk(&root, &root, depth, home, &mut out);
    out
}

fn walk(root: &str, current: &str, depth: usize, home: Option<&str>, out: &mut Vec<Value>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    let dirs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let relative = relpath(current, root);
    let level = if relative == "." {
        0
    } else {
        relative.matches('/').count() + 1
    };
    let is_env = |name: &str| PYTHON_ENVS.contains(&name) || name == NODE_ENV;
    let mut found: Vec<String> = dirs.iter().filter(|name| is_env(name)).cloned().collect();
    let descend: Vec<String> = if level >= depth {
        Vec::new()
    } else {
        dirs.iter()
            .filter(|name| !is_env(name) && !name.starts_with('.'))
            .cloned()
            .collect()
    };
    found.sort();
    for name in found {
        let path = path_join(current, &name);
        if !inside(&path, root) {
            continue;
        }
        let mut entry = Map::new();
        entry.insert(
            "path".to_string(),
            Value::String(if relative == "." {
                name.clone()
            } else {
                path_join(&relative, &name)
            }),
        );
        let kind = if name == NODE_ENV { "node_modules" } else { "virtualenv" };
        entry.insert("kind".to_string(), Value::String(kind.to_string()));
        if name != NODE_ENV {
            let python = join_all(&[&path, "bin", "python"]);
            if !lexists(&python) {
                continue;
            }
            let interpreter = match interpreter_home(&python, home) {
                Some(prefix) if !inside(&prefix, root) => prefix,
                _ => "system".to_string(),
            };
            entry.insert("interpreter".to_string(), Value::String(interpreter));
        }
        out.push(Value::Object(entry));
    }
    for name in descend {
        let path = path_join(current, &name);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        walk(root, &path, depth, home, out);
    }
}

// --- Test runners -------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestRunner {
    pub name: String,
    /// The files that made the choice, for the record.
    pub because: String,
}

impl TestRunner {
    pub fn new(name: &str, because: impl Into<String>) -> TestRunner {
        TestRunner {
            name: name.to_string(),
            because: because.into(),
        }
    }

    pub fn command(&self, paths: &[String], select: Option<&str>) -> Vec<String> {
        let s = |text: &str| text.to_string();
        let select = select.filter(|text| !text.is_empty());
        match self.name.as_str() {
            "pytest" => {
                let mut argv: Vec<String> = [
                    "python3",
                    "-m",
                    "pytest",
                    "-q",
                    "-rA",
                    "--no-header",
                    "-p",
                    "no:cacheprovider",
                    "--tb=short",
                ]
                .map(s)
                .to_vec();
                if let Some(select) = select {
                    argv.extend([s("-k"), s(select)]);
                }
                argv.extend(paths.iter().cloned());
                argv
            }
            "vitest" | "jest" => {
                let mut argv = vec![s("npx"), s("--no-install"), self.name.clone()];
                if self.name == "vitest" {
                    argv.push(s("run"));
                }
                if let Some(select) = select {
                    argv.extend([s("-t"), s(select)]);
                }
                argv.extend(paths.iter().cloned());
                argv
            }
            "npm" | "pnpm" | "yarn" => {
                let mut argv = vec![self.name.clone(), s("test")];
                if !paths.is_empty() && self.name != "yarn" {
                    argv.push(s("--"));
                }
                argv.extend(paths.iter().cloned());
                argv
            }
            "go" => {
                let mut dirs: Vec<String> = Vec::new();
                for path in paths {
                    let dir = dirname(path);
                    let dir = format!("./{}", if dir.is_empty() { "." } else { &dir });
                    if !dirs.contains(&dir) {
                        dirs.push(dir);
                    }
                }
                if dirs.is_empty() {
                    dirs.push(s("./..."));
                }
                let mut argv = vec![s("go"), s("test")];
                if let Some(select) = select {
                    argv.extend([s("-run"), s(select)]);
                }
                argv.extend(dirs);
                argv
            }
            "cargo" => {
                let mut argv = vec![s("cargo"), s("test"), s("--offline")];
                argv.extend(select.map(s));
                argv
            }
            _ => vec![s("make"), s("test")],
        }
    }
}

/// Finds a program on the PATH (`shutil.which`).
pub type Which = dyn Fn(&str) -> Option<String>;

/// Reads a tracked file of the reviewed commit; None when it has none.
pub type ReadFile<'a> = dyn Fn(&str) -> Option<String> + 'a;

fn object_of(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// (devDependencies + dependencies, scripts.test as a string) of a package.json's text.
fn package_facts(text: Option<String>) -> (Map<String, Value>, String) {
    let package: Value = serde_json::from_str(&text.filter(|t| !t.is_empty()).unwrap_or_else(|| "{}".to_string()))
        .unwrap_or(Value::Object(Map::new()));
    let mut deps = object_of(package.get("devDependencies"));
    for (key, value) in object_of(package.get("dependencies")) {
        deps.insert(key, value);
    }
    let script = object_of(package.get("scripts"))
        .get("test")
        .filter(|v| truthy(v))
        .map_or_else(String::new, py::str_of);
    (deps, script)
}

fn matches(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).is_ok_and(|re| re.is_match(text))
}

/// The project's test runner, from its tracked files; None when none is recognized.
pub fn detect_runner(tracked: &[String], read: &ReadFile) -> Option<TestRunner> {
    let files: HashSet<&str> = tracked.iter().map(String::as_str).collect();
    let has = |name: &str, needle: &str| files.contains(name) && read(name).unwrap_or_default().contains(needle);
    if files.contains("pytest.ini")
        || has("pyproject.toml", "[tool.pytest")
        || has("tox.ini", "pytest")
        || has("setup.cfg", "[tool:pytest]")
        || files.contains("conftest.py")
    {
        return Some(TestRunner::new("pytest", "pytest configuration"));
    }
    if files.contains("package.json") {
        let (deps, script) = package_facts(read("package.json"));
        for name in ["vitest", "jest"] {
            if deps.contains_key(name) || script.contains(name) {
                return Some(TestRunner::new(name, format!("package.json ({name})")));
            }
        }
        if !script.is_empty() && !script.contains("no test specified") {
            let manager = if files.contains("pnpm-lock.yaml") {
                "pnpm"
            } else if files.contains("yarn.lock") {
                "yarn"
            } else {
                "npm"
            };
            return Some(TestRunner::new(manager, "package.json scripts.test"));
        }
    }
    if files.contains("go.mod") {
        return Some(TestRunner::new("go", "go.mod"));
    }
    if files.contains("Cargo.toml") {
        return Some(TestRunner::new("cargo", "Cargo.toml"));
    }
    if files.contains("Makefile") && matches(r"(?m)^test:", &read("Makefile").unwrap_or_default()) {
        return Some(TestRunner::new("make", "Makefile test target"));
    }
    if files
        .iter()
        .any(|path| matches(r"(^|/)(test_[^/]+|[^/]+_test)\.py$", path))
    {
        return Some(TestRunner::new("pytest", "python test files"));
    }
    None
}

/// The closest directory at or above `path`'s that holds one of `names` ("" is the root); None when none does.
pub fn nearest(tracked: &HashSet<String>, path: &str, names: &[&str]) -> Option<String> {
    let mut directory = dirname(path);
    loop {
        if names.iter().any(|name| {
            tracked.contains(&if directory.is_empty() {
                name.to_string()
            } else {
                format!("{directory}/{name}")
            })
        }) {
            return Some(directory);
        }
        if directory.is_empty() {
            return None;
        }
        directory = dirname(&directory);
    }
}

/// The test runner a package.json declares: vitest or jest, else its `test` script.
pub fn package_runner(tracked: &HashSet<String>, read: &ReadFile, directory: &str) -> Option<TestRunner> {
    let manifest = if directory.is_empty() {
        "package.json".to_string()
    } else {
        format!("{directory}/package.json")
    };
    let (deps, script) = package_facts(read(&manifest));
    for name in ["vitest", "jest"] {
        if deps.contains_key(name) || script.contains(name) {
            return Some(TestRunner::new(name, format!("{manifest} ({name})")));
        }
    }
    if !script.is_empty() && !script.contains("no test specified") {
        let root = directory.split('/').next().unwrap_or_default();
        let manager = if tracked.contains("pnpm-lock.yaml") {
            "pnpm"
        } else if tracked.contains("yarn.lock") {
            "yarn"
        } else {
            "npm"
        };
        let suffix = if root.is_empty() {
            String::new()
        } else {
            format!(" ({root})")
        };
        return Some(TestRunner::new(manager, format!("{manifest} scripts.test{suffix}")));
    }
    None
}

/// The runner of one test file and the directory it runs in, by the file's type and its nearest manifest: a Python
/// file is pytest's, a JavaScript or TypeScript file belongs to the closest package.json's runner (so a monorepo's
/// packages each get their own), a Go file to its module, a Rust file to its crate.
pub fn runner_for(path: &str, tracked: &HashSet<String>, read: &ReadFile) -> Option<(TestRunner, String)> {
    if path.ends_with(".py") {
        return Some((
            TestRunner::new("pytest", "a Python test file"),
            nearest(tracked, path, &PY_CONFIG).unwrap_or_default(),
        ));
    }
    if matches(r"\.(?:[cm]?[jt]s|[jt]sx)$", path) {
        let mut directory = dirname(path);
        // The closest package.json that names a runner; a package without one defers to its parents.
        loop {
            let manifest = if directory.is_empty() {
                "package.json".to_string()
            } else {
                format!("{directory}/package.json")
            };
            if tracked.contains(&manifest)
                && let Some(runner) = package_runner(tracked, read, &directory)
            {
                return Some((runner, directory));
            }
            if directory.is_empty() {
                return None;
            }
            directory = dirname(&directory);
        }
    }
    if path.ends_with(".go") {
        return nearest(tracked, path, &["go.mod"]).map(|dir| (TestRunner::new("go", "go.mod"), dir));
    }
    if path.ends_with(".rs") {
        return nearest(tracked, path, &["Cargo.toml"]).map(|dir| (TestRunner::new("cargo", "Cargo.toml"), dir));
    }
    None
}

/// One test's result in a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestResult {
    pub id: String,
    /// passed, failed or error.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn missing_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)ModuleNotFoundError|No module named|ImportError while (importing|loading)|Cannot find module|command not found|: not found|npm ERR! missing|ERR_MODULE_NOT_FOUND|could not determine executable|no required module provides|cannot find package|error: no matching package|failed to select a version|errors? during collection|Could not resolve dependencies|network is unreachable|Temporary failure in name resolution|getaddrinfo",
        )
        .expect("missing pattern")
    })
}

fn result_pattern(runner: &str) -> Option<&'static Regex> {
    static PYTEST: OnceLock<Regex> = OnceLock::new();
    static GO: OnceLock<Regex> = OnceLock::new();
    static CARGO: OnceLock<Regex> = OnceLock::new();
    let build =
        |cell: &'static OnceLock<Regex>, pattern: &str| cell.get_or_init(|| Regex::new(pattern).expect("pattern"));
    match runner {
        "pytest" => Some(build(&PYTEST, r"(?m)^(PASSED|FAILED|ERROR)\s+(\S+)(?:\s+-\s+(.*))?$")),
        "go" => Some(build(&GO, r"(?m)^\s*--- (PASS|FAIL): (\S+)")),
        "cargo" => Some(build(&CARGO, r"(?m)^test (\S+) \.\.\. (ok|FAILED)")),
        _ => None,
    }
}

/// (status, per-test results) of one run: passed, failed, unavailable (it could not run: dependencies missing,
/// nothing collected), or timeout.
pub fn parse_outcome(runner: &str, code: i32, output: &str) -> (String, Vec<TestResult>) {
    let mut tests: Vec<TestResult> = Vec::new();
    if let Some(pattern) = result_pattern(runner) {
        for caps in pattern.captures_iter(output) {
            let group = |i: usize| caps.get(i).map_or("", |m| m.as_str());
            tests.push(match runner {
                "pytest" => TestResult {
                    id: group(2).to_string(),
                    status: group(1).to_lowercase(),
                    detail: Some(group(3)).filter(|d| !d.is_empty()).map(|d| head_chars(d, 200)),
                },
                "go" => TestResult {
                    id: group(2).to_string(),
                    status: if group(1) == "PASS" { "passed" } else { "failed" }.to_string(),
                    detail: None,
                },
                _ => TestResult {
                    id: group(1).to_string(),
                    status: if group(2) == "ok" { "passed" } else { "failed" }.to_string(),
                    detail: None,
                },
            });
        }
    }
    let status = if code == 124 {
        "timeout"
    } else if code == 0 {
        "passed"
    } else {
        let failed = tests.iter().any(|item| item.status == "failed");
        // Nothing ran, or it stopped before running: not a verdict on the code.
        if code == 127
            || (!failed && missing_pattern().is_match(output))
            || (runner == "pytest" && matches!(code, 4 | 5) && !failed)
        {
            "unavailable"
        } else {
            "failed"
        }
    };
    (status.to_string(), tests)
}

// --- A review's test session --------------------------------------------------------------------------------------

/// A disposable copy of the commit's tree (`git archive` into a temporary directory).
pub fn export_commit(root: &str, rev: &str) -> Result<String, String> {
    let failed = || format!("the commit {} could not be exported", head_chars(rev, 12));
    let target = make_temp_dir("ultron-autoreview-run-")?;
    let env: Env = [
        (
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| SANDBOX_PATH.to_string()),
        ),
        ("GIT_OPTIONAL_LOCKS", "0".to_string()),
        ("GIT_TERMINAL_PROMPT", "0".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let archive = Command::new("git")
        .args(["-C", root, "archive", "--format=tar", rev])
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut archive = match archive {
        Ok(child) => child,
        Err(_) => {
            remove_tree(&target);
            return Err(failed());
        }
    };
    let extracted = match archive.stdout.take() {
        Some(stdout) => Command::new("tar")
            .args(["-x", "-C", &target])
            .env_clear()
            .envs(&env)
            .stdin(Stdio::from(stdout))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
        None => false,
    };
    let archived = archive.wait().is_ok_and(|status| status.success());
    if !archived || !extracted {
        remove_tree(&target);
        return Err(failed());
    }
    Ok(target)
}

/// Which commit a run is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rev {
    Head,
    Base,
}

impl Rev {
    pub fn as_str(self) -> &'static str {
        match self {
            Rev::Head => "head",
            Rev::Base => "base",
        }
    }
}

/// The line a mutation run changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    pub path: String,
    pub line: i64,
    pub was: String,
    pub replacement: String,
}

/// One sandboxed execution, as kept in `TestSession::records` and shown to models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub n: usize,
    /// run, automatic, base or mutation.
    pub kind: String,
    pub rev: String,
    pub runner: String,
    pub cwd: String,
    pub command: String,
    pub paths: Vec<String>,
    pub status: String,
    pub exit: i32,
    pub tests: Vec<TestResult>,
    pub passed: usize,
    pub failed: usize,
    pub ms: i64,
    pub environment: Vec<String>,
    pub toolchain: Vec<String>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation: Option<Mutation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caught: Option<bool>,
}

/// A test that fails at head and passed at base, with the two runs that showed it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Regression {
    #[serde(flatten)]
    pub test: TestResult,
    pub head: Record,
    pub base: Record,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Compared {
    pub runs: Vec<Record>,
    pub regressions: Vec<Regression>,
}

/// How a session runs: limits, environments, and the hooks tests script.
pub struct SessionOptions {
    pub runs: i64,
    pub timeout_s: f64,
    pub env_dir: Option<String>,
    pub checkout: Option<String>,
    pub toolchain: Vec<String>,
    pub env_kind: Option<String>,
    pub executor: Box<dyn Executor>,
    pub export: Box<dyn Exporter>,
    pub clock: Box<dyn Fn() -> f64>,
    /// Finds `node` for a Node runner (`shutil.which`).
    pub which: Box<Which>,
    /// The sandbox launcher's environment; None reads the process environment at each run.
    pub launch_env: Option<Env>,
    /// The home `interpreter_home` keeps out of interpreter binds (environments and `node`); None is `$HOME`.
    pub home: Option<String>,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            runs: DEFAULT_RUNS,
            timeout_s: DEFAULT_TIMEOUT_S,
            env_dir: None,
            checkout: None,
            toolchain: Vec::new(),
            env_kind: None,
            executor: Box::new(ProcessExecutor),
            export: Box::new(GitExport),
            clock: Box::new(monotonic),
            which: Box::new(which),
            launch_env: None,
            home: None,
        }
    }
}

/// The test executions of one review: bounded in number and time, each in the sandbox, each in an export.
pub struct TestSession {
    pub root: String,
    /// Exact install directories of the toolchain (a mise python or node, `<prefix>/bin/<tool>`), bound read-only
    /// into the sandbox with their bin first on its PATH. Never a whole tool tree, never shims.
    pub toolchain: Vec<String>,
    /// Where the environment came from: prepared, checkout, testEnv or none.
    pub env_kind: String,
    pub head: String,
    pub base: Option<String>,
    pub tracked: HashSet<String>,
    pub sandbox: Sandbox,
    pub limit: usize,
    pub timeout_s: f64,
    pub env_dir: Option<String>,
    /// A local checkout of the same repository whose prepared environments may be used (read-only). An explicit
    /// environment directory takes its place.
    pub checkout: Option<String>,
    /// The repository's own runner (for directories and file types without one of their own).
    pub runner: Option<TestRunner>,
    /// Every execution, in order.
    pub records: Vec<Record>,
    read: Box<ReadFile<'static>>,
    executor: Box<dyn Executor>,
    export: Box<dyn Exporter>,
    clock: Box<dyn Fn() -> f64>,
    which: Box<Which>,
    launch_env: Option<Env>,
    home: Option<String>,
    dirs: IndexMap<Rev, String>,
}

impl Drop for TestSession {
    fn drop(&mut self) {
        self.close();
    }
}

impl TestSession {
    pub fn new(
        root: &str,
        head: &str,
        base: Option<&str>,
        tracked: &[String],
        read: Box<ReadFile<'static>>,
        sandbox: Sandbox,
        options: SessionOptions,
    ) -> TestSession {
        let toolchain: Vec<String> = options
            .toolchain
            .into_iter()
            .filter(|path| is_dir(path) && is_dir(&path_join(path, "bin")))
            .collect();
        let env_dir = options.env_dir.filter(|dir| !dir.is_empty() && is_dir(dir));
        let checkout = options.checkout.filter(|dir| !dir.is_empty() && is_dir(dir));
        let env_kind = options.env_kind.filter(|kind| !kind.is_empty()).unwrap_or_else(|| {
            if env_dir.is_some() {
                "testEnv"
            } else if checkout.is_some() {
                "checkout"
            } else {
                "none"
            }
            .to_string()
        });
        let runner = detect_runner(tracked, &*read);
        TestSession {
            root: root.to_string(),
            toolchain,
            env_kind,
            head: head.to_string(),
            base: base.map(str::to_string),
            tracked: tracked.iter().cloned().collect(),
            sandbox,
            limit: options.runs.max(0) as usize,
            timeout_s: options.timeout_s,
            checkout: if env_dir.is_none() { checkout } else { None },
            env_dir,
            runner,
            records: Vec::new(),
            read,
            executor: options.executor,
            export: options.export,
            clock: options.clock,
            which: options.which,
            launch_env: options.launch_env,
            home: options.home,
            dirs: IndexMap::new(),
        }
    }

    /// Replace the executor (tests that make the sandbox fail mid-review).
    pub fn set_executor(&mut self, executor: Box<dyn Executor>) {
        self.executor = executor;
    }

    /// Test executions left in this review.
    pub fn left(&self) -> usize {
        self.limit.saturating_sub(self.records.len())
    }

    /// The exports made so far (head and/or base), in the order they were made.
    pub fn exports(&self) -> Vec<String> {
        self.dirs.values().cloned().collect()
    }

    /// The export of `which`, once a run has made it.
    pub fn export_dir(&self, which: Rev) -> Option<&str> {
        self.dirs.get(&which).map(String::as_str)
    }

    /// Remove every export.
    pub fn close(&mut self) {
        for path in self.dirs.values() {
            remove_tree(path);
        }
        self.dirs.clear();
    }

    fn dir(&mut self, which: Rev) -> Result<String, TestsError> {
        if let Some(path) = self.dirs.get(&which) {
            return Ok(path.clone());
        }
        let rev = match which {
            Rev::Head => Some(self.head.clone()),
            Rev::Base => self.base.clone(),
        }
        .filter(|rev| !rev.is_empty())
        .ok_or_else(|| rejected("the base commit is not known"))?;
        let path = self.export.export(&self.root, &rev).map_err(TestsError::Export)?;
        self.dirs.insert(which, path.clone());
        Ok(path)
    }

    /// Existing, tracked test files (or test directories), or TestsRejected. `raw` is the request's JSON value.
    pub fn paths(&self, raw: Option<&Value>) -> Result<Vec<String>, TestsRejected> {
        let items: &[Value] = match raw {
            Some(Value::Array(items)) => items,
            _ => &[],
        };
        if items.is_empty() || items.len() > MAX_PATHS {
            return Err(TestsRejected(format!("give 1 to {MAX_PATHS} test paths")));
        }
        let mut out = Vec::new();
        for item in items {
            let bad = || TestsRejected(format!("{} is not a path inside the repository", py::repr_value(item)));
            let Value::String(text) = item else {
                return Err(bad());
            };
            if text.is_empty()
                || text.starts_with(['/', '-', '~', ':'])
                || text.contains('\0')
                || text.split('/').any(|part| part == "..")
                || text.contains('\\')
            {
                return Err(bad());
            }
            let path = normpath(text);
            let prefix = format!("{path}/");
            if !self.tracked.contains(&path) && !self.tracked.iter().any(|name| name.starts_with(&prefix)) {
                return Err(TestsRejected(format!(
                    "{path} is not a tracked file or directory at the reviewed commit"
                )));
            }
            out.push(path);
        }
        Ok(out)
    }

    /// `paths` grouped by the runner that owns each and the directory it runs in: (runner, directory, paths
    /// relative to it). A file whose type has no runner here is left out; a directory, or a file of another type,
    /// goes to the repository's own runner when it has one.
    pub fn plan(&self, paths: &[String]) -> Vec<(TestRunner, String, Vec<String>)> {
        let mut groups: IndexMap<(String, String), (TestRunner, String, Vec<String>)> = IndexMap::new();
        for path in paths {
            let tracked = self.tracked.contains(path);
            let mut found = if tracked {
                runner_for(path, &self.tracked, &*self.read)
            } else {
                None
            };
            if found.is_none() && (!tracked || !matches(r"\.(py|[cm]?[jt]sx?|go|rs)$", path)) {
                found = self.runner.clone().map(|runner| (runner, String::new()));
            }
            let Some((runner, directory)) = found else {
                continue;
            };
            let relative = if directory.is_empty() {
                path.clone()
            } else {
                relpath(path, &directory)
            };
            groups
                .entry((runner.name.clone(), directory.clone()))
                .or_insert_with(|| (runner, directory, Vec::new()))
                .2
                .push(relative);
        }
        groups.into_values().collect()
    }

    /// One sandboxed execution of the project's tests on `paths`; counts against the review's limit.
    pub fn run(
        &mut self,
        paths: &[String],
        select: Option<&str>,
        which: Rev,
        kind: &str,
    ) -> Result<Record, TestsError> {
        let select = select.map(|text| Value::String(text.to_string()));
        self.run_value(paths, select.as_ref(), which, kind)
    }

    fn run_value(
        &mut self,
        paths: &[String],
        select: Option<&Value>,
        which: Rev,
        kind: &str,
    ) -> Result<Record, TestsError> {
        let mut groups = self.plan(paths);
        if groups.is_empty() {
            let named: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
            return Err(rejected(format!("no test runner is known for {}", named.join(", "))));
        }
        if groups.len() > 1 {
            let described: Vec<String> = groups
                .iter()
                .map(|(runner, directory, _)| {
                    format!(
                        "{} in {}",
                        runner.name,
                        if directory.is_empty() { "." } else { directory }
                    )
                })
                .collect();
            return Err(rejected(format!(
                "these paths belong to different test runners ({}): ask for them separately",
                described.join("; ")
            )));
        }
        let (runner, directory, relative) = groups.remove(0);
        let select = match select {
            None | Some(Value::Null) => None,
            Some(Value::String(text))
                if text.chars().count() <= MAX_SELECT_CHARS && matches(r"^[\w .:\[\]()/,=-]+$", text) =>
            {
                Some(text.clone())
            }
            Some(_) => return Err(rejected("select must be a short test name expression")),
        };
        if self.records.len() >= self.limit {
            return Err(rejected(format!(
                "the limit of {} test executions per review is reached",
                self.limit
            )));
        }
        let workdir = self.dir(which)?;
        let mut command = runner.command(&relative, select.as_deref());
        let mut extra_dirs: Vec<String> = self.toolchain.iter().map(|path| path_join(path, "bin")).collect();
        if let Some(env_dir) = &self.env_dir
            && is_dir(&path_join(env_dir, "bin"))
        {
            extra_dirs.push(path_join(env_dir, "bin"));
        }
        let joined = extra_dirs.join(":");
        let mut env = sandbox_env(Some(&joined));
        if let Some(env_dir) = &self.env_dir
            && !is_dir(&path_join(env_dir, "bin"))
        {
            env.insert("NODE_PATH".to_string(), env_dir.clone());
        }
        let mut binds: Vec<Bind> = self.env_dir.iter().map(|dir| Bind::Same(dir.clone())).collect();
        binds.extend(self.toolchain.iter().map(|path| Bind::Same(path.clone())));
        let mut used: Vec<String> = Vec::new();
        if let Some(checkout) = self.checkout.clone()
            && self.sandbox.mechanism != Mechanism::Unshare
        {
            let found = discover_environment(&checkout, &directory, self.home.as_deref());
            binds.extend(
                found
                    .binds
                    .iter()
                    .map(|(source, relative)| Bind::Pair(source.clone(), path_join(&workdir, relative))),
            );
            binds.extend(found.interpreters.iter().map(|prefix| Bind::Same(prefix.clone())));
            used = found.binds.iter().map(|(_, relative)| relative.clone()).collect();
            // A Node runner needs a `node`: the one on PATH when it is not a system one (a mise install, resolved
            // through its symlink chain to its own directory), unless the toolchain already names one.
            let node_runner = matches!(runner.name.as_str(), "vitest" | "jest" | "npm" | "pnpm" | "yarn");
            if node_runner
                && !self
                    .toolchain
                    .iter()
                    .any(|path| exists(&join_all(&[path, "bin", "node"])))
            {
                let prefix = (self.which)("node").and_then(|node| interpreter_home(&node, self.home.as_deref()));
                if let Some(prefix) = prefix
                    && !binds.iter().any(|bind| bind.is(&prefix))
                {
                    binds.push(Bind::Same(prefix.clone()));
                    let path = format!(
                        "{}:{}",
                        path_join(&prefix, "bin"),
                        env.get("PATH").cloned().unwrap_or_default()
                    );
                    env.insert("PATH".to_string(), path);
                }
            }
            if runner.name == "pytest"
                && let Some(python) = &found.python
            {
                // The checkout's own interpreter and packages, at the same place in the export.
                command[0] = path_join(&workdir, python);
            } else if runner.name == "vitest" || runner.name == "jest" {
                for (_, relative) in &found.binds {
                    let binary = join_all(&[&checkout, relative, ".bin", &runner.name]);
                    if relative.ends_with(NODE_ENV) && lexists(&binary) {
                        let local = join_all(&[&workdir, relative, ".bin", &runner.name]);
                        command.splice(..3.min(command.len()), [local]);
                        break;
                    }
                }
            }
        }
        let argv = self.sandbox.wrap(&command, &workdir, &env, &binds, &directory);
        let began = (self.clock)();
        let launch = self.launch_env.clone().unwrap_or_else(launch_env);
        let (code, output) = self
            .executor
            .execute(&argv, &workdir, &launch, self.timeout_s)
            .map_err(TestsError::Os)?;
        let (status, mut tests) = parse_outcome(&runner.name, code, &output);
        if !directory.is_empty() {
            // Test ids are relative to where the runner ran; make them repository paths again.
            for item in &mut tests {
                item.id = format!("{directory}/{}", item.id);
            }
        }
        let passed = tests.iter().filter(|item| item.status == "passed").count();
        let failed = tests
            .iter()
            .filter(|item| item.status == "failed" || item.status == "error")
            .count();
        tests.truncate(60);
        let record = Record {
            n: self.records.len() + 1,
            kind: kind.to_string(),
            rev: which.as_str().to_string(),
            runner: runner.name.clone(),
            cwd: if directory.is_empty() {
                ".".to_string()
            } else {
                directory.clone()
            },
            command: command.join(" "),
            paths: paths.to_vec(),
            status,
            exit: code,
            tests,
            passed,
            failed,
            ms: (((self.clock)() - began) * 1000.0).trunc() as i64,
            environment: used,
            toolchain: self
                .toolchain
                .iter()
                .map(|path| format!("{}/{}", basename(&dirname(path)), basename(path)))
                .collect(),
            output: tail_chars(&output, OUTPUT_TAIL_CHARS).replace(&workdir, "."),
            mutation: None,
            caught: None,
        };
        self.records.push(record.clone());
        Ok(record)
    }

    /// Replace one line of a tracked source file in the head export, run `tests`, and restore the file. The
    /// arguments are the request's JSON values.
    pub fn mutation(
        &mut self,
        raw_path: Option<&Value>,
        line: Option<&Value>,
        replacement: Option<&Value>,
        tests: Option<&Value>,
    ) -> Result<Record, TestsError> {
        let path = match raw_path {
            Some(raw @ Value::String(_)) => Some(self.paths(Some(&Value::Array(vec![raw.clone()])))?.remove(0)),
            _ => None,
        };
        let path = path
            .filter(|path| self.tracked.contains(path))
            .ok_or_else(|| rejected("mutation_check needs the path of a tracked file"))?;
        let replacement = match replacement {
            Some(Value::String(text))
                if !text.contains('\n') && !text.contains('\0') && text.chars().count() <= MAX_REPLACEMENT_CHARS =>
            {
                text.clone()
            }
            _ => {
                return Err(rejected(format!(
                    "replacement must be one line of at most {MAX_REPLACEMENT_CHARS} characters"
                )));
            }
        };
        let test_paths = self.paths(tests)?;
        if self.records.len() >= self.limit {
            return Err(rejected(format!(
                "the limit of {} test executions per review is reached",
                self.limit
            )));
        }
        let target = path_join(&self.dir(Rev::Head)?, &path);
        let original = fs::read(&target).map_err(|e| TestsError::Os(e.to_string()))?;
        let text = String::from_utf8_lossy(&original).into_owned();
        let mut lines: Vec<String> = splitlines_keepends(&text).into_iter().map(str::to_string).collect();
        let number = line
            .and_then(py_int)
            .ok_or_else(|| rejected("mutation_check needs a line number"))?;
        if original.iter().take(8192).any(|b| *b == 0) || !(1..=lines.len() as i128).contains(&number) {
            return Err(rejected(format!("{path} has no line {}", py::fstr(line))));
        }
        let index = (number - 1) as usize;
        let current = &lines[index];
        let ending = if current.ends_with("\r\n") {
            "\r\n"
        } else if current.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let was = current.trim_end_matches(['\r', '\n']).to_string();
        lines[index] = format!("{replacement}{ending}");
        let result = fs::write(&target, lines.concat())
            .map_err(|e| TestsError::Os(e.to_string()))
            .and_then(|()| self.run(&test_paths, None, Rev::Head, "mutation"));
        let restored = fs::write(&target, &original).map_err(|e| TestsError::Os(e.to_string()));
        let mut record = result?;
        restored?;
        record.mutation = Some(Mutation {
            path,
            line: number as i64,
            was: head_chars(&was, 200),
            replacement,
        });
        // A mutant the tests do not notice is the evidence: nothing pins that line.
        record.caught = Some(record.status == "failed");
        if let Some(last) = self.records.last_mut() {
            *last = record.clone();
        }
        Ok(record)
    }

    /// Run `paths` at the head commit (one execution per runner and directory) and, where anything fails, at the
    /// base commit too: a test that fails at head and passed at base is a regression; one that failed before is not
    /// this change's.
    pub fn compare(&mut self, paths: &[String]) -> Result<Compared, TestsError> {
        let mut out = Compared::default();
        for (_, directory, relative) in self.plan(paths) {
            if self.records.len() >= self.limit {
                break;
            }
            let group: Vec<String> = relative
                .iter()
                .map(|item| {
                    if directory.is_empty() {
                        item.clone()
                    } else {
                        path_join(&directory, item)
                    }
                })
                .collect();
            let head = self.run(&group, None, Rev::Head, "automatic")?;
            out.runs.push(head.clone());
            if head.status != "failed"
                || self.base.as_deref().is_none_or(str::is_empty)
                || self.records.len() >= self.limit
            {
                continue;
            }
            let base = match self.run(&group, None, Rev::Base, "base") {
                Ok(base) => base,
                Err(TestsError::Rejected(_) | TestsError::Export(_)) => continue,
                Err(error) => return Err(error),
            };
            let passed_before: HashSet<&str> = base
                .tests
                .iter()
                .filter(|item| item.status == "passed")
                .map(|item| item.id.as_str())
                .collect();
            let mut regressed: Vec<TestResult> = head
                .tests
                .iter()
                .filter(|item| {
                    (item.status == "failed" || item.status == "error") && passed_before.contains(item.id.as_str())
                })
                .cloned()
                .collect();
            if base.status == "passed" && head.tests.is_empty() {
                // A runner without per-test results: the selection as a whole regressed.
                regressed = vec![TestResult {
                    id: group.join(" "),
                    status: "failed".to_string(),
                    detail: None,
                }];
            }
            out.regressions.extend(regressed.into_iter().map(|test| Regression {
                test,
                head: head.clone(),
                base: base.clone(),
            }));
        }
        Ok(out)
    }

    /// Serve an investigator's `run_tests` request: `{"paths": [...], "select": "name"}`.
    pub fn run_tests(&mut self, args: &Value) -> Result<Record, TestsError> {
        let paths = self.paths(args.get("paths"))?;
        self.run_value(&paths, args.get("select"), Rev::Head, "run")
    }

    /// Serve an investigator's `mutation_check` request: `{"path", "line", "replacement", "tests"}`.
    pub fn mutation_check(&mut self, args: &Value) -> Result<Record, TestsError> {
        self.mutation(
            args.get("path"),
            args.get("line"),
            args.get("replacement"),
            args.get("tests"),
        )
    }

    /// The deep pass's reply to a test request (`kind` is "run_tests" or "mutation_check"): ("test run N", the
    /// summary). Refusals come back as `TestsError::Rejected`, whose message goes back to the investigator.
    pub fn serve(&mut self, kind: &str, args: &Value) -> Result<(String, String), TestsError> {
        let record = match kind {
            "run_tests" => self.run_tests(args)?,
            "mutation_check" => self.mutation_check(args)?,
            other => return Err(rejected(format!("unknown test request {}", py::repr(other)))),
        };
        Ok((format!("test run {}", record.n), summarize(&record)))
    }
}

/// Python's `int(value)` for a JSON value: ints, floats truncated, booleans, and decimal strings (surrounding
/// whitespace, a sign, `_` between digits). None where Python raised `TypeError` or `ValueError`.
fn py_int(value: &Value) -> Option<i128> {
    match value {
        Value::Bool(b) => Some(i128::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i128)),
        Value::String(text) => {
            let text = py::strip(text);
            let (negative, digits) = match text.as_bytes().first() {
                Some(b'-') => (true, &text[1..]),
                Some(b'+') => (false, &text[1..]),
                _ => (false, text),
            };
            let bytes = digits.as_bytes();
            let valid = !bytes.is_empty()
                && bytes.first().is_some_and(u8::is_ascii_digit)
                && bytes.last().is_some_and(u8::is_ascii_digit)
                && bytes.iter().all(|b| b.is_ascii_digit() || *b == b'_')
                && !digits.contains("__");
            if !valid {
                return None;
            }
            let number = digits.bytes().filter(u8::is_ascii_digit).fold(0i128, |acc, b| {
                acc.saturating_mul(10).saturating_add(i128::from(b - b'0'))
            });
            Some(if negative { -number } else { number })
        }
        _ => None,
    }
}

/// `str.splitlines(keepends=True)`.
pub fn splitlines_keepends(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if !matches!(
            c,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        ) {
            continue;
        }
        let mut end = index + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
            chars.next();
            end += 1;
        }
        lines.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// One run for an investigator or the brief: what ran, the outcome, the failing tests, the trimmed output.
pub fn summarize(record: &Record) -> String {
    summarize_value(&serde_json::to_value(record).unwrap_or(Value::Null))
}

/// [`summarize`] over a run record as the kernel API passes it (a JSON object: `n`, `kind`, `rev`, `command`,
/// `cwd`, `status`, `tests`, `passed`, `failed`, `output`, `mutation`, `caught`), read as the Python read the dict.
pub fn summarize_value(record: &Value) -> String {
    let get = |key: &str| record.get(key);
    let fstr = |key: &str| py::fstr(get(key));
    let place = match get("cwd") {
        None => String::new(),
        Some(cwd) if py::str_of(cwd) == "." => String::new(),
        Some(_) => format!(" in {}", fstr("cwd")),
    };
    let status = fstr("status");
    let tests: &[Value] = get("tests").and_then(Value::as_array).map_or(&[], Vec::as_slice);
    let counts = if py_truthy(get("tests")) {
        format!(" ({} passed, {} failed)", fstr("passed"), fstr("failed"))
    } else {
        String::new()
    };
    let mut lines = vec![format!(
        "run {} ({}, at the {} commit, sandboxed, no network): `{}`{place} -> {status}{counts}",
        fstr("n"),
        fstr("kind"),
        fstr("rev"),
        fstr("command"),
    )];
    if py_truthy(get("mutation")) {
        let change = get("mutation").cloned().unwrap_or(Value::Null);
        let verdict = if py_truthy(get("caught")) {
            "the tests caught it"
        } else if status == "passed" {
            "the tests still pass (nothing pins this line)"
        } else {
            "no verdict"
        };
        lines.push(format!(
            "  with {}:{} changed from `{}` to `{}`: {verdict}",
            py::fstr(change.get("path")),
            py::fstr(change.get("line")),
            py::fstr(change.get("was")),
            py::fstr(change.get("replacement")),
        ));
    }
    if status == "unavailable" {
        lines.push(
            "  the tests could not run (missing dependencies or nothing collected; the sandbox has no network and \
             nothing is installed): this says nothing about the code"
                .to_string(),
        );
    }
    for item in tests
        .iter()
        .filter(|item| py::fstr(item.get("status")) != "passed")
        .take(8)
    {
        let detail = if py_truthy(item.get("detail")) {
            format!(" - {}", py::fstr(item.get("detail")))
        } else {
            String::new()
        };
        lines.push(format!(
            "  {}: {}{detail}",
            py::fstr(item.get("status")),
            py::fstr(item.get("id"))
        ));
    }
    if matches!(status.as_str(), "failed" | "timeout" | "unavailable") {
        let output = fstr("output");
        let all = py::splitlines(&output);
        let tail: Vec<String> = all[all.len().saturating_sub(25)..]
            .iter()
            .map(|line| format!("    {line}"))
            .collect();
        lines.push(format!("  output (end):\n{}", tail.join("\n")));
    }
    lines.join("\n")
}

// --- Command line -------------------------------------------------------------------------------------------------

/// `json.dumps(value)`: Python's default separators and `ensure_ascii`.
pub fn dumps(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => py::float_repr(f),
            _ => n.to_string(),
        },
        Value::String(text) => {
            let mut out = String::from("\"");
            for c in text.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0c}' => out.push_str("\\f"),
                    ' '..='~' => out.push(c),
                    _ => {
                        let mut units = [0u16; 2];
                        for unit in c.encode_utf16(&mut units) {
                            out.push_str(&format!("\\u{unit:04x}"));
                        }
                    }
                }
            }
            out.push('"');
            out
        }
        Value::Array(items) => format!("[{}]", items.iter().map(dumps).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!("{}: {}", dumps(&Value::String(key.clone())), dumps(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What the `autoreview_tests.py` command line printed and its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The command line: `doctor [docker-image]` prints the doctor report, `environments <checkout>` the checkout's
/// prepared environments, as JSON; anything else is the usage on stderr with status 2. `doctor` is the report
/// function (`doctor` for this machine).
pub fn command_line(args: &[String], doctor: &dyn Fn(Option<&str>) -> Result<Value, String>) -> CliOutput {
    let printed = |value: Value| CliOutput {
        code: 0,
        stdout: format!("{}\n", dumps(&value)),
        stderr: String::new(),
    };
    match args.first().map(String::as_str) {
        Some("doctor") => match doctor(args.get(1).map(String::as_str)) {
            Ok(report) => printed(report),
            Err(error) => CliOutput {
                code: 1,
                stdout: String::new(),
                stderr: format!("{error}\n"),
            },
        },
        Some("environments") if args.len() == 2 => printed(Value::Array(list_environments(&args[1], 3, None))),
        _ => CliOutput {
            code: 2,
            stdout: String::new(),
            stderr: "usage: autoreview_tests.py doctor [docker-image]\n".to_string(),
        },
    }
}
