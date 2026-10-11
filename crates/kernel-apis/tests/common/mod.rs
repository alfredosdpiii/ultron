//! Shared by the fixture replays: runs a recorded scenario's cells through crates/kernel with a `KernelApis`
//! handler and normalizes the outcome exactly as fixtures/generate.mjs does.
#![allow(dead_code)]

use fancy_regex::Regex;
use kernel::{HostHandler, Kernel, KernelOptions, Signal};
use kernel_apis::KernelApis;
use kernel_apis::util::{Clock, stringify};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

pub const FIXED_NOW: i64 = 1_760_000_000_000;

pub fn fixture(area: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/{area}.json"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

/// generate.mjs `normalize`, applied to the JSON text.
pub fn normalize(text: &str, dir: &str) -> String {
    let mut out = text.replace(dir, "<dir>");
    for (pattern, replacement) in [
        (
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            "<uuid>",
        ),
        (r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z", "<time>"),
        (r#"(durationMs\\*["']):\s?\d+"#, "$1: 0"),
        (r#"[^\s"'\\]*ultron-skill-test-[A-Za-z0-9_]+"#, "<skilltest>"),
    ] {
        out = Regex::new(pattern).unwrap().replace_all(&out, replacement).into_owned();
    }
    out
}

/// Files under `dir` (relative path → content), skipping lock directories.
pub fn tree(dir: &Path) -> Value {
    fn walk(root: &Path, path: &Path, files: &mut Map<String, Value>) {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
        names.sort();
        for name in names {
            if name == ".lock" {
                continue;
            }
            let full = path.join(&name);
            if full.is_dir() {
                walk(root, &full, files);
            } else {
                let relative = full.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                files.insert(relative, std::fs::read_to_string(&full).unwrap().into());
            }
        }
    }
    let mut files = Map::new();
    walk(dir, dir, &mut files);
    Value::Object(files)
}

/// The fixture host's other module: `test.clock` advances the grants clock.
pub fn inner(clock: Arc<AtomicI64>) -> Arc<dyn HostHandler> {
    Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
        if kind == "test.clock" {
            let ms = payload["ms"].as_i64().unwrap();
            return Ok(json!(clock.fetch_add(ms, Ordering::SeqCst) + ms));
        }
        Err(format!("Ultron RLM host request is not wired: {kind}"))
    })
}

pub struct Made {
    pub handler: KernelApis,
    pub env: Vec<(String, String)>,
    pub state: Box<dyn Fn() -> Value>,
}

pub fn run_scenario(scenario: &Value, make: &dyn Fn(&Path) -> Made) -> Value {
    let temp = tempfile::Builder::new()
        .prefix("ultron-kernel-apis-")
        .tempdir()
        .unwrap();
    let dir = temp.path().canonicalize().unwrap();
    let made = make(&dir);
    let mut options = KernelOptions::new(&dir, runtime());
    options.env.push(("ULTRON_RLM_SPILL_DIR".into(), String::new()));
    options.env.push(("TMPDIR".into(), dir.to_string_lossy().into_owned()));
    options.env.extend(made.env.clone());
    let kernel = Kernel::new(options, Arc::new(made.handler)).unwrap();
    let mut cells = Vec::new();
    for cell in scenario["cells"].as_array().unwrap() {
        let code = cell["code"].as_str().unwrap();
        let result = kernel.execute(code, None, None).unwrap();
        let error = result.error.map(|error| format!("{}: {}", error.ename, error.evalue));
        cells.push(json!({"code": code, "status": result.status, "stdout": result.stdout, "result": result.result, "error": error}));
    }
    kernel.shutdown();
    let recorded = json!({"name": scenario["name"], "cells": cells, "state": (made.state)()});
    serde_json::from_str(&normalize(&stringify(&recorded), &dir.to_string_lossy())).unwrap()
}

pub fn assert_area(area: &str, make: &dyn Fn(&Path) -> Made) {
    let fixture = fixture(area);
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let actual = run_scenario(scenario, make);
        let (want, got) = (&scenario["cells"], &actual["cells"]);
        for (index, expected) in want.as_array().unwrap().iter().enumerate() {
            assert_eq!(got[index], *expected, "{area} / {} / cell {index}", scenario["name"]);
        }
        let (want, got) = (&scenario["state"], &actual["state"]);
        if let (Some(want), Some(got)) = (want.as_object(), got.as_object()) {
            for (key, expected) in want {
                if let (Some(want), Some(got)) = (expected.as_object(), got[key].as_object()) {
                    for (file, content) in want {
                        assert_eq!(
                            got.get(file),
                            Some(content),
                            "{area} / {} / state {key}.{file}",
                            scenario["name"]
                        );
                    }
                }
                assert_eq!(got[key], *expected, "{area} / {} / state {key}", scenario["name"]);
            }
        }
        assert_eq!(actual, *scenario, "{area} / {}", scenario["name"]);
    }
}

pub fn fixed(now: i64) -> Clock {
    Arc::new(move || now)
}
