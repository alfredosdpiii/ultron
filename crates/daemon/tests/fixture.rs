//! Replays `fixtures/daemon.json` (recorded by `fixtures/generate.mjs` from the TypeScript sources).

use chord_delta::{Encoder, Op};
use serde_json::Value;
use std::path::Path;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/daemon.json")).unwrap()
}

#[test]
fn daemon_diff_revisions_matches_chord() {
    for case in fixture()["diffs"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let ops = daemon::diff::diff_revisions(&case["before"], &case["after"]);
        let json: Vec<Value> = ops.iter().map(Op::to_json).collect();
        assert_eq!(Value::Array(json), case["ops"], "{name} ops");
        let wire = Encoder::default().encode(&ops);
        assert_eq!(Value::Array(wire), case["wire"], "{name} wire");
        let applied = chord_delta::apply(case["before"].clone(), ops).unwrap();
        assert_eq!(applied, case["after"], "{name} applies");
    }
}

#[test]
fn daemon_fallback_server_id_matches_typescript() {
    for case in fixture()["fallbacks"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let fallback = daemon::paths::fallback_server_id(id);
        assert_eq!(fallback, case["fallback"].as_str().unwrap(), "{id}");
        assert!(protocol::is_server_id(&fallback));
    }
}

#[test]
fn daemon_control_lines_match_encode_control_line() {
    for case in fixture()["control"].as_array().unwrap() {
        let line = daemon::control::encode_control_line(&case["message"]).unwrap();
        assert_eq!(line, case["line"].as_str().unwrap());
    }
}

#[test]
fn daemon_project_instance_directories_follow_the_naming_rule() {
    for case in fixture()["projects"].as_array().unwrap() {
        let root = case["root"].as_str().unwrap();
        let dir = daemon::instance::project_dir(Path::new("/home/u"), Path::new(root));
        assert_eq!(dir.display().to_string(), case["dir"].as_str().unwrap(), "{root}");
    }
}

#[test]
fn daemon_wire_encoder_resets_on_snapshot_and_interns_paths() {
    // The server encodes each subscription's snapshot then its updates with one encoder per state member.
    let mut encoder = daemon::chord::WireEncoder::default();
    let snapshot = serde_json::json!({"serviceId": "s", "mode": "singleton", "instances": [{"members": [
        {"name": "m", "kind": "method"}, {"name": "state", "kind": "state", "sequence": 1, "ops": [["r", {"a": {"b": 1}}]]}]}]});
    assert_eq!(encoder.encode_snapshot(&snapshot).unwrap(), snapshot);
    let update = |ops: Value| serde_json::json!({"type": "state", "member": "state", "sequence": 2, "ops": ops});
    let first = encoder
        .encode_update(&update(serde_json::json!([["s", ["a", "b"], 2]])))
        .unwrap();
    assert_eq!(first["ops"], serde_json::json!([["s", ["a", "b"], 2]]));
    let second = encoder
        .encode_update(&update(serde_json::json!([["s", ["a", "b"], 3]])))
        .unwrap();
    assert_eq!(second["ops"], serde_json::json!([["#", 0, ["a", "b"]], ["s", 0, 3]]));
}
