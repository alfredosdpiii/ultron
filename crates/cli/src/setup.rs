//! `ultron setup` (`cli/setup/command.ts`): help, argument checks, the answer without a terminal, the guided wizard
//! on the terminal (`setup_wizard`, drawn by `setup_tui`), and the offer to run it when an interactive start finds
//! no usable model (`offerSetupOnFirstRun`, with `hasUsableModel` and `noModelHint`).

use crate::Env;
use crate::args::Args;
use crate::setup_wizard::{SetupDeps, SetupResult};
use serde_json::Value;
use std::io::IsTerminal;

/// Handle `setup ...`; None when `argv` is not the setup command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("setup") {
        return None;
    }
    let agent_dir = env.agent_dir();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        let _ = writeln!(
            out,
            "Usage:
  ultron setup

A guided setup: checks python3 (the REPL's interpreter), sets up a provider (subscription login, API key or a
custom OpenAI-compatible endpoint) and the default model with a live test, and finds or installs the Hindsight
memory server. Every step can be skipped; run it again at any time.

Files are written to {} (secrets with mode 0600).",
            agent_dir.display()
        );
        return Some(0);
    }
    if let Some(option) = argv.get(1) {
        let _ = writeln!(err, "Unknown option for \"ultron setup\": {option}");
        return Some(1);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let _ = writeln!(
            err,
            "ultron setup is interactive and needs a terminal.\nWithout one, configure {} directly: provider keys in environment variables (e.g. ANTHROPIC_API_KEY), custom endpoints in models.json, and ULTRON_HINDSIGHT_URL.",
            agent_dir.display()
        );
        return Some(1);
    }
    let mut deps = crate::setup_wizard::create_setup_deps(env);
    let result = match run_on_terminal(&mut deps) {
        Ok(result) => result,
        Err(error) => {
            let _ = writeln!(err, "Error: {error}");
            return Some(1);
        }
    };
    let _ = write!(
        out,
        "{}",
        if result.completed {
            "\nSetup finished. Start with: ultron\n"
        } else {
            "\nSetup stopped. Run \"ultron setup\" to continue.\n"
        }
    );
    Some(0)
}

/// `runSetupOnTerminal`: the wizard on the process terminal; the transcript stays in the scrollback.
pub fn run_on_terminal(deps: &mut SetupDeps) -> std::io::Result<SetupResult> {
    let mut ui = crate::setup_tui::TuiSetupUi::start(&deps.env)?;
    let result = crate::setup_wizard::run_setup_wizard(&mut ui, deps);
    ui.stop();
    Ok(result)
}

/// What a start would run on (`startModelState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartModelState {
    Usable,
    None,
    DefaultWithoutCredentials,
    UnknownDefault,
}

fn text(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(str::to_string))
}

/// `startModelState`: the saved default when its provider has credentials, else any available model. Claude
/// subscription models are listed whenever Claude Code is installed, but the worker never falls back to them, so
/// they only count under `ultron --claude` (`ULTRON_ROOT=claude`).
pub fn start_model_state(env: &Env) -> StartModelState {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let runtime = crate::setup_wizard::ProfileRuntime::load(env);
    let provider = text(settings.default_provider());
    let id = text(settings.default_model());
    let saved = match (&provider, &id) {
        (Some(p), Some(i)) => runtime.get_model(p, i),
        _ => None,
    };
    if saved.as_ref().is_some_and(|m| runtime.has_configured_auth(&m.provider)) {
        return StartModelState::Usable;
    }
    let claude_root = (env.var)("ULTRON_ROOT").is_some_and(|r| crate::js::trim(&r).to_lowercase() == "claude");
    if runtime
        .available
        .iter()
        .any(|m| claude_root || m.provider != ai::claude_subscription::PROVIDER)
    {
        return StartModelState::Usable;
    }
    if provider.is_none() || id.is_none() {
        return StartModelState::None;
    }
    if saved.is_some() {
        StartModelState::DefaultWithoutCredentials
    } else {
        StartModelState::UnknownDefault
    }
}

/// `hasUsableModel`: a saved default (taken on trust), or a model the worker would fall back to.
pub fn has_usable_model(env: &Env) -> bool {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    if settings.default_provider().is_some() && settings.default_model().is_some() {
        return true;
    }
    start_model_state(env) == StartModelState::Usable
}

/// `noModelHint`: the hint of a failed start that had no model to start with.
pub fn no_model_hint(env: &Env) -> Option<String> {
    match start_model_state(env) {
        StartModelState::None => Some(
            "No model is configured. Run \"ultron setup\", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY."
                .into(),
        ),
        StartModelState::DefaultWithoutCredentials => {
            let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
            Some(format!(
                "The default model {}/{} has no credentials, and no other model is set up. Run \"ultron setup\" to sign in again or choose another model.",
                text(settings.default_provider()).unwrap_or_default(),
                text(settings.default_model()).unwrap_or_default()
            ))
        }
        _ => None,
    }
}

/// `interactiveStartWithoutModel`: whether this command starts the TUI, and whether it names a model.
fn interactive_start(parsed: &Args) -> (bool, bool) {
    let interactive = parsed.mode.is_none()
        && !parsed.print
        && parsed.messages.is_empty()
        && parsed.file_args.is_empty()
        && !parsed.help
        && !parsed.version
        && parsed.list_models.is_none()
        && parsed.export.is_none();
    let explicit = parsed.model.is_some()
        || parsed.provider.is_some()
        || parsed.api_key.is_some()
        || parsed.extensions.as_ref().is_some_and(|e| !e.is_empty())
        || parsed.models.as_ref().is_some_and(|m| !m.is_empty());
    (interactive, explicit)
}

/// `offerSetupOnFirstRun`: on an interactive start with no usable model, offer `ultron setup` ("Don't ask again" is
/// saved). Returns false when the start should end here: the wizard was quit, or still left no model.
pub fn offer_setup_on_first_run(parsed: &Args, env: &Env, out: &mut dyn std::io::Write) -> bool {
    let mut settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let (interactive, explicit_model) = interactive_start(parsed);
    let dismissed = settings.global_settings().get("skipSetupPrompt") == Some(&Value::Bool(true));
    let offer = crate::setup_core::should_offer_setup(&crate::setup_core::SetupOfferInput {
        stdin_is_tty: std::io::stdin().is_terminal(),
        stdout_is_tty: std::io::stdout().is_terminal(),
        interactive,
        explicit_model,
        var: &|k| (env.var)(k),
        dismissed,
        has_usable_model: &|| has_usable_model(env),
    });
    if !offer {
        return true;
    }
    let choice = crate::setup_tui::show_startup_selector(
        env,
        "No model is set up for ultron yet. Run the guided setup now?",
        &["Run setup (recommended)", "Not now", "Don't ask again"],
    )
    .unwrap_or(None);
    match choice {
        Some(2) => {
            settings.set_global("skipSetupPrompt", Some(Value::Bool(true)));
            let _ = writeln!(out, "You can run \"ultron setup\" at any time.");
            true
        }
        Some(0) => {
            let mut deps = crate::setup_wizard::create_setup_deps(env);
            let completed = run_on_terminal(&mut deps).is_ok_and(|r| r.completed);
            if completed && has_usable_model(env) {
                return true;
            }
            let _ = write!(out, "\nNo model is set up yet. Run \"ultron setup\" to continue.\n");
            let _ = out.flush();
            false
        }
        _ => true,
    }
}
