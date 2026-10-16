//! `ultron autoreview prepare <dir> [--python 3.x] [--json]`, `autoreview review --repo-dir ... --prepare` and the
//! checkout lent by `--repo`/`--checkout-roots`, through `autoreview_cli::run_command_with`, replayed against
//! `fixtures/autoreview_prepare.json` (recorded from the TypeScript `runAutoreviewCommand` by
//! `fixtures/autoreview_prepare.mjs`): the same trees, the same scripted runner instead of uv and git, a fake engine
//! recording its spec; exit codes, stdout, stderr, runner calls and engine specs compared exactly. Plus the doctor's
//! prepared-environment lines. No network, no real uv, git or model.

#[path = "../../review/tests/github_common/mod.rs"]
mod github_common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cli::autoreview_cli::{Overrides, doctor_prepared_lines, run_command_with};
use github_common::FakeEngine;
use regex::Regex;
use review::runner::{RunOptions, RunResult, Runner};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/autoreview_prepare.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn env_for(base: &str) -> cli::Env {
    let base = base.to_string();
    let home = PathBuf::from(format!("{base}/fakehome"));
    let home_text = home.display().to_string();
    cli::Env {
        cwd: PathBuf::from(&base),
        home,
        var: Arc::new(move |key| match key {
            "PATH" => Some(format!("{base}/bin:{base}/sys")),
            "ULTRON_CODING_AGENT_DIR" => Some(format!("{base}/agent")),
            "ULTRON_AUTOREVIEW_CACHE_DIR" => Some(format!("{base}/cache")),
            "ULTRON_HINDSIGHT_URL" => Some("off".into()),
            "HOME" => Some(home_text.clone()),
            _ => None,
        }),
        docs: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
    }
}

struct Scripted {
    rules: Vec<Value>,
    used: Mutex<HashMap<usize, usize>>,
    calls: Mutex<Vec<Vec<String>>>,
    norm: Box<dyn Fn(&str) -> String + Send + Sync>,
}

impl Runner for Scripted {
    fn run(&self, argv: &[String], _options: &RunOptions) -> RunResult {
        let joined = (self.norm)(&argv.join(" "));
        self.calls
            .lock()
            .unwrap()
            .push(argv.iter().map(|arg| (self.norm)(arg)).collect());
        let Some(index) = self
            .rules
            .iter()
            .position(|rule| joined.starts_with(rule["match"].as_str().unwrap()))
        else {
            return RunResult {
                code: 127,
                stdout: String::new(),
                stderr: format!("{}: not scripted", argv[0]),
            };
        };
        let count = {
            let mut used = self.used.lock().unwrap();
            let count = used.entry(index).or_insert(0);
            *count += 1;
            *count - 1
        };
        let results = self.rules[index]["results"].as_array().unwrap();
        let result = &results[count.min(results.len() - 1)];
        RunResult {
            code: result["code"].as_i64().unwrap() as i32,
            stdout: result["stdout"].as_str().unwrap().into(),
            stderr: result["stderr"].as_str().unwrap().into(),
        }
    }
}

fn engine_result() -> Value {
    json!({
        "complete": true, "label": "local", "files": 1, "added": 1, "removed": 1, "findings": [], "alsoRaised": [],
        "earlier": [], "dropped": { "rejected": 0, "duplicates": 0 },
        "timing": { "totalMs": 42000, "scopeMs": 100, "findMs": 30000, "verifyMs": 11000 },
        "usage": { "inputTokens": 900, "outputTokens": 40, "costUsd": 0.01, "frames": 2, "tokens": 940, "budget": 300000 },
        "model": "stub/frames", "verifyModel": "stub/frames", "notChecked": [], "incomplete": [], "diffLines": {},
    })
}

fn build(base: &str) {
    std::fs::create_dir_all(format!("{base}/repo")).unwrap();
    std::fs::write(format!("{base}/repo/requirements.txt"), "a\n").unwrap();
    std::fs::create_dir_all(format!("{base}/bin")).unwrap();
    std::fs::write(format!("{base}/bin/uv"), "").unwrap();
    std::fs::create_dir_all(format!("{base}/sys")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", format!("{base}/sys/python3")).unwrap();
    std::fs::create_dir_all(format!("{base}/agent")).unwrap();
    std::fs::write(
        format!("{base}/agent/settings.json"),
        json!({"autoreview": {"mise": false}}).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(format!("{base}/checkouts/r/.git")).unwrap();
    std::fs::create_dir_all(format!("{base}/fakehome")).unwrap();
}

#[test]
fn cli_autoreview_prepare_cases_match_typescript() {
    let ms = Regex::new(r#""ms":\d+"#).unwrap();
    let at = Regex::new(r#""at":"[^"]*""#).unwrap();
    let seconds = Regex::new(r"(?m)^  \d+ s$").unwrap();
    for case in fixture()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap().display().to_string();
        build(&base);
        let hashes: Vec<String> = [None, Some("3.11"), Some("3.12")]
            .into_iter()
            .map(|python| review::prepare::environment_hash(Path::new(&format!("{base}/repo")), python))
            .collect();
        let norm_base = base.clone();
        let norm = move |text: &str| {
            hashes.iter().fold(text.replace(&norm_base, "<root>"), |out, hash| {
                out.replace(hash, "<hash>")
            })
        };
        let runner = Arc::new(Scripted {
            rules: case["rules"].as_array().unwrap().clone(),
            used: Mutex::default(),
            calls: Mutex::default(),
            norm: Box::new(norm.clone()),
        });
        let engine = FakeEngine::new(vec![Ok(engine_result())]);
        let overrides = Overrides {
            runner: Some(runner.clone()),
            engine: Some(engine.clone()),
        };
        let env = env_for(&base);
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().replace("<root>", &base))
            .collect();
        for (round, run) in case["runs"].as_array().unwrap().iter().enumerate() {
            let label = format!("{name} round {round}");
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run_command_with(&args, &env, &overrides, &mut out, &mut err);
            let stdout = norm(&String::from_utf8(out).unwrap());
            let stdout = ms.replace_all(&stdout, r#""ms":0"#).into_owned();
            let stdout = at.replace_all(&stdout, r#""at":"<at>""#).into_owned();
            let stdout = seconds.replace(&stdout, "  0 s").into_owned();
            let stderr = norm(&String::from_utf8(err).unwrap());
            assert_eq!(json!(code), run["code"], "{label}: code; stderr {stderr}");
            assert_eq!(json!(stderr), run["stderr"], "{label}: stderr");
            assert_eq!(json!(stdout), run["stdout"], "{label}: stdout");
        }
        assert_eq!(json!(*runner.calls.lock().unwrap()), case["calls"], "{name}: calls");
        let specs: Vec<Value> = engine
            .specs()
            .into_iter()
            .map(|mut spec| {
                if let Some(spec) = spec.as_object_mut() {
                    spec.shift_remove("diff");
                }
                serde_json::from_str(&norm(&spec.to_string())).unwrap()
            })
            .collect();
        let recorded = case["specs"].as_array().unwrap();
        assert_eq!(specs.len(), recorded.len(), "{name}: specs");
        for (got, want) in specs.iter().zip(recorded) {
            for key in [
                "testCheckout",
                "testToolchain",
                "testEnvKind",
                "testEnv",
                "repoDir",
                "runTests",
                "repo",
            ] {
                assert_eq!(got.get(key), want.get(key), "{name}: spec {key}");
            }
            assert_eq!(got, want, "{name}: spec");
        }
    }
}

/// The doctor's lines about the prepared environment (cli.ts `preparedLines`): none without `--repo`; a hint when the
/// lent checkout has none; else its directory, what was prepared and the toolchain.
#[test]
fn cli_autoreview_doctor_prepared_lines() {
    assert!(doctor_prepared_lines(false, None).is_empty());
    assert_eq!(
        doctor_prepared_lines(true, None),
        vec!["Prepared environment: none (run `ultron autoreview prepare <repo-dir>`)"]
    );
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap().display().to_string();
    let repo = format!("{base}/repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(format!("{repo}/requirements.txt"), "a\n").unwrap();
    let hash = review::prepare::environment_hash(Path::new(&repo), None);
    let dir = format!("{base}/cache/envs/{hash}");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        format!("{dir}/toolchain.json"),
        json!({
            "dir": "/recorded/dir", "hash": hash, "prepared": ["python: uv pip install -r requirements.txt"],
            "cached": false, "failures": [], "ms": 3, "ok": true,
            "toolchain": [{"tool": "python", "version": "", "path": "/usr", "source": "system"},
                          {"tool": "node", "version": "20.1.0", "path": "/m/node", "source": "mise", "installed": true}],
        })
        .to_string(),
    )
    .unwrap();
    let cached = review::prepare_env::cached_environment(&repo, &format!("{base}/cache"), None).expect("cached");
    assert_eq!(cached.json["cached"], json!(true));
    assert_eq!(
        doctor_prepared_lines(true, Some(&cached)),
        vec![
            "Prepared environment: /recorded/dir",
            "  python: uv pip install -r requirements.txt",
            "  toolchain: python (version not pinned) from system: /usr",
            "  toolchain: node 20.1.0 from mise (installed by prepare): /m/node",
        ]
    );
    // A record that says it failed is not a prepared environment.
    std::fs::write(format!("{dir}/toolchain.json"), r#"{"ok": false, "dir": "/x"}"#).unwrap();
    assert!(review::prepare_env::cached_environment(&repo, &format!("{base}/cache"), None).is_none());
}
