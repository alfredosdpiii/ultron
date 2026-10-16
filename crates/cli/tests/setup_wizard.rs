//! Ported from packages/coding-agent/test/ultron-setup-{core,wizard,start-model}.test.ts: the pure parts of
//! `ultron setup` and the wizard flow over a scripted UI, with the TypeScript tests' literal expectations.
//! Fakes only: a scripted fetch, probe, live test and docker; private temp profiles.

use cli::setup_core::{self, CustomEndpoint, FetchResponse, HindsightDockerOptions, SetupOfferInput, UrlSource};
use cli::setup_wizard::{
    Choice, InputOptions, LoginOutcome, ModelTestResult, SetupDeps, SetupQuit, SetupUi, Tone, run_setup_wizard,
};
use interactive::selectors::AuthProvider;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ------------------------------------------------------------------------------------------------------- core

fn endpoint() -> CustomEndpoint {
    CustomEndpoint {
        provider_id: "proxy".into(),
        base_url: "http://127.0.0.1:8317/v1/".into(),
        api_key: Some("sk-secret".into()),
        api: "openai-completions".into(),
        model_ids: vec!["kimi-k3".into(), "grok-4.5".into()],
    }
}

#[test]
fn cli_setup_core_custom_endpoint_merges_into_models_json_like_typescript() {
    let (text, replaced) = setup_core::merge_custom_endpoint(None, &endpoint()).unwrap();
    assert!(!replaced);
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"providers": {"proxy": {"baseUrl": "http://127.0.0.1:8317/v1", "api": "openai-completions",
            "apiKey": "sk-secret", "models": [{"id": "kimi-k3"}, {"id": "grok-4.5"}]}}})
    );
    let existing = r#"// comments are allowed in models.json
{
  "providers": {
    "ollama": { "baseUrl": "http://localhost:11434/v1", "api": "openai-completions", "apiKey": "ollama", "models": [{ "id": "qwen" }] },
    "proxy": { "baseUrl": "http://old", "apiKey": "old", "models": [] }
  }
}"#;
    let (text, replaced) = setup_core::merge_custom_endpoint(Some(existing), &endpoint()).unwrap();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert!(replaced);
    assert_eq!(parsed["providers"]["ollama"]["models"], json!([{"id": "qwen"}]));
    assert_eq!(parsed["providers"]["proxy"]["baseUrl"], "http://127.0.0.1:8317/v1");
    let mut blank = endpoint();
    blank.api_key = Some("  ".into());
    let (text, _) = setup_core::merge_custom_endpoint(None, &blank).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["providers"]["proxy"]["apiKey"],
        "none"
    );
    assert!(
        setup_core::merge_custom_endpoint(Some("{ not json"), &endpoint())
            .unwrap_err()
            .contains("not valid JSON")
    );
    assert!(
        setup_core::merge_custom_endpoint(Some("[]"), &endpoint())
            .unwrap_err()
            .contains("JSON object")
    );
}

#[test]
fn cli_setup_core_validates_names_urls_and_model_lists() {
    assert_eq!(setup_core::validate_provider_id("my-proxy"), None);
    assert!(setup_core::validate_provider_id("My Proxy").is_some());
    assert_eq!(setup_core::validate_base_url("http://localhost:11434/v1"), None);
    assert_eq!(
        setup_core::validate_base_url("ftp://x").as_deref(),
        Some("The URL must start with http:// or https://")
    );
    assert_eq!(
        setup_core::validate_base_url("nope").as_deref(),
        Some("Not a valid URL (e.g. http://localhost:11434/v1)")
    );
    assert_eq!(
        setup_core::validate_base_url("http://a/v1http://b/v1").as_deref(),
        Some("That looks like two URLs run together; enter one")
    );
    assert_eq!(setup_core::parse_model_ids(" a, b\nc a "), ["a", "b", "c"]);
}

fn fetch_routes(routes: Vec<(&'static str, u16, &'static str)>, log: Arc<Mutex<Vec<String>>>) -> setup_core::Fetch {
    Arc::new(move |request: &setup_core::FetchRequest| {
        let mut entry = request.url.clone();
        for (k, v) in &request.headers {
            entry.push_str(&format!(" {k}={v}"));
        }
        log.lock().unwrap().push(entry);
        match routes.iter().find(|(route, _, _)| request.url.ends_with(route)) {
            Some((_, status, body)) => Ok(FetchResponse {
                status: *status,
                body: body.to_string(),
            }),
            None => Err("connection refused".into()),
        }
    })
}

#[test]
fn cli_setup_core_lists_endpoint_models_without_the_key_in_the_url() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let fetch = fetch_routes(
        vec![(
            "/v1/models",
            200,
            r#"{"data": [{"id": "b"}, {"id": "a"}, {"nope": 1}]}"#,
        )],
        log.clone(),
    );
    assert_eq!(
        setup_core::list_endpoint_models("http://proxy/v1/", Some("k"), &fetch),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(log.lock().unwrap()[0], "http://proxy/v1/models authorization=Bearer k");
    let failing = fetch_routes(vec![("/models", 404, "no")], log);
    assert_eq!(
        setup_core::list_endpoint_models("http://proxy/v1", None, &failing),
        None
    );
}

#[test]
fn cli_setup_core_hindsight_docker_args_pass_the_key_by_name_only() {
    let args = setup_core::hindsight_docker_args(&HindsightDockerOptions {
        port: 8888,
        provider: "openai".into(),
        model: Some("gpt-5-mini".into()),
        base_url: None,
        has_api_key: true,
    });
    assert_eq!(
        args,
        [
            "run",
            "-d",
            "--name",
            "hindsight",
            "--restart",
            "unless-stopped",
            "-p",
            "8888:8888",
            "-e",
            "HINDSIGHT_API_LLM_PROVIDER=openai",
            "-e",
            "HINDSIGHT_API_LLM_API_KEY",
            "-e",
            "HINDSIGHT_API_LLM_MODEL=gpt-5-mini",
            "-v",
            "hindsight-data:/home/hindsight/.pg0",
            "ghcr.io/vectorize-io/hindsight:latest",
        ]
    );
    assert!(!setup_core::format_command("docker", &args).contains("sk-"));
    let args = setup_core::hindsight_docker_args(&HindsightDockerOptions {
        port: 9000,
        provider: "ollama".into(),
        model: None,
        base_url: Some("http://host.docker.internal:11434/v1".into()),
        has_api_key: false,
    });
    assert!(args.contains(&"9000:8888".to_string()));
    assert!(!args.contains(&"HINDSIGHT_API_LLM_API_KEY".to_string()));
    assert!(args.contains(&"host.docker.internal:host-gateway".to_string()));
    assert_eq!(setup_core::hindsight_port("http://localhost:9000"), 9000);
    assert_eq!(setup_core::hindsight_port("https://mem"), 443);
    assert_eq!(setup_core::hindsight_port("nope"), 8888);
    assert!(
        setup_core::hindsight_manual_instructions("http://localhost:9000")
            .join("\n")
            .contains("hindsight-api --port 9000")
    );
    assert!(
        setup_core::hindsight_manual_instructions("http://localhost:8888")
            .join("\n")
            .contains("  hindsight-api\n")
    );
    assert_eq!(
        setup_core::format_command("docker", &["a b".into(), "it's".into(), "x=1".into()]),
        "docker 'a b' 'it'\\''s' x=1"
    );
}

#[test]
fn cli_setup_core_probes_hindsight_health_over_http() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let up = fetch_routes(vec![("/health", 200, "{}")], log.clone());
    assert_eq!(setup_core::probe_hindsight("http://localhost:8888/", &up), Ok(()));
    assert_eq!(log.lock().unwrap()[0], "http://localhost:8888/health");
    let busy = fetch_routes(vec![("/health", 503, "")], log);
    assert_eq!(
        setup_core::probe_hindsight("http://localhost:8888", &busy),
        Err("HTTP 503".into())
    );
    // The real fetch against a closed local port: Node's describeFetchError says "connection refused".
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    assert_eq!(
        setup_core::probe_hindsight(&format!("http://127.0.0.1:{port}"), &setup_core::http_fetch()),
        Err("connection refused".into())
    );
}

#[test]
fn cli_setup_core_hindsight_url_comes_from_env_then_setting_then_default() {
    assert_eq!(
        setup_core::effective_hindsight_url(None, None),
        (Some("http://localhost:8888".into()), UrlSource::Default)
    );
    assert_eq!(
        setup_core::effective_hindsight_url(None, Some("http://mem:9000")),
        (Some("http://mem:9000".into()), UrlSource::Setting)
    );
    assert_eq!(
        setup_core::effective_hindsight_url(Some("off"), Some("http://mem:9000")),
        (None, UrlSource::Env)
    );
    assert_eq!(
        setup_core::effective_hindsight_url(Some(""), Some("OFF")),
        (None, UrlSource::Setting)
    );
}

#[test]
fn cli_setup_core_environment_flags_missing_python_and_docker_is_optional() {
    let probe: setup_core::ProbeCommand =
        Arc::new(|command: &str, _: &[&str]| (command == "docker").then(|| "Docker version 27".to_string()));
    let checks = setup_core::check_environment(&probe, "python3");
    let summary: Vec<(String, bool)> = checks.iter().map(|c| (c.label.clone(), c.ok)).collect();
    assert_eq!(summary, [("python3".to_string(), false), ("Docker".to_string(), true)]);
    assert!(checks[0].fix.as_deref().unwrap().contains("python3"));
    let good: setup_core::ProbeCommand = Arc::new(|_: &str, _: &[&str]| Some("Python 3.12.1".to_string()));
    assert!(setup_core::check_environment(&good, "python3").iter().all(|c| c.ok));
}

#[test]
fn cli_setup_core_first_run_offer_only_on_a_terminal_start_without_a_model() {
    let none = |_: &str| None::<String>;
    let calls = std::cell::Cell::new(0);
    let no_model = || {
        calls.set(calls.get() + 1);
        false
    };
    fn never() -> bool {
        false
    }
    fn base<'a>(var: &'a dyn Fn(&str) -> Option<String>) -> SetupOfferInput<'a> {
        SetupOfferInput {
            stdin_is_tty: true,
            stdout_is_tty: true,
            interactive: true,
            explicit_model: false,
            var,
            dismissed: false,
            has_usable_model: &never,
        }
    }
    assert!(setup_core::should_offer_setup(&base(&none)));
    assert!(!setup_core::should_offer_setup(&SetupOfferInput {
        has_usable_model: &|| true,
        ..base(&none)
    }));
    let skip = |k: &str| (k == "ULTRON_SKIP_SETUP").then(|| "1".to_string());
    let skip_off = |k: &str| (k == "ULTRON_SKIP_SETUP").then(|| "0".to_string());
    let ci = |k: &str| (k == "CI").then(|| "true".to_string());
    for input in [
        SetupOfferInput {
            stdin_is_tty: false,
            ..base(&none)
        },
        SetupOfferInput {
            stdout_is_tty: false,
            ..base(&none)
        },
        SetupOfferInput {
            interactive: false,
            ..base(&none)
        },
        SetupOfferInput {
            explicit_model: true,
            ..base(&none)
        },
        SetupOfferInput {
            dismissed: true,
            ..base(&none)
        },
        base(&skip),
        base(&ci),
    ] {
        assert!(!setup_core::should_offer_setup(&SetupOfferInput {
            has_usable_model: &no_model,
            ..input
        }));
    }
    // The model catalog is not even loaded in those cases.
    assert_eq!(calls.get(), 0);
    assert!(setup_core::should_offer_setup(&base(&skip_off)));
}

#[test]
fn cli_setup_core_writes_private_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/models.json");
    setup_core::write_private_file(&path, "{}").unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
}

// ----------------------------------------------------------------------------------------------------- wizard

/// One scripted answer: the question it expects, and the choice (a label pattern) or text; `esc` presses Esc.
enum Step {
    Select(&'static str, &'static str),
    Input(&'static str, &'static str),
    Quit,
}

struct ScriptedUi {
    transcript: Vec<String>,
    secrets: Vec<String>,
    steps: VecDeque<Step>,
}

impl ScriptedUi {
    fn new(steps: Vec<Step>) -> Self {
        ScriptedUi {
            transcript: Vec::new(),
            secrets: Vec::new(),
            steps: steps.into(),
        }
    }

    fn text(&self) -> String {
        self.transcript.join("\n")
    }

    fn next(&mut self, select: bool, title: &str) -> Result<Step, SetupQuit> {
        let step = self
            .steps
            .pop_front()
            .unwrap_or_else(|| panic!("Unscripted {title}\n{}", self.text()));
        match &step {
            Step::Quit => return Err(SetupQuit),
            Step::Select(pattern, _) | Step::Input(pattern, _) => {
                assert_eq!(select, matches!(step, Step::Select(..)), "asked {title}");
                assert!(
                    Regex::new(pattern).unwrap().is_match(title),
                    "expected {pattern} but was asked {title}\n{}",
                    self.text()
                );
            }
        }
        Ok(step)
    }
}

impl SetupUi for ScriptedUi {
    fn heading(&mut self, title: &str) {
        self.transcript.push(format!("# {title}"));
    }

    fn note(&mut self, text: &str, _tone: Tone) {
        self.transcript.push(text.to_string());
    }

    fn select(
        &mut self,
        title: &str,
        choices: &[Choice],
        _description: Option<&str>,
    ) -> Result<Option<usize>, SetupQuit> {
        let Step::Select(_, pick) = self.next(true, title)? else {
            unreachable!()
        };
        if pick == "esc" {
            return Ok(None);
        }
        let pattern = Regex::new(pick).unwrap();
        let index = choices
            .iter()
            .position(|c| pattern.is_match(&c.label))
            .unwrap_or_else(|| panic!("no choice {pick} in {title}: {choices:?}"));
        Ok(Some(index))
    }

    fn input(&mut self, title: &str, options: InputOptions) -> Result<Option<String>, SetupQuit> {
        let Step::Input(_, text) = self.next(false, title)? else {
            unreachable!()
        };
        if text == "esc" {
            return Ok(None);
        }
        if options.secret {
            self.secrets.push(text.into());
        }
        if let Some(problem) = options.validate.as_ref().and_then(|v| v(text)) {
            panic!("invalid scripted answer for {title}: {problem}");
        }
        Ok(Some(text.into()))
    }

    fn pick_login_provider(&mut self, _: &[AuthProvider], _: &str) -> Result<Option<usize>, SetupQuit> {
        panic!("not scripted")
    }

    fn login(&mut self, _: &cli::Env, _: &AuthProvider) -> Result<LoginOutcome, SetupQuit> {
        panic!("not scripted")
    }

    fn busy<T: Send + 'static>(&mut self, _: &str, task: impl FnOnce() -> T + Send + 'static) -> Result<T, SetupQuit> {
        Ok(task())
    }
}

struct Profile {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Profile {
    fn new() -> Profile {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("agent")).unwrap();
        Profile { _dir: dir, root }
    }

    fn agent(&self) -> PathBuf {
        self.root.join("agent")
    }

    fn env(&self, vars: &[(&str, &str)]) -> cli::Env {
        // No provider keys from the machine running the tests: only these variables exist.
        let mut map: std::collections::HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        map.insert("ULTRON_CODING_AGENT_DIR".into(), self.agent().display().to_string());
        map.insert("PATH".into(), self.root.join("no-bin").display().to_string());
        cli::Env {
            cwd: self.root.clone(),
            home: self.root.clone(),
            var: Arc::new(move |k| map.get(k).cloned()),
            docs: self.root.join("docs"),
        }
    }

    fn settings(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.agent().join("settings.json")).unwrap()).unwrap()
    }
}

type Tested = Arc<Mutex<Vec<String>>>;

fn deps(profile: &Profile, vars: &[(&str, &str)]) -> (SetupDeps, Tested) {
    let env = profile.env(vars);
    let tested: Tested = Arc::default();
    let log = tested.clone();
    let deps = SetupDeps {
        settings: config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false),
        env,
        probe: Arc::new(|command: &str, _: &[&str]| (command == "python3").then(|| "Python 3.12.0".to_string())),
        python: "python3".into(),
        fetch: fetch_routes(
            vec![("/v1/models", 200, r#"{"data": [{"id": "m1"}, {"id": "m2"}]}"#)],
            Arc::default(),
        ),
        test_model: Arc::new(move |_: &cli::Env, model: &ai::Model| {
            log.lock().unwrap().push(format!("{}/{}", model.provider, model.id));
            ModelTestResult {
                ok: true,
                reply: Some("ok".into()),
                error: None,
                ms: 420,
            }
        }),
        run_docker: Arc::new(|_: &[String], _: &[(String, String)]| (true, String::new())),
        hindsight_wait: Duration::from_millis(50),
        hindsight_interval: Duration::from_millis(5),
        loki_version: None,
    };
    (deps, tested)
}

fn custom_endpoint() -> Vec<Step> {
    vec![
        Step::Select("reach a model", "Custom OpenAI-compatible"),
        Step::Input("Base URL", "http://127.0.0.1:8317/v1"),
        Step::Input("API key", "sk-proxy-secret"),
        Step::Input("short name", "proxy"),
        Step::Select("Which API", "Chat Completions"),
        Step::Input("Model ids", "m1, m2"),
        Step::Select("Default model", "^m2$"),
    ]
}

fn statuses(lines: &[cli::setup_wizard::SummaryLine]) -> Vec<(String, &'static str)> {
    lines.iter().map(|l| (l.step.clone(), l.status)).collect()
}

#[test]
fn cli_setup_wizard_configures_a_custom_endpoint_and_the_default_model() {
    use std::os::unix::fs::PermissionsExt;
    let profile = Profile::new();
    let (mut deps, tested) = deps(&profile, &[]);
    let mut steps = custom_endpoint();
    steps.extend([
        Step::Select("Set up Hindsight", "manual install"),
        Step::Select("Check again", "Continue without"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let mut ui = ScriptedUi::new(steps);
    let result = run_setup_wizard(&mut ui, &mut deps);
    assert!(ui.steps.is_empty());
    assert!(result.completed);
    assert!(result.model_ready);
    let models = profile.agent().join("models.json");
    assert_eq!(std::fs::metadata(&models).unwrap().permissions().mode() & 0o777, 0o600);
    let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&models).unwrap()).unwrap();
    assert_eq!(
        parsed["providers"]["proxy"],
        json!({"baseUrl": "http://127.0.0.1:8317/v1", "api": "openai-completions", "apiKey": "sk-proxy-secret",
            "models": [{"id": "m1"}, {"id": "m2"}]})
    );
    let saved = profile.settings();
    assert_eq!(saved["defaultProvider"], "proxy");
    assert_eq!(saved["defaultModel"], "m2");
    assert_eq!(*tested.lock().unwrap(), ["proxy/m2"]);
    let text = ui.text();
    for expected in [
        "The endpoint lists 2 models: m1, m2",
        "✓ Live test passed in 0.4s: \"ok\"",
        "pip install hindsight-api",
        "# Step 1/5: Environment",
        "✓ python3: Python 3.12.0",
        "– Docker: not found (only needed to install Hindsight)",
        "# Step 3/5: Hindsight memory server",
        "# Step 4/5: Loki guardrails",
        "# Step 5/5: Summary",
        "Provider and model: configured — proxy/m2, live test passed",
        "Hindsight: skipped — manual install instructions shown",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert_eq!(ui.secrets, ["sk-proxy-secret"]);
    assert!(!text.contains("sk-proxy-secret"));
    assert_eq!(
        statuses(&result.lines),
        [
            ("Provider and model".to_string(), "configured"),
            ("Hindsight".to_string(), "skipped"),
            ("Loki".to_string(), "unchanged")
        ]
    );
}

#[test]
fn cli_setup_wizard_keeps_a_working_setup_and_asks_before_replacing_a_provider() {
    let profile = Profile::new();
    let (mut first, _) = deps(&profile, &[]);
    let mut steps = custom_endpoint();
    steps.extend([
        Step::Select("Set up Hindsight", "esc"),
        Step::Select("^Loki guardrails$", "esc"),
    ]);
    run_setup_wizard(&mut ScriptedUi::new(steps), &mut first);
    let before = std::fs::read_to_string(profile.agent().join("models.json")).unwrap();
    let (mut second, _) = deps(&profile, &[]);
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Custom OpenAI-compatible"),
        Step::Input("Base URL", "http://other:1/v1"),
        Step::Input("API key", ""),
        Step::Input("short name", "proxy"),
        Step::Select("already has a provider named \"proxy\"", "Choose another name"),
        Step::Input("short name", "esc"),
        Step::Select("reach a model", "^Keep proxy/m2$"),
        Step::Select("Set up Hindsight", "^Skip$"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut second);
    assert!(ui.steps.is_empty());
    assert_eq!(
        std::fs::read_to_string(profile.agent().join("models.json")).unwrap(),
        before
    );
    assert_eq!(result.lines[0].status, "unchanged");
    assert_eq!(result.lines[0].detail, "proxy/m2");
    assert!(ui.text().contains("Current default: proxy/m2"));
}

#[test]
fn cli_setup_wizard_reports_a_failed_live_test_and_offers_another_model() {
    let profile = Profile::new();
    let (mut deps, _) = deps(&profile, &[]);
    let answers = Arc::new(Mutex::new(VecDeque::from([
        ModelTestResult {
            ok: false,
            reply: None,
            error: Some("401 invalid api key".into()),
            ms: 80,
        },
        ModelTestResult {
            ok: true,
            reply: Some("ok".into()),
            error: None,
            ms: 90,
        },
    ])));
    deps.test_model = Arc::new(move |_: &cli::Env, _: &ai::Model| answers.lock().unwrap().pop_front().unwrap());
    let mut steps = custom_endpoint();
    steps.extend([
        Step::Select("live test failed", "Try another model"),
        Step::Select("Default model", "^m1$"),
        Step::Quit,
    ]);
    let mut ui = ScriptedUi::new(steps);
    let result = run_setup_wizard(&mut ui, &mut deps);
    assert!(ui.text().contains("✗ Live test failed after 0.1s: 401 invalid api key"));
    assert_eq!(profile.settings()["defaultModel"], "m1");
    assert!(!result.completed);
    assert!(
        ui.text()
            .contains("Setup stopped. Anything saved before this point is kept.")
    );
    assert!(ui.text().contains("Not all steps ran."));
}

/// One `docker` run: its arguments and the environment it was given.
type DockerCall = (Vec<String>, Vec<(String, String)>);

#[test]
fn cli_setup_wizard_installs_hindsight_with_docker_passing_the_key_through_the_environment() {
    let profile = Profile::new();
    let (mut deps, _) = deps(&profile, &[]);
    let healthy = Arc::new(Mutex::new(false));
    let calls: Arc<Mutex<Vec<DockerCall>>> = Arc::default();
    let (h, c) = (healthy.clone(), calls.clone());
    deps.run_docker = Arc::new(move |args: &[String], env: &[(String, String)]| {
        *h.lock().unwrap() = true;
        c.lock().unwrap().push((args.to_vec(), env.to_vec()));
        (true, "container-id\n".into())
    });
    deps.fetch = Arc::new(move |request: &setup_core::FetchRequest| {
        assert!(request.url.ends_with("/health"));
        Ok(FetchResponse {
            status: if *healthy.lock().unwrap() { 200 } else { 503 },
            body: String::new(),
        })
    });
    deps.probe = Arc::new(|command: &str, args: &[&str]| {
        Some(match (command, args.first()) {
            ("docker", Some(&"ps")) => String::new(),
            ("docker", _) => "Docker version 27.0.0".into(),
            _ => "Python 3.12.0".into(),
        })
    });
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("Set up Hindsight", "Install it with Docker"),
        Step::Select("LLM provider for Hindsight", "^Anthropic$"),
        Step::Input("API key for Anthropic", "sk-ant-hindsight"),
        Step::Input("Model for Hindsight", ""),
        Step::Select("Run it now", "Run docker"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut deps);
    assert!(ui.steps.is_empty());
    let (args, env) = calls.lock().unwrap()[0].clone();
    assert!(args.contains(&"HINDSIGHT_API_LLM_PROVIDER=anthropic".to_string()));
    assert!(args.contains(&"HINDSIGHT_API_LLM_API_KEY".to_string()));
    assert!(!args.join(" ").contains("sk-ant-hindsight"));
    assert_eq!(
        env,
        [("HINDSIGHT_API_LLM_API_KEY".to_string(), "sk-ant-hindsight".to_string())]
    );
    assert!(!ui.text().contains("sk-ant-hindsight"));
    assert!(
        ui.text()
            .contains("Hindsight is not reachable at http://localhost:8888 (HTTP 503).")
    );
    assert_eq!(
        (result.lines[1].step.as_str(), result.lines[1].status),
        ("Hindsight", "configured")
    );
    assert_eq!(
        result.lines[1].detail,
        "Docker container \"hindsight\" at http://localhost:8888"
    );
}

#[test]
fn cli_setup_wizard_detects_a_running_hindsight_and_saves_a_custom_url() {
    let profile = Profile::new();
    let up = || fetch_routes(vec![("mem.local:9000/health", 200, "{}")], Arc::default());
    let (mut first, _) = deps(&profile, &[]);
    first.fetch = up();
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("Set up Hindsight", "set its URL"),
        Step::Input("Hindsight URL", "http://mem.local:9000/"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut first);
    assert!(ui.steps.is_empty());
    assert_eq!(profile.settings()["hindsightUrl"], "http://mem.local:9000");
    assert_eq!(
        (result.lines[1].status, result.lines[1].detail.as_str()),
        ("configured", "http://mem.local:9000")
    );
    assert!(
        ui.text()
            .contains("Hindsight is not reachable at http://localhost:8888 (connection refused).")
    );
    let (mut second, _) = deps(&profile, &[]);
    second.fetch = up();
    let mut again = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("^Hindsight$", "Use it"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let second = run_setup_wizard(&mut again, &mut second);
    assert!(
        again
            .text()
            .contains("✓ Hindsight is running at http://mem.local:9000.")
    );
    assert_eq!(second.lines[1].status, "unchanged");
}

#[test]
fn cli_setup_wizard_shows_loki_analyzers_and_saves_its_toggles() {
    let profile = Profile::new();
    let (mut deps, _) = deps(&profile, &[("ULTRON_LOKI_AUTOINIT", "off")]);
    deps.loki_version = Some("0.1.2 (35aa99c2aeee)".into());
    deps.probe =
        Arc::new(|command: &str, _: &[&str]| (command == "python3" || command == "ruff").then(|| "found".to_string()));
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("Set up Hindsight", "^Skip$"),
        Step::Select("^Loki guardrails$", "^Turn auto-commit off$"),
        Step::Select("^Loki guardrails$", "^Advise only"),
        Step::Select("^Loki guardrails$", "^Keep"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut deps);
    assert!(ui.steps.is_empty());
    let text = ui.text();
    for expected in [
        "✓ Bundled Loki 0.1.2 (35aa99c2aeee)",
        "✓ ruff (Python)",
        "– mypy (Python types): not found. uv tool install mypy",
        "– clippy (Rust): not found. rustup component add clippy",
        "ULTRON_LOKI_AUTOINIT=off is set",
        "Missing analyzers are reported as NOT CHECKED, never as clean.",
        "Loki: blocking, auto-install on, auto-commit on.",
        "Loki: blocking, auto-install on, auto-commit off.",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    assert_eq!(
        profile.settings()["loki"],
        json!({"autoCommit": false, "mode": "advise"})
    );
    let last = result.lines.last().unwrap();
    assert_eq!(
        (last.step.as_str(), last.status, last.detail.as_str()),
        ("Loki", "configured", "advise-only, auto-install on, auto-commit off")
    );
}

#[test]
fn cli_setup_wizard_turns_memory_off_and_back_on() {
    let profile = Profile::new();
    let (mut deps, _) = deps(&profile, &[]);
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("Set up Hindsight", "Turn memory off"),
        Step::Select("^Loki guardrails$", "^Turn Loki off$"),
        Step::Select("^Loki guardrails$", "^Keep Loki off$"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut deps);
    assert!(ui.steps.is_empty());
    assert_eq!(profile.settings()["hindsightUrl"], "off");
    assert_eq!(profile.settings()["loki"]["mode"], "off");
    assert_eq!(result.lines[1].detail, "off");
    assert_eq!(result.lines[2].detail, "off");
    let (mut again, _) = deps_again(&profile);
    let mut ui = ScriptedUi::new(vec![
        Step::Select("reach a model", "Skip"),
        Step::Select("Turn memory on", "No, leave it off"),
        Step::Select("^Loki guardrails$", "^Turn Loki on$"),
        Step::Select("^Loki guardrails$", "^Keep these"),
    ]);
    let result = run_setup_wizard(&mut ui, &mut again);
    assert!(ui.text().contains("Memory is turned off."));
    assert_eq!(
        (result.lines[1].status, result.lines[1].detail.as_str()),
        ("unchanged", "off")
    );
    assert_eq!(result.lines[2].status, "configured");
}

fn deps_again(profile: &Profile) -> (SetupDeps, Tested) {
    deps(profile, &[])
}

// ------------------------------------------------------------------------------------------------ start model

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[test]
fn cli_setup_start_model_nothing_configured_offers_setup_with_the_hint() {
    let profile = Profile::new();
    let env = profile.env(&[]);
    assert!(!cli::setup::has_usable_model(&env));
    assert_eq!(
        cli::setup::no_model_hint(&env).as_deref(),
        Some(
            "No model is configured. Run \"ultron setup\", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY."
        )
    );
}

#[test]
fn cli_setup_start_model_stored_credentials_make_their_models_usable() {
    let profile = Profile::new();
    write(
        &profile.agent().join("auth.json"),
        r#"{"anthropic": {"type": "api_key", "key": "sk-test"}}"#,
    );
    let env = profile.env(&[]);
    assert!(cli::setup::has_usable_model(&env));
    assert_eq!(cli::setup::no_model_hint(&env), None);
}

#[test]
fn cli_setup_start_model_a_default_without_credentials_is_named_in_the_hint() {
    let profile = Profile::new();
    let id = cli::setup_wizard::ProfileRuntime::load(&profile.env(&[("ANTHROPIC_API_KEY", "k")]))
        .available
        .iter()
        .find(|m| m.provider == "anthropic")
        .unwrap()
        .id
        .clone();
    write(
        &profile.agent().join("settings.json"),
        &json!({"defaultProvider": "anthropic", "defaultModel": id}).to_string(),
    );
    let env = profile.env(&[]);
    // A saved default is taken on trust before the start.
    assert!(cli::setup::has_usable_model(&env));
    let hint = cli::setup::no_model_hint(&env).unwrap();
    assert!(hint.contains(&format!("The default model anthropic/{id} has no credentials")));
    assert!(hint.contains("Run \"ultron setup\""));
}

#[test]
fn cli_setup_start_model_an_unknown_default_gets_no_misleading_hint() {
    let profile = Profile::new();
    write(
        &profile.agent().join("settings.json"),
        r#"{"defaultProvider": "my-extension-provider", "defaultModel": "m1"}"#,
    );
    assert_eq!(cli::setup::no_model_hint(&profile.env(&[])), None);
}
