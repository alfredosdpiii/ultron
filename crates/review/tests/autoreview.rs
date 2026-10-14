//! Replays crates/review/fixtures/{settings,prepare,plan}.json, recorded from autoreview/config.ts, prepare.ts and
//! plan.ts (fixtures/generate.mjs), and requires identical outputs.

use std::collections::HashMap;
use std::path::Path;

use review::plan::{
    EngineFinding, EngineResult, LEVELS, PlanOptions, comment_marker, comment_text, cost_line, decide_verdict,
    level_of, place_line, plan_review, rank_findings, severity_of, without_inline,
};
use review::prepare::{
    ToolchainEntry, describe_toolchain, detect_ecosystems, environment_hash, find_mise, toolchain_requests,
};
use review::settings::{
    AutoreviewSettings, Mise, ModelFallbacks, Stage, autoreview_paths, engine_settings, resolve_config, tests_eligible,
    thinking_default,
};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn from<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).expect("fixture shape")
}

fn json<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("json")
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

#[test]
fn review_settings_resolve_like_config_ts() {
    let fx = fixture("settings.json");
    let logo = fx["logo"].as_str().expect("logo");
    for case in fx["settings"].as_array().expect("settings") {
        let settings: AutoreviewSettings = from(&case["settings"]);
        let f = &case["fallbacks"];
        let available: Option<Vec<String>> = f.get("hasModel").map(from);
        let has = |model: &str| {
            available
                .as_ref()
                .is_some_and(|list| list.iter().any(|item| item == model))
        };
        let fallbacks = ModelFallbacks {
            review_model: text(&f["reviewModel"]),
            frame_model: text(&f["rlm"]["frameModel"]),
            default_provider: text(&f["defaultProvider"]),
            default_model: text(&f["defaultModel"]),
            has_model: available.is_some().then_some(&has as &dyn Fn(&str) -> bool),
        };
        let config = resolve_config(&settings, &fallbacks, logo);
        assert_eq!(json(&config), case["config"], "{}", case["settings"]);
        assert_eq!(json(engine_settings(&config)), case["engine"], "{}", case["settings"]);
        let eligible: Vec<bool> = [("o", true), ("O", false), ("acme", false), ("me", false)]
            .iter()
            .map(|(owner, push)| tests_eligible(&config, owner, *push))
            .collect();
        assert_eq!(json(eligible), case["eligible"], "{}", case["settings"]);
    }
    for case in fx["thinking"].as_array().expect("thinking") {
        let model = case["model"].as_str();
        let levels: Vec<&str> = ["finders", "verifier", "investigators", "planner", "asks"]
            .iter()
            .map(|stage| thinking_default(Stage::parse(stage).expect("stage"), model))
            .collect();
        assert_eq!(json(levels), case["levels"], "{model:?}");
    }
    for case in fx["paths"].as_array().expect("paths") {
        let env: HashMap<String, String> = from(&case["env"]);
        let paths = autoreview_paths(case["agentDir"].as_str().expect("dir"), &env, "/home/u");
        assert_eq!(json(paths), case["paths"], "{}", case["agentDir"]);
    }
}

fn build(base: &Path, tree: &Value) {
    for entry in tree.as_array().expect("tree") {
        let path = base.join(entry["path"].as_str().expect("path"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        if entry["dir"] == Value::Bool(true) {
            std::fs::create_dir_all(&path).expect("mkdir");
        } else if let Some(target) = entry["link"].as_str() {
            std::os::unix::fs::symlink(target, &path).expect("symlink");
        } else {
            std::fs::write(&path, entry["content"].as_str().expect("content")).expect("write");
        }
    }
}

#[test]
fn review_prepare_discovers_dependencies_like_prepare_ts() {
    let fx = fixture("prepare.json");
    for case in fx["repos"].as_array().expect("repos") {
        let repo = tempfile::tempdir().expect("tempdir");
        build(repo.path(), &case["tree"]);
        assert_eq!(json(detect_ecosystems(repo.path())), case["detect"], "{}", case["tree"]);
        assert_eq!(
            json(toolchain_requests(repo.path())),
            case["requests"],
            "{}",
            case["tree"]
        );
        assert_eq!(
            json(environment_hash(repo.path(), None)),
            case["hash"],
            "{}",
            case["tree"]
        );
        assert_eq!(
            json(environment_hash(repo.path(), Some("3.13"))),
            case["hashPython"],
            "{}",
            case["tree"]
        );
    }
    let root = tempfile::tempdir().expect("tempdir");
    build(root.path(), &fx["miseTree"]);
    let base = root.path().to_str().expect("utf-8 temp dir");
    for case in fx["mise"].as_array().expect("mise") {
        let mut env = HashMap::new();
        if let Some(path) = case["path"].as_str() {
            env.insert("PATH".to_string(), path.replace("<root>", base));
        }
        let setting: Option<Mise> = from(&case["setting"]);
        let found =
            find_mise(&env, &format!("{base}/fakehome"), setting.as_ref()).map(|path| path.replace(base, "<root>"));
        assert_eq!(json(found), case["found"], "{case}");
    }
    let entries: Vec<ToolchainEntry> = serde_json::from_value(serde_json::json!([
        { "tool": "python", "version": "3.12.9", "path": "/m/python/3.12.9", "source": "mise" },
        { "tool": "node", "version": "20.0", "path": "/m/node/20.0", "source": "mise", "installed": true },
        { "tool": "python", "version": "", "path": "/usr", "source": "system" },
        { "tool": "node", "version": "18", "path": "/u", "source": "uv", "installed": false },
    ]))
    .expect("entries");
    assert_eq!(json(describe_toolchain(&entries)), fx["describe"]);
}

#[test]
fn review_plan_posts_like_plan_ts() {
    let fx = fixture("plan.json");
    for (number, case) in fx["plans"].as_array().expect("plans").iter().enumerate() {
        let result: EngineResult = from(&case["result"]);
        let options: PlanOptions = from(&case["options"]);
        let verdict = decide_verdict(
            &result,
            options.self_authored,
            &options.state,
            options.block_at.as_deref(),
        );
        assert_eq!(json(&verdict), case["verdict"], "case {number}");
        assert_eq!(json(rank_findings(&result)), case["ranked"], "case {number}");
        let plan = plan_review(&result, &options);
        assert_eq!(json(&plan), case["plan"], "case {number}");
        assert_eq!(
            json(without_inline(&plan, &result)),
            case["withoutInline"],
            "case {number}"
        );
        let texts: Vec<String> = result.findings.iter().map(comment_text).collect();
        assert_eq!(json(texts), case["comments"], "case {number}");
        assert_eq!(json(cost_line(&result)), case["cost"], "case {number}");
    }
    let ranges = [(10, 20), (40, 45)];
    for place in fx["places"].as_array().expect("places") {
        let line = place[0].as_i64().expect("line");
        assert_eq!(json(place_line(Some(&ranges), line)), place[1], "line {line}");
    }
    assert_eq!(place_line(Some(&[]), 3), None);
    assert_eq!(place_line(None, 3), None);
    assert_eq!(fx["emptyPlace"], Value::Null);
    for pair in fx["levels"].as_array().expect("levels") {
        let finding = &pair[0];
        assert_eq!(
            level_of(finding["level"].as_str(), finding["severity"].as_str()),
            pair[1],
            "{finding}"
        );
    }
    let severities: Vec<&str> = LEVELS.iter().map(|level| severity_of(level)).collect();
    assert_eq!(json(severities), fx["severities"]);
    assert_eq!(comment_marker("k-1"), fx["marker"]);
    // An EngineFinding without optional fields deserializes (the old-scale engines' shape).
    let finding: EngineFinding =
        from(&serde_json::json!({ "file": "a", "line": 1, "claim": "c", "verification": "confirmed" }));
    assert_eq!(finding.level(), "low");
}
