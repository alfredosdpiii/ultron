//! `ultron autoreview` (`autoreview/cli.ts`): the automated pull-request reviewer's command.
//!
//!   ultron autoreview review --repo-dir <dir> --base <sha> --head <sha> [--json] [--dry-run]
//!                                               review a local diff, no GitHub access (the benchmark entry)
//!   ultron autoreview review <owner/repo#N|URL> review one pull request now
//!   ultron autoreview run | once | status | doctor | prepare | install | uninstall
//!
//! RLM-first: a review is `autoreview_api.run` in a Python kernel of its own (`review::engine`); its only model calls
//! are `rlm.map` frames on the models the configuration names. stdout carries only the command's own output (one
//! JSON object with `--json`); log lines go to stderr.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use review::engine::{EngineOptions, KernelEngine, Log, ModelResolver, ReviewEngine};
use review::plan::{EngineResult, PlanOptions, decide_verdict, level_of, plan_review, rank_findings, severity_of};
use review::settings::{
    AutoreviewConfig, AutoreviewSettings, MIN_BUDGET_TOKENS, ModelFallbacks, RECOMMENDED_MODELS, autoreview_paths,
    engine_settings, resolve_config,
};
use serde_json::{Map, Value, json};
use session_core::ModelFlags;

use crate::Env;

const APP_NAME: &str = "ultron";
const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
const REVIEW_MODES: [&str; 5] = ["fast", "deep", "both", "compiled", "hybrid"];
const PLAN_STYLES: [&str; 2] = ["cell", "frame"];
const FRAME_THINKING_LEVELS: [&str; 7] = config::settings::FRAME_THINKING_LEVELS;

pub fn usage() -> String {
    format!(
        "Usage: {APP_NAME} autoreview <command> [options]

Review pull requests automatically, as the accounts gh is logged in to: a review starts when one of them is
requested as reviewer or @mentioned on a pull request, and is posted under that account.

Commands:
  run                          Poll and review until stopped (Ctrl+C, or stop the service)
  once                         One poll cycle: review what is due, then exit
  review <owner/repo#N | URL>  Review one pull request now
  review --repo-dir <dir> --base <sha> --head <sha>
                               Review a local diff with no GitHub access; with --json, one JSON object on stdout
  status                       Accounts, last poll, queue and recent reviews
  doctor [--repo owner/name]   The sandbox test execution would use, and a self-check of its isolation; with --repo,
                               the local checkout whose environments would be bound into it
  prepare <repo-dir> [--python 3.x]
                               Build the repository's test environment now, with the network (uv virtualenv, npm ci,
                               mise toolchains), into the cache the sandbox binds read-only during reviews
  install | uninstall          Write or remove the user service that runs \"{APP_NAME} autoreview run\"

Options:
  --dry-run                    Write the would-be review to <agent dir>/autoreview/dry-run/ instead of posting
  --json                       Machine-readable output (review, once, status)
  --account <login>            review: the account to review as (default: the first logged-in account of the host)
  --model <provider/model>     review: the model of the finder frames (default: autoreview.model, then review.model,
                               then rlm.frameModel, then the default model)
  --verify-model <p/m>         review: the model of the verifier frames (default: the finder model)
  --budget <tokens>            review: a token cap, e.g. 300000 or 300k (default: autoreview.budget, none)
  --thinking <level>           review: thinking level of the finder frames (off, minimal, low, medium, high, ...;
                               default: autoreview.thinking, low)
  --verify-thinking <level>    review: thinking level of the verifier frames (default: autoreview.verifyThinking, low)
  --mode <fast|deep|both|compiled|hybrid>
                               review: fast reviews the diff; deep investigates beyond it by read-only lookups in the
                               repository (nothing is executed); both (default) does one after the other, each
                               finding verified by a verifier frame. Experimental: hybrid runs both as discovery and
                               verifies each candidate with a host-written check program; compiled has one planner
                               write the whole program
  --verify-candidates <n>      review --mode hybrid (experimental): candidates verified at most (default:
                               autoreview.verifyCandidates, 12)
  --deep-model <p/m>           review: the model of the deep pass's investigators (default: the finder model)
  --deep-thinking <level>      review: their thinking level (default: autoreview.deepThinking, high; medium was
                               measured to find a third as much). Every --*-thinking default is per model: the
                               recommended model thinks high at every stage
  --verify-batch <n>           review: findings of one file judged by one verifier frame (default:
                               autoreview.verifyBatch, 4; 1 is one frame per finding)
  --plan-model <p/m>           review --mode compiled: the planner's model (default: autoreview.planModel, the finder model)
  --plan-thinking <level>      review --mode compiled: its thinking level (default: autoreview.planThinking; medium for
                               cells, high for a frame)
  --ask-model <p/m>            review --mode compiled: the small model the program's questions go to (default:
                               autoreview.askModel, the finder model)
  --ask-thinking <level>       review --mode compiled: its thinking level (default: autoreview.askThinking, low)
  --plan-style <cell|frame>    review --mode compiled: the planner as sandboxed Python cells over the rv API (default)
                               or as one JSON-program frame
  --plan-cells <n>             review --mode compiled: cells the planner may run (default: autoreview.planCells, 4)
  --dump-program <path>        review --mode compiled: save the validated program as JSON for inspection
  --program <path>             review --mode compiled: execute this saved program instead of calling the planner
  --run-tests | --no-run-tests review --repo-dir: let the deep pass run the project's tests in a sandbox (default:
                               autoreview.runTests, on); never without a sandbox
  --prepare                    review --repo-dir: prepare the environment first (as \"prepare\" does), then review
  --python <3.x>               prepare, review --prepare: the Python version to build the virtualenv with
  --test-env <dir>             review --repo-dir: a pre-built environment (virtualenv, node_modules) to bind read-only
  --checkout-roots <dir,...>   review: directories of local checkouts whose environments may serve the tests
                               (with --repo owner/name, whose remote the checkout must have)
  --guides <path,...>          review: private review guides (markdown files or directories)
  --block-at <level>           review: request changes from this level up (critical, high, medium, low, nit)
  --max-comments <n>           review: inline comments at most
  --deadline <seconds>         review: give up unfinished passes after this long and report the rest (default:
                               autoreview.deadlineSeconds, none)

Settings (global settings.json): autoreview.accounts, pollSeconds, concurrency, model, verifyModel, budget, dryRun,
frameConcurrency, modelConcurrency, mode, deepModel, deepThinking, deepRounds, referenceView, verifyBatch, planModel, planThinking, askModel, askThinking, planStyle, planCells, verifyCandidates, prepareEnvs, mise, blockAt, maxComments, runTests, testOwners, testRuns, testTimeoutSeconds, testEnv, testImage, checkoutRoots, guides, thinking, verifyThinking, deadlineSeconds, frameTimeoutSeconds, ack, ackLines, ackArt, signature, learn, skipBots.
See docs/autoreview.md."
    )
}

/// A command-line mistake: exit code 2.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageError(pub String);

/// The parsed arguments (`Parsed` of cli.ts).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub command: String,
    pub target: Option<String>,
    pub dry_run: bool,
    pub json: bool,
    pub account: Option<String>,
    pub model: Option<String>,
    pub verify_model: Option<String>,
    pub budget: Option<f64>,
    pub thinking: Option<String>,
    pub verify_thinking: Option<String>,
    pub mode: Option<String>,
    pub deep_model: Option<String>,
    pub deep_thinking: Option<String>,
    pub plan_model: Option<String>,
    pub plan_thinking: Option<String>,
    pub ask_model: Option<String>,
    pub ask_thinking: Option<String>,
    pub plan_style: Option<String>,
    pub plan_cells: Option<f64>,
    pub verify_candidates: Option<f64>,
    pub verify_batch: Option<f64>,
    pub program_path: Option<String>,
    pub dump_program_path: Option<String>,
    pub deadline_seconds: Option<f64>,
    pub run_tests: Option<bool>,
    pub test_env: Option<String>,
    pub prepare: bool,
    pub python: Option<String>,
    pub repo: Option<String>,
    pub guides: Option<Vec<String>>,
    pub checkout_roots: Option<Vec<String>>,
    pub block_at: Option<String>,
    pub max_comments: Option<f64>,
    pub repo_dir: Option<String>,
    pub base: Option<String>,
    pub head: Option<String>,
    pub help: bool,
}

/// JavaScript `Number(text)` for the decimal forms a command line carries: trimmed, empty is 0, else a finite
/// decimal (no hex, no `Infinity`), otherwise NaN.
fn js_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    let valid = trimmed
        .trim_start_matches(['+', '-'])
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !valid {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn whole(value: f64, min: f64) -> Option<f64> {
    (value.is_finite() && value.fract() == 0.0 && value >= min).then_some(value)
}

/// `parseTokens`: `300000`, `300k`, `1.5m` (commas and underscores ignored), at least the minimum budget.
pub fn parse_tokens(value: &str) -> Result<f64, UsageError> {
    let cleaned: String = value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ',' | '_'))
        .collect();
    let refused = || {
        UsageError(format!(
            "--budget takes a token count such as 300000 or 300k, not {value}"
        ))
    };
    let (digits, unit) = match cleaned.strip_suffix('k') {
        Some(rest) => (rest, 1_000.0),
        None => match cleaned.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000.0),
            None => (cleaned.as_str(), 1.0),
        },
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !numeric(int) || (digits.contains('.') && !numeric(frac)) {
        return Err(refused());
    }
    let amount = (digits.parse::<f64>().map_err(|_| refused())? * unit).floor();
    if amount < MIN_BUDGET_TOKENS {
        return Err(UsageError(format!(
            "--budget must be at least {MIN_BUDGET_TOKENS} tokens"
        )));
    }
    Ok(amount)
}

/// `MODEL_REF`: `provider/model[/more]` without whitespace.
fn is_model_ref(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && !part.chars().any(char::is_whitespace))
}

fn is_python_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() <= 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// `parseAutoreviewArgs`.
pub fn parse_args(args: &[String]) -> Result<Parsed, UsageError> {
    let mut parsed = Parsed {
        command: args.first().cloned().unwrap_or_default(),
        ..Parsed::default()
    };
    if matches!(parsed.command.as_str(), "" | "--help" | "-h" | "help") {
        parsed.help = true;
        return Ok(parsed);
    }
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].clone();
        let mut value = || -> Result<String, UsageError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))
        };
        let model = |ref_: String| -> Result<String, UsageError> {
            if is_model_ref(&ref_) {
                Ok(ref_)
            } else {
                Err(UsageError(format!("{arg} takes provider/model, not {ref_}")))
            }
        };
        let list = |text: String| -> Vec<String> {
            text.split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        };
        let thinking = |name: String| -> Result<String, UsageError> {
            if FRAME_THINKING_LEVELS.contains(&name.as_str()) {
                Ok(name)
            } else {
                Err(UsageError(format!(
                    "{arg} takes one of {}, not {name}",
                    FRAME_THINKING_LEVELS.join(", ")
                )))
            }
        };
        match arg.as_str() {
            "--help" | "-h" => parsed.help = true,
            "--dry-run" => parsed.dry_run = true,
            "--json" => parsed.json = true,
            "--account" => parsed.account = Some(value()?),
            "--model" => parsed.model = Some(model(value()?)?),
            "--verify-model" => parsed.verify_model = Some(model(value()?)?),
            "--budget" => parsed.budget = Some(parse_tokens(&value()?)?),
            "--thinking" => parsed.thinking = Some(thinking(value()?)?),
            "--verify-thinking" => parsed.verify_thinking = Some(thinking(value()?)?),
            "--deep-thinking" => parsed.deep_thinking = Some(thinking(value()?)?),
            "--deep-model" => parsed.deep_model = Some(model(value()?)?),
            "--plan-model" => parsed.plan_model = Some(model(value()?)?),
            "--plan-thinking" => parsed.plan_thinking = Some(thinking(value()?)?),
            "--ask-model" => parsed.ask_model = Some(model(value()?)?),
            "--ask-thinking" => parsed.ask_thinking = Some(thinking(value()?)?),
            "--plan-style" => {
                let name = value()?;
                if !PLAN_STYLES.contains(&name.as_str()) {
                    return Err(UsageError(format!("--plan-style takes cell or frame, not {name}")));
                }
                parsed.plan_style = Some(name);
            }
            "--plan-cells" => {
                parsed.plan_cells = Some(
                    whole(js_number(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--plan-cells takes a whole number of cells".into()))?,
                );
            }
            "--verify-candidates" => {
                parsed.verify_candidates = Some(
                    whole(js_number(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--verify-candidates takes a whole number".into()))?,
                );
            }
            "--verify-batch" => {
                parsed.verify_batch = Some(
                    whole(js_number(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--verify-batch takes a whole number".into()))?,
                );
            }
            "--program" => parsed.program_path = Some(value()?),
            "--dump-program" => parsed.dump_program_path = Some(value()?),
            "--run-tests" => parsed.run_tests = Some(true),
            "--no-run-tests" => parsed.run_tests = Some(false),
            "--test-env" => parsed.test_env = Some(value()?),
            "--prepare" => parsed.prepare = true,
            "--python" => {
                let version = value()?;
                if !is_python_version(&version) {
                    return Err(UsageError(format!(
                        "--python takes a version such as 3.12, not {version}"
                    )));
                }
                parsed.python = Some(version);
            }
            "--repo" => parsed.repo = Some(value()?),
            "--guides" => parsed.guides = Some(list(value()?)),
            "--checkout-roots" => parsed.checkout_roots = Some(list(value()?)),
            "--block-at" => {
                let name = value()?;
                if !LEVELS.contains(&name.as_str()) {
                    return Err(UsageError(format!(
                        "--block-at takes one of {}, not {name}",
                        LEVELS.join(", ")
                    )));
                }
                parsed.block_at = Some(name);
            }
            "--max-comments" => {
                parsed.max_comments = Some(
                    whole(js_number(&value()?), 0.0)
                        .ok_or_else(|| UsageError("--max-comments takes a whole number".into()))?,
                );
            }
            "--mode" => {
                let name = value()?;
                if !REVIEW_MODES.contains(&name.as_str()) {
                    return Err(UsageError(format!(
                        "--mode takes fast, deep, both, compiled or hybrid, not {name}"
                    )));
                }
                parsed.mode = Some(name);
            }
            "--deadline" => {
                parsed.deadline_seconds = Some(
                    whole(js_number(&value()?), 0.0)
                        .ok_or_else(|| UsageError("--deadline takes whole seconds (0 for no deadline)".into()))?,
                );
            }
            "--repo-dir" => parsed.repo_dir = Some(value()?),
            "--base" => parsed.base = Some(value()?),
            "--head" => parsed.head = Some(value()?),
            _ if arg.starts_with('-') => {
                return Err(UsageError(format!("unknown option for {APP_NAME} autoreview: {arg}")));
            }
            _ if parsed.target.is_none() && matches!(parsed.command.as_str(), "review" | "prepare") => {
                parsed.target = Some(arg.clone());
            }
            _ => return Err(UsageError(format!("unexpected argument: {arg}"))),
        }
        index += 1;
    }
    Ok(parsed)
}

/// `describeModels`: the model each stage runs on, with its thinking level.
pub fn describe_models(config: &AutoreviewConfig) -> Vec<String> {
    let finder = config
        .model
        .clone()
        .unwrap_or_else(|| "the session's default model".into());
    let stage = |name: &str, model: &Option<String>, thinking: &str| {
        format!(
            "  {name}: {} (thinking {thinking})",
            model.clone().unwrap_or_else(|| finder.clone())
        )
    };
    let recommended = config
        .model
        .as_deref()
        .is_some_and(|model| RECOMMENDED_MODELS.contains(&model));
    vec![
        format!(
            "Models{}:",
            if recommended {
                " (the recommended set; it thinks high at every stage)"
            } else {
                ""
            }
        ),
        stage("finders", &config.model, &config.thinking),
        stage("investigators", &config.deep_model, &config.deep_thinking),
        stage("verifier", &config.verify_model, &config.verify_thinking),
        stage("planner (compiled)", &config.plan_model, &config.plan_thinking),
        stage("questions (compiled)", &config.ask_model, &config.ask_thinking),
    ]
}

/// `describeStages`: "; map 1 s, tests 20 s, find 31 s" from the per-stage milliseconds (zero stages left out).
pub fn describe_stages(stages: Option<&Map<String, Value>>) -> String {
    let Some(stages) = stages else {
        return String::new();
    };
    let parts: Vec<String> = stages
        .iter()
        .filter_map(|(name, ms)| {
            let ms = ms.as_f64().filter(|ms| *ms > 0.0)?;
            let name = name.strip_suffix("Ms").unwrap_or(name);
            Some(format!("{name} {} s", review::js::round(ms / 1000.0)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("; {}", parts.join(", "))
    }
}

/// Present (even null): the TS `x === undefined ? {} : {key: x}`.
fn put_present(out: &mut Map<String, Value>, key: &str, from: &Map<String, Value>, field: &str) {
    if let Some(value) = from.get(field) {
        out.insert(key.into(), value.clone());
    }
}

/// `x ?? fallback`: a missing or null value takes the fallback.
fn or_else(from: &Map<String, Value>, field: &str, fallback: Value) -> Value {
    match from.get(field) {
        None | Some(Value::Null) => fallback,
        Some(value) => value.clone(),
    }
}

fn object<'a>(value: Option<&'a Value>, empty: &'a Map<String, Value>) -> &'a Map<String, Value> {
    value.and_then(Value::as_object).unwrap_or(empty)
}

fn array_copy(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Array(items)) => Value::Array(items.clone()),
        _ => Value::Array(Vec::new()),
    }
}

/// `offlineJson`: the JSON `review --repo-dir ... --json` prints, with where each finding would go (inline, named in
/// the body, or only counted) computed by the same ranking and cap the poster uses.
pub fn offline_json(raw: &Value, startup_ms: Option<u64>, block_at: Option<&str>, max_comments: Option<i64>) -> Value {
    let empty = Map::new();
    let result = object(Some(raw), &empty);
    let typed: EngineResult = serde_json::from_value(raw.clone()).unwrap_or_default();
    let verdict = decide_verdict(&typed, false, "open", block_at).verdict;
    let options = PlanOptions {
        self_authored: false,
        state: "open".into(),
        head_sha: "0".repeat(40),
        since_sha: None,
        signature: false,
        block_at: block_at.map(str::to_string),
        max_comments,
    };
    let plan = plan_review(&typed, &options);
    let inline: HashSet<usize> = plan.comments.iter().map(|comment| comment.finding).collect();
    let in_body: HashSet<usize> = plan.in_summary.iter().copied().collect();
    let ranks: HashMap<usize, usize> = rank_findings(&typed)
        .into_iter()
        .enumerate()
        .map(|(position, index)| (index, position + 1))
        .collect();
    let findings: Vec<Value> = result
        .get("findings")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(index, finding)| {
            let finding = object(Some(finding), &empty);
            let text = |field: &str| finding.get(field).and_then(Value::as_str);
            let level = level_of(text("level"), text("severity"));
            let mut out = Map::new();
            let posted = if inline.contains(&index) {
                "inline"
            } else if in_body.contains(&index) {
                "body"
            } else {
                "counted"
            };
            out.insert("posted".into(), posted.into());
            out.insert("rank".into(), ranks.get(&index).map_or(Value::Null, |rank| json!(rank)));
            put_present(&mut out, "unpinned", finding, "unpinned");
            put_present(&mut out, "consequence", finding, "consequence");
            if finding.get("unclear") == Some(&Value::Bool(true)) {
                out.insert("unclear".into(), Value::Bool(true));
            }
            put_present(&mut out, "file", finding, "file");
            put_present(&mut out, "line", finding, "line");
            put_present(&mut out, "endLine", finding, "endLine");
            out.insert("severity".into(), severity_of(level).into());
            out.insert("level".into(), level.into());
            out.insert(
                "finderSeverity".into(),
                or_else(finding, "finderSeverity", severity_of(level).into()),
            );
            out.insert("finderLevel".into(), or_else(finding, "finderLevel", level.into()));
            put_present(&mut out, "verifierLevel", finding, "verifierLevel");
            put_present(&mut out, "verifierScenarioHolds", finding, "verifierScenarioHolds");
            out.insert("scenario".into(), or_else(finding, "scenario", "".into()));
            put_present(&mut out, "category", finding, "category");
            put_present(&mut out, "claim", finding, "claim");
            put_present(&mut out, "why", finding, "why");
            put_present(&mut out, "suggestedFix", finding, "suggestedFix");
            put_present(&mut out, "verification", finding, "verification");
            put_present(&mut out, "confidence", finding, "confidence");
            put_present(&mut out, "alsoAt", finding, "alsoAt");
            out.insert("source".into(), or_else(finding, "source", "fast".into()));
            put_present(&mut out, "verifiedBy", finding, "verifiedBy");
            out.insert("evidence".into(), or_else(finding, "evidence", "".into()));
            out.insert("howVerified".into(), or_else(finding, "howVerified", "".into()));
            Value::Object(out)
        })
        .collect();
    let dropped_in = object(result.get("dropped"), &empty);
    let mut dropped = Map::new();
    put_present(&mut dropped, "rejected", dropped_in, "rejected");
    put_present(&mut dropped, "duplicates", dropped_in, "duplicates");
    dropped.insert("generic".into(), or_else(dropped_in, "generic", json!(0)));
    dropped.insert("refutedByTest".into(), or_else(dropped_in, "refutedByTest", json!(0)));
    dropped.insert("duplicateOf".into(), or_else(dropped_in, "duplicateOf", json!([])));
    let timing_in = object(result.get("timing"), &empty);
    let mut timing = Map::new();
    for key in ["totalMs", "scopeMs", "findMs", "verifyMs"] {
        put_present(&mut timing, key, timing_in, key);
    }
    if let Some(ms) = startup_ms {
        timing.insert("startupMs".into(), json!(ms));
    }
    if let Some(stages) = timing_in.get("stages").filter(|stages| !stages.is_null()) {
        timing.insert("stages".into(), stages.clone());
    }
    timing.insert("frames".into(), array_copy(timing_in.get("frames")));
    timing.insert("investigators".into(), array_copy(timing_in.get("investigators")));
    if let Some(reference) = timing_in.get("reference").filter(|value| truthy(value)) {
        timing.insert("reference".into(), reference.clone());
    }
    let usage_in = object(result.get("usage"), &empty);
    let mut usage = Map::new();
    put_present(&mut usage, "inputTokens", usage_in, "inputTokens");
    put_present(&mut usage, "outputTokens", usage_in, "outputTokens");
    put_present(&mut usage, "costUsd", usage_in, "costUsd");
    put_present(&mut usage, "frames", usage_in, "frames");
    if let Some(by_phase) = usage_in.get("byPhase") {
        usage.insert("byPhase".into(), by_phase.clone());
    }
    let assurance: Vec<String> = result
        .get("assurance")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(js_text).collect())
        .unwrap_or_default();
    let program = match result.get("program") {
        None | Some(Value::Null) => Value::Null,
        Some(program) => {
            let mut program = program.as_object().cloned().unwrap_or_default();
            program.insert("steps".into(), array_copy(timing_in.get("program")));
            Value::Object(program)
        }
    };
    let mut out = Map::new();
    out.insert("verdict".into(), serde_json::to_value(verdict).unwrap_or(Value::Null));
    put_present(&mut out, "complete", result, "complete");
    out.insert("findings".into(), Value::Array(findings));
    out.insert("dropped".into(), Value::Object(dropped));
    out.insert("timing".into(), Value::Object(timing));
    out.insert("usage".into(), Value::Object(usage));
    put_present(&mut out, "model", result, "model");
    put_present(&mut out, "verifyModel", result, "verifyModel");
    out.insert("mode".into(), or_else(result, "mode", "fast".into()));
    out.insert("assurance".into(), assurance.join(" ").into());
    out.insert(
        "tests".into(),
        or_else(
            result,
            "tests",
            json!({"enabled": false, "mechanism": null, "note": null, "runs": []}),
        ),
    );
    for key in [
        "deepModel",
        "deepThinking",
        "thinking",
        "verifyThinking",
        "planModel",
        "planThinking",
        "askModel",
        "askThinking",
        "planStyle",
        "verification",
    ] {
        out.insert(key.into(), or_else(result, key, Value::Null));
    }
    out.insert("program".into(), program);
    out.insert("notChecked".into(), array_copy(result.get("notChecked")));
    Value::Object(out)
}

/// JavaScript truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

/// `String(value)` for an array element joined by `Array.join`.
fn js_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => session_file::js::stringify(other),
    }
}

/// `logoText`: Ultron's half-size logo, trimmed of blank rows and common indentation (the acknowledgement's art).
pub fn logo_text() -> String {
    let mut rows: Vec<String> = interactive::header::halve_logo(interactive::header::ULTRON_LOGO)
        .into_iter()
        .map(|row| row.replace('$', " ").trim_end().to_string())
        .collect();
    while rows.first().is_some_and(String::is_empty) {
        rows.remove(0);
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let indent = rows
        .iter()
        .filter(|row| !row.is_empty())
        .map(|row| row.len() - row.trim_start().len())
        .min()
        .unwrap_or(0);
    rows.iter()
        .map(|row| row.get(indent..).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Log lines (`createLogger`): `<ISO time> <line with secrets masked>` on the process's stderr as they happen (and,
/// for the service, in the day's log file).
fn logger(var: &crate::VarLookup) -> Log {
    let mask = var("ULTRON_MASK_SECRETS");
    let masker = Arc::new(host::secrets::OutputMasker::default());
    Arc::new(move |line: &str| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let (masked, _) = masker.mask_counted(line, mask.as_deref());
        eprintln!("{} {masked}", kernel_apis::util::iso(now));
    })
}

/// The settings `ultron autoreview` reads (global only: a reviewed repository cannot change them).
struct Saved {
    autoreview: AutoreviewSettings,
    retry: review::engine::RetryPolicy,
    enabled: Option<bool>,
    fallbacks_review_model: Option<String>,
    frame_model: Option<String>,
    frame_thinking: Option<String>,
    child_model: Option<String>,
    default_provider: Option<String>,
    default_model: Option<String>,
}

fn saved_settings(env: &Env) -> Saved {
    let agent_dir = env.agent_dir();
    let manager = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
    let raw = Value::Object(manager.autoreview_settings());
    let autoreview: AutoreviewSettings = serde_json::from_value(raw.clone()).unwrap_or_default();
    let global = manager.global_settings();
    let rlm = global.get("rlm").and_then(Value::as_object);
    let frame_thinking = rlm
        .and_then(|rlm| rlm.get("frameThinking"))
        .and_then(Value::as_str)
        .filter(|level| FRAME_THINKING_LEVELS.contains(level))
        .map(str::to_string);
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
    let retry = manager.retry_settings();
    let number = |key: &str, default: f64| retry[key].as_f64().unwrap_or(default);
    Saved {
        retry: review::engine::RetryPolicy {
            enabled: retry["enabled"].as_bool().unwrap_or(true),
            max_retries: number("maxRetries", 3.0).max(0.0) as u32,
            base_delay_ms: number("baseDelayMs", 2000.0),
            max_delay_ms: number("maxAgentDelayMs", 60_000.0),
        },
        enabled: raw.get("enabled").and_then(Value::as_bool),
        fallbacks_review_model: global
            .get("review")
            .and_then(Value::as_object)
            .and_then(|review| config::settings::model_ref(review.get("model"))),
        frame_model: rlm.and_then(|rlm| config::settings::model_ref(rlm.get("frameModel"))),
        frame_thinking,
        child_model: rlm
            .and_then(|rlm| config::settings::model_ref(rlm.get("childModel")))
            .filter(|model| !model.starts_with("claude-code/")),
        default_provider: text(manager.default_provider().as_ref()),
        default_model: text(manager.default_model().as_ref()),
        autoreview,
    }
}

/// `hasModel`: a model of the user's own catalog (models.json), or Claude Code's when its CLI would be found.
fn has_model(env: &Env, reference: &str) -> bool {
    if reference.starts_with("claude-code/") {
        let mut map = BTreeMap::new();
        for key in [ai::claude_code_cli::CLAUDE_CODE_BIN_ENV, "PATH"] {
            if let Some(value) = (env.var)(key) {
                map.insert(key.to_string(), value);
            }
        }
        return ai::claude_code_cli::resolve_claude_cli(&map).is_ok();
    }
    let Some((provider, id)) = reference.split_once('/') else {
        return false;
    };
    if provider.is_empty() {
        return false;
    }
    let path = config::paths::AgentPaths::new(env.agent_dir()).models();
    let catalog = ai::model_config::ModelConfig::load_with_home(path.to_str(), &env.home);
    catalog
        .provider(provider)
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|model| model.get("id").and_then(Value::as_str) == Some(id))
        })
}

/// The configuration of this run: the saved settings with the command line's overrides.
fn configure(parsed: &Parsed, saved: &Saved, env: &Env) -> AutoreviewConfig {
    let mut settings = saved.autoreview.clone();
    let set = |target: &mut Option<String>, value: &Option<String>| {
        if value.is_some() {
            *target = value.clone();
        }
    };
    set(&mut settings.model, &parsed.model);
    set(&mut settings.verify_model, &parsed.verify_model);
    if parsed.budget.is_some() {
        settings.budget = parsed.budget;
    }
    set(&mut settings.thinking, &parsed.thinking);
    set(&mut settings.verify_thinking, &parsed.verify_thinking);
    set(&mut settings.mode, &parsed.mode);
    if parsed.guides.is_some() {
        settings.guides = parsed.guides.clone();
    }
    if parsed.checkout_roots.is_some() {
        settings.checkout_roots = parsed.checkout_roots.clone();
    }
    set(&mut settings.block_at, &parsed.block_at);
    if parsed.max_comments.is_some() {
        settings.max_comments = parsed.max_comments;
    }
    set(&mut settings.deep_model, &parsed.deep_model);
    set(&mut settings.deep_thinking, &parsed.deep_thinking);
    set(&mut settings.plan_model, &parsed.plan_model);
    set(&mut settings.plan_thinking, &parsed.plan_thinking);
    set(&mut settings.ask_model, &parsed.ask_model);
    set(&mut settings.ask_thinking, &parsed.ask_thinking);
    set(&mut settings.plan_style, &parsed.plan_style);
    if parsed.plan_cells.is_some() {
        settings.plan_cells = parsed.plan_cells;
    }
    if parsed.verify_candidates.is_some() {
        settings.verify_candidates = parsed.verify_candidates;
    }
    if parsed.verify_batch.is_some() {
        settings.verify_batch = parsed.verify_batch;
    }
    if parsed.deadline_seconds.is_some() {
        settings.deadline_seconds = parsed.deadline_seconds;
    }
    let has = |reference: &str| has_model(env, reference);
    let fallbacks = ModelFallbacks {
        review_model: saved.fallbacks_review_model.clone(),
        frame_model: saved.frame_model.clone(),
        default_provider: saved.default_provider.clone(),
        default_model: saved.default_model.clone(),
        has_model: Some(&has),
    };
    let mut config = resolve_config(&settings, &fallbacks, &logo_text());
    config.dry_run = parsed.dry_run || config.dry_run;
    config
}

/// The process environment as a map, for the names the engine reads.
fn frame_env(env: &Env) -> HashMap<String, String> {
    ["ULTRON_RLM_FRAME_MODEL", "ULTRON_REVIEW_MODEL", "ULTRON_RLM_MAP_TOKENS"]
        .into_iter()
        .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
        .collect()
}

/// The kernel-backed engine on the configured model; models the frames name resolve as the session's do: the
/// built-in catalog and models.json (custom providers and models, their keys and headers) with the profile's
/// credentials.
fn open_engine(
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    dir: &Path,
    log: Log,
) -> Result<KernelEngine, String> {
    let resolver_env = env.clone();
    let resolve: ModelResolver = Arc::new(move |reference: &str| {
        let flags = ModelFlags {
            model: Some(reference.to_string()),
            ..ModelFlags::default()
        };
        let model = session_core::lane::resolve_model(&flags, &resolver_env)?;
        let key = session_core::lane::api_key(&flags, &resolver_env, &model.provider);
        Ok((model, key))
    });
    let reference = config
        .model
        .clone()
        .ok_or("no model for autoreview: set autoreview.model or a default model (ultron --model, /model)")?;
    let (model, api_key) = resolve(&reference)?;
    let options = EngineOptions {
        dir: dir.to_path_buf(),
        runtime_path: config::paths::normalize(&env.docs.join("../src/ultron/rlm/runtime.py")),
        python: None,
        model,
        api_key,
        resolve,
        transport: Arc::new(ai::ReqwestTransport::new(None)),
        retry: saved.retry,
        frame_env: frame_env(env),
        settings: inference::ModelSettings {
            frame_model: saved.frame_model.clone(),
            frame_thinking: saved.frame_thinking.clone(),
            child_model: saved.child_model.clone(),
            review_model: saved.fallbacks_review_model.clone(),
        },
        kernel_env: Vec::new(),
        log: Some(log),
    };
    KernelEngine::open(options)
}

fn absolute(cwd: &Path, path: &str) -> PathBuf {
    config::paths::normalize(&cwd.join(path))
}

/// The engine spec of a local review (`review --repo-dir`).
fn offline_spec(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    repo_dir: &Path,
    lent: Option<&str>,
    prepared: Option<&review::prepare_env::Prepared>,
) -> Value {
    let mut spec = Map::new();
    spec.insert("repoDir".into(), repo_dir.display().to_string().into());
    spec.insert("base".into(), parsed.base.clone().unwrap_or_default().into());
    spec.insert("head".into(), parsed.head.clone().unwrap_or_default().into());
    if let Ok(Value::Object(settings)) = serde_json::to_value(engine_settings(config)) {
        spec.extend(settings);
    }
    // A local repository is the user's own: its tests may run (sandboxed) unless turned off.
    let run_tests = config.mode != "fast" && parsed.run_tests.unwrap_or(config.run_tests);
    spec.insert("runTests".into(), run_tests.into());
    match &parsed.test_env {
        Some(test_env) => {
            spec.insert(
                "testEnv".into(),
                absolute(&env.cwd, test_env).display().to_string().into(),
            );
            spec.insert("testEnvKind".into(), "testEnv".into());
        }
        None => match prepared {
            Some(prepared) => {
                spec.insert("testCheckout".into(), prepared.dir.clone().into());
                let paths: Vec<Value> = prepared
                    .toolchain
                    .iter()
                    .map(|entry| entry.path.clone().into())
                    .collect();
                spec.insert("testToolchain".into(), Value::Array(paths));
                spec.insert("testEnvKind".into(), "prepared".into());
            }
            // A checkout of the named repository under --checkout-roots, else the repository given: it is itself
            // a local checkout, and its own prepared environments serve the tests.
            None => {
                let checkout = lent.map_or_else(|| repo_dir.display().to_string(), String::from);
                spec.insert("testCheckout".into(), checkout.into());
                spec.insert("testEnvKind".into(), "checkout".into());
            }
        },
    }
    if !config.guides.is_empty() {
        let home = env.home.display().to_string();
        let guides: Vec<Value> = config
            .guides
            .iter()
            .map(|path| expand_path(path, &home, &env.cwd).into())
            .collect();
        spec.insert("guides".into(), Value::Array(guides));
    }
    if let Some(repo) = &parsed.repo {
        spec.insert("repo".into(), repo.clone().into());
    }
    if let Some(path) = &parsed.program_path {
        spec.insert(
            "programPath".into(),
            absolute(&env.cwd, path).display().to_string().into(),
        );
    }
    if let Some(path) = &parsed.dump_program_path {
        spec.insert(
            "dumpProgramPath".into(),
            absolute(&env.cwd, path).display().to_string().into(),
        );
    }
    Value::Object(spec)
}

/// `expandPath`: `~` and relative paths made absolute.
pub fn expand_path(path: &str, home: &str, cwd: &Path) -> String {
    let text = path.trim();
    if text == "~" {
        return home.to_string();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return review::js::path_join(&[home, rest]);
    }
    absolute(cwd, text).display().to_string()
}

enum Failure {
    Usage(String),
    Error(String),
}

impl From<UsageError> for Failure {
    fn from(error: UsageError) -> Self {
        Failure::Usage(error.0)
    }
}

/// Runs `ultron autoreview ...`; None when `argv` is another command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("autoreview") {
        return None;
    }
    Some(run_command(&argv[1..], env, out, err))
}

/// `runAutoreviewCommand`: the exit code (2 for a usage error).
pub fn run_command(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_command_with(args, env, &Overrides::default(), out, err)
}

/// [`run_command`] with `gh`/git and the engine replaced.
pub fn run_command_with(args: &[String], env: &Env, ov: &Overrides, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(UsageError(message)) => {
            let _ = writeln!(err, "Error: {message}");
            return 2;
        }
    };
    if parsed.help {
        let _ = writeln!(out, "{}", usage());
        return 0;
    }
    let outcome = execute(&parsed, env, ov, out, err);
    match outcome {
        Ok(code) => code,
        Err(Failure::Usage(message)) => {
            let _ = writeln!(err, "Error: {message}");
            2
        }
        Err(Failure::Error(message)) => {
            let _ = writeln!(err, "Error: {message}");
            1
        }
    }
}

fn model_lines(config: &AutoreviewConfig) -> Vec<String> {
    describe_models(config)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `age`: "never", "<n> s ago", "<n> min ago" or "<n> h ago".
pub fn age(iso: Option<&str>, now: i64) -> String {
    let Some(at) = iso.filter(|iso| !iso.is_empty()) else {
        return "never".into();
    };
    let parsed = review::ledger::parse_iso_ms(at).map_or(f64::NAN, |ms| ms as f64);
    let seconds = review::js::round((now as f64 - parsed) / 1000.0).max(0.0);
    if seconds.is_nan() {
        return "NaN h ago".into();
    }
    if seconds < 90.0 {
        format!("{} s ago", review::js::number(seconds))
    } else if seconds < 5400.0 {
        format!("{} min ago", review::js::number(review::js::round(seconds / 60.0)))
    } else {
        format!("{} h ago", review::js::number(review::js::round(seconds / 3600.0)))
    }
}

/// What tests replace (`AutoreviewEnvironment.runner` / `.engine` of cli.ts): `gh` and git, and the review engine.
#[derive(Default, Clone)]
pub struct Overrides {
    pub runner: Option<Arc<dyn review::runner::Runner>>,
    pub engine: Option<Arc<dyn ReviewEngine>>,
}

impl Overrides {
    fn runner(&self) -> Arc<dyn review::runner::Runner> {
        self.runner
            .clone()
            .unwrap_or_else(|| Arc::new(review::runner::ProcessRunner))
    }
}

/// `status`: accounts, last poll, queue and recent reviews (`--json`: one object).
fn status(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    state_path: &str,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let state = review::state::StateStore::new(state_path).read();
    let accounts = review::accounts::list_accounts(&*ov.runner(), config.accounts.as_deref()).unwrap_or_default();
    let now = now_ms();
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        let listed: Vec<Value> = accounts
            .iter()
            .map(|account| {
                let mut entry = serde_json::to_value(account).unwrap_or(Value::Null);
                if let (Some(entry), Some(saved)) = (
                    entry.as_object_mut(),
                    state.accounts.get(&review::accounts::account_key(account)),
                ) && let Ok(Value::Object(saved)) = serde_json::to_value(saved)
                {
                    entry.extend(saved);
                }
                entry
            })
            .collect();
        let json = json!({
            "accounts": listed,
            "queue": state.queue.clone().unwrap_or_default(),
            "recent": serde_json::to_value(&state.recent).unwrap_or(Value::Null),
            "daemon": state.daemon.as_ref().map_or(Value::Null, |d| serde_json::to_value(d).unwrap_or(Value::Null)),
            "dryRun": config.dry_run,
        });
        writeln!(out, "{}", review::state::to_json_1(&json)).map_err(io)?;
        return Ok(0);
    }
    let mut lines = vec![format!(
        "Posting: {}; poll every {} s; up to {} reviews at once; model {}",
        if config.dry_run { "off (dry run)" } else { "on" },
        review::js::number(config.poll_seconds.0),
        review::js::number(config.concurrency.0),
        config.model.clone().unwrap_or_else(|| "the default model".into())
    )];
    if let Some(daemon) = &state.daemon {
        lines.push(format!(
            "Daemon: pid {}, started {}{}",
            review::js::number(daemon.pid.0),
            age(Some(&daemon.started_at), now),
            daemon.engine_start_ms.map_or(String::new(), |ms| format!(
                ", engine start {} ms",
                review::js::number(ms.0)
            ))
        ));
    }
    lines.push(String::new());
    lines.push("Accounts:".into());
    if accounts.is_empty() {
        lines.push("  none (gh auth login)".into());
    }
    for account in &accounts {
        let entry = state
            .accounts
            .get(&review::accounts::account_key(account))
            .cloned()
            .unwrap_or_default();
        let paused = entry
            .paused_until
            .as_deref()
            .filter(|until| review::ledger::parse_iso_ms(until).is_some_and(|at| at > now))
            .map_or(String::new(), |until| format!(", paused until {until}"));
        let error = entry
            .last_error
            .as_deref()
            .filter(|error| !error.is_empty())
            .map_or(String::new(), |error| format!(", last error: {error}"));
        lines.push(format!(
            "  {} ({}): last poll {}{paused}{error}",
            account.login,
            account.host,
            age(entry.last_poll_at.as_deref(), now)
        ));
    }
    let queue = state.queue.clone().unwrap_or_default();
    lines.push(String::new());
    lines.push(format!("Queue: {}", if queue.is_empty() { "empty" } else { "" }));
    lines.extend(queue.iter().map(|item| format!("  {item}")));
    lines.push(String::new());
    lines.push("Recent reviews:".into());
    if state.recent.is_empty() {
        lines.push("  none".into());
    }
    for record in state.recent.iter().rev().take(15) {
        let ms = |value: Option<review::js::Num>| value.map_or(0.0, |n| n.0);
        let stages = record.stages.as_ref().map(|stages| {
            stages
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<Map<String, Value>>()
        });
        lines.push(format!(
            "  {}  {} {} as {}: {}{}, {} findings, pipeline {} s, {}{}pickup to post {} s{}{}",
            record.at,
            record.pull,
            record.sha.chars().take(7).collect::<String>(),
            record.account.rsplit('/').next().unwrap_or_default(),
            record.outcome,
            record
                .verdict
                .as_deref()
                .filter(|v| !v.is_empty())
                .map_or(String::new(), |v| format!(" {v}")),
            review::js::number(ms(record.findings)),
            review::js::number(review::js::round(ms(record.total_ms) / 1000.0)),
            record.tag_to_ack_ms.map_or(String::new(), |n| format!(
                "tag to ack {} s, ",
                review::js::to_fixed(n.0 / 1000.0, 1)
            )),
            record.ack_to_post_ms.map_or(String::new(), |n| format!(
                "ack to review {} s, ",
                review::js::number(review::js::round(n.0 / 1000.0))
            )),
            review::js::number(review::js::round(ms(record.pickup_to_post_ms) / 1000.0)),
            record
                .cost_usd
                .filter(|cost| cost.0 != 0.0)
                .map_or(String::new(), |cost| format!(", ${}", review::js::to_fixed(cost.0, 2))),
            describe_stages(stages.as_ref())
        ));
    }
    writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    Ok(0)
}

/// `doctor [--repo owner/name]`: the sandbox test runs would use and its self-check (the host's own: no Python
/// script), and with `--repo`, the local checkout whose prepared environments would be bound read-only.
fn doctor(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    model_lines: &[String],
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let report = review::review_tests::doctor(config.test_image.as_deref()).map_err(|error| {
        Failure::Error(format!(
            "the sandbox check did not run: {}",
            error.chars().take(300).collect::<String>()
        ))
    })?;
    let home = env.home.display().to_string();
    let mut lent: Option<(String, Option<String>, Vec<Value>)> = None;
    if let Some(repo) = &parsed.repo {
        let target = review::github::parse_pull_target(&format!("{repo}#1"), "github.com")
            .ok_or_else(|| Failure::Usage(format!("--repo takes owner/name, not {repo}")))?;
        let roots: Vec<String> = config
            .checkout_roots
            .iter()
            .map(|root| review::local::expand_path(root, &home))
            .collect();
        let checkout = review::local::find_checkout(&*ov.runner(), &roots, &target);
        let environments = checkout
            .as_deref()
            .map(|dir| review::review_tests::list_environments(dir, 3, Some(&home)))
            .unwrap_or_default();
        lent = Some((repo.clone(), checkout, environments));
    }
    let lent_lines: Vec<String> = match &lent {
        None => Vec::new(),
        Some((repo, None, _)) => vec![format!(
            "Local checkout of {repo}: none under autoreview.checkoutRoots with a matching remote"
        )],
        Some((repo, Some(checkout), environments)) => {
            let mut lines = vec![format!("Local checkout of {repo}: {checkout}")];
            if environments.is_empty() {
                lines
                    .push("  no prepared environment (.venv, venv, node_modules) found: nothing would be bound".into());
            }
            for item in environments {
                let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                let interpreter = text("interpreter");
                let with = if !interpreter.is_empty() && interpreter != "system" {
                    format!(", with its interpreter {interpreter}")
                } else {
                    String::new()
                };
                lines.push(format!("  bound read-only: {} ({}{with})", text("path"), text("kind")));
            }
            lines.push("  never bound: the checkout's source, .git, .env files".into());
            lines
        }
    };
    // The prepared environment of that checkout, when there is one: its toolchain and where it came from.
    let prepared = lent
        .as_ref()
        .and_then(|(_, checkout, _)| checkout.as_deref())
        .and_then(|checkout| review::prepare_env::cached_environment(checkout, cache_dir, None));
    let prepared_lines = doctor_prepared_lines(lent.is_some(), prepared.as_ref());
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if parsed.json {
        let mut json = report.as_object().cloned().unwrap_or_default();
        json.insert("runTests".into(), config.run_tests.into());
        if let Some((repo, checkout, environments)) = &lent {
            json.insert(
                "lent".into(),
                json!({"repo": repo, "checkout": checkout, "environments": environments}),
            );
        }
        if let Some(prepared) = &prepared {
            json.insert("prepared".into(), prepared.json.clone());
        }
        writeln!(out, "{}", session_file::js::stringify(&Value::Object(json))).map_err(io)?;
    } else if report.get("mechanism").is_none_or(Value::is_null) {
        let message = report.get("message").and_then(Value::as_str).unwrap_or_default();
        let mut lines = model_lines.to_vec();
        lines.push(format!("Sandbox: none. {message}"));
        lines.extend(lent_lines);
        lines.extend(prepared_lines);
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    } else {
        let check = report.get("selfCheck").cloned().unwrap_or(json!({}));
        let shown = |key: &str| match check.get(key) {
            None => "undefined".to_string(),
            Some(Value::String(text)) => text.clone(),
            Some(other) => session_file::js::stringify(other),
        };
        let owners = if config.test_owners.is_empty() {
            String::new()
        } else {
            format!(" and owners {}", config.test_owners.join(", "))
        };
        let tests_line = if !config.run_tests {
            "off (autoreview.runTests)".to_string()
        } else if ok {
            format!("run for repositories the account can push to{owners}")
        } else {
            "not run (the self-check failed)".to_string()
        };
        let mut lines = model_lines.to_vec();
        lines.extend([
            format!(
                "Sandbox: {}",
                report.get("isolation").and_then(Value::as_str).unwrap_or_default()
            ),
            format!("Self-check: {}", if ok { "passed" } else { "FAILED" }),
            format!("  network: {}", shown("network")),
            format!("  canary file in the real home: {}", shown("canary")),
            format!("  token-like variable of the caller: {}", shown("token")),
            format!("  home inside the sandbox: {}", shown("home")),
            format!(
                "  exported commit: {}; system directories: {}",
                shown("workdir"),
                shown("system")
            ),
            format!("  Docker socket: {}", shown("dockerSocket")),
            format!("Tests in reviews: {tests_line}"),
        ]);
        lines.extend(lent_lines);
        lines.extend(prepared_lines);
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    }
    Ok(if ok { 0 } else { 1 })
}

/// The environment `prepareEnvironment` reads: PATH, where mise, uv and the toolchains are looked for (the
/// processes it runs inherit the rest).
fn prepare_vars(env: &Env) -> HashMap<String, String> {
    (env.var)("PATH")
        .map(|path| HashMap::from([("PATH".to_string(), path)]))
        .unwrap_or_default()
}

/// `prepareEnvironment` of `repo_dir` as the command line runs it: log lines to stderr.
#[allow(clippy::too_many_arguments)]
fn run_prepare(
    repo_dir: &str,
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    err: &mut dyn Write,
    force: bool,
) -> Result<review::prepare_env::Prepared, Failure> {
    let runner = ov.runner();
    let err = std::cell::RefCell::new(err);
    let log = |line: &str| {
        let _ = writeln!(err.borrow_mut(), "{line}");
    };
    let options = review::prepare_env::PrepareOptions {
        runner: &*runner,
        cache_dir: cache_dir.to_string(),
        python: parsed.python.clone(),
        mise: Some(config.mise.clone()),
        env: prepare_vars(env),
        home: env.home.display().to_string(),
        log: &log,
        timeout_ms: None,
        force,
        now: &now_ms,
    };
    review::prepare_env::prepare_environment(repo_dir, &options).map_err(Failure::Error)
}

/// `prepare <repo-dir> [--python 3.x]`: build the test environment now (always rebuilt), and say what was prepared;
/// exit 1 when anything failed.
fn prepare_command(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    let Some(target) = &parsed.target else {
        return Err(Failure::Usage("prepare needs the repository directory".into()));
    };
    let repo_dir = absolute(&env.cwd, target).display().to_string();
    let prepared = run_prepare(&repo_dir, parsed, config, env, cache_dir, ov, err, true)?;
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        writeln!(out, "{}", session_file::js::stringify(&prepared.json)).map_err(io)?;
    } else {
        let mut lines = vec![format!(
            "Environment {} at {}{}",
            prepared.hash,
            prepared.dir,
            if prepared.cached { " (cached)" } else { "" }
        )];
        lines.extend(prepared.prepared.iter().map(|item| format!("  prepared: {item}")));
        lines.extend(
            review::prepare::describe_toolchain(&prepared.toolchain)
                .into_iter()
                .map(|line| format!("  toolchain: {line}")),
        );
        lines.extend(prepared.failures.iter().map(|item| format!("  failed: {item}")));
        lines.push(format!(
            "  {} s",
            review::js::number(review::js::round(prepared.ms as f64 / 1000.0))
        ));
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    }
    Ok(if prepared.failures.is_empty() { 0 } else { 1 })
}

/// The doctor's lines about the prepared environment of the lent checkout: none without `--repo`.
pub fn doctor_prepared_lines(repo_given: bool, prepared: Option<&review::prepare_env::Prepared>) -> Vec<String> {
    if !repo_given {
        return Vec::new();
    }
    let Some(prepared) = prepared else {
        return vec![format!(
            "Prepared environment: none (run `{APP_NAME} autoreview prepare <repo-dir>`)"
        )];
    };
    let mut lines = vec![format!("Prepared environment: {}", prepared.dir)];
    lines.extend(prepared.prepared.iter().map(|item| format!("  {item}")));
    lines.extend(
        review::prepare::describe_toolchain(&prepared.toolchain)
            .into_iter()
            .map(|line| format!("  toolchain: {line}")),
    );
    lines
}

/// `outcomeJson`: a posted or dry-run review as the offline JSON plus what was posted; otherwise kind and reason.
pub fn outcome_json(outcome: &review::reviewer::Outcome) -> Value {
    match outcome.reviewed() {
        Some(reviewed) => {
            let mut out = Map::new();
            out.insert("kind".into(), outcome.kind().into());
            out.insert("sha".into(), reviewed.sha.clone().into());
            out.insert("event".into(), reviewed.plan.event.into());
            if let Value::Object(offline) = offline_json(&reviewed.result, None, None, None) {
                out.extend(offline);
            }
            out.insert("verdict".into(), reviewed.verdict.into());
            out.insert("inlineComments".into(), json!(reviewed.plan.comments.len()));
            if let Some(id) = reviewed.review_id {
                out.insert("reviewId".into(), json!(id));
            }
            if let Some(path) = &reviewed.path {
                out.insert("files".into(), json!([format!("{path}.json"), format!("{path}.md")]));
            }
            Value::Object(out)
        }
        None => json!({"kind": outcome.kind(), "reason": outcome.reason().unwrap_or_default()}),
    }
}

/// `outcomeText`.
pub fn outcome_text(name: &str, outcome: &review::reviewer::Outcome) -> String {
    if let Some(reason) = outcome.reason() {
        return format!("{name}: {} ({reason})", outcome.kind());
    }
    let Some(reviewed) = outcome.reviewed() else {
        return format!("{name}: {}", outcome.kind());
    };
    let short: String = reviewed.sha.chars().take(7).collect();
    let findings = reviewed
        .result
        .get("findings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if outcome.kind() == "posted" {
        format!(
            "Posted {} on {name} at {short}: {findings} findings, {} inline.",
            reviewed.plan.event,
            reviewed.plan.comments.len()
        )
    } else {
        format!(
            "Dry run of {name} at {short}: {}, {findings} findings. Written to {}.md and .json",
            reviewed.plan.event,
            reviewed.path.clone().unwrap_or_default()
        )
    }
}

/// The reviewer's dependencies over the real `gh`, git, state, engine and (with `learn`) Hindsight.
fn reviewer_deps(
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
) -> Result<review::reviewer::ReviewerDeps, Failure> {
    let log = logger(&env.var);
    let engine: Arc<dyn ReviewEngine> = match &ov.engine {
        Some(engine) => engine.clone(),
        None => Arc::new(open_engine(config, saved, env, Path::new(&paths.dir), log.clone()).map_err(Failure::Error)?),
    };
    let runner = ov.runner();
    let runner_for_envs = runner.clone();
    let lessons: Option<Arc<dyn review::reviewer::LessonMemory>> = if config.learn {
        let agent_dir = env.agent_dir();
        let manager = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
        let url = hindsight::resolve::hindsight_url(
            (env.var)("ULTRON_HINDSIGHT_URL").as_deref(),
            hindsight::resolve::setting_url(&manager).as_deref(),
        );
        url.and_then(|url| hindsight::lessons::HindsightLessons::new(&url, Box::new(now_ms), None).ok())
            .map(|lessons| Arc::new(lessons) as Arc<dyn review::reviewer::LessonMemory>)
    } else {
        None
    };
    Ok(review::reviewer::ReviewerDeps {
        runner: runner.clone(),
        tokens: Arc::new(review::accounts::TokenStore::new(runner.clone())),
        engine,
        store: Arc::new(review::state::StateStore::new(&paths.state)),
        checkouts: Arc::new(review::checkout::CheckoutManager::new(runner, &paths.cache)),
        config: config.clone(),
        paths: paths.clone(),
        log,
        now: Arc::new(now_ms),
        random: Arc::new(random),
        lessons,
        // Test environments are built before a review (autoreview.prepareEnvs), with the network, into the cache.
        environments: Some(Arc::new(review::prepare_env::RunnerPreparer {
            runner: runner_for_envs,
            cache_dir: paths.cache.clone(),
            mise: Some(config.mise.clone()),
            env: prepare_vars(env),
            home: env.home.display().to_string(),
            now: Arc::new(now_ms),
        })),
        home: env.home.display().to_string(),
    })
}

/// A number in [0, 1) (`Math.random`), for the acknowledgement line.
fn random() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom_fill(&mut bytes).is_err() {
        return 0.0;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

fn getrandom_fill(bytes: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}

/// `review <owner/repo#N | URL>`: one pull request now, as the host's active account (or `--account`).
fn review_pull_request(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let target = parsed
        .target
        .as_deref()
        .ok_or_else(|| Failure::Usage("review needs a pull request (owner/repo#N or its URL) or --repo-dir".into()))?;
    let pull = review::github::parse_pull_target(target, "github.com").ok_or_else(|| {
        Failure::Usage(format!(
            "not a pull request: {target} (use owner/repo#N or the pull request URL)"
        ))
    })?;
    let only = match &parsed.account {
        Some(account) => Some(vec![account.clone()]),
        None => config.accounts.clone(),
    };
    let accounts: Vec<review::accounts::Account> = review::accounts::list_accounts(&*ov.runner(), only.as_deref())
        .map_err(|error| Failure::Error(error.to_string()))?
        .into_iter()
        .filter(|account| account.host == pull.host)
        .collect();
    let account = accounts
        .iter()
        .find(|a| a.active)
        .or(accounts.first())
        .cloned()
        .ok_or_else(|| {
            Failure::Error(format!(
                "gh is not logged in to {}{}",
                pull.host,
                parsed.account.as_ref().map_or(String::new(), |a| format!(" as {a}"))
            ))
        })?;
    let deps = reviewer_deps(config, saved, env, paths, ov)?;
    let candidate = review::reviewer::Candidate {
        account,
        pull: pull.clone(),
        reasons: vec!["manual".into()],
        picked_at: now_ms(),
        notified_at: None,
    };
    let options = review::reviewer::ReviewOptions {
        force: true,
        dry_run: Some(config.dry_run),
    };
    let outcome =
        review::reviewer::review_pull(&deps, &candidate, options).map_err(|error| Failure::Error(error.to_string()))?;
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        writeln!(out, "{}", session_file::js::stringify(&outcome_json(&outcome))).map_err(io)?;
    } else {
        writeln!(out, "{}", outcome_text(&review::github::pull_key(&pull), &outcome)).map_err(io)?;
    }
    Ok(if matches!(outcome.kind(), "posted" | "dry-run") {
        0
    } else {
        1
    })
}

/// `once` (one poll cycle, then exit) and `run` (poll and review until SIGINT/SIGTERM): the daemon over every
/// account gh is logged in to (or `autoreview.accounts`), holding the agent directory's daemon lock.
fn daemon_command(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let mut deps = reviewer_deps(config, saved, env, paths, ov)?;
    let mask_setting = (env.var)("ULTRON_MASK_SECRETS");
    let masker = Arc::new(host::secrets::OutputMasker::default());
    deps.log = review::daemon::create_logger(
        Some(paths.logs.clone()),
        Arc::new(now_ms),
        review::daemon::stderr_writer(),
        Arc::new(move |line: &str| masker.mask_counted(line, mask_setting.as_deref()).0),
    );
    let log = deps.log.clone();
    let store = deps.store.clone();
    let accounts = review::accounts::list_accounts(&*ov.runner(), config.accounts.as_deref())
        .map_err(|error| Failure::Error(error.to_string()))?;
    if accounts.is_empty() {
        return Err(Failure::Error(
            "gh is not logged in to any account (gh auth login)".into(),
        ));
    }
    let outcomes: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let recorded = outcomes.clone();
    let on_outcome: review::daemon::OnOutcome = Arc::new(move |candidate, reported| {
        let mut entry = Map::new();
        entry.insert("pull".into(), review::github::pull_key(&candidate.pull).into());
        entry.insert("account".into(), candidate.account.login.clone().into());
        let details = match reported {
            review::daemon::Reported::Outcome(outcome) => outcome_json(outcome),
            review::daemon::Reported::Failed(error) => json!({"kind": "failed", "error": error}),
        };
        if let Value::Object(details) = details {
            entry.extend(details);
        }
        recorded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Value::Object(entry));
    });
    let daemon = review::daemon::Daemon::new(review::daemon::DaemonDeps {
        reviewer: deps,
        accounts: accounts.clone(),
        sleep: None,
        on_outcome: Some(on_outcome),
    });
    let lock = review::state::acquire_daemon_lock(&paths.dir).map_err(|error| match error {
        review::github::Error::DaemonRunning(_) => Failure::Error(error.to_string()),
        other => Failure::Error(format!("cannot lock {}: {other}", paths.dir)),
    })?;
    let names: Vec<String> = accounts.iter().map(|a| format!("{}@{}", a.login, a.host)).collect();
    log(&format!(
        "autoreview {}: {}; posting {}",
        parsed.command,
        names.join(", "),
        if config.dry_run { "off (dry run)" } else { "on" }
    ));
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.command == "once" {
        daemon.once();
        lock.release();
        if parsed.json {
            let reviews = outcomes.lock().unwrap_or_else(|e| e.into_inner()).clone();
            writeln!(out, "{}", session_file::js::stringify(&json!({"reviews": reviews}))).map_err(io)?;
        }
        return Ok(0);
    }
    let _ = review::daemon::record_daemon_start(&store, std::process::id(), now_ms(), None);
    let stopper = daemon.stopper();
    daemon::signals::set_handler(Arc::new(move |_| stopper.stop()));
    daemon.run();
    daemon::signals::clear_handler();
    let _ = review::daemon::record_daemon_stop(&store);
    log("autoreview stopped");
    lock.release();
    Ok(0)
}

/// `install | uninstall`: the user service file is written or removed; enabling it is printed, never run.
/// The platform name upstream uses (`process.platform`): `darwin` for macOS.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// The user service that runs `ultron autoreview run` on `platform`, or None where neither systemd nor launchd
/// applies (`ultron autoreview install`, `/autoreview on`).
pub fn user_service(env: &Env, platform: &str) -> Result<Option<review::service::ServiceFile>, String> {
    let me = claude_root::self_cmd::self_command(&*env.var)?;
    let me = review::service::SelfCommand {
        command: me.command,
        args: me.args,
    };
    let service_env: HashMap<String, String> = ["PATH", "ULTRON_CODING_AGENT_DIR", "XDG_CONFIG_HOME"]
        .into_iter()
        .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
        .collect();
    Ok(review::service::service_file(
        &me,
        platform,
        &env.home.display().to_string(),
        &service_env,
    ))
}

fn service(parsed: &Parsed, config: &AutoreviewConfig, env: &Env, out: &mut dyn Write) -> Result<i32, Failure> {
    let platform = platform();
    let Some(file) = user_service(env, platform).map_err(Failure::Error)? else {
        return Err(Failure::Error(format!(
            "no user service on {platform}; run \"{APP_NAME} autoreview run\" under your own supervisor"
        )));
    };
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.command == "install" {
        review::service::install_service(&file).map_err(Failure::Error)?;
        let enable: Vec<String> = file.enable.iter().map(|line| format!("  {line}")).collect();
        writeln!(
            out,
            "Wrote {}\n{}\nNot enabled. To start it now and at login:\n{}",
            file.path,
            describe_models(config).join("\n"),
            enable.join("\n")
        )
        .map_err(io)?;
    } else if review::service::uninstall_service(&file) {
        let disable: Vec<String> = file.disable.iter().map(|line| format!("  {line}")).collect();
        writeln!(
            out,
            "Removed {}\nIf it is still running:\n{}",
            file.path,
            disable.join("\n")
        )
        .map_err(io)?;
    } else {
        writeln!(out, "No service file at {}", file.path).map_err(io)?;
    }
    Ok(0)
}

fn execute(
    parsed: &Parsed,
    env: &Env,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    let saved = saved_settings(env);
    let config = configure(parsed, &saved, env);
    let process_env: HashMap<String, String> = ["ULTRON_AUTOREVIEW_CACHE_DIR", "XDG_CACHE_HOME"]
        .into_iter()
        .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
        .collect();
    let paths = autoreview_paths(
        &env.agent_dir().display().to_string(),
        &process_env,
        &env.home.display().to_string(),
    );
    match parsed.command.as_str() {
        "review" => {}
        "run" | "once" => {
            // `/autoreview off` (autoreview.enabled: false): the loop and a one-off poll do nothing.
            if saved.enabled == Some(false) {
                let _ = writeln!(
                    err,
                    "autoreview is off (autoreview.enabled: false; /autoreview on or {APP_NAME} autoreview is not needed for one review)"
                );
                return Ok(0);
            }
            return daemon_command(parsed, &config, &saved, env, &paths, ov, out);
        }
        "install" | "uninstall" => return service(parsed, &config, env, out),
        "status" => return status(parsed, &config, &paths.state, ov, out),
        "doctor" => return doctor(parsed, &config, &model_lines(&config), env, &paths.cache, ov, out),
        "prepare" => return prepare_command(parsed, &config, env, &paths.cache, ov, out, err),
        other => {
            return Err(Failure::Usage(format!("unknown command: {other}\n\n{}", usage())));
        }
    }
    let Some(repo_dir) = &parsed.repo_dir else {
        return review_pull_request(parsed, &config, &saved, env, &paths, ov, out);
    };
    let log = logger(&env.var);
    let dir = PathBuf::from(&paths.dir);
    if parsed.target.is_some() {
        return Err(UsageError("give a pull request or --repo-dir, not both".into()).into());
    }
    let (Some(base), Some(head)) = (&parsed.base, &parsed.head) else {
        return Err(UsageError("--repo-dir needs --base and --head".into()).into());
    };
    if base.is_empty() || head.is_empty() {
        return Err(UsageError("--repo-dir needs --base and --head".into()).into());
    }
    if (parsed.program_path.is_some() || parsed.dump_program_path.is_some()) && config.mode != "compiled" {
        return Err(UsageError("--program and --dump-program need --mode compiled".into()).into());
    }
    let repo_dir = absolute(&env.cwd, repo_dir);
    // A checkout of the named repository under --checkout-roots lends its environments to the tests.
    let lent = match &parsed.repo {
        Some(repo) if !config.checkout_roots.is_empty() => {
            review::github::parse_pull_target(&format!("{repo}#1"), "github.com")
                .and_then(|named| review::local::find_checkout(&*ov.runner(), &config.checkout_roots, &named))
        }
        _ => None,
    };
    // --prepare: build (or reuse) the environment first, with the network; the review then binds it read-only.
    let prepared = if parsed.prepare {
        let prepared = run_prepare(
            &repo_dir.display().to_string(),
            parsed,
            &config,
            env,
            &paths.cache,
            ov,
            err,
            false,
        )?;
        let mut lines = vec![format!(
            "prepared environment {}{}: {}",
            prepared.hash,
            if prepared.cached { " (cached)" } else { "" },
            if prepared.prepared.is_empty() {
                "nothing".to_string()
            } else {
                prepared.prepared.join("; ")
            }
        )];
        lines.extend(prepared.failures.iter().map(|item| format!("prepare: {item}")));
        for line in lines {
            let _ = writeln!(err, "{line}");
        }
        Some(prepared)
    } else {
        None
    };
    let use_prepared = prepared
        .as_ref()
        .filter(|prepared| prepared.failures.is_empty() && !prepared.prepared.is_empty());
    let (engine, start_ms): (Arc<dyn ReviewEngine>, Option<u64>) = match &ov.engine {
        Some(engine) => (engine.clone(), None),
        None => {
            let engine = open_engine(&config, &saved, env, &dir, log).map_err(Failure::Error)?;
            let start_ms = engine.start_ms;
            (Arc::new(engine), Some(start_ms))
        }
    };
    let spec = offline_spec(parsed, &config, env, &repo_dir, lent.as_deref(), use_prepared);
    let result = engine.review(&spec, None).map_err(Failure::Error)?;
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        let max_comments = Some(config.max_comments.0 as i64);
        let json = offline_json(&result, start_ms, Some(&config.block_at), max_comments);
        writeln!(out, "{}", session_file::js::stringify(&json)).map_err(io)?;
    } else {
        let typed: EngineResult = serde_json::from_value(result).unwrap_or_default();
        let plan = plan_review(
            &typed,
            &PlanOptions {
                self_authored: false,
                state: "open".into(),
                head_sha: head.clone(),
                since_sha: None,
                signature: config.signature,
                block_at: Some(config.block_at.clone()),
                max_comments: Some(config.max_comments.0 as i64),
            },
        );
        writeln!(out, "{}", plan.body).map_err(io)?;
    }
    Ok(0)
}
