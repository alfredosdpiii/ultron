//! Replays crates/kernel/fixtures/session.json, recorded from the TypeScript supervisor (kernel.ts) by
//! fixtures/generate.mjs: configuration functions over input tables, and one scripted session through the real
//! runtime.py whose results, errors and protocol lines (both directions, captured by fixtures/tap.py) must be equal.

use kernel::{
    EchoHandler, Kernel, KernelOptions, LimitOverrides, kernel_environment, kernel_resource_limits, rlm_output_budget,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/session.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture JSON")
}

fn rlm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/coding-agent/src/ultron/rlm")
        .canonicalize()
        .unwrap()
}

#[test]
fn kernel_environment_filters_credentials_like_typescript() {
    for case in fixture()["environment"].as_array().unwrap() {
        let env: Vec<(String, String)> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_string()))
            .collect();
        let kept: serde_json::Map<String, Value> = kernel_environment(env)
            .into_iter()
            .map(|(name, value)| (name, Value::from(value)))
            .collect();
        assert_eq!(Value::Object(kept), case["result"], "{}", case["env"]);
    }
}

#[test]
fn kernel_resource_limits_match_typescript() {
    for case in fixture()["limits"].as_array().unwrap() {
        let env = case["env"].as_object().unwrap().clone();
        let overrides = LimitOverrides {
            max_memory_mb: case["overrides"]["maxMemoryMb"].as_u64(),
            max_cpu_seconds: case["overrides"]["maxCpuSeconds"].as_u64(),
        };
        let got = kernel_resource_limits(
            |name| env.get(name).and_then(Value::as_str).map(String::from),
            overrides,
        );
        let got = match got {
            Ok(limits) => json!({"ok": {"maxMemoryMb": limits.max_memory_mb, "maxCpuSeconds": limits.max_cpu_seconds}}),
            Err(error) => json!({"error": error.message()}),
        };
        let mut expected = case.clone();
        let expected = expected.as_object_mut().unwrap();
        expected.remove("env");
        expected.remove("overrides");
        assert_eq!(got, Value::Object(expected.clone()), "{case}");
    }
}

#[test]
fn kernel_output_budget_reads_the_environment_like_typescript() {
    for case in fixture()["budget"].as_array().unwrap() {
        assert_eq!(
            rlm_output_budget(case["value"].as_str()),
            case["result"].as_u64().unwrap(),
            "{case}"
        );
    }
}

/// Host-made UUIDs become req-N in order of first use, the kernel pid 0, the checkout path <root>
/// (fixtures/generate.mjs normalizeWire).
fn normalize_wire(log: &str) -> Value {
    let mut ids: Vec<String> = Vec::new();
    let (mut h2k, mut k2h) = (Vec::new(), Vec::new());
    for entry in log.lines().filter(|line| !line.is_empty()) {
        let entry: Value = serde_json::from_str(entry).unwrap();
        let mut line = entry["line"].as_str().unwrap().to_string();
        let mut out = String::new();
        while !line.is_empty() {
            let is_uuid = line.len() >= 36 && line.is_char_boundary(36) && is_uuid(&line[..36]);
            if is_uuid {
                let id = line[..36].to_string();
                if !ids.contains(&id) {
                    ids.push(id.clone());
                }
                out.push_str(&format!(
                    "req-{}",
                    ids.iter().position(|known| *known == id).unwrap() + 1
                ));
                line.drain(..36);
            } else {
                let character = line.remove(0);
                out.push(character);
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = root.canonicalize().unwrap().display().to_string();
        out = out.replace(&root, "<root>");
        if let Some(start) = out.find("\"pid\":") {
            let digits = out[start + 6..].bytes().take_while(u8::is_ascii_digit).count();
            out.replace_range(start + 6..start + 6 + digits, "0");
        }
        match entry["dir"].as_str().unwrap() {
            "h2k" => h2k.push(Value::from(out)),
            _ => k2h.push(Value::from(out)),
        }
    }
    json!({"h2k": h2k, "k2h": k2h})
}

fn is_uuid(text: &str) -> bool {
    text.char_indices().all(|(index, character)| match index {
        8 | 13 | 18 | 23 => character == '-',
        14 => character == '4',
        19 => matches!(character, '8' | '9' | 'a' | 'b'),
        _ => character.is_ascii_digit() || ('a'..='f').contains(&character),
    })
}

#[test]
fn kernel_session_replays_typescript_results_and_protocol_lines() {
    let fixture = fixture();
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let log = cwd.join("protocol.log");
    let rlm = rlm_dir();
    let mut options = KernelOptions::new(&cwd, Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tap.py"));
    options.limits = LimitOverrides {
        max_memory_mb: Some(4096),
        max_cpu_seconds: Some(1800),
    };
    options.env = vec![
        (
            "ULTRON_TAP_RUNTIME".into(),
            rlm.join("runtime.py").display().to_string(),
        ),
        ("ULTRON_TAP_LOG".into(), log.display().to_string()),
        ("ULTRON_RLM_SPILL_DIR".into(), String::new()),
        ("ULTRON_RLM_OUTPUT_BYTES".into(), "20000".into()),
    ];
    let kernel = Kernel::new(options, Arc::new(EchoHandler)).unwrap();
    let normalize = |text: &str| {
        text.replace(&rlm.display().to_string(), "<rlm>")
            .replace(&cwd.display().to_string(), "<cwd>")
    };
    for step in fixture["session"]["steps"].as_array().unwrap() {
        let outcome = if step["reset"] == true {
            kernel.reset_scratch()
        } else if let Some(comment) = step["comment"].as_u64() {
            kernel.execute(&format!("#{}", "x".repeat(comment as usize)), None, None)
        } else {
            kernel.execute(step["code"].as_str().unwrap(), None, None)
        };
        let got = match outcome {
            Ok(result) => {
                let text = serde_json::to_string(&result).unwrap();
                json!({"ok": serde_json::from_str::<Value>(&normalize(&text)).unwrap()})
            }
            Err(error) => json!({"error": normalize(error.message())}),
        };
        let expected = step
            .get("ok")
            .map_or_else(|| json!({"error": step["error"]}), |ok| json!({"ok": ok}));
        assert_eq!(got, expected, "step {}", step["code"]);
    }
    kernel.shutdown();
    let wire = normalize_wire(&std::fs::read_to_string(&log).unwrap());
    assert_eq!(wire["h2k"], fixture["session"]["wire"]["h2k"]);
    assert_eq!(wire["k2h"], fixture["session"]["wire"]["k2h"]);
}
