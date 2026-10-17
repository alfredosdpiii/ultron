//! `/export` and `/share` in the TUI's theme: replays fixtures/export_theme.json, recorded from the TypeScript
//! `generateHtml(sessionData, theme.name)` and `getShareViewerUrl` by fixtures/export_theme.mjs. The theme files are
//! written from the fixture into a private temp root; every page must equal the TypeScript one byte for byte (by
//! sha256, with its <style> compared as text), and every failure must carry the TypeScript message.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/export_theme.json")).unwrap()
}

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn root(fixture: &Value, tag: &str) -> Root {
    let dir = std::env::temp_dir().join(format!("cli-export-theme-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, content) in fixture["files"].as_object().unwrap() {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content.as_str().unwrap()).unwrap();
    }
    Root(dir)
}

fn style(html: &str) -> &str {
    let start = html.find("<style>").unwrap();
    let end = html.find("</style>").unwrap() + "</style>".len();
    &html[start..end]
}

fn check(case: &Value, got: Result<String, String>, root: &Path) {
    let label = format!("theme {} COLORFGBG {}", case["theme"], case["colorfgbg"]);
    match case["error"].as_str() {
        Some(want) => {
            let error = got.expect_err(&label).replace(&root.display().to_string(), "<ROOT>");
            assert_eq!(error, want, "{label}");
        }
        None => {
            let html = got.unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(style(&html), case["style"].as_str().unwrap(), "{label}: style");
            assert_eq!(
                format!("{:x}", Sha256::digest(html.as_bytes())),
                case["sha256"].as_str().unwrap(),
                "{label}: page"
            );
        }
    }
}

#[test]
fn cli_export_theme_pages_match_typescript_generate_html() {
    let fixture = fixture();
    let root = root(&fixture, "pages");
    let registered: Vec<(String, Option<String>)> = fixture["registered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let path = r["path"].as_str().map(|p| root.0.join(p).display().to_string());
            (r["name"].as_str().unwrap().to_string(), path)
        })
        .collect();
    let custom_dir = root.0.join("agent/themes");
    let sources = cli::export_html::ThemeSources {
        registered: &registered,
        custom_dir: &custom_dir,
    };
    for case in fixture["cases"].as_array().unwrap() {
        let got = cli::export_html::generate_html_themed(
            &fixture["sessionData"],
            case["theme"].as_str(),
            case["colorfgbg"].as_str(),
            &sources,
        );
        check(case, got, &root.0);
    }
}

/// The page the TUI's `/export` writes (`export_page`): registered themes come from the profile's settings
/// (`themes` entries resolved by the package manager), custom ones from `<agent dir>/themes`.
#[test]
fn cli_export_theme_export_page_finds_registered_and_custom_themes() {
    let fixture = fixture();
    let root = root(&fixture, "env");
    let agent = root.0.join("agent");
    let harbor = root.0.join("pkg/harbor.json");
    std::fs::write(
        agent.join("settings.json"),
        serde_json::json!({"themes": [harbor.display().to_string()]}).to_string(),
    )
    .unwrap();
    let cwd = root.0.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let agent_var = agent.display().to_string();
    let env = cli::Env {
        cwd,
        home: root.0.join("home"),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent_var.clone())),
        docs: root.0.join("docs"),
    };
    let cases = fixture["cases"].as_array().unwrap();
    for name in ["dark", "light", "ocean", "harbor", "missing"] {
        let case = cases.iter().find(|c| c["theme"] == name).unwrap();
        check(
            case,
            cli::export_html::export_page(&fixture["sessionData"], Some(name), &env),
            &root.0,
        );
    }
}

#[test]
fn cli_export_theme_share_viewer_url_falls_back_on_empty_like_ts() {
    for case in fixture()["share"].as_array().unwrap() {
        let value = case["env"].as_str().map(str::to_string);
        let var = move |k: &str| (k == "PI_SHARE_VIEWER_URL").then(|| value.clone()).flatten();
        assert_eq!(
            cli::export_html::share_viewer_url(&var, case["gistId"].as_str().unwrap()),
            case["url"].as_str().unwrap(),
            "PI_SHARE_VIEWER_URL={}",
            case["env"]
        );
    }
}
