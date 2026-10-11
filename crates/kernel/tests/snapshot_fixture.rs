//! Replays crates/kernel/fixtures/snapshot.json, recorded by fixtures/generate-snapshot.mjs from the TypeScript
//! supervisor (kernel.ts) and snapshot-auth.ts: signing and verification over an input table, and scripted kernels
//! (snapshot, explicit and startup restore, refused snapshots) through the real runtime.py whose results, signed
//! files and protocol lines (both directions, captured by fixtures/tap.py) must be equal.

use kernel::{Kernel, KernelOptions, LimitOverrides, sign_snapshot, verify_snapshot};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/snapshot.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture JSON")
}

fn key(fixture: &Value, name: &str) -> Vec<u8> {
    let hex = fixture[name].as_str().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

#[test]
fn kernel_snapshot_signatures_match_typescript() {
    let fixture = fixture();
    let key = key(&fixture, "key");
    for case in fixture["sign"].as_array().unwrap() {
        let signed = sign_snapshot(&key, case["content"].as_str().unwrap().as_bytes());
        assert_eq!(String::from_utf8(signed).unwrap(), case["signed"], "{case}");
    }
    for case in fixture["verify"].as_array().unwrap() {
        let got = match verify_snapshot(&key, case["input"].as_str().unwrap().as_bytes()) {
            Ok(sha) => json!({"input": case["input"], "ok": sha}),
            Err(error) => json!({"input": case["input"], "error": error.0, "name": "SnapshotIntegrityError"}),
        };
        assert_eq!(&got, case);
    }
}

fn rlm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/coding-agent/src/ultron/rlm")
        .canonicalize()
        .unwrap()
}

fn is_uuid(text: &str) -> bool {
    text.char_indices().all(|(index, character)| match index {
        8 | 13 | 18 | 23 => character == '-',
        14 => character == '4',
        19 => matches!(character, '8' | '9' | 'a' | 'b'),
        _ => character.is_ascii_digit() || ('a'..='f').contains(&character),
    })
}

/// fixtures/generate-snapshot.mjs normalizeWire: host-made UUIDs become req-N in order of first use, the kernel pid
/// 0, the snapshot directory <dir> and the checkout path <root>.
fn normalize_wire(log: &str, dir: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let mut ids: Vec<String> = Vec::new();
    let (mut h2k, mut k2h) = (Vec::new(), Vec::new());
    for entry in log.lines().filter(|line| !line.is_empty()) {
        let entry: Value = serde_json::from_str(entry).unwrap();
        let line = entry["line"].as_str().unwrap();
        let mut out = String::new();
        let mut index = 0;
        while index < line.len() {
            if line.len() - index >= 36 && line.is_char_boundary(index + 36) && is_uuid(&line[index..index + 36]) {
                let id = line[index..index + 36].to_string();
                if !ids.contains(&id) {
                    ids.push(id.clone());
                }
                out.push_str(&format!(
                    "req-{}",
                    ids.iter().position(|known| *known == id).unwrap() + 1
                ));
                index += 36;
            } else {
                let character = line[index..].chars().next().unwrap();
                out.push(character);
                index += character.len_utf8();
            }
        }
        if let Some(start) = out.find("\"pid\":") {
            let digits = out[start + 6..].bytes().take_while(u8::is_ascii_digit).count();
            out.replace_range(start + 6..start + 6 + digits, "0");
        }
        let out = out.replace(dir, "<dir>").replace(&root, "<root>");
        match entry["dir"].as_str().unwrap() {
            "h2k" => h2k.push(Value::from(out)),
            _ => k2h.push(Value::from(out)),
        }
    }
    json!({"h2k": h2k, "k2h": k2h})
}

#[test]
fn kernel_snapshot_sessions_replay_typescript_results_files_and_protocol_lines() {
    let fixture = fixture();
    let signing_key = key(&fixture, "key");
    let other_key = key(&fixture, "otherKey");
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path().canonicalize().unwrap();
    let dir_text = dir.display().to_string();
    let file = |label: &str| dir.join(format!("{label}.snapshot"));
    let normalize =
        |value: Value| -> Value { serde_json::from_str(&value.to_string().replace(&dir_text, "<dir>")).unwrap() };
    let rlm = rlm_dir();
    for spec in fixture["kernels"].as_array().unwrap() {
        let name = spec["name"].as_str().unwrap();
        let log = dir.join(format!("{}.log", name.replace(' ', "-")));
        let mut options = KernelOptions::new(&dir, Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tap.py"));
        options.limits = LimitOverrides {
            max_memory_mb: Some(4096),
            max_cpu_seconds: Some(1800),
        };
        options.snapshot_key = Some(signing_key.clone());
        options.snapshot_path = spec["snapshotPath"].as_str().map(file);
        options.env = vec![
            (
                "ULTRON_TAP_RUNTIME".into(),
                rlm.join("runtime.py").display().to_string(),
            ),
            ("ULTRON_TAP_LOG".into(), log.display().to_string()),
            ("ULTRON_RLM_SPILL_DIR".into(), String::new()),
            ("ULTRON_RLM_OUTPUT_BYTES".into(), "20000".into()),
            ("PYTHONHASHSEED".into(), "0".into()),
        ];
        let kernel = Kernel::new(
            options,
            Arc::new(|_: &str, _: Map<String, Value>, _: &kernel::Signal| Ok(Value::Null)),
        )
        .unwrap();
        for expected in spec["steps"].as_array().unwrap() {
            let step = expected["step"].as_array().unwrap();
            let kind = step[0].as_str().unwrap();
            let arg = step[1].as_str().unwrap();
            match kind {
                "read" => {
                    let text = std::fs::read_to_string(file(arg)).unwrap();
                    assert_eq!(text, expected["file"].as_str().unwrap(), "{name}: signed file {arg}");
                    continue;
                }
                "copy" | "unsign" | "foreign" | "write" => {
                    let mut text = if kind == "write" {
                        step[2].as_str().unwrap().to_string()
                    } else {
                        std::fs::read_to_string(file(arg)).unwrap()
                    };
                    let target = if kind == "write" {
                        arg
                    } else {
                        step[2].as_str().unwrap()
                    };
                    let content = || text.split('\n').skip(1).collect::<Vec<_>>().join("\n");
                    match kind {
                        "copy" => {
                            for pair in step[3].as_array().unwrap() {
                                let (from, to) = (pair[0].as_str().unwrap(), pair[1].as_str().unwrap());
                                text = text.replacen(from, to, 1);
                            }
                        }
                        "unsign" => text = content(),
                        "foreign" => text = String::from_utf8(sign_snapshot(&other_key, content().as_bytes())).unwrap(),
                        _ => {}
                    }
                    std::fs::write(file(target), text).unwrap();
                    continue;
                }
                _ => {}
            }
            let outcome = match kind {
                "execute" => kernel.execute_cell(arg, None, None),
                "snapshot" => kernel.snapshot(Some(&file(arg))),
                _ => kernel.restore(Some(&file(arg))),
            };
            let got = match outcome {
                Ok(reply) => json!({"step": step, "ok": normalize(serde_json::to_value(&reply).unwrap())}),
                Err(error) => json!({"step": step, "error": normalize(Value::from(error.message()))}),
            };
            assert_eq!(&got, expected, "{name}");
        }
        kernel.shutdown();
        let wire = normalize_wire(&std::fs::read_to_string(&log).unwrap_or_default(), &dir_text);
        assert_eq!(wire, spec["wire"], "{name}: protocol lines");
    }
}
