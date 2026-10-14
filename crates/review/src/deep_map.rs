//! The deep pass's map (`autoreview_deep.py` `Brief`, `_add`, `extract`, `build_brief`): what the diff defines,
//! changes, calls and claims, and where those names live in the rest of the repository at the head commit,
//! rendered as a bounded investigation brief with file:line anchors. No model runs here.
//!
//! The structural reference is built by the caller (it is the `autoreview_reference` port) and handed in, built or
//! failed; the Python built it inside `build_brief` (`Reference(repo, files, base_repo=...).build()`). Everything
//! else is as recorded.
//!
//! Known differences, all outside recorded behaviour: Python's `re` `\s` also takes U+001C..U+001F, which the
//! `regex` crate's `\s` does not; path patterns end in `$` (Python's `$` also matches before a final newline, which
//! a tracked path does not end in); a git failure inside `git grep` (not an empty result) ends the map with
//! [`Rejected`] as the Python raised it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::deep_repo::{Hit, Reference, Rejected, Repo, definition_pattern, numbered};
use crate::diff::{FileDiff, file_kind, skip_reason};
use crate::findings::text as py_text;
use crate::levels::is_code;
use crate::py;

pub const BRIEF_CHARS: usize = 8_000;

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

/// `review_api._DEF` (also `views.rs` `DEF`, private there).
pub(crate) static DEF: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s*(?:export\s+)?(?:async\s+)?(?:def|function|class|func|fn)\s+([A-Za-z_]\w*)|^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?(?:function|\()",
    )
});
/// `review_api._CALL`.
static CALL: LazyLock<Regex> = LazyLock::new(|| regex(r"\b([A-Za-z_]\w{2,})\s*\("));
/// `review_api._COMMON`.
const COMMON: &[&str] = &[
    "if",
    "for",
    "while",
    "return",
    "print",
    "len",
    "range",
    "str",
    "int",
    "float",
    "list",
    "dict",
    "set",
    "tuple",
    "isinstance",
    "super",
    "self",
    "this",
    "new",
    "await",
    "async",
    "function",
    "typeof",
    "catch",
    "switch",
    "open",
    "map",
    "filter",
    "sorted",
    "min",
    "max",
    "sum",
    "any",
    "all",
    "enumerate",
    "zip",
    "format",
    "append",
    "push",
    "get",
    "join",
    "split",
    "strip",
    "require",
    "console",
    "log",
    "Error",
    "Exception",
    "assert",
    "expect",
];
/// `review_api._TEST` (also `diff.rs` `TEST`, private there).
pub(crate) static TEST: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$"));

static CONSTANT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\s*(?:export\s+)?(?:const\s+|final\s+|static\s+)*([A-Z][A-Z0-9_]{2,})\s*[:=]"));
static ENV: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(?:environ(?:\.get)?\s*[\[(]\s*|getenv\(\s*|process\.env\.|\$\{?)["']?([A-Z][A-Z0-9_]{3,})"#)
});
static FLAG: LazyLock<Regex> = LazyLock::new(|| regex(r#"["'\s](--[a-z][a-z0-9-]{2,})"#));
static TABLE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?i)\b(?:alter|create)\s+table\s+(?:if\s+(?:not\s+)?exists\s+)?[`"\[]?(\w+)|\badd\s+column\s+(?:if\s+not\s+exists\s+)?[`"\[]?(\w+)"#,
    )
});
static COMMENT: LazyLock<Regex> = LazyLock::new(|| regex(r#"^\s*(?:#+|//+|/?\*+|--|<!--|"""|''')\s?(.{15,})\n?\z"#));
static DESCRIPTION: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\b(?:description|help|doc|summary)\s*[:=]\s*["'](.{15,})["']"#));
static DOCLIKE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\.(md|mdx|rst|txt|adoc|ya?ml|toml|cfg|ini|json|sh|tf|env)$|(^|/)(\.github|ci|scripts|docs?)/|(^|/)Dockerfile",
    )
});
const STRUCTURE: &str = r"parametrize|fixture|it\.each|test\.each|describe\(|@pytest\.mark";
static CONFIG_PATH: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\.(ya?ml|toml|cfg|ini|tf|env|properties)$|(^|/)(\.github|ci|deploy|k8s|helm|charts|infra|terraform)/|(^|/)(Dockerfile|docker-compose[^/]*|Makefile|package\.json|requirements[^/]*\.txt|pyproject\.toml|go\.mod|Cargo\.toml|Gemfile|pom\.xml|build\.gradle[^/]*)$",
    )
});
static INPUT_HINT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\bre\.|regex|RegExp|\.match\(|\.test\(|\.search\(|parse|split\(|strip\(|startswith|endswith|validate|sanitiz|escape|limit|max_|min_|len\(|\.length|truncate|slice\(|\bint\(|float\(|isinstance|typeof|\bif .*(<|>|<=|>=|==|!=)",
    )
});
static REQUEST_INPUT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"\b(?:request|req|ctx)\.(?:args|form|json|GET|POST|params|query|body|headers|data)(?:\.get\(\s*|\[\s*|\.)["']?([A-Za-z_][A-Za-z0-9_]{2,})"#,
    )
});
static CONFIG_KEY: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"^\s*-?\s*["']?([A-Za-z_][A-Za-z0-9_.-]{2,})["']?\s*[:=]"#));
static FIELD: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"^\s*\{?\s*(?:key|name|field|flag|option)\s*[:=]\s*["']([A-Za-z_][A-Za-z0-9_.-]{2,})["']"#)
});
static CONFIG_FILE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\.(ya?ml|toml|ini|cfg|env|properties|json|tf)$|(^|/)\.env"));
static WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"`([^`]{2,60})`|\b([A-Za-z_][A-Za-z0-9_]{3,})\b"));
static RISK: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)auth|token|password|passwd|secret|credential|permission|role|session|cookie|sql|query\(|execute\(|eval\(|exec\(|subprocess|os\.system|shell|pickle|yaml\.load|request\.|route|endpoint|router\.|@app\.|email|phone|ssn|pii|encrypt|decrypt|hash|cors|csrf|upload|redirect",
    )
});
static TRIPLE_DOUBLE: LazyLock<Regex> = LazyLock::new(|| regex(r#"^"""[^"]*("""|\n?\z)"#));
static TRIPLE_SINGLE: LazyLock<Regex> = LazyLock::new(|| regex(r"^'''[^']*('''|\n?\z)"));
static STRING_LINE: LazyLock<Regex> = LazyLock::new(|| regex(r#"^["'][^"']*["']\s*,?\s*\n?\z"#));

/// `_BEHAVIOUR.match(text)`: a changed code line that is behaviour: not blank, not a comment, not a bare docstring
/// or string line. (The Python pattern's look-aheads all apply at the first non-blank character.)
pub fn is_behaviour(text: &str) -> bool {
    let rest = text.trim_start_matches(py::is_space);
    if rest.is_empty() {
        return false;
    }
    if ["#", "//", "/*", "*", "--", "<!--", "-->"]
        .iter()
        .any(|prefix| rest.starts_with(prefix))
    {
        return false;
    }
    !(TRIPLE_DOUBLE.is_match(rest) || TRIPLE_SINGLE.is_match(rest) || STRING_LINE.is_match(rest))
}

/// `posixpath.split(path)`.
pub(crate) fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(index) => {
            let head = &path[..=index];
            let trimmed = head.trim_end_matches('/');
            (if trimmed.is_empty() { head } else { trimmed }, &path[index + 1..])
        }
        None => ("", path),
    }
}

/// `_add`: append `item` unless present or `items` holds `limit` already.
pub fn add<T: PartialEq>(items: &mut Vec<T>, item: T, limit: usize) {
    if !items.contains(&item) && items.len() < limit {
        items.push(item);
    }
}

/// A claim a changed comment or document makes: (path, line, text).
pub type Claim = (String, Option<u64>, String);

/// What the diff defines, changes, calls and claims (`extract`'s dict).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    pub defined: Vec<String>,
    pub changed: Vec<String>,
    pub calls: Vec<String>,
    pub constants: Vec<String>,
    pub env: Vec<String>,
    pub flags: Vec<String>,
    pub tables: Vec<String>,
    pub claims: Vec<Claim>,
    pub inputs: Vec<String>,
    pub config: Vec<String>,
    pub fields: Vec<String>,
}

impl Extracted {
    /// The dict as the Python returned it (claims as `[path, line, text]`).
    pub fn to_json(&self) -> Value {
        json!({
            "defined": self.defined, "changed": self.changed, "calls": self.calls, "constants": self.constants,
            "env": self.env, "flags": self.flags, "tables": self.tables,
            "claims": self.claims.iter().map(|(path, line, text)| json!([path, line, text])).collect::<Vec<_>>(),
            "inputs": self.inputs, "config": self.config, "fields": self.fields,
        })
    }
}

fn group(captures: &regex::Captures<'_>) -> String {
    captures
        .get(1)
        .filter(|found| !found.as_str().is_empty())
        .or_else(|| captures.get(2))
        .map_or(String::new(), |found| found.as_str().to_string())
}

fn dedupe(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names.into_iter().filter(|name| seen.insert(name.clone())).collect()
}

/// What the diff defines, changes, calls and claims: names worth following into the repository.
pub fn extract(files: &[FileDiff], mut read_file: impl FnMut(&str) -> Option<Vec<String>>) -> Extracted {
    let mut out = Extracted::default();
    for item in files {
        if skip_reason(item).is_some() {
            continue;
        }
        let doc = file_kind(&item.path) == "doc";
        let source = read_file(&item.path);
        let config_file = CONFIG_FILE.is_match(&item.path);
        for hunk in &item.hunks {
            let added: Vec<_> = hunk.lines.iter().filter(|line| line.kind == '+').collect();
            // The definition an added line sits in changed, even when its `def` line did not.
            if let Some(source) = source.as_ref().filter(|source| !source.is_empty() && !doc) {
                for line in &added {
                    let first = line.new.unwrap_or_else(|| hunk.new_first()) as i64;
                    let top = first.min(source.len() as i64) - 1;
                    let floor = (first - 80).max(-1);
                    let mut index = top;
                    while index > floor {
                        if let Some(found) = usize::try_from(index)
                            .ok()
                            .and_then(|at| source.get(at))
                            .and_then(|text| DEF.captures(text))
                        {
                            add(&mut out.changed, group(&found), 12);
                            break;
                        }
                        index -= 1;
                    }
                }
            }
            for line in &added {
                let text = line.text.as_str();
                let lead = text.trim_start_matches(py::is_space);
                if config_file
                    && let Some(key) = CONFIG_KEY.captures(text)
                    && !lead.starts_with('#')
                    && !lead.starts_with("//")
                {
                    add(&mut out.config, key[1].to_string(), 8);
                }
                if doc {
                    if py::strip(text).chars().count() >= 20 {
                        add(
                            &mut out.claims,
                            (item.path.clone(), line.new, py::clip(py::strip(text), 200)),
                            14,
                        );
                    }
                    for flag in FLAG.captures_iter(&format!(" {text}")) {
                        add(&mut out.flags, flag[1].to_string(), 6);
                    }
                    continue;
                }
                if let Some(found) = DEF.captures(text) {
                    add(&mut out.defined, group(&found), 12);
                }
                if let Some(found) = CONSTANT.captures(text) {
                    add(&mut out.constants, found[1].to_string(), 10);
                }
                if let Some(found) = FIELD.captures(text) {
                    add(&mut out.fields, found[1].to_string(), 8);
                }
                for found in CALL.captures_iter(text) {
                    if !COMMON.contains(&&found[1]) {
                        add(&mut out.calls, found[1].to_string(), 16);
                    }
                }
                for found in ENV.captures_iter(text) {
                    add(&mut out.env, found[1].to_string(), 6);
                }
                for found in REQUEST_INPUT.captures_iter(text) {
                    add(&mut out.inputs, found[1].to_string(), 6);
                }
                for found in FLAG.captures_iter(&format!(" {text}")) {
                    add(&mut out.flags, found[1].to_string(), 6);
                }
                for found in TABLE.captures_iter(text) {
                    add(&mut out.tables, group(&found), 6);
                }
                let claim = COMMENT.captures(text).or_else(|| DESCRIPTION.captures(text));
                if let Some(claim) = claim {
                    add(
                        &mut out.claims,
                        (item.path.clone(), line.new, py::clip(py::strip(&claim[1]), 200)),
                        14,
                    );
                }
            }
        }
    }
    let own: HashSet<&String> = out.defined.iter().chain(&out.changed).collect();
    let calls: Vec<String> = out
        .calls
        .iter()
        .filter(|name| !own.contains(name))
        .take(8)
        .cloned()
        .collect();
    out.calls = calls;
    out
}

/// The investigation brief.
#[derive(Clone)]
pub struct Brief {
    pub text: String,
    pub lenses: Vec<String>,
    pub symbols: Vec<String>,
    pub callers: usize,
    pub tests: Vec<String>,
    pub claims: usize,
    /// Names an investigator must look up before it may finish, by part: the changed signatures and exported
    /// names (siblings), and the new config fields, flags, environment variables and request inputs (claims).
    pub required: IndexMap<String, Vec<String>>,
    /// The claims as listed in the brief: (source, text).
    pub claim_list: Vec<(String, String)>,
    pub helpers: Vec<String>,
    pub extracted: Extracted,
    /// The structural reference, its view for the frames, and its stats (`{"error": ...}` when it failed).
    pub reference: Option<Arc<dyn Reference>>,
    pub reference_text: String,
    pub reference_stats: Map<String, Value>,
}

impl std::fmt::Debug for Brief {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Brief")
            .field("text", &self.text)
            .field("lenses", &self.lenses)
            .field("symbols", &self.symbols)
            .field("callers", &self.callers)
            .field("tests", &self.tests)
            .field("claims", &self.claims)
            .field("reference", &self.reference.is_some())
            .finish_non_exhaustive()
    }
}

/// What `build_brief` needs besides the diff: the size limit, the pull request's title and description, the base
/// commit (for commit messages), and the structural reference as the caller built it (`Err` with
/// `"<Type>: <message>"` when it could not).
pub struct BriefInput<'a> {
    pub limit: usize,
    pub title: &'a str,
    pub description: &'a str,
    pub base: Option<&'a str>,
    pub reference: Result<Arc<dyn Reference>, String>,
}

impl Default for BriefInput<'_> {
    fn default() -> Self {
        Self {
            limit: BRIEF_CHARS,
            title: "",
            description: "",
            base: None,
            reference: Err("no structural reference".to_string()),
        }
    }
}

fn hit_line((path, line, body): &Hit, width: usize) -> String {
    format!("{path}:{line}: {}", py::clip(py::strip(body), width))
}

/// The investigation brief: what the change claims, where each claim must hold, and where the diff's names live
/// in the rest of the repository.
pub fn build_brief(
    repo: &Repo<'_>,
    files: &[FileDiff],
    read_file: impl FnMut(&str) -> Option<Vec<String>>,
    input: BriefInput<'_>,
) -> Result<Brief, Rejected> {
    let found = extract(files, read_file);
    let (reference, reference_error) = match input.reference {
        Ok(reference) => {
            repo.set_reference(Some(reference.clone()));
            (Some(reference), None)
        }
        Err(error) => {
            repo.set_reference(None);
            (None, Some(error))
        }
    };
    let changed_lines: HashMap<&str, HashSet<i64>> = files
        .iter()
        .map(|item| {
            let lines = item
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .filter(|line| line.kind == '+')
                .filter_map(|line| line.new.map(|new| new as i64))
                .collect();
            (item.path.as_str(), lines)
        })
        .collect();
    let reviewable: Vec<&FileDiff> = files.iter().filter(|item| skip_reason(item).is_none()).collect();
    let code_files: Vec<&str> = reviewable
        .iter()
        .filter(|item| file_kind(&item.path) == "code")
        .map(|item| item.path.as_str())
        .collect();
    let test_files: Vec<&str> = reviewable
        .iter()
        .filter(|item| file_kind(&item.path) == "test")
        .map(|item| item.path.as_str())
        .collect();
    let mut sections: Vec<String> = Vec::new();
    let mut callers = 0;
    let mut tests: Vec<String> = Vec::new();
    // Where each name is used outside the changed lines, for the claims' obligations.
    let mut used_at: HashMap<String, Vec<String>> = HashMap::new();

    let mut uses = |names: &[String], title: &str, sections: &mut Vec<String>| -> Result<(), Rejected> {
        let per = 8;
        for name in names {
            let mut hits: Vec<Hit> = repo
                .grep(name, true, true, None, per + 12)?
                .into_iter()
                .filter(|hit| {
                    !changed_lines
                        .get(hit.0.as_str())
                        .is_some_and(|lines| lines.contains(&hit.1))
                })
                .collect();
            let mut resolved = "";
            if let Some(reference) = reference.as_ref()
                && !reference.symbols_named(name).is_empty()
            {
                let calls: Vec<Hit> = reference
                    .callers(name, true)
                    .into_iter()
                    .map(|call| (call.path, call.line, call.text))
                    .collect();
                if !calls.is_empty() {
                    hits = calls
                        .into_iter()
                        .chain(hits.into_iter().filter(|hit| !reference.indexed(&hit.0)))
                        .collect();
                    resolved = " (callers resolved)";
                }
            }
            let in_tests: Vec<&Hit> = hits.iter().filter(|hit| TEST.is_match(&hit.0)).collect();
            let in_docs: Vec<&Hit> = hits
                .iter()
                .filter(|hit| DOCLIKE.is_match(&hit.0) && !TEST.is_match(&hit.0))
                .collect();
            let others: Vec<&Hit> = hits
                .iter()
                .filter(|hit| !TEST.is_match(&hit.0) && !DOCLIKE.is_match(&hit.0))
                .collect();
            callers += others.len();
            used_at.insert(
                name.clone(),
                others
                    .iter()
                    .chain(&in_tests)
                    .take(5)
                    .map(|(path, line, _)| format!("{path}:{line}"))
                    .collect(),
            );
            for hit in &in_tests {
                add(&mut tests, hit.0.clone(), 6);
            }
            let used = others
                .iter()
                .take(per)
                .map(|hit| hit_line(hit, 110))
                .collect::<Vec<_>>()
                .join("; ");
            let tested = in_tests
                .iter()
                .take(5)
                .map(|hit| hit_line(hit, 110))
                .collect::<Vec<_>>()
                .join("; ");
            let mut lines = vec![
                format!("{title} `{name}`{resolved}:"),
                format!("  used at: {}", if used.is_empty() { "nowhere else" } else { &used }),
                format!(
                    "  tests: {}",
                    if tested.is_empty() {
                        "no test mentions it"
                    } else {
                        &tested
                    }
                ),
            ];
            if !in_docs.is_empty() {
                lines.push(format!(
                    "  docs and configs: {}",
                    in_docs
                        .iter()
                        .take(6)
                        .map(|(path, line, _)| format!("{path}:{line}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            sections.push(lines.join("\n"));
        }
        Ok(())
    };

    let symbols: Vec<String> = dedupe(found.defined.iter().chain(&found.changed).cloned())
        .into_iter()
        .take(8)
        .collect();
    uses(&symbols, "Changed or added", &mut sections)?;
    let constants: Vec<String> = found.constants.iter().take(5).cloned().collect();
    uses(&constants, "Constant or member", &mut sections)?;
    let named: Vec<String> = found
        .env
        .iter()
        .chain(&found.flags)
        .chain(&found.tables)
        .take(6)
        .cloned()
        .collect();
    uses(&named, "Named in the change", &mut sections)?;
    let mut helpers = Vec::new();
    for name in found.calls.iter().take(5) {
        let hits = repo.grep(&definition_pattern(name), false, false, None, 3)?;
        let Some((path, line, _)) = hits.first() else {
            continue;
        };
        let Some(lines) = repo.lines(path).filter(|lines| !lines.is_empty()) else {
            continue;
        };
        helpers.push(name.clone());
        sections.push(format!(
            "Called by the change, `{name}` is defined at {path}:{line}:\n{}",
            numbered(&lines, *line, (lines.len() as i64).min(line + 11))
        ));
    }
    let structured: Vec<String> = dedupe(
        tests
            .iter()
            .cloned()
            .chain(test_files.iter().map(|path| path.to_string())),
    )
    .into_iter()
    .take(4)
    .collect();
    for path in &structured {
        let hits = repo.grep(STRUCTURE, false, false, Some(path.as_str()), 8)?;
        if !hits.is_empty() {
            sections.push(format!(
                "Structure of {path}: {}",
                hits.iter()
                    .map(|(_, line, body)| format!("{line}: {}", py::clip(py::strip(body), 120)))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }
    let tracked = repo.files();
    for path in code_files.iter().take(4) {
        let (directory, name) = split_path(path);
        let beside: Vec<&String> = tracked
            .iter()
            .filter(|other| split_path(other).0 == directory && other != path)
            .take(8)
            .collect();
        let twins: Vec<&String> = tracked
            .iter()
            .filter(|other| split_path(other).1 == name && other != path)
            .take(5)
            .collect();
        if !beside.is_empty() || !twins.is_empty() {
            let shown = beside
                .iter()
                .map(|other| split_path(other).1)
                .collect::<Vec<_>>()
                .join(", ");
            let mut line = format!("Beside {path}: {}", if shown.is_empty() { "nothing" } else { &shown });
            if !twins.is_empty() {
                line.push_str(&format!(
                    "; same name elsewhere: {}",
                    twins.iter().map(|other| other.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            sections.push(line);
        }
    }
    // The claims: what the change says it does, from its title and description, its commit messages, and the
    // comments and documents it changes; under each, where it has to be true.
    let mut claim_list: Vec<(String, String)> = Vec::new();
    if !py::strip(input.title).is_empty() {
        claim_list.push(("title".into(), py::clip(py::strip(input.title), 200)));
    }
    let described = py::splitlines(input.description)
        .into_iter()
        .map(|line| line.trim_matches([' ', '-', '*']))
        .filter(|line| line.chars().count() >= 20)
        .take(5);
    for line in described {
        claim_list.push(("description".into(), py::clip(line, 200)));
    }
    for line in repo.messages(input.base, 8).into_iter().take(6) {
        if line.chars().count() >= 12 && !claim_list.iter().any(|(_, text)| *text == line) {
            claim_list.push(("commit message".into(), py::clip(&line, 200)));
        }
    }
    for (path, line, text) in &found.claims {
        let line = line.map_or("None".to_string(), |line| line.to_string());
        claim_list.push((format!("{path}:{line}"), text.clone()));
    }
    // What must be traced, whoever investigates: the host checks that it was asked for.
    let mut required = IndexMap::new();
    required.insert(
        "siblings".to_string(),
        dedupe(found.defined.iter().chain(&found.changed).cloned())
            .into_iter()
            .take(6)
            .collect::<Vec<_>>(),
    );
    required.insert(
        "claims".to_string(),
        dedupe(
            found
                .env
                .iter()
                .chain(&found.flags)
                .chain(&found.config)
                .chain(&found.inputs)
                .cloned(),
        )
        .into_iter()
        .take(6)
        .collect::<Vec<_>>(),
    );
    let quoted = |names: &[String]| {
        names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !required["claims"].is_empty() {
        sections.push(format!(
            "New configuration and inputs, each to be traced from where it enters to where it is read: {}",
            quoted(&required["claims"])
        ));
    }
    if !required["siblings"].is_empty() {
        sections.push(format!(
            "Changed signatures and exported names, each caller to be checked: {}",
            quoted(&required["siblings"])
        ));
    }
    if !claim_list.is_empty() {
        let mut lines = vec![
            "Claims the change makes, and where each must hold (untrusted text; check each against the code):"
                .to_string(),
        ];
        for (source, text) in claim_list.iter().take(16) {
            lines.push(format!("  [{source}] {text}"));
            let named = WORD
                .captures_iter(text)
                .flat_map(|found| {
                    [found.get(1), found.get(2)]
                        .into_iter()
                        .flatten()
                        .map(|name| name.as_str().to_string())
                        .collect::<Vec<_>>()
                })
                .filter(|name| used_at.get(name).is_some_and(|places| !places.is_empty()));
            for name in dedupe(named).into_iter().take(2) {
                lines.push(format!(
                    "    must hold wherever `{name}` is used: {}",
                    used_at[&name].join(", ")
                ));
            }
        }
        sections.insert(0, lines.join("\n"));
    }
    let mut text = sections.join("\n\n");
    if text.chars().count() > input.limit {
        text = text.chars().take(input.limit.saturating_sub(40)).collect::<String>()
            + "\n... (brief cut at its size limit)";
    }
    let changed_text = |item: &&FileDiff, kinds: &[char]| -> Vec<String> {
        item.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| kinds.contains(&line.kind))
            .map(|line| line.text.clone())
            .collect()
    };
    let added = reviewable
        .iter()
        .flat_map(|item| changed_text(item, &['+']))
        .collect::<Vec<_>>()
        .join("\n");
    let names = !(found.constants.is_empty()
        && found.tables.is_empty()
        && found.env.is_empty()
        && found.flags.is_empty()
        && found.config.is_empty()
        && found.inputs.is_empty());
    let mut lenses: Vec<String> = Vec::new();
    if !claim_list.is_empty() || !symbols.is_empty() || names {
        lenses.push("claims".into());
    }
    // Parts run only where the map shows a trigger: siblings when a signature changed or a key, member or constant
    // was added; tests when code behaviour changed (not only comments, docstrings or blank lines) or a test did.
    let keys_added = !(found.fields.is_empty()
        && found.constants.is_empty()
        && found.env.is_empty()
        && found.flags.is_empty()
        && found.config.is_empty()
        && found.tables.is_empty());
    let code_lines = |behaviour_only: bool| -> Vec<String> {
        reviewable
            .iter()
            .filter(|item| file_kind(&item.path) == "code" && (!behaviour_only || is_code(&item.path)))
            .flat_map(|item| changed_text(item, &['+', '-']))
            .collect()
    };
    let signature_changed = code_lines(false).iter().any(|line| DEF.is_match(line));
    if !code_files.is_empty() && (signature_changed || keys_added) {
        lenses.push("siblings".into());
    }
    if !found.env.is_empty()
        || !found.flags.is_empty()
        || !found.config.is_empty()
        || files.iter().any(|item| CONFIG_PATH.is_match(&item.path))
    {
        lenses.push("deployment".into());
    }
    let behaviour = code_lines(true).iter().any(|line| is_behaviour(line));
    if (!code_files.is_empty() && behaviour) || !test_files.is_empty() {
        lenses.push("tests".into());
    }
    if !code_files.is_empty() && (INPUT_HINT.is_match(&added) || RISK.is_match(&added)) {
        lenses.push("inputs".into());
    }
    let claims = claim_list.len();
    let mut brief = Brief {
        text: if text.is_empty() {
            "(nothing in the repository mentions the changed names)".to_string()
        } else {
            text
        },
        lenses,
        symbols,
        callers,
        tests,
        claims,
        required,
        claim_list,
        helpers,
        extracted: found,
        reference: None,
        reference_text: String::new(),
        reference_stats: Map::new(),
    };
    if let Some(reference) = reference {
        brief.reference_text = reference.block();
        brief.reference_stats = reference.stats();
        brief.reference = Some(reference);
    }
    if let Some(error) = reference_error {
        let mut stats = Map::new();
        stats.insert("error".into(), py_text(Some(&Value::String(error)), 200).into());
        brief.reference_stats = stats;
    }
    Ok(brief)
}
