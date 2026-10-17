//! `ultron autoreview doctor [--repo owner/name]` runs the review's own test launcher script, as cli.ts does
//! (`<python> <rlm dir>/autoreview_tests.py doctor [image]`, then `environments <checkout>` for `--repo`), through
//! `autoreview_cli::run_command_with` with a scripted runner. Ported from ultron-autoreview.test.ts ("doctor reports
//! the sandbox and its self-check; without one, that tests are not run", "doctor --repo shows the local checkout and
//! exactly what would be bound"), plus the image argument and the unparsable-report error of cli.ts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cli::autoreview_cli::{Overrides, run_command_with};
use review::runner::{RunOptions, RunResult, Runner};
use serde_json::{Value, json};

type Respond = Box<dyn Fn(&[String]) -> RunResult + Send + Sync>;

struct Fake {
    respond: Respond,
    calls: Mutex<Vec<(Vec<String>, Option<u64>)>>,
}

impl Runner for Fake {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        self.calls.lock().unwrap().push((argv.to_vec(), options.timeout_ms));
        (self.respond)(argv)
    }
}

fn ok(stdout: String) -> RunResult {
    RunResult {
        code: 0,
        stdout,
        stderr: String::new(),
    }
}

fn docs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")
}

fn script() -> String {
    config::paths::normalize(&docs().join("../src/ultron/rlm/autoreview_tests.py"))
        .display()
        .to_string()
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    calls: Vec<(Vec<String>, Option<u64>)>,
}

fn run(base: &Path, args: &[&str], settings: Value, respond: Respond) -> Run {
    let agent = base.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    let agent_text = agent.display().to_string();
    let cache = base.join("cache").display().to_string();
    let env = cli::Env {
        cwd: base.to_path_buf(),
        home: base.join("home"),
        var: Arc::new(move |key| match key {
            "ULTRON_CODING_AGENT_DIR" => Some(agent_text.clone()),
            "ULTRON_AUTOREVIEW_CACHE_DIR" => Some(cache.clone()),
            "ULTRON_HINDSIGHT_URL" => Some("off".into()),
            "ULTRON_PYTHON" => Some("/fake/python3".into()),
            _ => None,
        }),
        docs: docs(),
    };
    let runner = Arc::new(Fake {
        respond,
        calls: Mutex::default(),
    });
    let overrides = Overrides {
        runner: Some(runner.clone()),
        engine: None,
    };
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_command_with(&args, &env, &overrides, &mut out, &mut err);
    let calls = runner.calls.lock().unwrap().clone();
    Run {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        calls,
    }
}

#[test]
fn cli_autoreview_doctor_runs_the_review_test_script() {
    assert!(Path::new(&script()).is_file(), "the shipped script {}", script());
    let temp = tempfile::tempdir().unwrap();
    let report = json!({
        "mechanism": "bwrap",
        "isolation": "bubblewrap: new user, mount, pid, ipc, uts, cgroup and network namespaces",
        "ok": true,
        "selfCheck": {
            "network": "unreachable", "canary": "unreadable", "token": "absent", "home": "/tmp/home",
            "workdir": "writable", "system": "read-only", "dockerSocket": "hidden",
        },
    });
    let good = run(
        temp.path(),
        &["doctor"],
        json!({}),
        Box::new(move |_| ok(format!("progress\n{report}\n"))),
    );
    assert_eq!(good.code, 0, "{}", good.stderr);
    assert_eq!(
        good.calls,
        vec![(
            vec!["/fake/python3".to_string(), script(), "doctor".to_string()],
            Some(180_000)
        )]
    );
    for line in [
        "Sandbox: bubblewrap: new user, mount, pid, ipc, uts, cgroup and network namespaces\n",
        "Self-check: passed\n",
        "  network: unreachable\n",
        "  canary file in the real home: unreadable\n",
        "  token-like variable of the caller: absent\n",
        "  home inside the sandbox: /tmp/home\n",
        "  exported commit: writable; system directories: read-only\n",
        "  Docker socket: hidden\n",
        "Tests in reviews: run for repositories the account can push to\n",
    ] {
        assert!(good.stdout.contains(line), "{line:?} in {}", good.stdout);
    }

    let none = run(
        temp.path(),
        &["doctor"],
        json!({}),
        Box::new(|_| {
            ok(
                json!({"mechanism": null, "ok": false, "message": "tests are not run: no sandbox is available"})
                    .to_string(),
            )
        }),
    );
    assert_eq!(none.code, 1);
    assert!(none.stdout.contains("Models:\n  finders: "), "{}", none.stdout);
    assert!(
        none.stdout
            .ends_with("Sandbox: none. tests are not run: no sandbox is available\n"),
        "{}",
        none.stdout
    );

    // Only `mechanism: null` is "no sandbox" (cli.ts `report.mechanism === null`); a report without it is shown as
    // a sandbox, its missing fields as JavaScript's "undefined".
    let bare = run(
        temp.path(),
        &["doctor"],
        json!({}),
        Box::new(|_| ok(json!({"ok": true}).to_string())),
    );
    assert_eq!(bare.code, 0);
    assert!(
        bare.stdout.contains("Sandbox: undefined\nSelf-check: passed\n"),
        "{}",
        bare.stdout
    );

    // autoreview.testImage is passed to the script; --json is the report plus runTests.
    let image = run(
        temp.path(),
        &["doctor", "--json"],
        json!({"autoreview": {"testImage": "img:1"}}),
        Box::new(|_| ok(json!({"mechanism": "podman", "ok": false}).to_string())),
    );
    assert_eq!(image.code, 1);
    assert_eq!(image.calls[0].0[2..], ["doctor".to_string(), "img:1".to_string()]);
    let json: Value = serde_json::from_str(image.stdout.trim()).unwrap();
    assert_eq!(json, json!({"mechanism": "podman", "ok": false, "runTests": true}));

    // A script that printed no report: its stderr (else stdout), trimmed, in the error.
    let broken = run(
        temp.path(),
        &["doctor"],
        json!({}),
        Box::new(|_| RunResult {
            code: 1,
            stdout: "partial".into(),
            stderr: "  Traceback: boom\n".into(),
        }),
    );
    assert_eq!(broken.code, 1);
    assert!(
        broken.stderr.contains("the sandbox check did not run: Traceback: boom"),
        "{}",
        broken.stderr
    );
    assert!(broken.stdout.is_empty());
}

#[test]
fn cli_autoreview_doctor_repo_lists_bound_environments_from_the_script() {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let roots = base.join("roots");
    std::fs::create_dir_all(roots.join("r/.git")).unwrap();
    let checkout = roots.join("r").display().to_string();
    let result = run(
        &base,
        &["doctor", "--repo", "o/r"],
        json!({"autoreview": {"checkoutRoots": [roots.display().to_string()]}}),
        Box::new(|argv| {
            if argv[0] == "git" {
                return ok("remote.origin.url https://github.com/o/r.git\n".into());
            }
            if argv[2] == "environments" {
                return ok(json!([
                    {"path": ".venv", "kind": "virtualenv", "interpreter": "/home/u/.local/share/uv/python/cpython-3.12"},
                    {"path": "web/node_modules", "kind": "node_modules"},
                ])
                .to_string());
            }
            ok(json!({"mechanism": "bwrap", "isolation": "bubblewrap", "ok": true, "selfCheck": {}}).to_string())
        }),
    );
    assert_eq!(result.code, 0, "{}", result.stderr);
    let text = &result.stdout;
    assert!(text.contains(&format!("Local checkout of o/r: {checkout}\n")), "{text}");
    assert!(text.contains(
        "  bound read-only: .venv (virtualenv, with its interpreter /home/u/.local/share/uv/python/cpython-3.12)\n"
    ));
    assert!(text.contains("  bound read-only: web/node_modules (node_modules)\n"));
    assert!(text.contains("  never bound: the checkout's source, .git, .env files\n"));
    assert!(text.contains("  network: undefined\n"), "{text}");
    let scripted: Vec<&(Vec<String>, Option<u64>)> = result.calls.iter().filter(|(argv, _)| argv[0] != "git").collect();
    assert_eq!(
        scripted,
        vec![
            &(
                vec!["/fake/python3".to_string(), script(), "doctor".to_string()],
                Some(180_000)
            ),
            &(
                vec![
                    "/fake/python3".to_string(),
                    script(),
                    "environments".to_string(),
                    checkout.clone()
                ],
                Some(60_000)
            ),
        ]
    );
}
