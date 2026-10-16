//! `ultron setup` (cli/setup/wizard.ts): a guided first configuration in five steps (environment, provider and
//! model, Hindsight, Loki guardrails, summary). Every step can be skipped with Esc and the wizard can be run again;
//! nothing is overwritten without asking, and secrets are read masked and never printed. The questions go through
//! [`SetupUi`], so the flow is the same in the terminal (`setup_tui`) and in tests.

use crate::Env;
use crate::setup_core::{
    self, CustomEndpoint, Fetch, HINDSIGHT_CONTAINER, HINDSIGHT_IMAGE, HINDSIGHT_LLM_PROVIDERS, HindsightDockerOptions,
    ProbeCommand, UrlSource,
};
use ai::Model;
use config::settings::SettingsManager;
use interactive::selectors::AuthProvider;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
    Dim,
}

/// The user quit the whole wizard (Ctrl+C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupQuit;

/// One choice of a question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub description: Option<String>,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            description: None,
        }
    }

    pub fn described(label: impl Into<String>, description: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            description: Some(description.into()),
        }
    }
}

pub type Validate = Box<dyn Fn(&str) -> Option<String>>;

#[derive(Default)]
pub struct InputOptions {
    /// Read masked: the value is never drawn.
    pub secret: bool,
    pub initial: Option<String>,
    pub description: Option<String>,
    /// An error message for an unacceptable value; the question is asked again.
    pub validate: Option<Validate>,
}

/// How a login ended (`LoginOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    Cancelled,
    Error(String),
}

/// What the wizard asks through (`SetupUi`). `Err(SetupQuit)` is Ctrl+C; `Ok(None)` is Esc (skip or go back).
pub trait SetupUi {
    /// Start a step.
    fn heading(&mut self, title: &str);
    fn note(&mut self, text: &str, tone: Tone);
    /// The index of the picked choice.
    fn select(
        &mut self,
        title: &str,
        choices: &[Choice],
        description: Option<&str>,
    ) -> Result<Option<usize>, SetupQuit>;
    fn input(&mut self, title: &str, options: InputOptions) -> Result<Option<String>, SetupQuit>;
    /// Pi's `/login` provider picker for one kind of login; the index of the picked provider.
    fn pick_login_provider(&mut self, providers: &[AuthProvider], title: &str) -> Result<Option<usize>, SetupQuit>;
    /// Pi's login dialog (browser/device flows, masked API-key prompt); saves to the profile's auth.json.
    fn login(&mut self, env: &Env, option: &AuthProvider) -> Result<LoginOutcome, SetupQuit>;
    /// Show `message` with a spinner while `task` runs.
    fn busy<T: Send + 'static>(
        &mut self,
        message: &str,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, SetupQuit>;
}

fn pick<U: SetupUi, T: Clone>(
    ui: &mut U,
    title: &str,
    choices: &[(Choice, T)],
    description: Option<&str>,
) -> Result<Option<T>, SetupQuit> {
    let labels: Vec<Choice> = choices.iter().map(|(c, _)| c.clone()).collect();
    Ok(ui
        .select(title, &labels, description)?
        .and_then(|i| choices.get(i).map(|(_, v)| v.clone())))
}

fn confirm<U: SetupUi>(ui: &mut U, title: &str, yes: &str, no: &str) -> Result<bool, SetupQuit> {
    Ok(pick(ui, title, &[(Choice::new(yes), true), (Choice::new(no), false)], None)? == Some(true))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelTestResult {
    pub ok: bool,
    pub reply: Option<String>,
    pub error: Option<String>,
    pub ms: u64,
}

pub type TestModel = Arc<dyn Fn(&Env, &Model) -> ModelTestResult + Send + Sync>;
pub type RunDocker = Arc<dyn Fn(&[String], &[(String, String)]) -> (bool, String) + Send + Sync>;

pub struct SetupDeps {
    /// The profile: its agent dir holds settings.json, auth.json and models.json; `var` is the environment.
    pub env: Env,
    pub probe: ProbeCommand,
    /// The interpreter the REPL runs (probed with `--version`).
    pub python: String,
    pub fetch: Fetch,
    /// Global settings: default model and thinking level, the Hindsight URL, Loki.
    pub settings: SettingsManager,
    /// One tiny live request.
    pub test_model: TestModel,
    /// Run docker with extra environment (for the API key): whether it exited 0, and its output.
    pub run_docker: RunDocker,
    /// How long to wait for a new Hindsight container to answer, and how often to ask.
    pub hindsight_wait: Duration,
    pub hindsight_interval: Duration,
    /// The Loki engine version bundled with this installation, when it has one.
    pub loki_version: Option<String>,
}

impl SetupDeps {
    fn agent_dir(&self) -> PathBuf {
        self.env.agent_dir()
    }

    fn var(&self, name: &str) -> Option<String> {
        (self.env.var)(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLine {
    pub step: String,
    /// `configured`, `unchanged`, `skipped` or `failed`.
    pub status: &'static str,
    pub detail: String,
}

fn line(step: &str, status: &'static str, detail: impl Into<String>) -> SummaryLine {
    SummaryLine {
        step: step.into(),
        status,
        detail: detail.into(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupResult {
    pub completed: bool,
    pub lines: Vec<SummaryLine>,
    /// A default model is saved and its live test passed (or it was kept as it was).
    pub model_ready: bool,
}

/// The profile's models as the session worker sees them (`createProfileRuntime`), read fresh from disk.
#[derive(Clone)]
pub struct ProfileRuntime {
    env: Env,
    pub available: Vec<Model>,
    pub error: Option<String>,
}

impl ProfileRuntime {
    pub fn load(env: &Env) -> ProfileRuntime {
        let registry = session_core::lane::registry(env);
        ProfileRuntime {
            env: env.clone(),
            available: session_core::lane::available_models(env),
            error: registry.error(),
        }
    }

    /// `getModel`: a catalog or models.json model, ready for requests when it is available.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        if let Some(model) = self.available.iter().find(|m| m.provider == provider && m.id == id) {
            return Some(model.clone());
        }
        session_core::lane::registry(&self.env).find(provider, id)
    }

    /// `hasConfiguredAuth`.
    pub fn has_configured_auth(&self, provider: &str) -> bool {
        self.available.iter().any(|m| m.provider == provider)
    }

    /// `getAvailable(provider)`.
    pub fn available_of(&self, provider: &str) -> Vec<Model> {
        self.available
            .iter()
            .filter(|m| m.provider == provider)
            .cloned()
            .collect()
    }
}

const STEPS: usize = 5;

fn step_title(index: usize, title: &str) -> String {
    format!("Step {index}/{STEPS}: {title}")
}

fn describe(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn text(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(str::to_string))
}

/// Run the wizard; `SetupQuit` ends it early with what was done so far.
pub fn run_setup_wizard<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> SetupResult {
    let mut lines = Vec::new();
    let mut model_ready = false;
    let mut completed = false;
    let steps = (|| -> Result<(), SetupQuit> {
        environment_step(ui, deps);
        let (model_line, ready) = provider_step(ui, deps)?;
        lines.push(model_line);
        model_ready = ready;
        lines.push(hindsight_step(ui, deps)?);
        lines.push(loki_step(ui, deps)?);
        completed = true;
        Ok(())
    })();
    if steps.is_err() {
        ui.note(
            "Setup stopped. Anything saved before this point is kept.",
            Tone::Warning,
        );
    }
    summary_step(ui, deps, &lines, completed);
    SetupResult {
        completed,
        lines,
        model_ready,
    }
}

// ------------------------------------------------------------------------------------------------- 1. environment

fn environment_step<U: SetupUi>(ui: &mut U, deps: &SetupDeps) {
    ui.heading(&step_title(1, "Environment"));
    for check in setup_core::check_environment(&deps.probe, &deps.python) {
        let mark = if check.ok {
            "✓"
        } else if check.optional {
            "–"
        } else {
            "✗"
        };
        let tone = if check.ok {
            Tone::Success
        } else if check.optional {
            Tone::Dim
        } else {
            Tone::Error
        };
        ui.note(&format!("{mark} {}: {}", check.label, check.detail), tone);
        if let (false, Some(fix)) = (check.ok, &check.fix) {
            ui.note(&format!("  {fix}"), Tone::Warning);
        }
    }
}

// -------------------------------------------------------------------------------------------- 2. provider and model

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderPath {
    OAuth,
    ApiKey,
    Custom,
    Keep,
    Skip,
}

const PROVIDER_STEP: &str = "Provider and model";

fn load_runtime<U: SetupUi>(ui: &mut U, deps: &SetupDeps, message: &str) -> Result<ProfileRuntime, SetupQuit> {
    let env = deps.env.clone();
    ui.busy(message, move || ProfileRuntime::load(&env))
}

fn provider_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<(SummaryLine, bool), SetupQuit> {
    ui.heading(&step_title(2, PROVIDER_STEP));
    let mut runtime = load_runtime(ui, deps, "Loading providers…")?;
    let current = current_default_model(&runtime, &deps.settings);
    if let Some(current) = &current {
        ui.note(&format!("Current default: {}", describe(current)), Tone::Info);
    } else if !runtime.available.is_empty() {
        ui.note(
            &format!(
                "{} models are available, but no default is saved.",
                runtime.available.len()
            ),
            Tone::Info,
        );
    } else {
        ui.note("No provider is configured yet.", Tone::Warning);
    }
    loop {
        let mut choices = Vec::new();
        if let Some(current) = &current {
            choices.push((Choice::new(format!("Keep {}", describe(current))), ProviderPath::Keep));
        }
        choices.extend([
            (
                Choice::described(
                    "Sign in with a subscription",
                    "Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot, … (Pi's /login)",
                ),
                ProviderPath::OAuth,
            ),
            (
                Choice::described("Use an API key", "Anthropic, OpenAI, Google, OpenRouter, …"),
                ProviderPath::ApiKey,
            ),
            (
                Choice::described(
                    "Custom OpenAI-compatible endpoint",
                    "A proxy, Ollama, LM Studio, vLLM, … (written to models.json)",
                ),
                ProviderPath::Custom,
            ),
            (Choice::new("Skip this step"), ProviderPath::Skip),
        ]);
        let path = pick(ui, "How should Ultron reach a model?", &choices, None)?;
        let path = match path {
            None | Some(ProviderPath::Skip) => {
                let detail = current
                    .as_ref()
                    .map_or_else(|| "no model".to_string(), |c| format!("kept {}", describe(c)));
                return Ok((line(PROVIDER_STEP, "skipped", detail), false));
            }
            Some(path) => path,
        };
        if path == ProviderPath::Keep {
            let current = current.clone().expect("Keep is offered with a current default");
            let tested = test_and_report(ui, deps, &current)?;
            let detail = format!(
                "{}{}",
                describe(&current),
                if tested { "" } else { " (live test failed)" }
            );
            return Ok((line(PROVIDER_STEP, "unchanged", detail), tested));
        }
        let provider = if path == ProviderPath::Custom {
            custom_endpoint(ui, deps)?
        } else {
            login_provider(ui, deps, path == ProviderPath::OAuth)?
        };
        let Some(provider) = provider else {
            continue;
        };
        runtime = load_runtime(ui, deps, "Reloading providers…")?;
        if let Some(outcome) = choose_default_model(ui, deps, &runtime, &provider)? {
            return Ok(outcome);
        }
    }
}

fn current_default_model(runtime: &ProfileRuntime, settings: &SettingsManager) -> Option<Model> {
    let provider = text(settings.default_provider())?;
    let id = text(settings.default_model())?;
    let model = runtime.get_model(&provider, &id)?;
    runtime.has_configured_auth(&model.provider).then_some(model)
}

/// `loginProviderOptions(runtime, authType)`.
pub fn login_provider_options(env: &Env, oauth: bool) -> Vec<AuthProvider> {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let kind = if oauth { "oauth" } else { "api_key" };
    crate::interactive::login_options(env, &auth)
        .into_iter()
        .filter(|p| p.auth_type == kind)
        .collect()
}

/// Pi's `/login` for one provider; its id once credentials are saved.
fn login_provider<U: SetupUi>(ui: &mut U, deps: &SetupDeps, oauth: bool) -> Result<Option<String>, SetupQuit> {
    let options = login_provider_options(&deps.env, oauth);
    if options.is_empty() {
        ui.note(
            if oauth {
                "No subscription providers are available."
            } else {
                "No API-key providers are available."
            },
            Tone::Warning,
        );
        return Ok(None);
    }
    let title = if oauth {
        "Sign in with a subscription"
    } else {
        "Choose the provider for your API key"
    };
    loop {
        let Some(index) = ui.pick_login_provider(&options, title)? else {
            return Ok(None);
        };
        let option = options[index].clone();
        if let Some((kind, source)) = &option.status {
            let keep = confirm(
                ui,
                &format!(
                    "{} is already configured ({}).",
                    option.name,
                    source.as_deref().unwrap_or(kind)
                ),
                "Use it as it is",
                "Sign in again and replace it",
            )?;
            if keep {
                return Ok(Some(option.id));
            }
        }
        if option.auth_type == "api_key" && !option.api_key_method.as_ref().is_some_and(|(_, login)| *login) {
            let name = option
                .api_key_method
                .as_ref()
                .map_or(option.name.as_str(), |(name, _)| name.as_str());
            ui.note(
                &format!("{name} is configured outside Ultron (environment variables or cloud credentials)."),
                Tone::Warning,
            );
            continue;
        }
        match ui.login(&deps.env, &option)? {
            LoginOutcome::Ok => {
                let auth = deps.agent_dir().join("auth.json");
                let text = if option.auth_type == "oauth" {
                    format!("✓ Signed in to {}. Saved to {}.", option.name, auth.display())
                } else {
                    format!("✓ Saved the API key for {} to {}.", option.name, auth.display())
                };
                ui.note(&text, Tone::Success);
                return Ok(Some(option.id));
            }
            LoginOutcome::Cancelled => {
                ui.note(
                    &format!("Sign-in to {} cancelled; nothing was saved.", option.name),
                    Tone::Dim,
                );
            }
            LoginOutcome::Error(error) => ui.note(&format!("✗ {}: {error}", option.name), Tone::Error),
        }
    }
}

/// Ask for a custom OpenAI-compatible endpoint and write it to models.json; its provider id.
fn custom_endpoint<U: SetupUi>(ui: &mut U, deps: &SetupDeps) -> Result<Option<String>, SetupQuit> {
    let models_path = deps.agent_dir().join("models.json");
    ui.note(
        &format!(
            "The endpoint is saved in {} (readable only by you).",
            models_path.display()
        ),
        Tone::Dim,
    );
    let Some(base_url) = ui.input(
        "Base URL of the endpoint (including /v1)",
        InputOptions {
            initial: Some("http://localhost:11434/v1".into()),
            validate: Some(Box::new(setup_core::validate_base_url)),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let Some(api_key) = ui.input(
        "API key (leave empty if the endpoint needs none)",
        InputOptions {
            secret: true,
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let mut provider_id: Option<String> = None;
    let provider_id = loop {
        let Some(id) = ui.input(
            "A short name for this provider",
            InputOptions {
                initial: Some(provider_id.clone().unwrap_or_else(|| "custom".into())),
                validate: Some(Box::new(setup_core::validate_provider_id)),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        provider_id = Some(id.clone());
        if !setup_core::custom_provider_exists(&models_path, &id) {
            break id;
        }
        if confirm(
            ui,
            &format!("models.json already has a provider named \"{id}\"."),
            "Replace it",
            "Choose another name",
        )? {
            break id;
        }
    };
    let apis = [
        (
            Choice::described("OpenAI Chat Completions", "most proxies, Ollama, vLLM, LM Studio"),
            "openai-completions",
        ),
        (
            Choice::described("OpenAI Responses", "/v1/responses"),
            "openai-responses",
        ),
    ];
    let Some(api) = pick(ui, "Which API does it speak?", &apis, None)? else {
        return Ok(None);
    };
    let fetch = deps.fetch.clone();
    let (url, key) = (base_url.clone(), api_key.clone());
    let listed = ui.busy(&format!("Asking {base_url} for its models…"), move || {
        setup_core::list_endpoint_models(&url, Some(&key), &fetch)
    })?;
    match listed.as_ref().filter(|l| !l.is_empty()) {
        Some(listed) => {
            let shown = listed.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
            ui.note(
                &format!(
                    "The endpoint lists {} model{}: {shown}{}",
                    listed.len(),
                    if listed.len() == 1 { "" } else { "s" },
                    if listed.len() > 20 { ", …" } else { "" }
                ),
                Tone::Info,
            );
        }
        None => ui.note(
            "The endpoint did not list its models; enter the ids yourself.",
            Tone::Dim,
        ),
    }
    let model_ids = loop {
        let initial = match listed.as_deref() {
            Some([only]) => only.clone(),
            _ => String::new(),
        };
        let Some(answer) = ui.input(
            "Model ids to use (comma-separated)",
            InputOptions {
                initial: Some(initial),
                validate: Some(Box::new(|value: &str| {
                    setup_core::parse_model_ids(value)
                        .is_empty()
                        .then(|| "Enter at least one model id".to_string())
                })),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        let ids = setup_core::parse_model_ids(&answer);
        let unknown: Vec<String> = match &listed {
            Some(listed) => ids.iter().filter(|id| !listed.contains(id)).cloned().collect(),
            None => Vec::new(),
        };
        if unknown.is_empty() {
            break ids;
        }
        if confirm(
            ui,
            &format!("The endpoint does not list {}.", unknown.join(", ")),
            "Use them anyway",
            "Edit the list",
        )? {
            break ids;
        }
    };
    let existing = match setup_core::read_text_file(&models_path) {
        Ok(text) => text,
        Err(error) => {
            ui.note(
                &format!("✗ {error} ({} was not changed)", models_path.display()),
                Tone::Error,
            );
            return Ok(None);
        }
    };
    let endpoint = CustomEndpoint {
        provider_id: provider_id.clone(),
        base_url,
        api_key: Some(api_key).filter(|k| !crate::js::trim(k).is_empty()),
        api: api.to_string(),
        model_ids,
    };
    let (merged, replaced) = match setup_core::merge_custom_endpoint(existing.as_deref(), &endpoint) {
        Ok(merged) => merged,
        Err(error) => {
            ui.note(
                &format!("✗ {error} ({} was not changed)", models_path.display()),
                Tone::Error,
            );
            return Ok(None);
        }
    };
    if let Err(error) = setup_core::write_private_file(&models_path, &merged) {
        ui.note(
            &format!("✗ {error} ({} was not changed)", models_path.display()),
            Tone::Error,
        );
        return Ok(None);
    }
    ui.note(
        &format!(
            "✓ {} provider \"{provider_id}\" in {}.",
            if replaced { "Replaced" } else { "Added" },
            models_path.display()
        ),
        Tone::Success,
    );
    Ok(Some(provider_id))
}

/// `Ok(None)`: back to the choice of path.
fn choose_default_model<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    runtime: &ProfileRuntime,
    provider: &str,
) -> Result<Option<(SummaryLine, bool)>, SetupQuit> {
    if let Some(error) = &runtime.error {
        ui.note(error, Tone::Error);
    }
    let models = runtime.available_of(provider);
    if models.is_empty() {
        ui.note(
            &format!("No models of \"{provider}\" are available yet; check its credentials."),
            Tone::Error,
        );
        return Ok(None);
    }
    let name = interactive::selectors::provider_name(provider);
    let choices: Vec<(Choice, Model)> = models
        .iter()
        .map(|m| {
            let choice = if m.name != m.id {
                Choice::described(m.id.clone(), m.name.clone())
            } else {
                Choice::new(m.id.clone())
            };
            (choice, m.clone())
        })
        .collect();
    loop {
        let title = format!("Default model ({} from {name})", models.len());
        let Some(model) = pick(ui, &title, &choices, None)? else {
            return Ok(None);
        };
        let thinking = choose_thinking(ui, deps, &model)?;
        let mut tested = test_and_report(ui, deps, &model)?;
        if !tested {
            let next = pick(
                ui,
                "The live test failed.",
                &[
                    (Choice::new("Try another model"), "other"),
                    (Choice::new("Run the test again"), "retry"),
                    (Choice::new("Save it anyway"), "save"),
                ],
                None,
            )?;
            match next {
                None | Some("other") => continue,
                Some("retry") => {
                    tested = test_and_report(ui, deps, &model)?;
                    if !tested {
                        continue;
                    }
                }
                Some(_) => {}
            }
        }
        deps.settings
            .set_global("defaultProvider", Some(Value::String(model.provider.clone())));
        deps.settings
            .set_global("defaultModel", Some(Value::String(model.id.clone())));
        if let Some(thinking) = &thinking {
            deps.settings
                .set_global("defaultThinkingLevel", Some(Value::String(thinking.clone())));
        }
        ui.note(
            &format!(
                "✓ Default model saved: {}{}.",
                describe(&model),
                thinking.as_ref().map(|t| format!(", thinking {t}")).unwrap_or_default()
            ),
            Tone::Success,
        );
        let detail = format!(
            "{}{}",
            describe(&model),
            if tested {
                ", live test passed"
            } else {
                ", live test failed"
            }
        );
        return Ok(Some((line(PROVIDER_STEP, "configured", detail), tested)));
    }
}

fn choose_thinking<U: SetupUi>(ui: &mut U, deps: &SetupDeps, model: &Model) -> Result<Option<String>, SetupQuit> {
    let levels = ai::models::supported_thinking_levels(model);
    if levels.len() <= 1 {
        return Ok(None);
    }
    let current = deps
        .settings
        .settings()
        .get("defaultThinkingLevel")
        .and_then(Value::as_str)
        .unwrap_or(ai::model_resolver::DEFAULT_THINKING_LEVEL)
        .to_string();
    let mut choices = vec![(Choice::new(format!("Keep {current}")), None)];
    choices.extend(
        levels
            .iter()
            .filter(|l| **l != current)
            .map(|l| (Choice::new(*l), Some(l.to_string()))),
    );
    Ok(pick(ui, "Thinking level", &choices, None)?.flatten())
}

fn test_and_report<U: SetupUi>(ui: &mut U, deps: &SetupDeps, model: &Model) -> Result<bool, SetupQuit> {
    let (test, env, target) = (deps.test_model.clone(), deps.env.clone(), model.clone());
    let result = ui.busy(
        &format!("Testing {} with one tiny request…", describe(model)),
        move || test(&env, &target),
    )?;
    let seconds = crate::js::to_fixed(result.ms as f64 / 1000.0, 1);
    if result.ok {
        let reply = result
            .reply
            .as_deref()
            .map(|r| {
                let collapsed = r
                    .split(crate::js::is_space)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>();
                let joined = collapsed.join(" ");
                crate::js::head16(crate::js::trim(&joined), 60).to_string()
            })
            .unwrap_or_default();
        let shown = if reply.is_empty() {
            String::new()
        } else {
            format!(": \"{reply}\"")
        };
        ui.note(&format!("✓ Live test passed in {seconds}s{shown}"), Tone::Success);
    } else {
        ui.note(
            &format!(
                "✗ Live test failed after {seconds}s: {}",
                result.error.as_deref().unwrap_or("unknown error")
            ),
            Tone::Error,
        );
    }
    Ok(result.ok)
}

// ------------------------------------------------------------------------------------------------------ 3. Hindsight

#[derive(Clone, Copy, PartialEq, Eq)]
enum HindsightAction {
    Docker,
    Start,
    Url,
    Manual,
    Off,
    Skip,
}

const HINDSIGHT: &str = "Hindsight";

fn probe<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    message: String,
    url: &str,
) -> Result<Result<(), String>, SetupQuit> {
    let (fetch, url) = (deps.fetch.clone(), url.to_string());
    ui.busy(&message, move || setup_core::probe_hindsight(&url, &fetch))
}

fn hindsight_setting(settings: &SettingsManager) -> Option<String> {
    settings
        .global_settings()
        .get("hindsightUrl")
        .and_then(Value::as_str)
        .map(crate::js::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn hindsight_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(3, "Hindsight memory server"));
    ui.note(
        "Hindsight stores what the model keeps on purpose (memory.propose) and returns it when the model asks (memory.prepare).",
        Tone::Info,
    );
    let env_url = deps.var("ULTRON_HINDSIGHT_URL");
    let (effective, source) =
        setup_core::effective_hindsight_url(env_url.as_deref(), hindsight_setting(&deps.settings).as_deref());
    if source == UrlSource::Env {
        ui.note(
            &format!(
                "ULTRON_HINDSIGHT_URL is set ({}); it overrides anything saved here.",
                env_url.unwrap_or_default()
            ),
            Tone::Warning,
        );
    }
    let Some(url) = effective else {
        ui.note("Memory is turned off.", Tone::Info);
        if !confirm(ui, "Turn memory on?", "Yes, set a Hindsight URL", "No, leave it off")? {
            return Ok(line(HINDSIGHT, "unchanged", "off"));
        }
        return Ok(custom_hindsight_url(ui, deps)?.unwrap_or_else(|| line(HINDSIGHT, "unchanged", "off")));
    };
    loop {
        let probed = probe(ui, deps, format!("Looking for Hindsight at {url}…"), &url)?;
        let reason = match probed {
            Ok(()) => {
                ui.note(&format!("✓ Hindsight is running at {url}."), Tone::Success);
                let next = pick(
                    ui,
                    "Hindsight",
                    &[
                        (Choice::new("Use it"), false),
                        (Choice::new("Use a different URL"), true),
                    ],
                    None,
                )?;
                if next == Some(true) {
                    if let Some(saved) = custom_hindsight_url(ui, deps)? {
                        return Ok(saved);
                    }
                    continue;
                }
                return Ok(line(HINDSIGHT, "unchanged", format!("running at {url}")));
            }
            Err(reason) => reason,
        };
        ui.note(
            &format!("Hindsight is not reachable at {url} ({reason})."),
            Tone::Warning,
        );
        let docker_available = (deps.probe)("docker", &["--version"]).is_some();
        let filter = format!("name=^{HINDSIGHT_CONTAINER}$");
        let existing = if docker_available {
            (deps.probe)("docker", &["ps", "-a", "--filter", &filter, "--format", "{{.Status}}"])
        } else {
            None
        }
        .filter(|s| !s.is_empty());
        let local = setup_core::is_local_url(&url);
        let mut choices = Vec::new();
        if docker_available && local {
            match &existing {
                Some(status) => choices.push((
                    Choice::described(
                        format!("Start the existing \"{HINDSIGHT_CONTAINER}\" container"),
                        status.clone(),
                    ),
                    HindsightAction::Start,
                )),
                None => choices.push((
                    Choice::described("Install it with Docker", HINDSIGHT_IMAGE),
                    HindsightAction::Docker,
                )),
            }
        }
        choices.extend([
            (Choice::new("It runs elsewhere: set its URL"), HindsightAction::Url),
            (Choice::new("Show manual install instructions"), HindsightAction::Manual),
            (
                Choice::described("Turn memory off", "saves hindsightUrl: off"),
                HindsightAction::Off,
            ),
            (Choice::new("Skip"), HindsightAction::Skip),
        ]);
        let action = match pick(ui, "Set up Hindsight", &choices, None)? {
            None | Some(HindsightAction::Skip) => {
                return Ok(line(HINDSIGHT, "skipped", format!("not reachable at {url}")));
            }
            Some(action) => action,
        };
        match action {
            HindsightAction::Manual => {
                if !docker_available {
                    ui.note("Docker was not found on this machine.", Tone::Dim);
                }
                for instruction in setup_core::hindsight_manual_instructions(&url) {
                    ui.note(&instruction, Tone::Dim);
                }
                if confirm(
                    ui,
                    "Check again once it is running?",
                    "Check again",
                    "Continue without it",
                )? {
                    continue;
                }
                return Ok(line(HINDSIGHT, "skipped", "manual install instructions shown"));
            }
            HindsightAction::Off => {
                deps.settings
                    .set_global("hindsightUrl", Some(Value::String("off".into())));
                ui.note(
                    "Memory is turned off. Run `ultron setup` again to turn it on.",
                    Tone::Info,
                );
                return Ok(line(HINDSIGHT, "configured", "off"));
            }
            HindsightAction::Url => {
                if let Some(saved) = custom_hindsight_url(ui, deps)? {
                    return Ok(saved);
                }
                continue;
            }
            _ => {}
        }
        let started = if action == HindsightAction::Start {
            start_existing_container(ui, deps)?
        } else {
            install_with_docker(ui, deps, &url)?
        };
        let Some(started) = started else {
            continue;
        };
        if !started {
            return Ok(line(HINDSIGHT, "failed", "the container did not start"));
        }
        let (fetch, wait, interval, target) = (
            deps.fetch.clone(),
            deps.hindsight_wait,
            deps.hindsight_interval,
            url.clone(),
        );
        let minutes = (deps.hindsight_wait.as_millis() as f64 / 60_000.0).round();
        let up = ui.busy(
            &format!("Waiting for Hindsight at {url} (the first start downloads models; up to {minutes} min)…"),
            move || setup_core::wait_for_hindsight(&target, wait, interval, &fetch),
        )?;
        if up {
            ui.note(&format!("✓ Hindsight is running at {url}."), Tone::Success);
            return Ok(line(
                HINDSIGHT,
                "configured",
                format!("Docker container \"{HINDSIGHT_CONTAINER}\" at {url}"),
            ));
        }
        ui.note(
            &format!(
                "Hindsight did not answer within the wait. It may still be starting: `docker logs -f {HINDSIGHT_CONTAINER}`."
            ),
            Tone::Warning,
        );
        return Ok(line(
            HINDSIGHT,
            "failed",
            format!("container \"{HINDSIGHT_CONTAINER}\" started, not answering yet"),
        ));
    }
}

fn custom_hindsight_url<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<Option<SummaryLine>, SetupQuit> {
    let initial = hindsight_setting(&deps.settings).unwrap_or_else(|| "http://localhost:8888".into());
    let Some(url) = ui.input(
        "Hindsight URL",
        InputOptions {
            initial: Some(initial),
            validate: Some(Box::new(setup_core::validate_base_url)),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let trimmed = crate::js::trim(&url).trim_end_matches('/').to_string();
    let probed = probe(ui, deps, format!("Checking {trimmed}…"), &trimmed)?;
    match &probed {
        Err(reason) => {
            ui.note(
                &format!("Hindsight is not reachable at {trimmed} ({reason})."),
                Tone::Warning,
            );
            if !confirm(ui, "Save this URL anyway?", "Save it", "Back")? {
                return Ok(None);
            }
        }
        Ok(()) => ui.note(&format!("✓ Hindsight is running at {trimmed}."), Tone::Success),
    }
    deps.settings
        .set_global("hindsightUrl", Some(Value::String(trimmed.clone())));
    ui.note(
        &format!(
            "✓ Saved hindsightUrl in {}.",
            deps.agent_dir().join("settings.json").display()
        ),
        Tone::Success,
    );
    let detail = format!("{trimmed}{}", if probed.is_ok() { "" } else { " (not reachable yet)" });
    Ok(Some(line(HINDSIGHT, "configured", detail)))
}

/// The last three lines of docker's output, joined.
fn last_lines(output: &str) -> String {
    let lines: Vec<&str> = output.trim().split('\n').collect();
    lines[lines.len().saturating_sub(3)..].join(" ")
}

fn start_existing_container<U: SetupUi>(ui: &mut U, deps: &SetupDeps) -> Result<Option<bool>, SetupQuit> {
    let docker = deps.run_docker.clone();
    let (ok, output) = ui.busy(&format!("docker start {HINDSIGHT_CONTAINER}"), move || {
        docker(&["start".to_string(), HINDSIGHT_CONTAINER.to_string()], &[])
    })?;
    if !ok {
        ui.note(&format!("✗ docker start failed: {}", last_lines(&output)), Tone::Error);
    }
    Ok(Some(ok))
}

/// Ask for Hindsight's own LLM, confirm the exact command, then `docker run` it. `Ok(None)`: backed out.
fn install_with_docker<U: SetupUi>(ui: &mut U, deps: &SetupDeps, url: &str) -> Result<Option<bool>, SetupQuit> {
    ui.note(
        "Hindsight needs its own LLM to extract and summarise memories. Its key is passed to the container only.",
        Tone::Info,
    );
    let choices: Vec<(Choice, usize)> = HINDSIGHT_LLM_PROVIDERS
        .iter()
        .enumerate()
        .map(|(i, p)| (Choice::described(p.label, p.provider), i))
        .collect();
    let Some(index) = pick(ui, "LLM provider for Hindsight", &choices, None)? else {
        return Ok(None);
    };
    let choice = &HINDSIGHT_LLM_PROVIDERS[index];
    let mut base_url = None;
    if choice.needs_base_url {
        let Some(answer) = ui.input(
            "LLM base URL, as seen from inside the container",
            InputOptions {
                initial: Some(choice.default_base_url.unwrap_or("").to_string()),
                description: Some("Use host.docker.internal for a server on this machine.".into()),
                validate: Some(Box::new(setup_core::validate_base_url)),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        base_url = Some(answer);
    }
    let mut api_key = None;
    if choice.needs_key {
        let Some(answer) = ui.input(
            &format!("API key for {}", choice.label),
            InputOptions {
                secret: true,
                validate: Some(Box::new(|value: &str| {
                    crate::js::trim(value)
                        .is_empty()
                        .then(|| "Paste the key, or press Esc to go back".to_string())
                })),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        api_key = Some(answer);
    }
    let Some(model) = ui.input(
        "Model for Hindsight (empty: Hindsight's default for the provider)",
        InputOptions {
            initial: Some(String::new()),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let args = setup_core::hindsight_docker_args(&HindsightDockerOptions {
        port: setup_core::hindsight_port(url),
        provider: choice.provider.into(),
        model: Some(model),
        base_url: base_url.filter(|b| !b.is_empty()),
        has_api_key: api_key.is_some(),
    });
    ui.note(
        "This will run (the key is passed through the environment, not the command line):",
        Tone::Info,
    );
    ui.note(&format!("  {}", setup_core::format_command("docker", &args)), Tone::Dim);
    ui.note(
        "The image is large (several GB for the full image); the first pull takes a while.",
        Tone::Dim,
    );
    if !confirm(ui, "Run it now?", "Run docker", "Back")? {
        return Ok(None);
    }
    let docker = deps.run_docker.clone();
    let env: Vec<(String, String)> = api_key
        .map(|k| vec![("HINDSIGHT_API_LLM_API_KEY".to_string(), crate::js::trim(&k).to_string())])
        .unwrap_or_default();
    let (ok, output) = ui.busy("Pulling and starting Hindsight (docker run)…", move || {
        docker(&args, &env)
    })?;
    if !ok {
        ui.note(&format!("✗ docker run failed: {}", last_lines(&output)), Tone::Error);
        return Ok(Some(false));
    }
    ui.note(
        &format!("✓ Started container \"{HINDSIGHT_CONTAINER}\"."),
        Tone::Success,
    );
    Ok(Some(true))
}

// ----------------------------------------------------------------------------------------------------------- 4. Loki

#[derive(Clone, Copy, PartialEq, Eq)]
enum LokiAction {
    Keep,
    AutoInit,
    AutoCommit,
    Advise,
    Enforce,
    Off,
    On,
}

/// `describeLoki`.
pub fn describe_loki(settings: &Map<String, Value>) -> String {
    let mode = settings.get("mode").and_then(Value::as_str).unwrap_or("on");
    if mode == "off" {
        return "off".into();
    }
    let off = |key: &str| settings.get(key) == Some(&Value::Bool(false));
    [
        if mode == "advise" {
            "advise-only".to_string()
        } else {
            "blocking".to_string()
        },
        format!("auto-install {}", if off("autoInit") { "off" } else { "on" }),
        format!("auto-commit {}", if off("autoCommit") { "off" } else { "on" }),
    ]
    .join(", ")
}

fn loki_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(4, "Loki guardrails"));
    ui.note(
        "Loki checks every file the agent writes with deterministic rules and real analyzers: edit() and write() are checked before the file changes, other writes after the cell.",
        Tone::Info,
    );
    match &deps.loki_version {
        None => ui.note(
            "✗ This installation has no bundled Loki engine; guardrails run only where a repository has its own .loki/.",
            Tone::Warning,
        ),
        Some(version) => ui.note(&format!("✓ Bundled Loki {version}"), Tone::Success),
    }
    for name in ["ULTRON_LOKI", "ULTRON_LOKI_AUTOINIT", "ULTRON_LOKI_AUTOCOMMIT"] {
        if let Some(value) = deps.var(name).filter(|v| !crate::js::trim(v).is_empty()) {
            ui.note(
                &format!("{name}={value} is set; it overrides the settings saved here."),
                Tone::Warning,
            );
        }
    }
    let analyzers = setup_core::check_loki_analyzers(&deps.probe);
    for analyzer in &analyzers {
        let text = if analyzer.found {
            format!("✓ {} ({})", analyzer.name, analyzer.language)
        } else {
            format!(
                "– {} ({}): not found. {}",
                analyzer.name, analyzer.language, analyzer.hint
            )
        };
        ui.note(&text, if analyzer.found { Tone::Success } else { Tone::Dim });
    }
    if analyzers.iter().any(|a| !a.found) {
        ui.note(
            "Missing analyzers are reported as NOT CHECKED, never as clean.",
            Tone::Dim,
        );
    }
    let initial = deps.settings.loki_settings();
    let mut current = initial.clone();
    loop {
        ui.note(&format!("Loki: {}.", describe_loki(&current)), Tone::Info);
        let mode = current.get("mode").and_then(Value::as_str).unwrap_or("on").to_string();
        let off = |key: &str| current.get(key) == Some(&Value::Bool(false));
        let choices: Vec<(Choice, LokiAction)> = if mode == "off" {
            vec![
                (Choice::new("Keep Loki off"), LokiAction::Keep),
                (Choice::new("Turn Loki on"), LokiAction::On),
            ]
        } else {
            vec![
                (Choice::new("Keep these settings"), LokiAction::Keep),
                (
                    Choice::described(
                        format!("Turn auto-install {}", if off("autoInit") { "on" } else { "off" }),
                        "Create .loki/ (engine and default policy) in a Git repository that lacks it",
                    ),
                    LokiAction::AutoInit,
                ),
                (
                    Choice::described(
                        format!("Turn auto-commit {}", if off("autoCommit") { "on" } else { "off" }),
                        "Commit the .loki/ Ultron created, and only it; on a fork the commit rides along in pull requests",
                    ),
                    LokiAction::AutoCommit,
                ),
                if mode == "advise" {
                    (Choice::new("Block unsafe writes again"), LokiAction::Enforce)
                } else {
                    (Choice::new("Advise only (report, never block)"), LokiAction::Advise)
                },
                (Choice::new("Turn Loki off"), LokiAction::Off),
            ]
        };
        let action = match pick(ui, "Loki guardrails", &choices, None)? {
            None | Some(LokiAction::Keep) => break,
            Some(action) => action,
        };
        match action {
            LokiAction::AutoInit => {
                let value = off("autoInit");
                current.insert("autoInit".into(), Value::Bool(value));
            }
            LokiAction::AutoCommit => {
                let value = off("autoCommit");
                current.insert("autoCommit".into(), Value::Bool(value));
            }
            LokiAction::Advise => {
                current.insert("mode".into(), "advise".into());
            }
            LokiAction::Off => {
                current.insert("mode".into(), "off".into());
            }
            _ => {
                current.insert("mode".into(), "on".into());
            }
        }
    }
    if describe_loki(&current) == describe_loki(&initial) {
        return Ok(line("Loki", "unchanged", describe_loki(&current)));
    }
    let patch: Vec<(&str, Option<Value>)> = current.iter().map(|(k, v)| (k.as_str(), Some(v.clone()))).collect();
    deps.settings.set_loki_settings(&patch);
    Ok(line("Loki", "configured", describe_loki(&current)))
}

// -------------------------------------------------------------------------------------------------------- 5. summary

fn summary_step<U: SetupUi>(ui: &mut U, deps: &SetupDeps, lines: &[SummaryLine], completed: bool) {
    ui.heading(&step_title(5, "Summary"));
    for l in lines {
        let tone = match l.status {
            "configured" => Tone::Success,
            "failed" => Tone::Error,
            _ => Tone::Dim,
        };
        ui.note(&format!("{}: {} — {}", l.step, l.status, l.detail), tone);
    }
    if !completed {
        ui.note("Not all steps ran.", Tone::Warning);
    }
    ui.note(
        &format!(
            "Configuration lives in {} (settings.json, auth.json, models.json).",
            deps.agent_dir().display()
        ),
        Tone::Info,
    );
    ui.note(
        "Run `ultron setup` again at any time to change it; start working with `ultron`.",
        Tone::Info,
    );
}

// ------------------------------------------------------------------------------------------------- the live test

/// `MODEL_TEST_TIMEOUT_MS`.
const MODEL_TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `testModel`: one short request, as small as the provider allows.
pub fn test_model(env: &Env, model: &Model) -> ModelTestResult {
    use agent::StreamFn;
    let started = std::time::Instant::now();
    let elapsed = |started: std::time::Instant| started.elapsed().as_millis() as u64;
    let key = session_core::lane::api_key(&session_core::ModelFlags::default(), env, &model.provider);
    let provider = session_core::lane::Provider::with_key(key.clone());
    let context = ai::TranscriptContext {
        messages: vec![ai::Message::User(ai::UserMessage {
            content: ai::UserMessageContent::Text("Reply with the single word: ok".into()),
            timestamp: session_core::lane::now_ms(),
        })],
    };
    let abort = ai::AbortSignal::new();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = key;
    options.base.signal = Some(abort.clone());
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            return ModelTestResult {
                ok: false,
                reply: None,
                error: Some(error.to_string()),
                ms: elapsed(started),
            };
        }
    };
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = runtime.block_on(async {
        let stream = provider.stream(model, &context, &options, &mut sink);
        match tokio::time::timeout(MODEL_TEST_TIMEOUT, stream).await {
            Ok(message) => Ok(message),
            Err(_) => {
                abort.abort();
                Err("The operation was aborted due to timeout".to_string())
            }
        }
    });
    let ms = elapsed(started);
    match message {
        Err(error) => ModelTestResult {
            ok: false,
            reply: None,
            error: Some(error),
            ms,
        },
        Ok(message) => {
            let stop = serde_json::to_value(message.stop_reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            if stop == "error" || stop == "aborted" {
                return ModelTestResult {
                    ok: false,
                    reply: None,
                    error: Some(message.error_message.unwrap_or_else(|| format!("request {stop}"))),
                    ms,
                };
            }
            let reply = message
                .content
                .iter()
                .filter_map(|c| match c {
                    ai::AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            ModelTestResult {
                ok: true,
                reply: Some(reply),
                error: None,
                ms,
            }
        }
    }
}

/// `runDocker`: docker with the run's PATH and `extra` environment; exit status and stdout+stderr.
pub fn run_docker(path: Option<String>) -> RunDocker {
    Arc::new(move |args: &[String], extra: &[(String, String)]| {
        let mut command = std::process::Command::new("docker");
        command.args(args).stdin(std::process::Stdio::null());
        if let Some(path) = &path {
            command.env("PATH", path);
        }
        for (k, v) in extra {
            command.env(k, v);
        }
        match command.output() {
            Ok(out) => (
                out.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            ),
            Err(error) => (false, error.to_string()),
        }
    })
}

/// `createSetupDeps`: the real probes, fetch, live test and docker for `env`'s profile.
pub fn create_setup_deps(env: &Env) -> SetupDeps {
    let path = (env.var)("PATH");
    let probe_path = path.clone();
    SetupDeps {
        env: env.clone(),
        probe: Arc::new(move |command: &str, args: &[&str]| {
            setup_core::probe_command(probe_path.as_deref(), command, args)
        }),
        python: setup_core::python_command(&|k| (env.var)(k)),
        fetch: setup_core::http_fetch_with(ai::net::ProxyEnv::from_lookup(&*env.var)),
        settings: SettingsManager::create(&env.cwd, &env.agent_dir(), false),
        test_model: Arc::new(test_model),
        run_docker: run_docker(path),
        hindsight_wait: Duration::from_secs(5 * 60),
        hindsight_interval: Duration::from_secs(3),
        loki_version: setup_core::bundled_loki_version(),
    }
}
