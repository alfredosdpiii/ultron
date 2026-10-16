//! The startup notices: replays fixtures/startup-notices.json, recorded from interactive-mode.ts
//! (`getChangelogForDisplay`, `showStartupNoticesIfNeeded`, `showNewVersionNotification`, `showWarning`,
//! `maybeWarnAboutAnthropicSubscriptionAuth`, the real methods over a bare host) by
//! fixtures/generate-startup-notices.mjs.

use std::rc::Rc;

use interactive::startup_notices::{
    ANTHROPIC_SUBSCRIPTION_AUTH_WARNING, NewVersionNotice, WhatsNewNotice, changelog_for_display, warning_text,
    warns_about_subscription_auth,
};
use serde_json::Value;
use tui::components::{Component, Text};
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/startup-notices.json")).unwrap()
}

/// The fixture's sample CHANGELOG.md (generate-startup-notices.mjs `CHANGELOG`).
const CHANGELOG: &str = "# Changelog\n\n## [Unreleased]\n\n### Added\n- Pending work\n\n## [1.4.0] - 2026-10-01\n\n### Added\n- A [guide](docs/guide.md) for the `x` flag\n\n```\nultron --x\n```\n\n## [1.3.2] - 2026-09-20\n\n### Fixed\n- Fixed a crash ([#12](https://github.com/alfredosdpiii/ultron/issues/12))\n\n## [1.3.0] - 2026-09-01\n\n- Old news\n";

fn theme(name: &str) -> Rc<Theme> {
    let (name, mode) = name.split_once('-').unwrap();
    let mode = if mode == "truecolor" {
        ColorMode::Truecolor
    } else {
        ColorMode::Ansi256
    };
    Rc::new(create_theme(&builtin_theme_json(name).unwrap(), mode, None).unwrap())
}

fn lines(case: &Value) -> Vec<String> {
    case["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn interactive_startup_changelog_selection_matches_ts() {
    for case in fixture()["changelog"].as_array().unwrap() {
        let last = case["last"].as_str();
        let shown = changelog_for_display(Some(CHANGELOG), last, "1.4.0", case["messages"].as_u64().unwrap() > 0);
        assert_eq!(shown.markdown.as_deref(), case["markdown"].as_str(), "{case}");
        // What the settings hold afterwards: the saved version, else what was there.
        let saved = shown.save.as_deref().or(last);
        assert_eq!(saved, case["saved"].as_str(), "{case}");
    }
}

#[test]
fn interactive_startup_notices_render_like_ts() {
    let fixture = fixture();
    let markdown = fixture["changelog"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["last"] == "1.3.0" && c["messages"] == 0)
        .unwrap()["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    for case in fixture["render"].as_array().unwrap() {
        let t = theme(case["theme"].as_str().unwrap());
        let width = case["width"].as_u64().unwrap() as usize;
        let got = match case["kind"].as_str().unwrap() {
            "whats_new" => WhatsNewNotice {
                theme: t,
                markdown: markdown.clone(),
                collapse: case["collapse"] == true,
                version: "1.4.0".into(),
                code_block_indent: case["codeBlockIndent"].as_str().map(str::to_string),
                hyperlinks: false,
            }
            .render(width),
            "new_version" => {
                // The transcript's spacer, then the block.
                let mut out = vec![String::new()];
                out.extend(
                    NewVersionNotice {
                        theme: t,
                        version: case["version"].as_str().unwrap().into(),
                        hyperlinks: case["hyperlinks"] == true,
                    }
                    .render(width),
                );
                out
            }
            "warning" => {
                let mut out = vec![String::new()];
                out.extend(Text::new(&warning_text(&t, ANTHROPIC_SUBSCRIPTION_AUTH_WARNING), 1, 0, None).render(width));
                out
            }
            other => panic!("{other}"),
        };
        assert_eq!(got, lines(case), "{} {} at {width}", case["kind"], case["theme"]);
    }
    assert_eq!(fixture["warning"], ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
}

#[test]
fn interactive_subscription_auth_warning_decision_matches_ts() {
    for case in fixture()["warnings"].as_array().unwrap() {
        let enabled = case["settings"]["warnings"]["anthropicExtraUsage"] != false;
        let warns = warns_about_subscription_auth(
            case["provider"].as_str().unwrap(),
            enabled,
            case["authType"].as_str(),
            case["apiKey"].as_str(),
        );
        // TypeScript warns once however often it is asked.
        assert_eq!(case["shown"].as_array().unwrap().len(), usize::from(warns), "{case}");
    }
}
