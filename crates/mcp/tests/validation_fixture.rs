//! Replays crates/mcp/fixtures/validation.json, recorded from TypeScript by generate-validation.mjs:
//! `prepareToolArguments` against the `mcp` gateway's schema, V8's `JSON.parse` messages, and the gateway's reply to
//! an `args` string that is not a JSON object.

use mcp::gateway::{Gateway, GatewayOptions, gateway_parameters};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::OnceLock;

fn fixture() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/validation.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    })
}

fn outcome<T: Into<Value>>(result: Result<T, String>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value.into() }),
        Err(error) => json!({ "err": error }),
    }
}

fn expected(case: &Value) -> Value {
    match case.get("ok") {
        Some(ok) => json!({ "ok": ok }),
        None => json!({ "err": case["err"] }),
    }
}

#[test]
fn mcp_validation_gateway_schema_is_the_typescript_one() {
    assert_eq!(gateway_parameters(), fixture()["parameters"]);
}

#[test]
fn mcp_validation_prepare_tool_arguments_matches_typescript() {
    for case in fixture()["prepared"].as_array().unwrap() {
        let got =
            mcp::validation::prepare_tool_arguments("mcp", &gateway_parameters(), case["params"].as_object().unwrap());
        assert_eq!(outcome(got.map(Value::Object)), expected(case), "{}", case["params"]);
    }
}

#[test]
fn mcp_validation_json_parse_messages_match_v8() {
    for case in fixture()["parse"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(outcome(mcp::js_json::parse(input)), expected(case), "{input:?}");
    }
}

#[test]
fn mcp_validation_gateway_reports_invalid_args_strings_as_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let entry = mcp::config::ServerEntry {
        name: "s".into(),
        config: json!({ "command": "x" }).as_object().unwrap().clone(),
        source: "test".into(),
        scope: mcp::config::Scope::Global,
    };
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: dir.path().to_path_buf(),
        servers: Some(vec![entry]),
        ..GatewayOptions::default()
    });
    for case in fixture()["gateway"].as_array().unwrap() {
        let params = json!({ "tool": "t", "args": case["args"] });
        let result = gateway.execute(params.as_object().unwrap(), None).unwrap();
        assert_eq!(Gateway::text_of(&result), case["text"], "{}", case["args"]);
        assert_eq!(result["details"], case["details"], "{}", case["args"]);
    }
}

#[test]
fn mcp_validation_required_and_enum_match_typescript() {
    let schemas = fixture()["schemas"].as_array().unwrap();
    for case in fixture()["schemaCases"].as_array().unwrap() {
        let schema = &schemas[case["schema"].as_u64().unwrap() as usize];
        let got = mcp::validation::prepare_tool_arguments("probe", schema, case["params"].as_object().unwrap());
        assert_eq!(outcome(got.map(Value::Object)), expected(case), "{}", case["params"]);
    }
}

#[test]
fn mcp_validation_formats_match_typebox() {
    for case in fixture()["formatCases"].as_array().unwrap() {
        let (format, value) = (case["format"].as_str().unwrap(), case["value"].as_str().unwrap());
        assert_eq!(
            mcp::formats::matches(format, value),
            case["ok"] == true,
            "{format} {value:?}"
        );
    }
}

#[test]
fn mcp_validation_patterns_match_ecmascript() {
    for case in fixture()["patternCases"].as_array().unwrap() {
        let (pattern, value) = (case["pattern"].as_str().unwrap(), case["value"].as_str().unwrap());
        let schema = json!({ "type": "object", "properties": { "v": { "type": "string", "pattern": pattern } } });
        let got = mcp::validation::prepare_tool_arguments("probe", &schema, json!({ "v": value }).as_object().unwrap());
        assert_eq!(got.is_ok(), case["ok"] == true, "{pattern} {value:?}");
        if let Err(error) = got {
            assert_eq!(error.split('\n').nth(1), case["err"].as_str(), "{pattern} {value:?}");
        }
    }
}
