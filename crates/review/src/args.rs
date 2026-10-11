//! `/review`'s arguments (`review_api.parse_args`, `classify_targets`).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::prompt_text::{ALIASES, REVIEWERS, USAGE};
use crate::py;

pub const DEFAULT_BUDGET_TOKENS: u64 = 300_000;
pub const MIN_BUDGET_TOKENS: u64 = 10_000;
pub const DEEP_DEFAULT: u32 = 3;
pub const DEEP_MAX: u32 = 10;

/// A request `/review` cannot run; the message is shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewError(pub String);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Options {
    pub targets: Vec<String>,
    pub explicit_paths: Vec<String>,
    pub only: Option<Vec<String>>,
    pub budget_tokens: u64,
    pub model: Option<String>,
    pub post: bool,
    pub deep: u32,
    pub plan: bool,
    pub help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            explicit_paths: Vec::new(),
            only: None,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            model: None,
            post: false,
            deep: 0,
            plan: false,
            help: false,
        }
    }
}

static TOKENS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([0-9]+(?:\.[0-9]+)?)([km]?)$").expect("regex"));
static MODEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^/\s]+/[^/\s]+(?:/[^/\s]+)*$").expect("regex"));
static PR_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#?([0-9]+)$").expect("regex"));
static PR_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https?://[^/]+/[^/]+/[^/]+/pull/([0-9]+)(?:[/?#].*)?$").expect("regex"));

fn tokens(value: &str) -> Result<u64, ReviewError> {
    let cleaned = py::strip(value).to_lowercase().replace([',', '_'], "");
    let error = || {
        ReviewError(format!(
            "--budget takes a token count such as 200000, 200k or 1.5m, not {}",
            py::repr(value)
        ))
    };
    let captures = TOKENS.captures(&cleaned).ok_or_else(error)?;
    let amount: f64 = captures[1].parse().map_err(|_| error())?;
    let scale = match &captures[2] {
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => 1.0,
    };
    let amount = amount * scale;
    if amount < MIN_BUDGET_TOKENS as f64 {
        return Err(ReviewError("--budget must be at least 10,000 tokens".to_string()));
    }
    Ok(amount as u64)
}

fn reviewer_keys(value: &str) -> Result<Vec<String>, ReviewError> {
    let mut keys: Vec<String> = Vec::new();
    for raw in value.split(',') {
        let name = py::strip(raw).to_lowercase();
        if name.is_empty() {
            continue;
        }
        let key = ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
            .map_or(name.as_str(), |(_, key)| key);
        if !REVIEWERS.iter().any(|reviewer| reviewer.key == key) {
            let choices = REVIEWERS
                .iter()
                .map(|reviewer| reviewer.key)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ReviewError(format!(
                "unknown reviewer {}; choose from {choices}",
                py::repr(py::strip(raw))
            )));
        }
        if !keys.iter().any(|known| known == key) {
            keys.push(key.to_string());
        }
    }
    if keys.is_empty() {
        return Err(ReviewError("--only needs at least one reviewer".to_string()));
    }
    Ok(keys)
}

/// Parse `/review`'s argument string (shell-style quoting). `env` supplies flag defaults: ULTRON_REVIEW_BUDGET,
/// ULTRON_REVIEW_MODEL and ULTRON_REVIEW_ONLY.
pub fn parse_args(text: &str, env: &HashMap<String, String>) -> Result<Options, ReviewError> {
    let tokens_in =
        py::shlex_split(text).map_err(|error| ReviewError(format!("cannot parse the arguments: {error}")))?;
    parse_tokens(tokens_in, env)
}

/// `parse_args` over already split tokens.
pub fn parse_tokens(mut args: Vec<String>, env: &HashMap<String, String>) -> Result<Options, ReviewError> {
    let mut options = Options::default();
    let set = |key: &str| env.get(key).filter(|value| !py::strip(value).is_empty());
    if let Some(budget) = set("ULTRON_REVIEW_BUDGET") {
        options.budget_tokens = tokens(budget)?;
    }
    if let Some(only) = set("ULTRON_REVIEW_ONLY") {
        options.only = Some(reviewer_keys(only)?);
    }
    if let Some(model) = set("ULTRON_REVIEW_MODEL") {
        args.splice(0..0, ["--model".to_string(), py::strip(model).to_string()]);
    }
    let mut index = 0;
    while index < args.len() {
        let token = args[index].clone();
        let (flag, inline) = match token.split_once('=') {
            Some((flag, inline)) => (flag, Some(inline)),
            None => (token.as_str(), None),
        };
        if token == "--" {
            options.explicit_paths.extend(args[index + 1..].iter().cloned());
            break;
        }
        let mut value = |flag: &str| -> Result<String, ReviewError> {
            if let Some(inline) = inline {
                return Ok(inline.to_string());
            }
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| ReviewError(format!("{flag} needs a value")))
        };
        match flag {
            "-h" | "--help" => options.help = true,
            "--only" => options.only = Some(reviewer_keys(&value(flag)?)?),
            "--budget" => options.budget_tokens = tokens(&value(flag)?)?,
            "--model" => {
                let model = py::strip(&value(flag)?).to_string();
                if !MODEL.is_match(&model) {
                    return Err(ReviewError(format!(
                        "--model takes provider/model, not {}",
                        py::repr(&model)
                    )));
                }
                options.model = Some(model);
            }
            "--post" => options.post = true,
            "--plan" | "--dry-run" => options.plan = true,
            "--deep" => match inline {
                None => options.deep = DEEP_DEFAULT,
                Some(text) => match text.parse::<u32>() {
                    Ok(deep) if text.bytes().all(|b| b.is_ascii_digit()) && (1..=DEEP_MAX).contains(&deep) => {
                        options.deep = deep
                    }
                    _ => return Err(ReviewError(format!("--deep=N takes a number from 1 to {DEEP_MAX}"))),
                },
            },
            _ if token.starts_with('-') && token != "-" => {
                return Err(ReviewError(format!("unknown option {token}\n\n{USAGE}")));
            }
            _ => options.targets.push(token.clone()),
        }
        index += 1;
    }
    Ok(options)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Target {
    pub pr: Option<u64>,
    pub base: Option<String>,
    pub paths: Vec<String>,
}

/// Sort positional arguments into at most one PR or base ref, and paths.
pub fn classify_targets(
    options: &Options,
    exists: impl Fn(&str) -> bool,
    is_ref: impl Fn(&str) -> bool,
) -> Result<Target, ReviewError> {
    let mut target = Target {
        paths: options.explicit_paths.clone(),
        ..Target::default()
    };
    for token in &options.targets {
        let number = PR_NUMBER.captures(token).filter(|_| !exists(token));
        if let Some(captures) = number.or_else(|| PR_URL.captures(token)) {
            let pr: u64 = captures[1]
                .parse()
                .map_err(|_| ReviewError(format!("{} is not a PR number", py::repr(token))))?;
            if target.pr.is_some_and(|known| known != pr) {
                return Err(ReviewError("give at most one pull request".to_string()));
            }
            target.pr = Some(pr);
        } else if exists(token) {
            target.paths.push(token.clone());
        } else if is_ref(token) {
            if let Some(base) = &target.base {
                return Err(ReviewError(format!(
                    "give at most one base ref (got {} and {})",
                    py::repr(base),
                    py::repr(token)
                )));
            }
            target.base = Some(token.clone());
        } else {
            return Err(ReviewError(format!(
                "{} is not a path in this repository, a git ref, or a PR number",
                py::repr(token)
            )));
        }
    }
    if target.pr.is_some() && target.base.is_some() {
        return Err(ReviewError("give a PR number or a base ref, not both".to_string()));
    }
    Ok(target)
}
