//! The review engine's kernels start from the caller's session environment (`EngineOptions::base_env`): a review
//! kernel finds a fake tool only the session's PATH has, and not `sh`, which only this process's PATH has. The
//! kernel runs the real runtime.py from a copy of its directory whose `autoreview_api.run_file` (the review cell's
//! entry) is replaced by a probe that records what it finds. New in Rust: literal expectations.

use ai::{HttpRequest, HttpResponse, Model, Transport};
use review::engine::{EngineOptions, KernelEngine, ModelResolver, RetryPolicy, ReviewEngine};
use serde_json::{Value, json};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const TOOL: &str = "w17e-fake-tool";
const PROBE: &str = "import json, shutil\n\nasync def run_file(rlm, spec_path, result_path):\n    with open(result_path, 'w') as f:\n        json.dump({'fake': shutil.which('w17e-fake-tool'), 'sh': shutil.which('sh'), 'model': 'stub/frames'}, f)\n";

/// No frame runs in these reviews.
struct NoCalls;

impl Transport for NoCalls {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async { Err("no model call is expected".to_string()) })
    }
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "frames", "name": "frames", "api": "openai-completions", "provider": "stub", "baseUrl": "http://fake.invalid/v1",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 128000, "maxTokens": 8192,
    }))
    .unwrap()
}

/// runtime.py and its modules, copied, with the probe as `autoreview_api`.
fn probe_runtime(root: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm");
    let target = root.join("rlm");
    std::fs::create_dir(&target).unwrap();
    for entry in std::fs::read_dir(&source).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "py" || e == "json") {
            std::fs::copy(&path, target.join(path.file_name().unwrap())).unwrap();
        }
    }
    std::fs::write(target.join("autoreview_api.py"), PROBE).unwrap();
    target.join("runtime.py")
}

fn review(root: &Path, runtime: PathBuf, base_env: Option<Vec<(String, String)>>) -> Value {
    let resolve: ModelResolver = Arc::new(|reference: &str| Err(format!("Model \"{reference}\" not found")));
    let engine = KernelEngine::open(EngineOptions {
        dir: root.join("autoreview"),
        runtime_path: runtime,
        python: None,
        model: model(),
        api_key: Some("key".into()),
        resolve,
        transport: Arc::new(NoCalls),
        retry: RetryPolicy::default(),
        frame_env: Default::default(),
        settings: Default::default(),
        kernel_env: Vec::new(),
        base_env,
        log: None,
    })
    .unwrap();
    engine.review(&json!({"mode": "fast"}), None).unwrap()
}

#[test]
fn review_w17e_a_review_kernel_sees_the_session_path_not_the_process_path() {
    let process_path = std::env::var("PATH").unwrap_or_default();
    let process_sh = std::env::split_paths(&process_path)
        .map(|dir| dir.join("sh"))
        .find(|sh| sh.is_file())
        .unwrap_or_else(|| panic!("this process's PATH has no sh: {process_path}"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join(TOOL);
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runtime = probe_runtime(&root);

    let mut session = vec![("PATH".to_string(), bin.display().to_string())];
    if let Ok(python) = std::env::var("ULTRON_PYTHON") {
        session.push(("ULTRON_PYTHON".into(), python));
    }
    let seen = review(&root, runtime.clone(), Some(session));
    assert_eq!(seen["fake"], json!(fake.display().to_string()), "{seen}");
    assert_eq!(seen["sh"], Value::Null, "{seen}");
    assert_eq!(seen["model"], "stub/frames");

    // Without a session environment the kernel has this process's (the control: the probe sees sh there).
    let control = review(&root, runtime, None);
    assert_eq!(control["fake"], Value::Null, "{control}");
    assert_eq!(
        Path::new(control["sh"].as_str().unwrap()).file_name(),
        process_sh.file_name(),
        "{control}"
    );
}
