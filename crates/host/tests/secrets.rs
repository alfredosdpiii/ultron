//! Replays crates/host/fixtures/secrets.json, recorded by fixtures/secrets.mjs from the TypeScript masking
//! (secrets.ts, output-secrets.ts, sensitive.ts) over a corpus; plus literal cases from the upstream behavior.

use host::secrets::{
    OutputMasker, SecretDetector, contains_code_secret, contains_sensitive_memory, mask_secrets_enabled,
    preview_secret, shannon_entropy,
};
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/secrets.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The generator stores secret-looking strings as 8-character pieces.
fn joined(value: &Value) -> String {
    value.as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect()
}

/// An expected text: plain, or in pieces when it holds a secret-looking string.
fn text_or_joined(value: &Value) -> String {
    value.as_str().map_or_else(|| joined(value), str::to_owned)
}

#[test]
fn host_secrets_redaction_matches_typescript_corpus() {
    let data = fixture();
    let detector = SecretDetector::shipped();
    let mut failures = Vec::new();
    for case in data["cases"].as_array().unwrap() {
        let text = joined(&case["text"]);
        let (redacted, masked) = detector.redact_counted(&text);
        let want = (text_or_joined(&case["redacted"]), case["masked"].as_u64().unwrap() as usize);
        if (redacted.clone(), masked) != want {
            failures.push(format!("{text:?}: got {redacted:?}/{masked}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn host_secrets_scan_scope_findings_match_typescript_corpus() {
    let data = fixture();
    let mut failures = Vec::new();
    for case in data["cases"].as_array().unwrap() {
        let text = joined(&case["text"]);
        let got: Vec<Value> = SecretDetector::shipped()
            .scan(&text, false)
            .into_iter()
            .map(|f| json!([f.kind, &text[f.start..f.end], f.preview]))
            .collect();
        let want: Vec<Value> = case["scan"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| json!([f["kind"], joined(&f["value"]), f["preview"]]))
            .collect();
        if got != want {
            failures.push(format!("{text:?}: got {got:?}, want {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn host_secrets_sensitive_checks_match_typescript_corpus() {
    let data = fixture();
    for case in data["cases"].as_array().unwrap() {
        let text = joined(&case["text"]);
        assert_eq!(
            contains_sensitive_memory(&text),
            case["sensitive_memory"].as_bool().unwrap(),
            "{text:?}"
        );
        assert_eq!(
            contains_code_secret(&text),
            case["code_secret"].as_bool().unwrap(),
            "{text:?}"
        );
    }
}

#[test]
fn host_secrets_entropy_and_preview_match_typescript() {
    for case in fixture()["entropy"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        assert!(
            (shannon_entropy(value) - case["bits"].as_f64().unwrap()).abs() < 1e-12,
            "{value:?}"
        );
        assert_eq!(preview_secret(value), case["preview"].as_str().unwrap());
    }
}

#[test]
fn host_secrets_mask_switch_matches_typescript() {
    let masker = OutputMasker::default();
    for case in fixture()["off"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let (text, masked) = masker.mask_counted(&format!("k {}", joined(&fixture()["known_key"])), Some(value));
        assert_eq!(
            (text, masked as u64),
            (joined(&case["text"]), case["masked"].as_u64().unwrap()),
            "{value:?}"
        );
    }
    assert!(mask_secrets_enabled(None));
}

#[test]
fn host_secrets_known_values_match_typescript() {
    let data = fixture();
    let masker = OutputMasker::default();
    masker.register_secret_values(data["known"].as_array().unwrap());
    for case in data["known_cases"].as_array().unwrap() {
        let text = joined(&case["text"]);
        let (out, masked) = masker.mask_counted(&text, None);
        assert_eq!(out, case["redacted"].as_str().unwrap(), "{text:?}");
        assert_eq!(masked as u64, case["masked"].as_u64().unwrap(), "{text:?}");
    }
}
