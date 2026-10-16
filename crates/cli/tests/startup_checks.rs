//! The startup checks behind the TUI's notices, over a temp profile: `getChangelogForDisplay` reading the package's
//! CHANGELOG.md and recording `lastChangelogVersion` (the selection itself is replayed against TypeScript in
//! crates/interactive/tests/startup_notices.rs), and `maybeWarnAboutAnthropicSubscriptionAuth`'s credential lookup
//! (the decision is replayed there too).

use std::path::Path;
use std::sync::Arc;

use cli::Env;
use interactive::lane::StartupNotice;

fn env_in(root: &Path) -> Env {
    let agent = root.join("agent").display().to_string();
    Env {
        cwd: root.join("cwd"),
        home: root.join("home"),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent.clone())),
        docs: root.join("pkg/docs"),
    }
}

fn profile(settings: &str) -> (tempfile::TempDir, Env) {
    let dir = tempfile::tempdir().unwrap();
    for sub in ["agent", "cwd", "home", "pkg/docs"] {
        std::fs::create_dir_all(dir.path().join(sub)).unwrap();
    }
    std::fs::write(dir.path().join("agent/settings.json"), settings).unwrap();
    let env = env_in(dir.path());
    (dir, env)
}

fn saved(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("agent/settings.json")).unwrap()).unwrap()
}

#[test]
fn cli_startup_changelog_shows_new_entries_and_records_the_version() {
    let version = cli::version();
    let (dir, env) = profile(r#"{"lastChangelogVersion":"0.0.1"}"#);
    std::fs::write(
        dir.path().join("pkg/CHANGELOG.md"),
        format!("# Changelog\n\n## [{version}] - today\n\n- Shiny\n\n## [0.0.1]\n\n- Old\n"),
    )
    .unwrap();
    match cli::startup_checks::startup_changelog(&env) {
        Some(StartupNotice::WhatsNew {
            markdown,
            collapse,
            version: v,
        }) => {
            assert_eq!(markdown, format!("## [{version}] - today\n\n- Shiny"));
            assert!(!collapse);
            assert_eq!(v, version);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(saved(dir.path())["lastChangelogVersion"], version.as_str());
    // Seen now: nothing more to show.
    assert_eq!(cli::startup_checks::startup_changelog(&env), None);
}

#[test]
fn cli_startup_changelog_first_run_shows_and_records_nothing_and_collapses_on_request() {
    let (dir, env) = profile("{}");
    std::fs::write(dir.path().join("pkg/CHANGELOG.md"), "## [99.0.0]\n\n- Future\n").unwrap();
    assert_eq!(cli::startup_checks::startup_changelog(&env), None);
    // client-tui.ts never writes the setting: a first run leaves settings.json as it was.
    assert_eq!(saved(dir.path()), serde_json::json!({}));

    let (dir, env) = profile(r#"{"lastChangelogVersion":"0.0.1","collapseChangelog":true}"#);
    std::fs::write(dir.path().join("pkg/CHANGELOG.md"), "## [99.0.0]\n\n- Future\n").unwrap();
    assert!(matches!(
        cli::startup_checks::startup_changelog(&env),
        Some(StartupNotice::WhatsNew { collapse: true, .. })
    ));
    // A package without a CHANGELOG.md shows nothing.
    let (_dir, env) = profile(r#"{"lastChangelogVersion":"0.0.1"}"#);
    assert_eq!(cli::startup_checks::startup_changelog(&env), None);
}

#[test]
fn cli_subscription_auth_warning_reads_the_stored_credential_and_the_setting() {
    let case = |settings: &str, auth: Option<&str>, provider: &str| {
        let (dir, env) = profile(settings);
        if let Some(auth) = auth {
            std::fs::write(dir.path().join("agent/auth.json"), auth).unwrap();
        }
        cli::startup_checks::subscription_auth_warns(&env, provider)
    };
    let oauth = r#"{"anthropic":{"type":"oauth","access":"sk-ant-oat01-x","refresh":"r","expires":4102444800000}}"#;
    let oat_key = r#"{"anthropic":{"type":"api_key","key":"sk-ant-oat01-abc"}}"#;
    let api_key = r#"{"anthropic":{"type":"api_key","key":"sk-ant-api03-abc"}}"#;
    assert!(case("{}", Some(oauth), "anthropic"));
    assert!(case("{}", Some(oat_key), "anthropic"));
    assert!(!case("{}", Some(api_key), "anthropic"));
    assert!(!case("{}", None, "anthropic"));
    assert!(!case(
        r#"{"warnings":{"anthropicExtraUsage":false}}"#,
        Some(oauth),
        "anthropic"
    ));
    assert!(case(
        r#"{"warnings":{"anthropicExtraUsage":true}}"#,
        Some(oauth),
        "anthropic"
    ));
    assert!(!case("{}", Some(oauth), "openai"));
}
