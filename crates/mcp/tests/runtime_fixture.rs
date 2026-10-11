//! Replays crates/mcp/fixtures/runtime.json, recorded from the TypeScript MCP extension by generate-runtime.mjs.

use mcp::http::{SseItem, SseParser};
use mcp::resolve::{resolve_config_value_or_err, resolve_headers_or_err};
use mcp::results::{convert_mcp_result, is_mcp_app_resource, listed};
use mcp::runtime::{TransportSpec, transport_spec};
use serde_json::{Map, Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

fn fixture() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/runtime.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    })
}

/// The fixture's variables are passed as the explicit env map, so the test does not mutate the process environment.
fn env() -> HashMap<String, String> {
    fixture()["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
        .collect()
}

fn outcome<T: Into<Value>>(result: Result<T, String>) -> Value {
    match result {
        Ok(v) => json!({ "ok": v.into() }),
        Err(e) => json!({ "err": e }),
    }
}

fn expected(case: &Value) -> Value {
    match case.get("ok") {
        Some(ok) => json!({ "ok": ok }),
        None => json!({ "err": case["err"] }),
    }
}

#[test]
fn mcp_runtime_resolve_config_values_match_typescript() {
    for case in fixture()["resolveCases"].as_array().unwrap() {
        let got = outcome(resolve_config_value_or_err(
            case["value"].as_str().unwrap(),
            "thing",
            &env(),
        ));
        assert_eq!(got, expected(case), "{}", case["value"]);
    }
}

#[test]
fn mcp_runtime_resolve_headers_match_typescript() {
    for case in fixture()["headerCases"].as_array().unwrap() {
        let pairs: Vec<(String, String)> = case["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
            .collect();
        let got = outcome(
            resolve_headers_or_err(&pairs, "MCP server \"s\"", &env()).map(|h| match h {
                None => Value::Null,
                Some(h) => Value::Object(h.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
            }),
        );
        assert_eq!(got, expected(case), "{}", case["headers"]);
    }
}

#[test]
fn mcp_runtime_stdio_transport_options_match_typescript() {
    for case in fixture()["transportCases"].as_array().unwrap() {
        let config = case["config"].as_object().unwrap();
        let got = transport_spec(
            "srv",
            config,
            Path::new("/proj/root"),
            Path::new("/home/fixture"),
            &env(),
        )
        .map(|(spec, _)| {
            let TransportSpec::Stdio(o) = spec else {
                panic!("stdio expected")
            };
            let mut out = Map::new();
            out.insert("command".into(), json!(o.command));
            if o.inherit_env == Some(false) {
                out.insert("inheritEnv".into(), json!(false));
            }
            if config.contains_key("args") {
                out.insert("args".into(), json!(o.args));
            }
            out.insert("cwd".into(), json!(o.cwd.unwrap().to_string_lossy()));
            out.insert(
                "env".into(),
                Value::Object(o.env.into_iter().map(|(k, v)| (k, Value::String(v))).collect()),
            );
            out.insert("stderr".into(), json!("pipe"));
            Value::Object(out)
        });
        assert_eq!(outcome(got), expected(case), "{}", case["config"]);
    }
}

#[test]
fn mcp_runtime_convert_results_match_typescript() {
    for case in fixture()["convertCases"].as_array().unwrap() {
        let saved = RefCell::new(Vec::<Value>::new());
        let failing = case["failing"].as_bool().unwrap();
        let save = |data: &[u8], ext: &str| -> Result<String, String> {
            if failing {
                return Err("disk full".into());
            }
            use base64::Engine;
            let mut saved = saved.borrow_mut();
            saved.push(json!({ "data": base64::engine::general_purpose::STANDARD.encode(data), "extension": ext }));
            Ok(format!("/saved/{}{ext}", saved.len()))
        };
        let out = convert_mcp_result(
            "srv",
            "tool",
            &case["result"],
            case["readable"].as_bool().unwrap(),
            &save,
        );
        assert_eq!(out, case["out"], "{}", case["result"]);
        assert_eq!(Value::Array(saved.into_inner()), case["saved"]);
    }
}

#[test]
fn mcp_runtime_app_resources_and_listing_match_typescript() {
    for case in fixture()["appCases"].as_array().unwrap() {
        assert_eq!(
            json!(is_mcp_app_resource(&case["item"])),
            case["app"],
            "{}",
            case["item"]
        );
    }
    for case in fixture()["listedCases"].as_array().unwrap() {
        let got = listed("srv", &case["item"]);
        assert_eq!(got, case["out"]);
        // Key order too: `{ server, ...rest }`.
        assert_eq!(
            serde_json::to_string(&got).unwrap(),
            serde_json::to_string(&case["out"]).unwrap()
        );
    }
}

#[test]
fn mcp_runtime_sse_parser_matches_typescript() {
    for case in fixture()["sseCases"].as_array().unwrap() {
        let mut parser = SseParser::new(12);
        let mut events = Vec::new();
        let mut error = Value::Null;
        let mut run = || -> Result<(), String> {
            for chunk in case["chunks"].as_array().unwrap() {
                events.extend(parser.feed(chunk.as_str().unwrap().as_bytes())?);
            }
            events.extend(parser.finish()?);
            Ok(())
        };
        if let Err(e) = run() {
            error = json!(e);
        }
        let events: Vec<Value> = events
            .into_iter()
            .map(|item| match item {
                SseItem::Id(id) => json!({ "kind": "id", "id": id }),
                SseItem::Retry(ms) => json!({ "kind": "retry", "ms": ms }),
                SseItem::Event(e) => {
                    let mut out = json!({ "kind": "event" });
                    if let Some(name) = e.event {
                        out["event"] = json!(name);
                    }
                    out["data"] = json!(e.data);
                    if let Some(id) = e.id {
                        out["id"] = json!(id);
                    }
                    out
                }
            })
            .collect();
        assert_eq!(
            json!({ "events": events, "error": error }),
            json!({ "events": case["events"], "error": case["error"] }),
            "{}",
            case["chunks"]
        );
    }
}

#[test]
fn mcp_runtime_saved_binary_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let path = mcp::results::save_to_temp_file(b"\x00\x01", ".bin").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(data, b"\x00\x01");
    assert!(
        Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("ultron-mcp-")
    );
}
