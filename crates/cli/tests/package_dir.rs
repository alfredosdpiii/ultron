//! `PI_PACKAGE_DIR`: replays fixtures/package-dir.json (config.ts `getPackageDir`, `getDocsPath`,
//! `getChangelogPath` and `getRlmRuntimePath` under the variable, recorded by fixtures/generate-package-dir.mjs)
//! through `cli::assets::package_dir`.

use cli::assets;
use serde_json::Value;

#[test]
fn cli_pi_package_dir_selects_the_package_directory_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/package-dir.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(tmp.path()).unwrap().join("home");
    std::fs::create_dir_all(home.join("pkg/src/ultron/rlm")).unwrap();
    std::fs::write(home.join("pkg").join(assets::RUNTIME), "").unwrap();
    std::fs::create_dir_all(home.join("empty")).unwrap();
    let abs = |v: &str| v.replace("<home>", &home.display().to_string());
    let rel = |p: &std::path::Path| p.display().to_string().replace(&home.display().to_string(), "<home>");
    for case in fixture["cases"].as_array().unwrap() {
        let value = abs(case["value"].as_str().unwrap());
        let agent = home.join("agent").display().to_string();
        let lookup = |key: &str| match key {
            "PI_PACKAGE_DIR" => Some(value.clone()),
            "ULTRON_CODING_AGENT_DIR" => Some(agent.clone()),
            _ => None,
        };
        match assets::package_dir(&lookup, &home) {
            Ok(dir) => {
                assert_eq!(rel(&dir), case["packageDir"].as_str().unwrap(), "{case}");
                assert_eq!(rel(&dir.join("docs")), case["docs"].as_str().unwrap(), "{case}");
                assert_eq!(
                    rel(&dir.join("CHANGELOG.md")),
                    case["changelog"].as_str().unwrap(),
                    "{case}"
                );
                assert_eq!(
                    rel(&dir.join(assets::RUNTIME)),
                    case["runtime"]["value"].as_str().unwrap(),
                    "{case}"
                );
            }
            Err(error) => assert_eq!(error, case["runtime"]["error"].as_str().unwrap(), "{case}"),
        }
    }
    // ULTRON_DEV_ASSETS, when set, still decides (here: the embedded files).
    let agent = home.join("agent").display().to_string();
    let both = |key: &str| match key {
        "PI_PACKAGE_DIR" => Some(home.join("pkg").display().to_string()),
        "ULTRON_DEV_ASSETS" => Some("0".into()),
        "ULTRON_CODING_AGENT_DIR" => Some(agent.clone()),
        _ => None,
    };
    assert_eq!(
        assets::source(&both, &home).unwrap(),
        assets::Source::Embedded(home.join("agent/runtime"))
    );
}
