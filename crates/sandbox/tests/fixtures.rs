//! Differential tests against recorded reference output: fixtures/autoreview-argv.json, autoreview_tests.py's
//! `detect_sandbox` (fixtures/autoreview_argv.py; its `wrap`, `sandbox_env` and `describe` cases have no Rust port:
//! reviews run that Python).

use sandbox::launcher::{Mechanism, detect};
use serde_json::Value;
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
