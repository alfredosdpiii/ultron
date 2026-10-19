//! The interactive TUI's startup checks, run off the UI thread (interactive-mode.ts `init`):
//! - the update check (utils/version-check.ts `checkForNewVersion`): the npm registry's `latest` release, when it is
//!   newer than this build; off when `PI_OFFLINE` is set or `PI_SKIP_VERSION_CHECK` / `ULTRON_SKIP_VERSION_CHECK` is
//!   truthy, and quiet on any failure;
//! - `getChangelogForDisplay`: the CHANGELOG.md entries newer than the `lastChangelogVersion` setting (a first run,
//!   with no setting, records nothing: see [`startup_changelog`]);
//! - `maybeWarnAboutAnthropicSubscriptionAuth`'s credential lookup.

use interactive::lane::StartupNotice;

use crate::Env;

/// A Rust-only test hook: where the update check asks instead of the npm registry (a test's 127.0.0.1 fake).
pub const LATEST_VERSION_URL_VAR: &str = "ULTRON_LATEST_VERSION_URL";

/// `isTruthy`: set, not blank, and not `0`/`false`/`no` (any case).
fn truthy(value: Option<String>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim().to_lowercase();
        !v.is_empty() && !matches!(v.as_str(), "0" | "false" | "no")
    })
}

/// `areAutomaticVersionChecksDisabled`.
pub fn version_checks_disabled(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var("PI_OFFLINE").is_some_and(|v| !v.is_empty())
        || truthy(var("PI_SKIP_VERSION_CHECK"))
        || truthy(var("ULTRON_SKIP_VERSION_CHECK"))
}

/// The registry document the update check reads.
pub fn latest_version_url(env: &Env) -> String {
    (env.var)(LATEST_VERSION_URL_VAR)
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| crate::update_cli::LATEST_VERSION_URL.to_string())
}

/// `checkForNewVersion(current)`: the newer release's version, or None (disabled, failed, not newer). One attempt
/// within ten seconds (`getLatestRelease` without `retry`).
pub fn check_for_new_version(env: &Env, current: &str) -> Option<String> {
    if version_checks_disabled(&*env.var) {
        return None;
    }
    let latest = crate::update_cli::latest_release(env, &latest_version_url(env), 0).ok()??;
    crate::update_cli::is_newer(&latest, current).then_some(latest)
}

/// `getChangelogForDisplay` for a session without messages: reads the package's CHANGELOG.md and the
/// `lastChangelogVersion` setting, records this build's version when it should, and returns the notice to show.
pub fn startup_changelog(env: &Env) -> Option<StartupNotice> {
    let mut settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let content = env
        .docs
        .parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("CHANGELOG.md")).ok());
    let version = crate::version();
    let last = settings.last_changelog_version();
    let shown =
        interactive::startup_notices::changelog_for_display(content.as_deref(), last.as_deref(), &version, false);
    // interactive-mode.ts also records the version on a first run (no `lastChangelogVersion`); the shipped TUI
    // (client-tui.ts) never writes the setting, and its recorded settings.json has no such key, so a first run here
    // records nothing either. Once a version is recorded, newer entries show and the version moves on.
    if let Some(save) = shown.save.as_ref().filter(|_| last.is_some()) {
        settings.set_last_changelog_version(save);
    }
    let collapse = settings
        .settings()
        .get("collapseChangelog")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    shown.markdown.map(|markdown| StartupNotice::WhatsNew {
        markdown,
        collapse,
        version,
    })
}

/// `maybeWarnAboutAnthropicSubscriptionAuth` for `provider`: the `warnings.anthropicExtraUsage` setting, then the
/// stored credential's type (`checkAuth`) or, for a key, whether it is an OAuth token (`getAuth`).
pub fn subscription_auth_warns(env: &Env, provider: &str) -> bool {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let enabled = settings.anthropic_extra_usage_warning();
    if !enabled || provider != "anthropic" {
        return false;
    }
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let stored_type = config::auth::AuthStorage::create(auth)
        .list()
        .unwrap_or_default()
        .into_iter()
        .find(|(id, _)| id == provider)
        .and_then(|(_, credential)| credential["type"].as_str().map(str::to_string));
    // An OAuth credential is the subscription itself; reading its key could refresh it over the network.
    if stored_type.as_deref() == Some("oauth") {
        return true;
    }
    let key = session_core::lane::api_key(&Default::default(), env, provider);
    interactive::startup_notices::warns_about_subscription_auth(
        provider,
        enabled,
        stored_type.as_deref(),
        key.as_deref(),
    )
}
