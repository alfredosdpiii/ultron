//! Differential tests against recorded reference output:
//! - fixtures/autoreview-argv.json: autoreview_tests.py's `Sandbox.wrap`, `sandbox_env`, `describe` and
//!   `detect_sandbox` (fixtures/autoreview_argv.py, a fake host layout);
//! - fixtures/omp-approval.json: oh-my-pi's critical patterns, bash approval, tokenizers and `resolveApproval`
//!   (fixtures/omp-approval.mjs).

use sandbox::approval::{Decision, Mode, resolve, strictest};
use sandbox::launcher::{Entry, Layout, Mechanism, Review, detect, review_env};
use sandbox::patterns::{
    CRITICAL_PATTERN_SOURCES, bash_decision, critical_matches, has_shell_control, literal_and_chain, pattern_rules,
    tokenize_shell_segments,
};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::path::Path;

fn load(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The recorder's fake layout, and whether `setpriv` is on its PATH.
struct FakeLayout(Map<String, Value>, bool);
impl Layout for FakeLayout {
    fn entry(&self, path: &Path) -> Entry {
        match self.0.get(path.to_str().unwrap()).and_then(Value::as_str) {
            Some("dir") => Entry::Dir,
            Some(link) if link.starts_with("link:") => Entry::Symlink(link["link:".len()..].to_owned()),
            _ => Entry::Missing,
        }
    }
    fn which(&self, name: &str) -> bool {
        name == "setpriv" && self.1
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn env_pairs(value: &Value) -> Vec<(String, String)> {
    let pair = |item: &Value| {
        (
            item[0].as_str().unwrap().to_owned(),
            item[1].as_str().unwrap().to_owned(),
        )
    };
    value.as_array().unwrap().iter().map(pair).collect()
}

#[test]
fn sandbox_review_wrap_matches_autoreview_argv() {
    let fixture = load("autoreview-argv.json");
    let wraps = fixture["wraps"].as_array().unwrap();
    assert_eq!(wraps.len(), 16);
    for case in wraps {
        let layout = FakeLayout(
            fixture["layout"].as_object().unwrap().clone(),
            case["setpriv"].as_bool().unwrap(),
        );
        let review = Review {
            mechanism: Mechanism::parse(case["mechanism"].as_str().unwrap()).unwrap(),
            image: case["image"].as_str().map(str::to_owned),
            home: fixture["home"].as_str().unwrap().to_owned(),
            uid: fixture["uid"].as_u64().unwrap() as u32,
            gid: fixture["gid"].as_u64().unwrap() as u32,
        };
        let binds: Vec<(String, String)> = case["ro_binds"]
            .as_array()
            .map(|binds| {
                binds
                    .iter()
                    .map(|bind| match bind {
                        Value::String(path) => (path.clone(), path.clone()),
                        pair => (
                            pair[0].as_str().unwrap().to_owned(),
                            pair[1].as_str().unwrap().to_owned(),
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let env = env_pairs(&fixture["envs"][case["env"].as_str().unwrap()]);
        let argv = review.wrap(
            &layout,
            &strings(&case["command"]),
            case["workdir"].as_str().unwrap(),
            &env,
            &binds,
            case["cwd"].as_str().unwrap(),
        );
        assert_eq!(argv, strings(&case["argv"]), "{case}");
        assert_eq!(
            review.describe(),
            fixture["describe"][review.mechanism.name()].as_str().unwrap()
        );
    }
}

#[test]
fn sandbox_review_env_matches_autoreview_sandbox_env() {
    let fixture = load("autoreview-argv.json");
    assert_eq!(review_env(None), env_pairs(&fixture["envs"]["sandbox"]));
    assert_eq!(
        review_env(Some("/work/.venv/bin")),
        env_pairs(&fixture["envs"]["sandbox_extra"])
    );
}

#[test]
fn sandbox_detect_matches_autoreview_detect_sandbox() {
    let fixture = load("autoreview-argv.json");
    for case in fixture["detect"].as_array().unwrap() {
        let present = strings(&case["present"]);
        let working = strings(&case["working"]);
        let probed = RefCell::new(Vec::new());
        let found = detect(
            case["image"].as_str(),
            case["only"].as_str().and_then(Mechanism::parse),
            |name| present.iter().any(|p| p == name),
            |mechanism| {
                probed.borrow_mut().push(mechanism.name().to_owned());
                working.iter().any(|w| w == mechanism.name())
            },
        );
        assert_eq!(found.map(Mechanism::name), case["mechanism"].as_str(), "{case}");
        assert_eq!(*probed.borrow(), strings(&case["probed"]), "{case}");
    }
}

#[test]
fn sandbox_critical_pattern_sources_are_omps() {
    let fixture = load("omp-approval.json");
    let patterns = fixture["patterns"].as_array().unwrap();
    assert_eq!(patterns.len(), CRITICAL_PATTERN_SOURCES.len());
    for (pattern, (source, flags)) in patterns.iter().zip(CRITICAL_PATTERN_SOURCES) {
        assert_eq!(pattern["source"], *source);
        assert_eq!(pattern["flags"], *flags);
    }
}

#[test]
fn sandbox_critical_patterns_match_omps_table() {
    let fixture = load("omp-approval.json");
    let cases = fixture["critical"].as_array().unwrap();
    assert!(cases.len() > 80);
    for case in cases {
        let command = case["command"].as_str().unwrap();
        let expected: Vec<usize> = case["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_u64().unwrap() as usize)
            .collect();
        assert_eq!(critical_matches(command), expected, "{command:?}");
    }
}

#[test]
fn sandbox_shell_tokenizers_match_omps() {
    let fixture = load("omp-approval.json");
    let chain_json = |chain: Option<Vec<sandbox::patterns::ChainSegment>>| match chain {
        None => Value::Null,
        Some(segments) => Value::Array(
            segments
                .into_iter()
                .map(|s| json!({ "text": s.text, "argv": s.argv }))
                .collect(),
        ),
    };
    for case in fixture["segments"].as_array().unwrap() {
        let command = case["command"].as_str().unwrap();
        assert_eq!(json!(tokenize_shell_segments(command)), case["segments"], "{command:?}");
        assert_eq!(json!(has_shell_control(command)), case["shellControl"], "{command:?}");
        assert_eq!(
            chain_json(literal_and_chain(command)),
            case["literalChain"],
            "{command:?}"
        );
    }
    for case in fixture["control"].as_array().unwrap() {
        let command = case["command"].as_str().unwrap();
        assert_eq!(json!(has_shell_control(command)), case["shellControl"], "{command:?}");
    }
    for case in fixture["chains"].as_array().unwrap() {
        let command = case["command"].as_str().unwrap();
        assert_eq!(
            chain_json(literal_and_chain(command)),
            case["literalChain"],
            "{command:?}"
        );
    }
}

#[test]
fn sandbox_bash_decision_matches_omps_bash_approval() {
    let fixture = load("omp-approval.json");
    let decisions = fixture["decisions"].as_array().unwrap();
    assert!(decisions.len() > 300);
    for case in decisions {
        let rules = pattern_rules(fixture["patternSets"].get(case["set"].as_str().unwrap()));
        let command = case["command"].as_str().unwrap_or_default();
        let decision = bash_decision(command, &rules, case["compound"].as_bool().unwrap());
        assert_eq!(decision.to_json(), case["decision"], "{case}");
    }
}

#[test]
fn sandbox_resolve_matches_omps_resolve_approval() {
    let fixture = load("omp-approval.json");
    let cases = fixture["resolutions"].as_array().unwrap();
    assert_eq!(cases.len(), 480);
    for case in cases {
        let decision = Decision::from_json(Some(&case["decision"]).filter(|d| !d.is_null()));
        let mode = Mode::parse(&case["mode"]).unwrap();
        let resolved = resolve("bash", Some("old"), &decision, mode, case["user"].as_object().unwrap());
        assert_eq!(resolved.to_json(), case["resolved"], "{case}");
    }
}

#[test]
fn sandbox_strictest_matches_omps_strictest_approval() {
    let fixture = load("omp-approval.json");
    for case in fixture["strictest"].as_array().unwrap() {
        let decisions: Vec<Decision> = case["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| Decision::from_json(Some(d)))
            .collect();
        assert_eq!(strictest(&decisions).to_json(), case["result"], "{case}");
    }
}
