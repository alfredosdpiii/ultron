//! Ported from `test/ultron-autoreview.test.ts`: "accounts and auth", "GitHub client", "state", and the process
//! runner (`runner.ts`), against the fake hub, stub scripts in temp dirs, and temp state files.

#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;
mod github_common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use github_common::*;
use review::accounts::{Account, TokenStore, list_accounts, parse_auth_status};
use review::github::{Error, GitHub, parse_api_output, parse_pull_target};
use review::runner::{Env, MAX_OUTPUT_BYTES, ProcessRunner, RunOptions, Runner, argv};
use review::state::{StateStore, acquire_daemon_lock, empty_state};
use serde_json::json;

fn account(login: &str, host: &str, active: bool) -> Account {
    Account {
        login: login.into(),
        host: host.into(),
        active,
    }
}

#[test]
fn review_github_accounts_come_from_gh_auth_status_on_every_host() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_account("second", "github.com", false);
    hub.add_account("corp", "ghe.example.com", false);
    assert_eq!(
        list_accounts(hub.as_ref(), None).expect("accounts"),
        vec![
            account(BOT, "github.com", true),
            account("second", "github.com", false),
            account("corp", "ghe.example.com", false)
        ]
    );
    let only = list_accounts(hub.as_ref(), Some(&["SECOND".to_string()])).expect("accounts");
    assert_eq!(only.iter().map(|a| a.login.as_str()).collect::<Vec<_>>(), ["second"]);
    let by_host = list_accounts(hub.as_ref(), Some(&["GHE.example.com/corp".to_string()])).expect("accounts");
    assert_eq!(by_host.iter().map(|a| a.login.as_str()).collect::<Vec<_>>(), ["corp"]);
    // An older gh without --json prints text.
    assert_eq!(
        parse_auth_status(
            "",
            "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n  ✓ Logged in to ghe.io as hubot (oauth)\n"
        ),
        vec![
            account("octocat", "github.com", false),
            account("hubot", "ghe.io", false)
        ]
    );
}

/// A runner for `list_accounts`'s fallbacks: replies by the argv's length.
struct AuthStub {
    calls: Mutex<Vec<Vec<String>>>,
    json_code: i32,
}

impl Runner for AuthStub {
    fn run(&self, argv: &[String], _: &RunOptions) -> review::runner::RunResult {
        self.calls.lock().expect("calls").push(argv.to_vec());
        let (code, stdout, stderr) = match (argv.len(), self.json_code) {
            (5, 127) => (127, "", "gh: not found"),
            (5, code) => (code, "unknown flag --json", ""),
            _ => (0, "", "Logged in to github.com account legacy (keyring)"),
        };
        review::runner::RunResult {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

#[test]
fn review_github_list_accounts_falls_back_to_text_and_needs_gh() {
    let stub = AuthStub {
        calls: Mutex::new(Vec::new()),
        json_code: 1,
    };
    assert_eq!(
        list_accounts(&stub, None).expect("accounts"),
        vec![account("legacy", "github.com", false)]
    );
    assert_eq!(stub.calls.lock().expect("calls")[1], argv(&["gh", "auth", "status"]));
    let missing = AuthStub {
        calls: Mutex::new(Vec::new()),
        json_code: 127,
    };
    assert_eq!(
        list_accounts(&missing, None),
        Err(Error::Other(
            "ultron autoreview needs the GitHub CLI (gh), which is not installed".into()
        ))
    );
}

#[test]
fn review_github_token_reaches_gh_only_through_the_environment() {
    let context = setup(json!({}), None);
    let hub = context.hub.clone();
    let corp = hub.add_account("corp", "ghe.example.com", false);
    let registered = Arc::new(Mutex::new(Vec::new()));
    let sink = registered.clone();
    let tokens = TokenStore::new(hub.clone()).with_secret_sink(Arc::new(move |token: &str| {
        sink.lock().expect("sink").push(token.to_string())
    }));
    let env = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Env>()
    };
    assert_eq!(
        tokens.env(&Account::new(BOT, "github.com")).expect("env"),
        env(&[("GH_TOKEN", &context.token), ("GH_HOST", "github.com")])
    );
    assert_eq!(
        tokens.env(&Account::new("corp", "ghe.example.com")).expect("env"),
        env(&[("GH_ENTERPRISE_TOKEN", &corp), ("GH_HOST", "ghe.example.com")])
    );
    // Read once per account.
    tokens.env(&Account::new(BOT, "github.com")).expect("env");
    let reads = |hub: &FakeHub| hub.calls().iter().filter(|c| c.argv[2] == "token").count();
    assert_eq!(reads(&hub), 2);
    assert_eq!(
        *registered.lock().expect("registered"),
        vec![context.token.clone(), corp]
    );
    let missing = tokens.env(&Account::new("nobody", "github.com")).expect_err("no token");
    assert!(missing.message().contains("no token"), "{missing}");
    // Forgotten: read again.
    tokens.forget(&Account::new(BOT, "github.com"));
    tokens.env(&Account::new(BOT, "github.com")).expect("env");
    assert_eq!(reads(&hub), 4);
}

#[test]
fn review_github_api_output_and_pull_targets() {
    let parsed = parse_api_output("HTTP/2.0 200 OK\r\nEtag: \"abc\"\r\nX-Poll-Interval: 60\r\n\r\n{\"a\":1}");
    assert_eq!(parsed.status, 200);
    assert_eq!(parsed.headers.get("etag").map(String::as_str), Some("\"abc\""));
    assert_eq!(parsed.headers.get("x-poll-interval").map(String::as_str), Some("60"));
    assert_eq!(parsed.text, "{\"a\":1}");
    assert_eq!(
        parse_api_output("HTTP/2.0 304 Not Modified\nLast-Modified: x\n\n").status,
        304
    );
    assert_eq!(parse_api_output("gh: connection refused").status, 0);
    assert_eq!(
        parse_pull_target("o/r#12", "github.com"),
        Some(review::github::PullRef::new("github.com", "o", "r", 12))
    );
    assert_eq!(
        parse_pull_target("https://ghe.io/o/r/pull/7/files", "github.com"),
        Some(review::github::PullRef::new("ghe.io", "o", "r", 7))
    );
    assert_eq!(parse_pull_target("12", "github.com"), None);
}

#[test]
fn review_github_retry_after_and_exhausted_limit_become_rate_limits() {
    let context = setup(json!({}), None);
    let hub = context.hub.clone();
    hub.add_pull(FakePull::new("o", "r", 1));
    let tokens = context.deps.tokens.clone();
    let me = context.account.clone();
    let clock = hub.clone();
    let github = GitHub::new(
        hub.clone(),
        me.clone(),
        Arc::new(move || tokens.env(&me)),
        Some(Arc::new(move || clock.now())),
    );
    hub.fail(
        "GET repos/o/r/pulls/1",
        403,
        &[("Retry-After", "120")],
        "secondary rate limit",
    );
    let now = hub.now() as f64;
    match github.pull(&reference()) {
        Err(Error::RateLimit { resume_at, .. }) => assert_eq!(resume_at, now + 120_000.0),
        other => panic!("not a rate limit: {other:?}"),
    }
    let reset = (hub.now() / 1000 + 600).to_string();
    hub.fail(
        "GET repos/o/r/pulls/1",
        403,
        &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", &reset)],
        "API rate limit exceeded",
    );
    match github.pull(&reference()) {
        Err(Error::RateLimit { resume_at, .. }) => assert_eq!(resume_at, (hub.now() / 1000 + 600) as f64 * 1000.0),
        other => panic!("not a rate limit: {other:?}"),
    }
    // A plain 403 is not a rate limit.
    hub.fail("GET repos/o/r/pulls/1", 403, &[], "Resource not accessible");
    let error = github.pull(&reference()).expect_err("403");
    assert!(error.message().contains("403 Resource not accessible"), "{error}");
    assert!(matches!(error, Error::GitHub { status: 403, .. }));
}

#[test]
fn review_github_state_writes_are_atomic_and_serialized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("autoreview/state.json").to_string_lossy().into_owned();
    let threads: Vec<_> = (0..20)
        .map(|index| {
            let path = path.clone();
            std::thread::spawn(move || {
                StateStore::new(&path)
                    .update_pull(&format!("k{}", index % 4), |pull, _| {
                        pull.attempts.insert(format!("sha{index}"), 1);
                    })
                    .expect("update")
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("thread");
    }
    let state = StateStore::new(&path).read();
    // At most eight attempt shas are kept per pull request: 5 per key here, so all 20 land.
    assert_eq!(state.pulls.values().map(|p| p.attempts.len()).sum::<usize>(), 20);
    let names: Vec<String> = std::fs::read_dir(dir.path().join("autoreview"))
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|name| !name.ends_with(".tmp") && !name.ends_with(".lock")),
        "{names:?}"
    );
    // A missing or corrupt file reads as empty state instead of crashing the daemon.
    assert_eq!(StateStore::new(&format!("{path}.missing")).read(), empty_state());
    std::fs::write(&path, "{not json").expect("write");
    assert_eq!(StateStore::new(&path).read(), empty_state());
    assert_eq!(
        review::state::to_json_1(&empty_state()),
        "{\n \"version\": 1,\n \"accounts\": {},\n \"pulls\": {},\n \"recent\": []\n}"
    );
}

#[test]
fn review_github_state_keeps_recent_and_attempt_bounds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.json").to_string_lossy().into_owned();
    let store = StateStore::new(&path);
    store
        .update(|state| {
            for index in 0..60 {
                state.recent.push(review::state::ReviewRecord {
                    pull: format!("p{index}"),
                    ..Default::default()
                });
            }
        })
        .expect("update");
    for index in 0..10 {
        store
            .update_pull("k", |pull, _| {
                pull.attempts.insert(format!("sha{index}"), index);
            })
            .expect("update");
    }
    let state = store.read();
    assert_eq!(state.recent.len(), 50);
    assert_eq!(state.recent[0].pull, "p10");
    assert_eq!(
        state.pulls["k"].attempts.keys().cloned().collect::<Vec<_>>(),
        (2..10).map(|i| format!("sha{i}")).collect::<Vec<_>>()
    );
    // Unknown fields survive a round trip; a wrongly typed pull entry or findings list is read leniently.
    std::fs::write(
        &path,
        r#"{"version":1,"accounts":{"a":{"etag":"e","x":1}},"pulls":{"k":{"attempts":5,"findings":"no","note":"kept"},"bad":3},"recent":[{"bogus":true},{"account":"a","pull":"p","sha":"s","at":"t","outcome":"posted"}],"lessons":{"r":[{"id":1},{"id":"x","pull":"p","file":"f","line":1,"category":"c","kind":"fast","level":"low","claim":"c","fate":"accepted","how":"h","at":"t"}]},"extra":1}"#,
    )
    .expect("write");
    let state = store.read();
    assert_eq!(state.pulls.len(), 1);
    assert!(state.pulls["k"].attempts.is_empty() && state.pulls["k"].findings.is_empty());
    assert_eq!(state.pulls["k"].extra["note"], json!("kept"));
    assert_eq!(state.accounts["a"].extra["x"], json!(1));
    assert_eq!(state.recent.len(), 1);
    assert_eq!(state.lessons.as_ref().expect("lessons")["r"].len(), 1);
}

#[test]
fn review_github_only_one_daemon_holds_an_agent_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let autoreview = dir.path().join("autoreview").to_string_lossy().into_owned();
    let lock = acquire_daemon_lock(&autoreview).expect("lock");
    match acquire_daemon_lock(&autoreview) {
        Err(Error::DaemonRunning(message)) => {
            assert_eq!(
                message,
                format!("another ultron autoreview is already running for {autoreview}")
            )
        }
        other => panic!("expected DaemonRunning, got {:?}", other.map(|_| ())),
    }
    lock.release();
    acquire_daemon_lock(&autoreview).expect("again").release();
    // proper-lockfile's layout: a `daemon` file and a `daemon.lock` directory while held.
    let held = acquire_daemon_lock(&autoreview).expect("held");
    assert!(dir.path().join("autoreview/daemon").is_file());
    assert!(dir.path().join("autoreview/daemon.lock").is_dir());
    drop(held);
    assert!(!dir.path().join("autoreview/daemon.lock").exists());
}

#[test]
fn review_github_a_stale_daemon_lock_is_taken_over() {
    let dir = tempfile::tempdir().expect("tempdir");
    let autoreview = dir.path().join("autoreview");
    std::fs::create_dir_all(autoreview.join("daemon.lock")).expect("lock dir");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::open(autoreview.join("daemon.lock"))
        .expect("open")
        .set_modified(old)
        .expect("mtime");
    acquire_daemon_lock(&autoreview.to_string_lossy())
        .expect("stale lock taken")
        .release();
}

/// An executable the runner execs by path, written without a write descriptor in this process (ETXTBSY: see
/// `executable_support`).
fn stub(dir: &std::path::Path, name: &str, script: &str) -> String {
    let path = dir.join(name);
    executable_support::install(&path, &format!("#!/bin/sh\n{script}\n"));
    path.to_string_lossy().into_owned()
}

#[test]
fn review_github_process_runner_env_stdin_cwd_and_exit_codes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let echo = stub(
        dir.path(),
        "echo-env",
        "echo \"$GIT_PAGER $PAGER $GIT_TERMINAL_PROMPT $GH_PROMPT_DISABLED $GH_NO_UPDATE_NOTIFIER $NO_COLOR $GH_TOKEN $PWD\"; cat; echo err >&2; exit 3",
    );
    let mut env = Env::new();
    env.insert("GH_TOKEN".into(), "secret".into());
    env.insert("PAGER".into(), "less".into());
    let cwd = dir.path().to_string_lossy().into_owned();
    let result = ProcessRunner.run(
        std::slice::from_ref(&echo),
        &RunOptions {
            cwd: Some(cwd.clone()),
            env,
            input: Some("from stdin".into()),
            timeout_ms: None,
        },
    );
    assert_eq!(result.code, 3);
    assert_eq!(result.stdout, format!("cat less 0 1 1 1 secret {cwd}\nfrom stdin"));
    assert_eq!(result.stderr, "err\n");
    // No input: stdin is empty and closed.
    let quiet = ProcessRunner.run(&[echo], &RunOptions::default());
    assert!(quiet.stdout.ends_with('\n'), "{quiet:?}");
    let missing = ProcessRunner.run(&argv(&["/nonexistent/ultron-gh"]), &RunOptions::default());
    assert_eq!(missing.code, 127);
    assert_eq!(
        missing.stderr,
        "/nonexistent/ultron-gh: spawn /nonexistent/ultron-gh ENOENT"
    );
    assert_eq!(ProcessRunner.run(&[], &RunOptions::default()).code, 127);
}

#[test]
fn review_github_process_runner_times_out_with_124() {
    let started = std::time::Instant::now();
    let result = ProcessRunner.run(
        &argv(&["sleep", "300"]),
        &RunOptions {
            timeout_ms: Some(1_500),
            ..RunOptions::default()
        },
    );
    assert_eq!(result.code, 124);
    assert_eq!(result.stderr, "sleep: timed out after 2s");
    assert!(started.elapsed() < std::time::Duration::from_secs(120));
}

#[test]
fn review_github_process_runner_caps_output_and_kills_the_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let flood = stub(dir.path(), "flood", "exec yes ultron");
    let result = ProcessRunner.run(&[flood], &RunOptions::default());
    assert_eq!(result.code, 1);
    assert!(result.stdout.len() <= MAX_OUTPUT_BYTES, "{}", result.stdout.len());
    assert!(
        result.stdout.len() > MAX_OUTPUT_BYTES - 64 * 1024,
        "{}",
        result.stdout.len()
    );
    assert!(result.stdout.starts_with("ultron\nultron\n"));
}

#[test]
fn review_github_process_runner_is_shared_across_threads() {
    let runner: Arc<dyn Runner> = Arc::new(ProcessRunner);
    let outputs: HashSet<String> = (0..8)
        .map(|index| {
            let runner = runner.clone();
            std::thread::spawn(move || {
                runner
                    .run(
                        &argv(&["/bin/sh", "-c", "cat"]),
                        &RunOptions {
                            input: Some(format!("run {index}")),
                            ..RunOptions::default()
                        },
                    )
                    .stdout
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().expect("thread"))
        .collect();
    assert_eq!(outputs.len(), 8);
}
