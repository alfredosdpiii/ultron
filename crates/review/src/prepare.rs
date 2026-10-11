//! Dependency discovery for prepared test environments (`autoreview/prepare.ts`): which ecosystems a repository has
//! and the files that define them, the toolchain versions it asks for, the content hash that names its cache entry,
//! and the mise binary to use. Building the environment (uv, npm, mise install) runs processes and stays with the
//! host's process supervision; everything here only reads files.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::{cmp16, number, path_join, trim};
use crate::settings::Mise;

const PYTHON: [&str; 6] = [
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "requirements-dev.txt",
    "dev-requirements.txt",
];
const PYTHON_LOCKS: [&str; 5] = [
    "uv.lock",
    "poetry.lock",
    "requirements.txt",
    "requirements-dev.txt",
    "dev-requirements.txt",
];
const NODE: [&str; 1] = ["package.json"];
pub const NODE_LOCKS: [&str; 4] = [
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "npm-shrinkwrap.json",
];
const TOOLS: [&str; 6] = [
    "mise.toml",
    ".mise.toml",
    ".tool-versions",
    ".python-version",
    ".nvmrc",
    ".node-version",
];

/// Directories the walk for nested requirements files and pyprojects does not enter (hidden ones neither).
const NOT_A_SERVICE_DIR: [&str; 12] = [
    "node_modules",
    "venv",
    "dist",
    "build",
    "vendor",
    "docs",
    "mockups",
    "requirements",
    "site-packages",
    "__pycache__",
    "target",
    "coverage",
];
/// How many directory levels below the root the walk goes.
const NESTED_DEPTH: usize = 4;

/// A file's text, decoded lossily; none when it cannot be read (missing, a directory).
fn read(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The files that define each ecosystem, relative to the repository root.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ecosystems {
    pub python: Vec<String>,
    pub node: Vec<String>,
    pub tools: Vec<String>,
}

/// Which ecosystems a repository has, and the files that define them. Services kept below the root (`backend/`,
/// `services/api/`) with their own requirements files or pyprojects are found by a bounded walk that leaves out
/// build, vendor and hidden directories; everything found is installed into the one environment and hashed into
/// its key.
pub fn detect_ecosystems(repo: &Path) -> Ecosystems {
    let present = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .filter(|name| repo.join(name).exists())
            .map(|name| name.to_string())
            .collect()
    };
    let requirements_dir = repo.join("requirements");
    let mut extra: Vec<String> = Vec::new();
    if requirements_dir.exists() {
        let mut names: Vec<String> = fs::read_dir(&requirements_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        // Node's readdir lists names in byte order.
        names.sort();
        extra = names
            .into_iter()
            .filter(|name| name.ends_with(".txt"))
            .map(|name| format!("requirements/{name}"))
            .collect();
    }
    let mut nested = Vec::new();
    walk(repo, "", 0, &mut nested);
    nested.sort_by(|a, b| cmp16(a, b));
    let mut python = present(&PYTHON);
    python.extend(present(&PYTHON_LOCKS));
    python.extend(extra);
    python.extend(nested);
    let mut unique: Vec<String> = Vec::new();
    for item in python {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    let mut node = present(&NODE);
    node.extend(present(&NODE_LOCKS));
    Ecosystems {
        python: unique,
        node,
        tools: present(&TOOLS),
    }
}

fn walk(repo: &Path, dir: &str, depth: usize, nested: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(repo.join(dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if dir.is_empty() {
            name.clone()
        } else {
            format!("{dir}/{name}")
        };
        // The entry's own type: a symlink to a directory is not walked.
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir {
            if depth < NESTED_DEPTH && !name.starts_with('.') && !NOT_A_SERVICE_DIR.contains(&name.as_str()) {
                walk(repo, &relative, depth + 1, nested);
            }
        } else if !dir.is_empty()
            && ((name.starts_with("requirements") && name.ends_with(".txt") && name.len() >= "requirements.txt".len())
                || name == "pyproject.toml"
                || name == "setup.py")
        {
            nested.push(relative);
        }
    }
}

/// What the repository asks for: a Python and a Node version constraint, if any.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolchainRequests {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// JavaScript's `\s` as a regex class.
const S: &str = r"[\t\n\x0B\f\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";

fn regex(pattern: &str) -> Regex {
    Regex::new(&pattern.replace(r"\s", S)).unwrap_or_else(|error| panic!("pattern {pattern}: {error}"))
}

/// The version requests, first source wins: `mise.toml` (or `.mise.toml`) `[tools]`, `.tool-versions`,
/// `.python-version`, `.nvmrc` (or `.node-version`), `requires-python` (its lowest named minor), `engines.node`
/// (its first number).
pub fn toolchain_requests(repo: &Path) -> ToolchainRequests {
    let mut out = ToolchainRequests::default();
    let toml = read(&repo.join("mise.toml")).or_else(|| read(&repo.join(".mise.toml")));
    if let Some(toml) = toml.filter(|text| !text.is_empty()) {
        let tools = regex(r"(?s)\[tools\](.*?)(?:\n\[|\z)")
            .captures(&toml)
            .map(|found| found[1].to_string())
            .unwrap_or_default();
        let value = |pattern: &str| regex(pattern).captures(&tools).map(|found| found[1].to_string());
        out.python = value(r#"(?m)^\s*python\s*=\s*["']([^"']+)["']"#);
        out.node = value(r#"(?m)^\s*node(?:js)?\s*=\s*["']([^"']+)["']"#);
    }
    if let Some(versions) = read(&repo.join(".tool-versions")).filter(|text| !text.is_empty()) {
        for line in versions.split('\n') {
            let mut words = trim(line).split(crate::js::is_space).filter(|word| !word.is_empty());
            // `"".split(/\s+/)` is `[""]`: an empty line names no tool.
            let (tool, version) = (words.next().unwrap_or(""), words.next());
            if let Some(version) = version {
                if tool == "python" && out.python.is_none() {
                    out.python = Some(version.to_string());
                }
                if (tool == "node" || tool == "nodejs") && out.node.is_none() {
                    out.node = Some(version.to_string());
                }
            }
        }
    }
    if out.python.is_none()
        && let Some(text) = read(&repo.join(".python-version"))
    {
        let first = trim(trim(&text).split('\n').next().unwrap_or(""));
        if !first.is_empty() {
            out.python = Some(first.to_string());
        }
    }
    if let Some(text) = read(&repo.join(".nvmrc")).or_else(|| read(&repo.join(".node-version"))) {
        let version = trim(&text);
        let version = version.strip_prefix('v').unwrap_or(version);
        if !version.is_empty() && out.node.is_none() {
            out.node = Some(version.to_string());
        }
    }
    if out.python.is_none()
        && let Some(pyproject) = read(&repo.join("pyproject.toml"))
        && let Some(requires) = regex(r#"(?m)^\s*requires-python\s*=\s*["']([^"']+)["']"#).captures(&pyproject)
        && let Some(minimum) = regex(r"([0-9]+)\.([0-9]+)").captures(&requires[1])
    {
        out.python = Some(format!("{}.{}", &minimum[1], &minimum[2]));
    }
    if out.node.is_none()
        && let Some(package) = read(&repo.join("package.json")).filter(|text| !text.is_empty())
        && let Ok(parsed) = serde_json::from_str::<Value>(&package)
        && let Some(engines) = parsed
            .get("engines")
            .and_then(|engines| engines.get("node"))
            .filter(|node| truthy(node))
        && let Some(major) = regex("([0-9]+)").captures(&js_string(engines))
    {
        out.node = Some(major[1].to_string());
    }
    out
}

/// JavaScript truthiness of a parsed JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `String(value)` for a parsed JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(n) => n.as_f64().map(number).unwrap_or_default(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// The content hash that names the cache entry: every detected file's name and text (sorted by name), and the
/// explicit Python. The first 16 hex digits of a SHA-256.
pub fn environment_hash(repo: &Path, python: Option<&str>) -> String {
    let found = detect_ecosystems(repo);
    let mut names: Vec<&String> = found.python.iter().chain(&found.node).chain(&found.tools).collect();
    names.sort_by(|a, b| cmp16(a, b));
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for name in names {
        context.update(format!("{name}\n").as_bytes());
        context.update(read(&repo.join(name)).unwrap_or_default().as_bytes());
        context.update(b"\n");
    }
    context.update(format!("python={}\n", python.unwrap_or("")).as_bytes());
    let digest = context.finish();
    digest.as_ref()[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The mise binary to use: the setting as a path, else the first `mise` on PATH, under `~/.local/bin`, or in
/// `/usr/bin` or `/usr/local/bin`. None when the setting is `false` or nothing is found.
pub fn find_mise(env: &HashMap<String, String>, home: &str, setting: Option<&Mise>) -> Option<String> {
    match setting {
        Some(Mise::Enabled(false)) => return None,
        Some(Mise::Path(path)) if !trim(path).is_empty() && path != "true" => return Some(path.clone()),
        _ => {}
    }
    let path = env.get("PATH").map(String::as_str).unwrap_or("");
    path.split(':')
        .map(|dir| path_join(&[dir, "mise"]))
        .chain([
            path_join(&[home, ".local", "bin", "mise"]),
            "/usr/bin/mise".into(),
            "/usr/local/bin/mise".into(),
        ])
        .find(|candidate| candidate != "mise" && Path::new(candidate).exists())
}

/// One toolchain the sandbox binds: the version and the exact install prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolchainEntry {
    pub tool: String,
    pub version: String,
    pub path: String,
    /// `mise`, `system` or `uv`.
    pub source: String,
    /// True when `mise install` ran for it during this prepare.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<bool>,
}

/// One line per toolchain entry: the version and where it came from.
pub fn describe_toolchain(entries: &[ToolchainEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{} {} from {}{}: {}",
                entry.tool,
                if entry.version.is_empty() {
                    "(version not pinned)"
                } else {
                    &entry.version
                },
                entry.source,
                if entry.installed == Some(true) {
                    " (installed by prepare)"
                } else {
                    ""
                },
                entry.path
            )
        })
        .collect()
}
