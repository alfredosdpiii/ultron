//! Replays crates/mcp/fixtures/config.json (recorded from the TypeScript mcp.json validation and loading) and ports
//! packages/mcp/test/content.test.ts.

use mcp::config::{load_config, mcp_namespace, normalize_adapter_server_config, tool_exposure, validate_server_config};
use mcp::content::to_llm_content;
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/config.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn text(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn mcp_config_validation_matches_typescript() {
    for case in fixture()["validate"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let actual = match validate_server_config(name, &case["raw"]) {
            Ok(config) => Value::Object(config),
            Err(error) => Value::String(error),
        };
        assert_eq!(text(&actual), text(&case["result"]), "{}", text(&case["raw"]));
    }
}

#[test]
fn mcp_config_adapter_normalization_matches_typescript() {
    for case in fixture()["normalize"].as_array().unwrap() {
        let actual = normalize_adapter_server_config(&case["raw"], &case["defaults"]);
        assert_eq!(text(&actual), text(&case["result"]), "{}", text(&case["raw"]));
    }
}

#[test]
fn mcp_config_tool_exposure_and_namespace_match_typescript() {
    for case in fixture()["exposure"].as_array().unwrap() {
        let config = case["config"].as_object().unwrap();
        assert_eq!(
            tool_exposure(config, case["tool"].as_str().unwrap()),
            case["result"],
            "{}",
            text(case)
        );
    }
    for case in fixture()["namespace"].as_array().unwrap() {
        assert_eq!(mcp_namespace(case["name"].as_str().unwrap()), case["result"]);
    }
}

#[test]
fn mcp_config_loading_merges_global_and_project_files_like_typescript() {
    for case in fixture()["loads"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let agent = dir.path().join("agent");
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(cwd.join(".pi")).unwrap();
        for (key, path) in [
            ("global", agent.join("mcp.json")),
            ("project", cwd.join(".pi/mcp.json")),
        ] {
            if let Some(content) = case.get(key) {
                std::fs::write(path, serde_json::to_string_pretty(content).unwrap()).unwrap();
            }
        }
        let loaded = load_config(&agent, &cwd, case["trusted"] == json!(true));
        let servers: Vec<Value> = loaded
            .servers
            .iter()
            .map(|s| json!({ "name": s.name, "config": s.config, "source": s.source, "scope": s.scope.as_str() }))
            .collect();
        let mut actual = json!({ "servers": servers });
        if let Some(auto) = loaded.auto_enable_codemode {
            actual["autoEnableCodemode"] = json!(auto);
        }
        actual["errors"] = json!(loaded.errors);
        let actual = text(&actual).replace(&root, "$ROOT");
        assert_eq!(actual, text(&case["result"]));
    }
}

#[test]
fn mcp_config_reports_unparsable_files_with_their_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mcp.json"), "{ not json").unwrap();
    let loaded = load_config(dir.path(), dir.path(), false);
    assert!(loaded.servers.is_empty());
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].starts_with(&format!("{}: ", dir.path().join("mcp.json").display())));
}

#[test]
fn mcp_content_passes_text_and_images_through_and_replaces_other_blocks_with_text() {
    let result = json!({ "content": [
        { "type": "text", "text": "hello", "annotations": { "priority": 1 } },
        { "type": "image", "data": "aW1n", "mimeType": "image/png", "_meta": { "x": 1 } },
        { "type": "audio", "data": "YXVk", "mimeType": "audio/wav" },
        { "type": "resource_link", "uri": "file:///a.txt", "name": "a.txt" },
        { "type": "resource", "resource": { "uri": "file:///b.txt", "text": "inline" } },
        { "type": "resource", "resource": { "uri": "file:///c.png", "mimeType": "image/png", "blob": "Yw==" } },
        { "type": "resource", "resource": { "uri": "file:///d.bin", "blob": "ZA==" } },
        { "type": "video" },
    ]});
    assert_eq!(
        to_llm_content(&result),
        vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "image", "data": "aW1n", "mimeType": "image/png" }),
            json!({ "type": "text", "text": "[audio audio/wav omitted]" }),
            json!({ "type": "text", "text": "a.txt: file:///a.txt" }),
            json!({ "type": "text", "text": "inline" }),
            json!({ "type": "image", "data": "Yw==", "mimeType": "image/png" }),
            json!({ "type": "text", "text": "[binary resource file:///d.bin (unknown type) omitted]" }),
            json!({ "type": "text", "text": "[unsupported MCP content video]" }),
        ]
    );
}

#[test]
fn mcp_content_falls_back_to_structured_content_as_json_when_there_are_no_blocks() {
    assert_eq!(
        to_llm_content(&json!({ "content": [], "structuredContent": { "n": 1 } })),
        vec![json!({ "type": "text", "text": "{\n  \"n\": 1\n}" })]
    );
    assert_eq!(
        to_llm_content(&json!({ "content": [{ "type": "text", "text": "n=1" }], "structuredContent": { "n": 1 } })),
        vec![json!({ "type": "text", "text": "n=1" })]
    );
}
