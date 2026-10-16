//! Prepared test environments (`autoreview/prepare.ts`: `prepareEnvironment`, `resolveToolchain`,
//! `cachedEnvironment`) replayed against `fixtures/prepare_env.json`, recorded from the TypeScript by
//! `fixtures/generate_prepare_env.mjs`: the same repository and toolchain trees, the same scripted runner (no real uv,
//! mise, npm, pnpm or yarn; no network), and the returned object, every runner call, the log lines, the
//! `toolchain.json` text, the environment directory's files and the cache lookup compared exactly. Plus the daemon
//! side: a review whose checkout has a lockfile gets the prepared environment as its test checkout.

#[path = "github_common/mod.rs"]
mod github_common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use regex::Regex;
use review::prepare::environment_hash;
use review::prepare_env::{PrepareOptions, RunnerPreparer, cached_environment, prepare_environment};
use review::runner::{RunOptions, RunResult, Runner};
use review::settings::Mise;
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/prepare_env.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("prepare_env.json")).expect("json")
}

fn build(base: &str, tree: &Value) {
    for entry in tree.as_array().expect("tree") {
        let path = Path::new(base).join(entry["path"].as_str().expect("path"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        if entry.get("dir").is_some() {
            std::fs::create_dir_all(&path).expect("dir");
        } else if let Some(target) = entry.get("link").and_then(Value::as_str) {
            std::os::unix::fs::symlink(target.replace("<root>", base), &path).expect("symlink");
        } else {
            std::fs::write(&path, entry["content"].as_str().expect("content")).expect("write");
        }
    }
}

/// The generator's scripted runner: the first rule whose `match` starts the normalized argv answers, its results in
/// order (the last repeating); calls are recorded normalized.
struct Scripted {
    base: String,
    rules: Vec<Value>,
    used: Mutex<HashMap<usize, usize>>,
    calls: Mutex<Vec<Value>>,
    hash: Mutex<String>,
}

impl Scripted {
    fn norm(&self, text: &str) -> String {
        let hash = self.hash.lock().unwrap().clone();
        text.replace(&self.base, "<root>").replace(&hash, "<hash>")
    }
}

impl Runner for Scripted {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        let joined = self.norm(&argv.join(" "));
        let env: serde_json::Map<String, Value> = options
            .env
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(self.norm(value))))
            .collect();
        self.calls.lock().unwrap().push(json!({
            "argv": argv.iter().map(|arg| self.norm(arg)).collect::<Vec<_>>(),
            "cwd": options.cwd.as_ref().map(|cwd| self.norm(cwd)),
            "env": env,
            "timeoutMs": options.timeout_ms,
        }));
        let Some(index) = self
            .rules
            .iter()
            .position(|rule| joined.starts_with(rule["match"].as_str().expect("match")))
        else {
            return RunResult {
                code: 127,
                stdout: String::new(),
                stderr: format!("{}: not scripted", argv[0]),
            };
        };
        let results = self.rules[index]["results"].as_array().expect("results");
        let count = {
            let mut used = self.used.lock().unwrap();
            let count = used.entry(index).or_insert(0);
            *count += 1;
            *count - 1
        };
        let result = &results[count.min(results.len() - 1)];
        for made in result.get("mkdirs").and_then(Value::as_array).into_iter().flatten() {
            std::fs::create_dir_all(made.as_str().expect("mkdir").replace("<root>", &self.base)).expect("mkdirs");
        }
        let text = |key: &str| result[key].as_str().expect("text").replace("<root>", &self.base);
        RunResult {
            code: result["code"].as_i64().expect("code") as i32,
            stdout: text("stdout"),
            stderr: text("stderr"),
        }
    }
}

fn zero(value: &Value, norm: &dyn Fn(&str) -> String) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("ms") {
            object.insert("ms".into(), json!(0));
        }
        if object.contains_key("at") {
            object.insert("at".into(), json!("<at>"));
        }
    }
    serde_json::from_str(&norm(&serde_json::to_string(&value).expect("json"))).expect("json")
}

fn files_of(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn review_prepare_env_scenarios_match_typescript() {
    let recorded = fixture();
    let ms = Regex::new(r#""ms": \d+"#).unwrap();
    let at = Regex::new(r#""at": "[^"]*""#).unwrap();
    for scenario in recorded["scenarios"].as_array().expect("scenarios") {
        let name = scenario["name"].as_str().expect("name");
        let temp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(temp.path())
            .expect("canonical")
            .to_string_lossy()
            .into_owned();
        build(&base, &scenario["tree"]);
        let repo = format!("{base}/repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let cache = format!("{base}/cache");
        let env: HashMap<String, String> = scenario["env"]
            .as_object()
            .expect("env")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("value").replace("<root>", &base)))
            .collect();
        let runner = Scripted {
            base: base.clone(),
            rules: scenario["rules"].as_array().expect("rules").clone(),
            used: Mutex::default(),
            calls: Mutex::default(),
            hash: Mutex::default(),
        };
        for (index, run) in scenario["runs"].as_array().expect("runs").iter().enumerate() {
            let label = format!("{name} run {index}");
            let options = &run["options"];
            let python = options.get("python").and_then(Value::as_str).map(String::from);
            let hash = environment_hash(Path::new(&repo), python.as_deref());
            *runner.hash.lock().unwrap() = hash.clone();
            let norm = |text: &str| text.replace(&base, "<root>").replace(&hash, "<hash>");
            let env_dir = format!("{cache}/envs/{hash}");
            for step in run["before"].as_array().expect("before") {
                std::fs::create_dir_all(&env_dir).expect("env dir");
                std::fs::write(
                    format!("{env_dir}/toolchain.json"),
                    step["record"].as_str().expect("record"),
                )
                .expect("record");
            }
            runner.calls.lock().unwrap().clear();
            let logs: Mutex<Vec<String>> = Mutex::default();
            let log = |line: &str| logs.lock().unwrap().push(norm(line));
            let now = || 1_790_000_000_000_i64;
            let mise = match options.get("mise") {
                Some(Value::Bool(flag)) => Some(Mise::Enabled(*flag)),
                Some(Value::String(path)) => Some(Mise::Path(path.replace("<root>", &base))),
                _ => None,
            };
            let prepare_options = PrepareOptions {
                runner: &runner,
                cache_dir: cache.clone(),
                python: python.clone(),
                mise,
                env: env.clone(),
                home: format!("{base}/fakehome"),
                log: &log,
                timeout_ms: options.get("timeoutMs").and_then(Value::as_u64),
                force: options.get("force").and_then(Value::as_bool).unwrap_or(false),
                now: &now,
            };
            let outcome = prepare_environment(&repo, &prepare_options);
            let (result, error) = match outcome {
                Ok(prepared) => (zero(&prepared.json, &norm), Value::Null),
                Err(error) => (Value::Null, json!(error)),
            };
            assert_eq!(result, run["result"], "{label}: result");
            assert_eq!(error, run["error"], "{label}: error");
            assert_eq!(
                Value::Array(runner.calls.lock().unwrap().clone()),
                run["calls"],
                "{label}: calls"
            );
            assert_eq!(json!(*logs.lock().unwrap()), run["logs"], "{label}: logs");
            let record = std::fs::read_to_string(format!("{env_dir}/toolchain.json"))
                .ok()
                .map(|text| {
                    let text = norm(&text);
                    let text = ms.replace(&text, r#""ms": 0"#).into_owned();
                    at.replace(&text, r#""at": "<at>""#).into_owned()
                });
            assert_eq!(json!(record), run["record"], "{label}: toolchain.json");
            assert_eq!(json!(files_of(&env_dir)), run["files"], "{label}: files");
            let cached = cached_environment(&repo, &cache, python.as_deref())
                .map_or(Value::Null, |prepared| zero(&prepared.json, &norm));
            assert_eq!(cached, run["cached"], "{label}: cachedEnvironment");
        }
    }
}

/// The typed view the reviewer gets matches the record: the toolchain as its install paths.
#[test]
fn review_prepare_env_summary_is_what_the_reviewer_binds() {
    let temp = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(temp.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let repo = format!("{base}/repo");
    std::fs::create_dir_all(format!("{base}/opt/py/bin")).unwrap();
    std::fs::create_dir_all(format!("{base}/opt/py/lib")).unwrap();
    std::fs::write(format!("{base}/opt/py/bin/python3"), "").unwrap();
    std::fs::create_dir_all(format!("{base}/bin")).unwrap();
    std::fs::write(format!("{base}/bin/uv"), "").unwrap();
    std::os::unix::fs::symlink(format!("{base}/opt/py/bin/python3"), format!("{base}/bin/python3")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(format!("{repo}/requirements.txt"), "a\n").unwrap();
    let runner = Arc::new(Scripted {
        base: base.clone(),
        rules: vec![json!({"match": "", "results": [{"code": 0, "stdout": "", "stderr": ""}]})],
        used: Mutex::default(),
        calls: Mutex::default(),
        hash: Mutex::default(),
    });
    let preparer = RunnerPreparer {
        runner: runner.clone(),
        cache_dir: format!("{base}/cache"),
        mise: Some(Mise::Enabled(false)),
        env: HashMap::from([("PATH".to_string(), format!("{base}/bin"))]),
        home: format!("{base}/home"),
        now: Arc::new(|| 0),
    };
    let lines: Mutex<Vec<String>> = Mutex::default();
    let summary = review::reviewer::EnvironmentPreparer::prepare(&preparer, &repo, &|line| {
        lines.lock().unwrap().push(line.into())
    })
    .expect("prepared");
    let hash = environment_hash(Path::new(&repo), None);
    assert_eq!(
        summary,
        review::reviewer::PreparedEnvironment {
            hash: hash.clone(),
            cached: false,
            dir: format!("{base}/cache/envs/{hash}"),
            prepared: vec!["python: uv pip install -r requirements.txt".into()],
            failures: vec![],
            toolchain: vec![format!("{base}/opt/py")],
        }
    );
    // The second time it is a cache hit: nothing runs.
    let calls = runner.calls.lock().unwrap().len();
    let again = review::reviewer::EnvironmentPreparer::prepare(&preparer, &repo, &|_| {}).expect("cached");
    assert!(again.cached);
    assert_eq!(runner.calls.lock().unwrap().len(), calls);
}

/// The daemon builds the test environment of a pull request's checkout before the review (prepareEnvs, tests
/// allowed): the engine is told to test in the prepared directory with the toolchain bound; with prepareEnvs off,
/// nothing is built.
#[test]
fn review_prepare_env_daemon_review_uses_the_prepared_environment() {
    use github_common::{BOT, FakePull, setup};
    let tools = tempfile::tempdir().expect("tools");
    let tools_dir = std::fs::canonicalize(tools.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(format!("{tools_dir}/bin")).unwrap();
    std::fs::write(format!("{tools_dir}/bin/uv"), "").unwrap();
    std::os::unix::fs::symlink("/usr/bin/env", format!("{tools_dir}/bin/python3")).unwrap();
    for (prepare_envs, expect_prepared) in [(true, true), (false, false)] {
        let mut context = setup(json!({ "prepareEnvs": prepare_envs }), None);
        context.hub.lock().can_push = true;
        context.hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
        // The fake git's worktree gets a requirements file; uv answers through the same runner.
        let hub = context.hub.clone();
        let builds: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let seen = builds.clone();
        let runner: Arc<dyn Runner> = Arc::new(WithTools {
            hub: hub.clone(),
            builds: seen,
        });
        context.deps.environments = Some(Arc::new(RunnerPreparer {
            runner,
            cache_dir: context.deps.paths.cache.clone(),
            mise: Some(Mise::Enabled(false)),
            env: HashMap::from([("PATH".to_string(), format!("{tools_dir}/bin"))]),
            home: format!("{tools_dir}/home"),
            now: Arc::new(|| 0),
        }));
        let runner: Arc<dyn Runner> = Arc::new(WithTools {
            hub: hub.clone(),
            builds: Arc::default(),
        });
        context.deps.runner = runner.clone();
        context.deps.checkouts = Arc::new(review::checkout::CheckoutManager::new(
            runner,
            &context.deps.paths.cache,
        ));
        let candidate = context.requested();
        let outcome = review::reviewer::review_pull(&context.deps, &candidate, Default::default());
        assert!(outcome.is_ok(), "{:?}", outcome.err());
        let spec = context.engine.specs()[0].clone();
        assert_eq!(spec["runTests"], json!(true));
        if expect_prepared {
            let dir = spec["testCheckout"].as_str().expect("testCheckout").to_string();
            assert!(dir.starts_with(&format!("{}/envs/", context.deps.paths.cache)), "{dir}");
            assert_eq!(spec["testEnvKind"], json!("prepared"));
            assert_eq!(spec["testToolchain"], json!(["/usr"]));
            assert!(Path::new(&format!("{dir}/toolchain.json")).exists());
            let logs = context.logs();
            assert!(
                logs.contains(": environment ") && logs.contains("python: uv pip install -r requirements.txt"),
                "{logs}"
            );
            let builds = builds.lock().unwrap();
            assert_eq!(builds.len(), 2, "{builds:?}");
            assert_eq!(builds[0][1], "venv");
        } else {
            assert!(spec.get("testCheckout").is_none(), "{spec}");
            assert!(spec.get("testToolchain").is_none());
            assert!(builds.lock().unwrap().is_empty());
        }
    }
}

/// The fake hub, a `requirements.txt` in every worktree it adds, and uv answering success.
struct WithTools {
    hub: Arc<github_common::FakeHub>,
    builds: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Runner for WithTools {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        if argv[0].ends_with("/uv") {
            self.builds.lock().unwrap().push(argv.to_vec());
            return RunResult {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
        let result = self.hub.run(argv, options);
        if argv.iter().any(|arg| arg == "worktree") && argv.iter().any(|arg| arg == "add") {
            let dir = &argv[argv.len() - 2];
            std::fs::write(Path::new(dir).join("requirements.txt"), "a\n").expect("requirements");
        }
        result
    }
}
