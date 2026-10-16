//! Building prepared test environments (`autoreview/prepare.ts`: `resolveToolchain`, `prepareEnvironment`,
//! `cachedEnvironment`): toolchains resolved (and, when missing, installed) with mise, then the Python virtualenv
//! (uv) and/or the Node modules (npm, pnpm or yarn by lockfile), into `<cache>/envs/<hash>/` with a `toolchain.json`
//! record. Every process goes through the injected [`Runner`]; nothing is installed during a review, only before it.
//! Failures are listed in the result (a limit the review states), never an error; an error is only a file the cache
//! directory could not be written to.
//!
//! Known differences from the TypeScript, outside recorded behaviour:
//! - `pick` orders versions with a numeric, case-insensitive comparison (digit runs by value, other text by
//!   lowercased characters) where the TypeScript used ICU's `localeCompare(..., { numeric: true })`; they agree on
//!   version strings (digits, dots, letters) and may differ on other punctuation.
//! - A cached or listed record whose fields have the wrong types reads them as empty, where the TypeScript would throw
//!   later; an entry of `mise ls --json` whose `version` is not a string never matches.
//! - A cache directory that cannot be written is an `Err` with the OS error's text, not Node's error message.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::github::iso_from_ms;
use crate::js::{head16, path_join, trim};
use crate::prepare::{NODE_LOCKS, ToolchainEntry, ToolchainRequests, detect_ecosystems, environment_hash, find_mise};
use crate::reviewer::{EnvironmentPreparer, PreparedEnvironment};
use crate::runner::{RunOptions, RunResult, Runner};
use crate::settings::Mise;
use crate::state::to_json_1;

/// `PrepareOptions` of prepare.ts.
pub struct PrepareOptions<'a> {
    pub runner: &'a dyn Runner,
    /// `<cache>/envs` lives under this directory (`autoreviewPaths().cache`).
    pub cache_dir: String,
    /// `--python 3.x`: the Python version to build the virtualenv with (overrides the repository's).
    pub python: Option<String>,
    /// `autoreview.mise`: use mise to resolve and install toolchains (default: when a mise binary is found).
    pub mise: Option<Mise>,
    /// The environment PATH and the binaries are looked up in; also handed to mise.
    pub env: HashMap<String, String>,
    pub home: String,
    pub log: &'a dyn Fn(&str),
    pub timeout_ms: Option<u64>,
    /// Rebuild even when the cache has this hash.
    pub force: bool,
    /// Epoch milliseconds (`Date.now()`).
    pub now: &'a dyn Fn() -> i64,
}

/// What `prepareEnvironment` returns: the typed fields and the whole object as the TypeScript returns it (the
/// record written to `toolchain.json`, or a cached record with `dir`, `hash`, `cached` and `ms` replaced), which
/// `--json` prints.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub dir: String,
    pub hash: String,
    pub prepared: Vec<String>,
    pub cached: bool,
    pub toolchain: Vec<ToolchainEntry>,
    pub failures: Vec<String>,
    pub ms: i64,
    pub json: Value,
}

impl Prepared {
    /// Read from a record object (cached ones may have any shape: wrong types read as empty).
    fn from_record(json: Value) -> Self {
        let text = |key: &str| json.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let strings = |key: &str| -> Vec<String> {
            json.get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let toolchain = json
            .get("toolchain")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<ToolchainEntry>(item.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Prepared {
            dir: text("dir"),
            hash: text("hash"),
            prepared: strings("prepared"),
            cached: json.get("cached").and_then(Value::as_bool).unwrap_or(false),
            toolchain,
            failures: strings("failures"),
            ms: json.get("ms").and_then(Value::as_i64).unwrap_or(0),
            json,
        }
    }

    /// What the reviewer needs of it.
    pub fn summary(&self) -> PreparedEnvironment {
        PreparedEnvironment {
            hash: self.hash.clone(),
            cached: self.cached,
            dir: self.dir.clone(),
            prepared: self.prepared.clone(),
            failures: self.failures.clone(),
            toolchain: self.toolchain.iter().map(|entry| entry.path.clone()).collect(),
        }
    }
}

/// JavaScript truthiness of a parsed JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `(result.stderr || result.stdout).trim().slice(0, max)`.
fn output_head(result: &RunResult, max: usize) -> String {
    let text = if result.stderr.is_empty() {
        &result.stdout
    } else {
        &result.stderr
    };
    head16(trim(text), max).to_string()
}

/// Node's `path.dirname` for the paths used here.
fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(index) => path[..index].to_string(),
        None => ".".into(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

fn realpath(path: &str) -> Option<String> {
    fs::canonicalize(path)
        .ok()
        .map(|real| real.to_string_lossy().into_owned())
}

/// `mise ls --json`: installed versions per tool (an object), else nothing.
fn mise_installed(runner: &dyn Runner, mise: &str, env: &HashMap<String, String>) -> Map<String, Value> {
    let result = runner.run(
        &[mise.to_string(), "ls".into(), "--json".into()],
        &RunOptions {
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            timeout_ms: Some(60_000),
            ..RunOptions::default()
        },
    );
    if result.code != 0 {
        return Map::new();
    }
    match serde_json::from_str::<Value>(&result.stdout) {
        Ok(Value::Object(parsed)) => parsed,
        _ => Map::new(),
    }
}

/// Compare like `localeCompare(b, undefined, { numeric: true, sensitivity: "base" })` does on version strings.
fn compare_numeric(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn runs(text: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in text.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((kind, run)) if *kind == digit => run.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (left, right) = (runs(a), runs(b));
    for (x, y) in left.iter().zip(&right) {
        let order = match (x.0, y.0) {
            (true, true) => {
                let (p, q) = (x.1.trim_start_matches('0'), y.1.trim_start_matches('0'));
                p.len().cmp(&q.len()).then_with(|| p.cmp(q))
            }
            // A digit sorts before a letter or punctuation.
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => x.1.to_lowercase().cmp(&y.1.to_lowercase()),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

/// The installed version satisfying `request` (a version or prefix such as "3.12" or "20"), the newest.
fn pick(entries: Option<&Value>, request: &str) -> Option<(String, String)> {
    let stripped =
        request.trim_start_matches(|c: char| matches!(c, '~' | '^' | '>' | '=' | '<' | 'v') || crate::js::is_space(c));
    let wanted = stripped
        .split(|c: char| c == ',' || c == '<' || crate::js::is_space(c))
        .next()
        .unwrap_or(stripped);
    let mut candidates: Vec<(String, String)> = entries
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|entry| entry.get("installed") != Some(&Value::Bool(false)))
                .filter_map(|entry| {
                    let version = entry.get("version")?.as_str()?;
                    let path = entry.get("install_path").and_then(Value::as_str);
                    let matches = version == wanted || version.starts_with(&format!("{wanted}."));
                    (matches && path.is_some_and(exists))
                        .then(|| (version.to_string(), path.unwrap_or_default().to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    candidates.sort_by(|a, b| compare_numeric(&b.0, &a.0));
    candidates.into_iter().next()
}

/// The install prefix of a binary on PATH: a system one (`/usr`, `/bin`, ...), or a managed one (`<prefix>/bin`
/// with a `lib` beside it).
fn system_or_managed(binary: &str, env: &HashMap<String, String>) -> Option<(String, &'static str)> {
    let system = Regex::new(r"^/(usr|bin|sbin|lib)(/|$)").expect("static pattern");
    let path = env.get("PATH").map(String::as_str).unwrap_or("");
    for dir in path.split(':') {
        let candidate = path_join(&[dir, binary]);
        if !exists(&candidate) {
            continue;
        }
        let Some(real) = realpath(&candidate) else {
            continue;
        };
        let prefix = dirname(&dirname(&real));
        if system.is_match(&real) {
            return Some((prefix, "system"));
        }
        if basename(&dirname(&real)) == "bin" && exists(&path_join(&[&prefix, "lib"])) {
            return Some((prefix, "mise"));
        }
    }
    None
}

/// Resolve (and, when missing, install with mise) the toolchain the repository asks for: the entries and the
/// failures. An entry's `path` is the exact install directory the sandbox binds.
pub fn resolve_toolchain(
    options: &PrepareOptions,
    requests: &ToolchainRequests,
    needs_python: bool,
    needs_node: bool,
) -> (Vec<ToolchainEntry>, Vec<String>) {
    let env = &options.env;
    let mut entries = Vec::new();
    let mut failures = Vec::new();
    let mise = find_mise(env, &options.home, options.mise.as_ref());
    let mut installed = match &mise {
        Some(mise) => mise_installed(options.runner, mise, env),
        None => Map::new(),
    };
    for (tool, needed, request) in [
        ("python", needs_python, &requests.python),
        ("node", needs_node, &requests.node),
    ] {
        if !needed {
            continue;
        }
        let request = request.as_deref().filter(|request| !request.is_empty());
        if let (Some(mise), Some(request)) = (&mise, request) {
            let mut chosen = pick(installed.get(tool), request);
            let mut was_installed = false;
            if chosen.is_none() {
                (options.log)(&format!("prepare: mise install {tool}@{request}"));
                let result = options.runner.run(
                    &[mise.clone(), "install".into(), format!("{tool}@{request}")],
                    &RunOptions {
                        env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        timeout_ms: Some(options.timeout_ms.unwrap_or(600_000)),
                        ..RunOptions::default()
                    },
                );
                if result.code != 0 {
                    failures.push(format!(
                        "mise install {tool}@{request} failed: {}",
                        output_head(&result, 200)
                    ));
                } else {
                    was_installed = true;
                    installed = mise_installed(options.runner, mise, env);
                    chosen = pick(installed.get(tool), request);
                }
            }
            if let Some((version, path)) = chosen {
                entries.push(ToolchainEntry {
                    tool: tool.into(),
                    version,
                    path: realpath(&path).unwrap_or(path),
                    source: "mise".into(),
                    installed: was_installed.then_some(true),
                });
                continue;
            }
        }
        match system_or_managed(if tool == "python" { "python3" } else { "node" }, env) {
            Some((path, source)) => entries.push(ToolchainEntry {
                tool: tool.into(),
                version: request.unwrap_or_default().to_string(),
                path,
                source: source.into(),
                installed: None,
            }),
            None => failures.push(format!(
                "no {tool} found on PATH{}",
                request.map_or(String::new(), |request| format!(" for the requested {request}"))
            )),
        }
    }
    (entries, failures)
}

fn binary_of(entry: Option<&ToolchainEntry>, name: &str) -> Option<String> {
    let path = path_join(&[&entry?.path, "bin", name]);
    exists(&path).then_some(path)
}

fn io_error(what: &str, path: &str, error: std::io::Error) -> String {
    format!("{what} {path}: {error}")
}

/// The cached record of `dir` when it says it succeeded.
fn ok_record(record_path: &str) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(record_path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(record)) if truthy(record.get("ok")) => Some(record),
        _ => None,
    }
}

/// Prepare the environment of `repo_dir`: toolchains, then the Python virtualenv and/or the Node modules, into
/// `<cache_dir>/envs/<hash>/`. Cached when that hash's `toolchain.json` says it succeeded.
pub fn prepare_environment(repo_dir: &str, options: &PrepareOptions) -> Result<Prepared, String> {
    let started = (options.now)();
    let env = &options.env;
    let repo = Path::new(repo_dir);
    let found = detect_ecosystems(repo);
    let (needs_python, needs_node) = (!found.python.is_empty(), !found.node.is_empty());
    let hash = environment_hash(repo, options.python.as_deref());
    let dir = path_join(&[&options.cache_dir, "envs", &hash]);
    let record_path = path_join(&[&dir, "toolchain.json"]);
    if !options.force
        && exists(&record_path)
        && let Some(mut record) = ok_record(&record_path)
    {
        record.insert("dir".into(), json!(dir));
        record.insert("hash".into(), json!(hash));
        record.insert("cached".into(), json!(true));
        record.insert("ms".into(), json!((options.now)() - started));
        return Ok(Prepared::from_record(Value::Object(record)));
    }
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("cannot remove", &dir, error)),
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&dir)
            .map_err(|error| io_error("cannot create", &dir, error))?;
    }
    let mut prepared: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut requests = crate::prepare::toolchain_requests(repo);
    if let Some(python) = options.python.as_ref().filter(|python| !python.is_empty()) {
        requests.python = Some(python.clone());
    }
    let (toolchain, toolchain_failures) = resolve_toolchain(options, &requests, needs_python, needs_node);
    failures.extend(toolchain_failures);
    let python = toolchain.iter().find(|entry| entry.tool == "python");
    let timeout_ms = options.timeout_ms.unwrap_or(900_000);
    let local_bin = path_join(&[&options.home, ".local", "bin"]);
    let env_path = env.get("PATH").cloned().unwrap_or_default();
    // The toolchain's bin directories first on PATH for the build tools below.
    let mut path_parts: Vec<String> = toolchain.iter().map(|entry| path_join(&[&entry.path, "bin"])).collect();
    path_parts.push(local_bin.clone());
    path_parts.push(env_path.clone());
    let run_env: BTreeMap<String, String> = [
        ("PATH", path_parts.join(":")),
        ("UV_NO_PROGRESS", "1".into()),
        ("npm_config_update_notifier", "false".into()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    let run = |argv: Vec<String>, cwd: &str, extra: &[(&str, &str)]| {
        let mut env = run_env.clone();
        for (key, value) in extra {
            env.insert(key.to_string(), value.to_string());
        }
        options.runner.run(
            &argv,
            &RunOptions {
                cwd: Some(cwd.to_string()),
                env,
                input: None,
                timeout_ms: Some(timeout_ms),
            },
        )
    };
    let uv = env_path
        .split(':')
        .map(String::from)
        .chain(std::iter::once(local_bin))
        .map(|item| path_join(&[&item, "uv"]))
        .find(|candidate| exists(candidate));

    if needs_python {
        match &uv {
            None => failures.push(
                "uv is not installed (https://docs.astral.sh/uv/): the Python environment was not prepared".into(),
            ),
            Some(uv) => {
                let venv = path_join(&[&dir, ".venv"]);
                let python_arg = binary_of(python, "python3")
                    .or_else(|| binary_of(python, "python"))
                    .or_else(|| requests.python.clone().filter(|python| !python.is_empty()));
                let mut argv = vec![uv.clone(), "venv".into(), venv.clone()];
                if let Some(python_arg) = python_arg {
                    argv.extend(["--python".into(), python_arg]);
                }
                let created = run(argv, &dir, &[]);
                if created.code != 0 {
                    failures.push(format!("uv venv failed: {}", output_head(&created, 300)));
                } else {
                    let interpreter = path_join(&[&venv, "bin", "python"]);
                    let pip = |args: &[&str]| {
                        let mut argv = vec![
                            uv.clone(),
                            "pip".into(),
                            "install".into(),
                            "--python".into(),
                            interpreter.clone(),
                        ];
                        argv.extend(args.iter().map(|arg| arg.to_string()));
                        run(argv, repo_dir, &[])
                    };
                    let mut ok = false;
                    let has = |name: &str| found.python.iter().any(|item| item == name);
                    if has("uv.lock") && has("pyproject.toml") {
                        let synced = run(
                            vec![
                                uv.clone(),
                                "sync".into(),
                                "--frozen".into(),
                                "--all-extras".into(),
                                "--project".into(),
                                repo_dir.into(),
                            ],
                            repo_dir,
                            &[("UV_PROJECT_ENVIRONMENT", &venv)],
                        );
                        if synced.code == 0 {
                            prepared.push("python: uv sync --frozen --all-extras (uv.lock)".into());
                            ok = true;
                        } else {
                            failures.push(format!("uv sync failed: {}", output_head(&synced, 300)));
                        }
                    } else if has("pyproject.toml") || has("setup.py") {
                        for suffix in ["[dev,test]", "[dev]", "[test]", ""] {
                            let result = pip(&["-e", &format!("{repo_dir}{suffix}")]);
                            if result.code == 0 {
                                prepared.push(format!("python: uv pip install -e .{suffix}"));
                                ok = true;
                                break;
                            }
                            if suffix.is_empty() {
                                failures.push(format!("uv pip install -e . failed: {}", output_head(&result, 300)));
                            }
                        }
                    }
                    // Sub-projects (a pyproject or setup.py below the root): installed into the same environment,
                    // editable.
                    let manifest = Regex::new(r"(^|/)(pyproject\.toml|setup\.py)$").expect("static pattern");
                    for name in found
                        .python
                        .iter()
                        .filter(|name| name.contains('/') && manifest.is_match(name))
                    {
                        let sub = dirname(name);
                        let project = path_join(&[repo_dir, &sub]);
                        for suffix in ["[dev,test]", "[dev]", "[test]", ""] {
                            let result = pip(&["-e", &format!("{project}{suffix}")]);
                            if result.code == 0 {
                                prepared.push(format!("python: uv pip install -e {sub}{suffix}"));
                                ok = true;
                                break;
                            }
                            if suffix.is_empty() {
                                failures.push(format!("uv pip install -e {sub} failed: {}", output_head(&result, 300)));
                            }
                        }
                    }
                    let listed = Regex::new(r"requirements.*\.txt$|requirements/").expect("static pattern");
                    let requirements: Vec<&String> = found.python.iter().filter(|name| listed.is_match(name)).collect();
                    for file in &requirements {
                        let result = pip(&["-r", &path_join(&[repo_dir, file])]);
                        if result.code == 0 {
                            prepared.push(format!("python: uv pip install -r {file}"));
                            ok = true;
                        } else {
                            failures.push(format!(
                                "uv pip install -r {file} failed: {}",
                                output_head(&result, 300)
                            ));
                        }
                    }
                    if !ok && prepared.is_empty() && requirements.is_empty() {
                        prepared.push("python: .venv (no dependency list found)".into());
                    }
                }
            }
        }
    }
    if needs_node {
        match NODE_LOCKS
            .iter()
            .find(|name| found.node.iter().any(|item| item == *name))
        {
            None => failures.push(
                "no Node lockfile (package-lock.json, pnpm-lock.yaml, yarn.lock): node_modules was not prepared".into(),
            ),
            Some(lock) => {
                for name in ["package.json", lock, ".npmrc"] {
                    let from = path_join(&[repo_dir, name]);
                    if exists(&from) {
                        let to = path_join(&[&dir, name]);
                        fs::copy(&from, &to).map_err(|error| io_error("cannot copy", &from, error))?;
                    }
                }
                let argv: &[&str] = match *lock {
                    "pnpm-lock.yaml" => &["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"],
                    "yarn.lock" => &[
                        "yarn",
                        "install",
                        "--frozen-lockfile",
                        "--ignore-scripts",
                        "--non-interactive",
                    ],
                    _ => &["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"],
                };
                let line = argv.join(" ");
                let result = run(argv.iter().map(|arg| arg.to_string()).collect(), &dir, &[]);
                if result.code == 0 {
                    prepared.push(format!("node: {line} ({lock})"));
                } else {
                    failures.push(format!("{line} failed: {}", output_head(&result, 300)));
                }
            }
        }
    }
    let ok = !prepared.is_empty() && failures.is_empty();
    let now = (options.now)();
    let record = json!({
        "dir": dir,
        "hash": hash,
        "prepared": prepared,
        "cached": false,
        "toolchain": toolchain,
        "failures": failures,
        "ms": now - started,
        "ok": ok,
        "at": iso_from_ms(now),
        "repo": repo_dir,
    });
    write_record(&record_path, &to_json_1(&record))?;
    Ok(Prepared::from_record(record))
}

fn write_record(path: &str, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| io_error("cannot write", path, error))
}

/// The prepared environment of `repo_dir` already in the cache, if any (no build): the record as written, with
/// `cached` true.
pub fn cached_environment(repo_dir: &str, cache_dir: &str, python: Option<&str>) -> Option<Prepared> {
    let hash = environment_hash(Path::new(repo_dir), python);
    let record_path = path_join(&[cache_dir, "envs", &hash, "toolchain.json"]);
    let mut record = ok_record(&record_path)?;
    record.insert("cached".into(), json!(true));
    Some(Prepared::from_record(Value::Object(record)))
}

/// The daemon's [`EnvironmentPreparer`]: `prepareEnvironment` over the injected runner, with the cache, the mise
/// setting, the environment and the home the service runs with.
pub struct RunnerPreparer {
    pub runner: std::sync::Arc<dyn Runner>,
    pub cache_dir: String,
    pub mise: Option<Mise>,
    pub env: HashMap<String, String>,
    pub home: String,
    pub now: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl EnvironmentPreparer for RunnerPreparer {
    fn prepare(&self, work_dir: &str, log: &dyn Fn(&str)) -> Result<PreparedEnvironment, String> {
        let now = || (self.now)();
        let options = PrepareOptions {
            runner: &*self.runner,
            cache_dir: self.cache_dir.clone(),
            python: None,
            mise: self.mise.clone(),
            env: self.env.clone(),
            home: self.home.clone(),
            log,
            timeout_ms: None,
            force: false,
            now: &now,
        };
        prepare_environment(work_dir, &options).map(|prepared| prepared.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_prepare_env_versions_order_numerically() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_numeric("3.12.10", "3.12.9"), Greater);
        assert_eq!(compare_numeric("3.12", "3.12.1"), Less);
        assert_eq!(compare_numeric("20.0", "20.0"), Equal);
        assert_eq!(compare_numeric("3.9", "3.10"), Less);
        assert_eq!(compare_numeric("1.0RC", "1.0rc"), Equal);
    }

    #[test]
    fn review_prepare_env_dirname_like_node() {
        assert_eq!(dirname("/usr/bin/env"), "/usr/bin");
        assert_eq!(dirname("/usr"), "/");
        assert_eq!(dirname("x"), ".");
    }
}
