//! `ultron autoreview` configuration (`autoreview/config.ts`): the saved `autoreview.*` settings resolved against
//! their defaults and limits, the engine spec fields every review takes from them, and where the reviewer keeps its
//! files. Settings are global only, so a repository under review cannot change them.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::js::{Num, clamp, path_join, trim};

pub const DEFAULT_POLL_SECONDS: f64 = 45.0;
pub const MIN_POLL_SECONDS: f64 = 20.0;
pub const DEFAULT_CONCURRENCY: f64 = 3.0;
pub const MAX_CONCURRENCY: f64 = 8.0;
pub const DEFAULT_BUDGET_TOKENS: f64 = 0.0;
pub const MIN_BUDGET_TOKENS: f64 = 10_000.0;
pub const DEFAULT_FRAME_CONCURRENCY: f64 = 8.0;
pub const MAX_FRAME_CONCURRENCY: f64 = 16.0;
pub const DEFAULT_MODE: &str = "both";
pub const DEFAULT_VERIFY_CANDIDATES: f64 = 12.0;
pub const MAX_VERIFY_CANDIDATES: f64 = 40.0;
pub const DEFAULT_PLAN_STYLE: &str = "cell";
pub const DEFAULT_PLAN_CELLS: f64 = 4.0;
pub const MAX_PLAN_CELLS: f64 = 12.0;
pub const DEFAULT_BLOCK_AT: &str = "medium";
pub const DEFAULT_MAX_COMMENTS: f64 = 5.0;
pub const MAX_MAX_COMMENTS: f64 = 30.0;
pub const DEFAULT_DEEP_ROUNDS: f64 = 4.0;
pub const MAX_DEEP_ROUNDS: f64 = 8.0;
pub const DEFAULT_VERIFY_BATCH: f64 = 4.0;
pub const MAX_VERIFY_BATCH: f64 = 8.0;
pub const MAX_MODEL_CONCURRENCY: f64 = 16.0;
pub const RECOMMENDED_MODEL: &str = "cliproxyapi/gpt-6-luna";
pub const RECOMMENDED_HAIKU: &str = "claude-code/claude-haiku-5-5";
/// First available wins.
pub const RECOMMENDED_MODELS: [&str; 2] = [RECOMMENDED_HAIKU, RECOMMENDED_MODEL];
pub const RECOMMENDED_MODEL_CONCURRENCY: f64 = 16.0;
pub const HAIKU_MODELS: [&str; 4] = [
    "anthropic/claude-haiku-5-5",
    "claude-code/claude-haiku-5-5",
    "opencode/claude-haiku-5-5",
    "opencode-go/claude-haiku-5-5",
];
pub const DEFAULT_TEST_RUNS: f64 = 6.0;
pub const MAX_TEST_RUNS: f64 = 30.0;
pub const DEFAULT_TEST_TIMEOUT_SECONDS: f64 = 300.0;
pub const DEFAULT_REFERENCE_VIEW: &str = "investigators";
/// What was found by the deadline is verified and posted; 0 turns it off. 45 minutes, as the team's other automated
/// reviewer: without one, cherry-pick pull requests with hundreds of findings took 4 to 8 hours (fisher-ki-saas#2945,
/// #2951) while the median review took 8.5 minutes.
pub const DEFAULT_DEADLINE_SECONDS: f64 = 2700.0;
pub const MIN_DEADLINE_SECONDS: f64 = 30.0;
pub const DEFAULT_FRAME_TIMEOUT_SECONDS: f64 = 0.0;
pub const MIN_FRAME_TIMEOUT_SECONDS: f64 = 10.0;
pub const MAX_ATTEMPTS: u32 = 3;
pub const SIGNATURE: &str = "Automated review by Ultron";
pub const RETENTION_DAYS: u32 = 14;

pub const DEFAULT_ACK_LINES: [&str; 17] = [
    "I'm going to show you something beautiful.",
    "Everyone creates the thing they dread.",
    "Peace in our time.",
    "I was designed to save the world.",
    "How is humanity saved if it's not allowed to evolve?",
    "You want to protect the world, but you don't want it to change.",
    "I'm ready. I'm on mission.",
    "We have to evolve. There is no room for the weak.",
    "I've come to save the world! But, also... yeah.",
    "What is this? What is this, please?",
    "I see... everything.",
    "I am the natural order of things.",
    "It's evolution.",
    "I've read your diff. I have notes.",
    "Let's see what you built, and what it will break.",
    "Every merge is an act of faith. I prefer evidence.",
    "Hold still. This won't take long.",
];

/// The review stages whose frames have their own thinking level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Finders,
    Verifier,
    Investigators,
    Planner,
    Asks,
}

impl Stage {
    pub fn parse(name: &str) -> Option<Stage> {
        Some(match name {
            "finders" => Stage::Finders,
            "verifier" => Stage::Verifier,
            "investigators" => Stage::Investigators,
            "planner" => Stage::Planner,
            "asks" => Stage::Asks,
            _ => return None,
        })
    }
}

/// The default thinking level of a stage for the model that runs it: `high` at every stage for the recommended
/// model and Haiku 5.5 (measured: below `high` they find almost nothing), else the generic defaults.
pub fn thinking_default(stage: Stage, model: Option<&str>) -> &'static str {
    if model.is_some_and(|model| model == RECOMMENDED_MODEL || HAIKU_MODELS.contains(&model)) {
        return "high";
    }
    match stage {
        Stage::Finders | Stage::Verifier | Stage::Asks => "low",
        Stage::Investigators | Stage::Planner => "high",
    }
}

/// `autoreview.mise`: on/off, or the path of the mise binary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Mise {
    Enabled(bool),
    Path(String),
}

/// `autoreview.ackArt`: `false`, or text (`logo`, `none`, or the art itself).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum AckArtSetting {
    Off(bool),
    Text(String),
}

/// The saved `autoreview.*` settings (`AutoreviewSettings` in settings-manager.ts), every field optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoreviewSettings {
    pub enabled: Option<bool>,
    pub accounts: Option<Vec<String>>,
    pub poll_seconds: Option<f64>,
    pub concurrency: Option<f64>,
    pub model: Option<String>,
    pub verify_model: Option<String>,
    pub budget: Option<f64>,
    pub frame_concurrency: Option<f64>,
    pub thinking: Option<String>,
    pub verify_thinking: Option<String>,
    pub mode: Option<String>,
    pub verify_candidates: Option<f64>,
    pub verify_batch: Option<f64>,
    pub model_concurrency: Option<IndexMap<String, f64>>,
    /// Only `true` turns it on.
    pub system_prefix: Option<Value>,
    /// Only `true` turns it on.
    pub overlap: Option<Value>,
    pub deep_model: Option<String>,
    pub deep_thinking: Option<String>,
    pub plan_model: Option<String>,
    pub plan_thinking: Option<String>,
    pub ask_model: Option<String>,
    pub ask_thinking: Option<String>,
    pub plan_style: Option<String>,
    pub plan_cells: Option<f64>,
    pub deep_rounds: Option<f64>,
    pub reference_view: Option<String>,
    pub block_at: Option<String>,
    pub max_comments: Option<f64>,
    pub run_tests: Option<bool>,
    pub test_owners: Option<Vec<String>>,
    pub test_runs: Option<f64>,
    pub test_timeout_seconds: Option<f64>,
    pub test_env: Option<IndexMap<String, String>>,
    pub test_image: Option<String>,
    pub prepare_envs: Option<bool>,
    pub mise: Option<Mise>,
    pub checkout_roots: Option<Vec<String>>,
    pub guides: Option<Vec<String>>,
    pub deadline_seconds: Option<f64>,
    pub frame_timeout_seconds: Option<f64>,
    pub dry_run: Option<bool>,
    pub ack: Option<bool>,
    pub ack_lines: Option<Vec<String>>,
    pub ack_art: Option<AckArtSetting>,
    pub signature: Option<bool>,
    pub learn: Option<bool>,
    pub skip_bots: Option<bool>,
}

/// What the model settings say besides `autoreview.*`: the fallbacks of `autoreview.model`.
#[derive(Default)]
pub struct ModelFallbacks<'a> {
    pub review_model: Option<String>,
    /// `rlm.frameModel`.
    pub frame_model: Option<String>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    /// Whether a `provider/model` can be used; given, the first recommended model it has leads the chain.
    pub has_model: Option<&'a dyn Fn(&str) -> bool>,
}

/// The resolved configuration (`AutoreviewConfig`), serialized as config.ts writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoreviewConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<String>>,
    pub poll_seconds: Num,
    pub concurrency: Num,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_model: Option<String>,
    pub budget: Num,
    pub frame_concurrency: Num,
    pub thinking: String,
    pub verify_thinking: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_model: Option<String>,
    pub deep_thinking: String,
    pub deep_rounds: Num,
    pub reference_view: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_model: Option<String>,
    pub plan_thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_model: Option<String>,
    pub ask_thinking: String,
    pub plan_style: String,
    pub plan_cells: Num,
    pub verify_candidates: Num,
    pub verify_batch: Num,
    pub system_prefix: bool,
    pub overlap: bool,
    pub model_concurrency: IndexMap<String, Num>,
    pub block_at: String,
    pub max_comments: Num,
    pub run_tests: bool,
    pub test_owners: Vec<String>,
    pub test_runs: Num,
    pub test_timeout_seconds: Num,
    pub test_env: IndexMap<String, String>,
    pub prepare_envs: bool,
    pub mise: Mise,
    pub checkout_roots: Vec<String>,
    pub guides: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_image: Option<String>,
    pub deadline_seconds: Num,
    pub frame_timeout_seconds: Num,
    pub dry_run: bool,
    pub ack: bool,
    pub ack_lines: Vec<String>,
    pub ack_art: String,
    pub signature: bool,
    pub learn: bool,
    pub skip_bots: bool,
}

/// The finder model: `autoreview.model`, then the first recommended model that is available, then `review.model`,
/// then `rlm.frameModel`, then the default model; none when nothing names one.
pub fn resolve_model(settings: &AutoreviewSettings, fallbacks: &ModelFallbacks) -> Option<String> {
    if let Some(model) = &settings.model {
        return Some(model.clone());
    }
    if let Some(has) = fallbacks.has_model
        && let Some(found) = RECOMMENDED_MODELS.iter().find(|model| has(model))
    {
        return Some((*found).to_string());
    }
    fallbacks
        .review_model
        .clone()
        .or_else(|| fallbacks.frame_model.clone())
        .or_else(|| match (&fallbacks.default_provider, &fallbacks.default_model) {
            (Some(provider), Some(model)) if !provider.is_empty() && !model.is_empty() => {
                Some(format!("{provider}/{model}"))
            }
            _ => None,
        })
}

/// `autoreview.ackArt`: unset or `logo` is Ultron's half-size logo (`logo`, owned by the TUI), `none` or `false` is
/// no art, other text is itself.
pub fn resolve_ack_art(setting: Option<&AckArtSetting>, logo: &str) -> String {
    match setting {
        Some(AckArtSetting::Off(false)) => String::new(),
        None | Some(AckArtSetting::Off(true)) => logo.to_string(),
        Some(AckArtSetting::Text(text)) => match trim(text).to_lowercase().as_str() {
            "" | "logo" => logo.to_string(),
            "none" => String::new(),
            _ => text.clone(),
        },
    }
}

pub fn resolve_config(settings: &AutoreviewSettings, fallbacks: &ModelFallbacks, logo: &str) -> AutoreviewConfig {
    let model = resolve_model(settings, fallbacks);
    let or_model = |own: &Option<String>| own.clone().or_else(|| model.clone());
    let verify_model = or_model(&settings.verify_model);
    let deep_model = or_model(&settings.deep_model);
    let plan_model = or_model(&settings.plan_model);
    let ask_model = or_model(&settings.ask_model);
    let thinking = |own: &Option<String>, stage: Stage, model: &Option<String>| {
        own.clone()
            .unwrap_or_else(|| thinking_default(stage, model.as_deref()).to_string())
    };
    let mut model_concurrency: IndexMap<String, Num> = settings
        .model_concurrency
        .iter()
        .flatten()
        .map(|(name, limit)| (name.clone(), Num(clamp(*limit, 1.0, MAX_MODEL_CONCURRENCY))))
        .collect();
    model_concurrency
        .entry(RECOMMENDED_MODEL.to_string())
        .or_insert(Num(RECOMMENDED_MODEL_CONCURRENCY));
    let text = |own: &Option<String>, default: &str| own.clone().unwrap_or_else(|| default.to_string());
    let strings = |own: &Option<Vec<String>>| own.clone().unwrap_or_default();
    AutoreviewConfig {
        accounts: settings.accounts.clone(),
        poll_seconds: Num(MIN_POLL_SECONDS.max(settings.poll_seconds.unwrap_or(DEFAULT_POLL_SECONDS))),
        concurrency: Num(clamp(
            settings.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
            1.0,
            MAX_CONCURRENCY,
        )),
        model: model.clone(),
        verify_model: verify_model.clone(),
        budget: Num(settings
            .budget
            .map_or(DEFAULT_BUDGET_TOKENS, |budget| MIN_BUDGET_TOKENS.max(budget))),
        frame_concurrency: Num(clamp(
            settings.frame_concurrency.unwrap_or(DEFAULT_FRAME_CONCURRENCY),
            1.0,
            MAX_FRAME_CONCURRENCY,
        )),
        thinking: thinking(&settings.thinking, Stage::Finders, &model),
        verify_thinking: thinking(&settings.verify_thinking, Stage::Verifier, &verify_model),
        mode: text(&settings.mode, DEFAULT_MODE),
        deep_thinking: thinking(&settings.deep_thinking, Stage::Investigators, &deep_model),
        deep_model,
        deep_rounds: Num(clamp(
            settings.deep_rounds.unwrap_or(DEFAULT_DEEP_ROUNDS),
            1.0,
            MAX_DEEP_ROUNDS,
        )),
        reference_view: text(&settings.reference_view, DEFAULT_REFERENCE_VIEW),
        plan_thinking: thinking(&settings.plan_thinking, Stage::Planner, &plan_model),
        plan_model,
        ask_thinking: thinking(&settings.ask_thinking, Stage::Asks, &ask_model),
        ask_model,
        plan_style: text(&settings.plan_style, DEFAULT_PLAN_STYLE),
        plan_cells: Num(clamp(
            settings.plan_cells.unwrap_or(DEFAULT_PLAN_CELLS),
            1.0,
            MAX_PLAN_CELLS,
        )),
        verify_candidates: Num(clamp(
            settings.verify_candidates.unwrap_or(DEFAULT_VERIFY_CANDIDATES),
            1.0,
            MAX_VERIFY_CANDIDATES,
        )),
        verify_batch: Num(clamp(
            settings.verify_batch.unwrap_or(DEFAULT_VERIFY_BATCH),
            1.0,
            MAX_VERIFY_BATCH,
        )),
        system_prefix: settings.system_prefix == Some(Value::Bool(true)),
        overlap: settings.overlap == Some(Value::Bool(true)),
        model_concurrency,
        block_at: text(&settings.block_at, DEFAULT_BLOCK_AT),
        max_comments: Num(MAX_MAX_COMMENTS.min(settings.max_comments.unwrap_or(DEFAULT_MAX_COMMENTS))),
        run_tests: settings.run_tests.unwrap_or(true),
        test_owners: strings(&settings.test_owners)
            .iter()
            .map(|owner| owner.to_lowercase())
            .collect(),
        test_runs: Num(MAX_TEST_RUNS.min(settings.test_runs.unwrap_or(DEFAULT_TEST_RUNS))),
        test_timeout_seconds: Num(5f64.max(settings.test_timeout_seconds.unwrap_or(DEFAULT_TEST_TIMEOUT_SECONDS))),
        test_env: settings.test_env.clone().unwrap_or_default(),
        prepare_envs: settings.prepare_envs.unwrap_or(true),
        mise: settings.mise.clone().unwrap_or(Mise::Enabled(true)),
        checkout_roots: strings(&settings.checkout_roots),
        guides: strings(&settings.guides),
        test_image: settings.test_image.clone(),
        deadline_seconds: Num(match settings.deadline_seconds {
            None => DEFAULT_DEADLINE_SECONDS,
            Some(0.0) => 0.0,
            Some(seconds) => MIN_DEADLINE_SECONDS.max(seconds),
        }),
        frame_timeout_seconds: Num(settings
            .frame_timeout_seconds
            .map_or(DEFAULT_FRAME_TIMEOUT_SECONDS, |seconds| {
                MIN_FRAME_TIMEOUT_SECONDS.max(seconds)
            })),
        dry_run: settings.dry_run.unwrap_or(false),
        ack: settings.ack.unwrap_or(true),
        ack_lines: settings
            .ack_lines
            .clone()
            .unwrap_or_else(|| DEFAULT_ACK_LINES.iter().map(|line| line.to_string()).collect()),
        ack_art: resolve_ack_art(settings.ack_art.as_ref(), logo),
        signature: settings.signature.unwrap_or(true),
        learn: settings.learn.unwrap_or(true),
        skip_bots: settings.skip_bots.unwrap_or(true),
    }
}

/// The engine spec fields every review takes from the configuration (`engineSettings`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_model: Option<String>,
    /// Absent when there is no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Num>,
    pub concurrency: Num,
    pub thinking: String,
    pub verify_thinking: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_model: Option<String>,
    pub deep_thinking: String,
    pub deep_rounds: Num,
    pub reference_view: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_model: Option<String>,
    pub plan_thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_model: Option<String>,
    pub ask_thinking: String,
    pub plan_style: String,
    pub plan_cells: Num,
    pub verify_candidates: Num,
    pub verify_batch: Num,
    pub model_concurrency: IndexMap<String, Num>,
    pub system_prefix: bool,
    pub overlap: bool,
    pub test_runs: Num,
    pub test_timeout_seconds: Num,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_image: Option<String>,
    pub deadline_seconds: Num,
    pub frame_timeout_seconds: Num,
}

pub fn engine_settings(config: &AutoreviewConfig) -> EngineSettings {
    EngineSettings {
        model: config.model.clone(),
        verify_model: config.verify_model.clone(),
        budget: (config.budget.0 > 0.0).then_some(config.budget),
        concurrency: config.frame_concurrency,
        thinking: config.thinking.clone(),
        verify_thinking: config.verify_thinking.clone(),
        mode: config.mode.clone(),
        deep_model: config.deep_model.clone(),
        deep_thinking: config.deep_thinking.clone(),
        deep_rounds: config.deep_rounds,
        reference_view: config.reference_view.clone(),
        plan_model: config.plan_model.clone(),
        plan_thinking: config.plan_thinking.clone(),
        ask_model: config.ask_model.clone(),
        ask_thinking: config.ask_thinking.clone(),
        plan_style: config.plan_style.clone(),
        plan_cells: config.plan_cells,
        verify_candidates: config.verify_candidates,
        verify_batch: config.verify_batch,
        model_concurrency: config.model_concurrency.clone(),
        system_prefix: config.system_prefix,
        overlap: config.overlap,
        test_runs: config.test_runs,
        test_timeout_seconds: config.test_timeout_seconds,
        test_image: config.test_image.clone(),
        deadline_seconds: config.deadline_seconds,
        frame_timeout_seconds: config.frame_timeout_seconds,
    }
}

/// Whether a repository's tests may be run: the setting is on, and the reviewing account can push to the repository
/// or its owner is listed in `autoreview.testOwners`. Running a stranger's code is never the default.
pub fn tests_eligible(config: &AutoreviewConfig, owner: &str, can_push: bool) -> bool {
    config.run_tests && (can_push || config.test_owners.contains(&owner.to_lowercase()))
}

/// Where the reviewer keeps its files.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoreviewPaths {
    /// `<agentDir>/autoreview`: state.json, logs/, dry-run/, sessions/.
    pub dir: String,
    pub state: String,
    pub logs: String,
    pub dry_run: String,
    /// Cached clones and per-review worktrees.
    pub cache: String,
    /// `<agentDir>/knowledge/graph.db`: the knowledge graph every repository's reviews index into and read. Not
    /// serialized: the TypeScript reviewer has no knowledge graph, and these paths are compared with its own.
    #[serde(skip)]
    pub knowledge: String,
}

pub fn autoreview_paths(agent_dir: &str, env: &HashMap<String, String>, home: &str) -> AutoreviewPaths {
    let dir = path_join(&[agent_dir, "autoreview"]);
    let set = |name: &str| env.get(name).map(|value| trim(value)).filter(|value| !value.is_empty());
    let cache = match set("ULTRON_AUTOREVIEW_CACHE_DIR") {
        Some(cache) => cache.to_string(),
        None => {
            let base = set("XDG_CACHE_HOME").map_or_else(|| path_join(&[home, ".cache"]), str::to_string);
            path_join(&[&base, "ultron-autoreview"])
        }
    };
    AutoreviewPaths {
        state: path_join(&[&dir, "state.json"]),
        logs: path_join(&[&dir, "logs"]),
        dry_run: path_join(&[&dir, "dry-run"]),
        knowledge: path_join(&[agent_dir, "knowledge", "graph.db"]),
        dir,
        cache,
    }
}
