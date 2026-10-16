//! The startup package update notice: replays fixtures/package-updates.json, recorded from interactive-mode.ts
//! `showPackageUpdateNotification` (the real method over a bare chat container) by
//! fixtures/generate-package-updates.mjs, in both themes at two widths.

use std::rc::Rc;

use interactive::package_updates::PackageUpdateNotice;
use serde_json::Value;
use tui::components::Component;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

#[test]
fn interactive_package_update_notice_matches_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/package-updates.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let (name, mode) = case["theme"].as_str().unwrap().split_once('-').unwrap();
        let mode = if mode == "truecolor" {
            ColorMode::Truecolor
        } else {
            ColorMode::Ansi256
        };
        let theme = Rc::new(create_theme(&builtin_theme_json(name).unwrap(), mode, None).unwrap());
        let packages: Vec<String> = case["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        let width = case["width"].as_u64().unwrap() as usize;
        // The chat's notice spacer, then the notice.
        let mut lines = vec![String::new()];
        lines.extend(PackageUpdateNotice { theme, packages }.render(width));
        let want: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        assert_eq!(lines, want, "{} at {width}", case["theme"]);
    }
}
