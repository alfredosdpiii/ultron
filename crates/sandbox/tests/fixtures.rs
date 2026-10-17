//! Differential tests against recorded reference output:
//! - fixtures/autoreview-argv.json: autoreview_tests.py's `detect_sandbox` (fixtures/autoreview_argv.py; its `wrap`,
//!   `sandbox_env` and `describe` cases have no Rust port: reviews run that Python);
//! - fixtures/omp-approval.json: oh-my-pi's critical patterns, bash approval, tokenizers and `resolveApproval`
//!   (fixtures/omp-approval.mjs).

use sandbox::approval::{Decision, Mode, resolve};
use sandbox::launcher::{Mechanism, detect};
use sandbox::patterns::{
    CRITICAL_PATTERN_SOURCES, bash_decision, has_shell_control, is_critical, literal_and_chain, pattern_rules,
    tokenize_shell_segments,
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;

fn load(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
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
        // Which patterns matched is not product behaviour; whether any did is (`bash_decision`'s critical gate).
        let expected = !case["matches"].as_array().unwrap().is_empty();
        assert_eq!(is_critical(command), expected, "{command:?}");
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
