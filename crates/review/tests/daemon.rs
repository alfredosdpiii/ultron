//! The autoreview daemon (`autoreview/daemon.ts`): the pure parts replayed from `fixtures/daemon.json` (recorded from
//! the TypeScript by `fixtures/generate_daemon.mjs`), and `test/ultron-autoreview.test.ts` "discovery and the daemon"
//! ported against the fake hub and a scripted engine. Concurrency tests wait on events (a gate in the engine hook,
//! the outcome channel); every deadline is a hang guard of at least 120 s. No network, no model calls.

mod github_common;

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use github_common::*;
use review::accounts::{Account, TokenStore};
use review::daemon::{Daemon, DaemonDeps, Reported, blocked_pulls, create_logger, discover, prune_old};
use review::github::{Error, GitHub, PullRef};
use review::ledger::parse_iso_ms;
use review::runner::{RunOptions, RunResult, Runner};
use review::settings::RETENTION_DAYS;
use review::state::{AccountState, AutoreviewState, PullState};
use serde_json::{Value, json};

const GUARD: Duration = Duration::from_secs(120);

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/daemon.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("daemon.json")).expect("json")
}

fn s(value: &Value) -> &str {
    value.as_str().expect("string")
}

fn pull_ref(value: &Value) -> PullRef {
    PullRef::new(
        s(&value["host"]),
        s(&value["owner"]),
        s(&value["repo"]),
        value["number"].as_u64().expect("number"),
    )
}

fn ref_json(r: &PullRef) -> Value {
    json!({ "host": r.host, "owner": r.owner, "repo": r.repo, "number": r.number })
}

/// Wait until `condition` holds (a state the daemon's threads reach), failing after the hang guard.
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

// ---- Recorded from the TypeScript ----

#[test]
fn review_daemon_blocked_pulls_match_typescript() {
    let data = fixture();
    let mut state = AutoreviewState::default();
    for (key, pull) in data["blockedState"]["pulls"].as_object().expect("pulls") {
        let pull: PullState = serde_json::from_value(pull.clone()).expect("pull state");
        state.pulls.insert(key.clone(), pull);
    }
    for case in data["blocked"].as_array().expect("blocked") {
        let account = Account::new(s(&case["account"]["login"]), s(&case["account"]["host"]));
        let found: Vec<Value> = blocked_pulls(&state, &account).iter().map(ref_json).collect();
        assert_eq!(Value::Array(found), case["blocked"], "{account:?}");
    }
}

struct Scripted {
    replies: Mutex<VecDeque<RunResult>>,
    calls: Mutex<Vec<Value>>,
}

impl Runner for Scripted {
    fn run(&self, argv: &[String], options: &RunOptions) -> RunResult {
        self.calls
            .lock()
            .expect("calls")
            .push(json!({ "argv": argv, "env": options.env }));
        self.replies.lock().expect("replies").pop_front().unwrap_or(RunResult {
            code: 1,
            stdout: String::new(),
            stderr: "no reply".into(),
        })
    }
}

fn error_json(error: &Error) -> Value {
    match error {
        Error::RateLimit { message, resume_at } => {
            json!({ "name": "RateLimitError", "message": message, "status": null, "resumeAt": resume_at })
        }
        Error::GitHub { message, status } => {
            json!({ "name": "GitHubError", "message": message, "status": status, "resumeAt": null })
        }
        other => json!({ "name": "Error", "message": other.message(), "status": null, "resumeAt": null }),
    }
}

/// Numbers compared by value (an integral f64 equals the integer JavaScript wrote).
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| same(value, other)))
        }
        _ => a == b,
    }
}

#[test]
fn review_daemon_discover_matches_typescript() {
    let data = fixture();
    let cases = data["discover"].as_array().expect("discover");
    assert!(cases.len() >= 10);
    for case in cases {
        let name = s(&case["name"]);
        let replies = case["replies"]
            .as_array()
            .expect("replies")
            .iter()
            .map(|r| RunResult {
                code: r["code"].as_i64().expect("code") as i32,
                stdout: s(&r["stdout"]).into(),
                stderr: s(&r["stderr"]).into(),
            })
            .collect();
        let runner = Arc::new(Scripted {
            replies: Mutex::new(replies),
            calls: Mutex::new(Vec::new()),
        });
        let account = Account::new(s(&case["account"][0]), s(&case["account"][1]));
        let host = account.host.clone();
        let now = case["now"].as_i64().expect("now");
        let gh = GitHub::new(
            runner.clone(),
            account,
            Arc::new(move || {
                let mut env = review::runner::Env::new();
                env.insert("GH_TOKEN".into(), "t".into());
                env.insert("GH_HOST".into(), host.clone());
                Ok(env)
            }),
            Some(Arc::new(move || now)),
        );
        let state: AccountState = serde_json::from_value(case["state"].clone()).expect("state");
        let blocked: Vec<PullRef> = case["blocked"]
            .as_array()
            .expect("blocked")
            .iter()
            .map(pull_ref)
            .collect();
        let search = case["search"].as_bool().expect("search");
        match discover(&gh, &state, now, search, &blocked) {
            Ok((pulls, next)) => {
                assert!(case["error"].is_null(), "{name}: expected {}", case["error"]);
                let pulls: Vec<Value> = pulls
                    .iter()
                    .map(|found| {
                        let mut item = json!({ "ref": ref_json(&found.pull), "reasons": found.reasons });
                        if let Some(at) = found.notified_at {
                            item["notifiedAt"] = json!(at);
                        }
                        item
                    })
                    .collect();
                let result = json!({ "pulls": pulls, "state": serde_json::to_value(&next).expect("state") });
                assert!(same(&result, &case["result"]), "{name}: {result} != {}", case["result"]);
            }
            Err(error) => assert!(
                same(&error_json(&error), &case["error"]),
                "{name}: {error:?} != {}",
                case["error"]
            ),
        }
        let calls = runner.calls.lock().expect("calls").clone();
        assert_eq!(Value::Array(calls), case["calls"], "{name}");
    }
}

fn set_mtime(path: &Path, at_ms: i64) {
    let at = if at_ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(at_ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(at_ms.unsigned_abs())
    };
    let file = std::fs::File::options().write(true).open(path).expect("open");
    file.set_modified(at).expect("mtime");
}

#[test]
fn review_daemon_prune_old_matches_typescript() {
    let data = fixture();
    let now = data["now"].as_i64().expect("now");
    for case in data["prune"].as_array().expect("prune") {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().join("prune");
        for file in data["pruneFiles"].as_array().expect("files") {
            let path = base.join(s(&file["name"]));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(&path, "x").expect("write");
            set_mtime(&path, now - file["age"].as_i64().expect("age"));
        }
        std::fs::create_dir(base.join("a/sub")).expect("sub");
        std::os::unix::fs::symlink(base.join("a/old.md"), base.join("a/link.md")).expect("link");
        let dirs: Vec<String> = case["dirs"]
            .as_array()
            .expect("dirs")
            .iter()
            .map(|dir| base.join(s(dir)).to_string_lossy().into_owned())
            .collect();
        let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
        let days = case["days"].as_u64().map_or(RETENTION_DAYS, |days| days as u32);
        let removed = prune_old(&dirs, now, days);
        assert_eq!(removed as u64, case["removed"].as_u64().expect("removed"), "{case}");
        let mut left = Vec::new();
        for dir in ["a", "b"] {
            let mut names: Vec<String> = std::fs::read_dir(base.join(dir))
                .expect("read")
                .map(|entry| format!("{dir}/{}", entry.expect("entry").file_name().to_string_lossy()))
                .collect();
            names.sort();
            left.extend(names);
        }
        assert_eq!(json!(left), case["left"], "{case}");
    }
}

#[test]
fn review_daemon_create_logger_matches_typescript() {
    for case in fixture()["logger"].as_array().expect("logger") {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("log");
        std::fs::create_dir(&root).expect("log");
        let dir = case["logs"]
            .as_str()
            .map(|logs| root.join(logs).to_string_lossy().into_owned());
        let nows: Vec<i64> = case["nows"]
            .as_array()
            .expect("nows")
            .iter()
            .map(|n| n.as_i64().expect("n"))
            .collect();
        let index = Arc::new(Mutex::new(0usize));
        let clock_index = index.clone();
        let written = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = written.clone();
        let log = create_logger(
            dir.clone(),
            Arc::new(move || nows[(*clock_index.lock().expect("i")).min(nows.len() - 1)]),
            Arc::new(move |text: &str| sink.lock().expect("written").push(text.into())),
            Arc::new(|line: &str| line.to_string()),
        );
        for line in case["lines"].as_array().expect("lines") {
            log(s(line));
            *index.lock().expect("i") += 1;
        }
        assert_eq!(json!(*written.lock().expect("written")), case["written"]);
        let mut files = Vec::new();
        if let Some(dir) = &dir {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .expect("dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for name in names {
                let path = Path::new(dir).join(&name);
                let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
                files
                    .push(json!({ "name": name, "text": std::fs::read_to_string(&path).expect("text"), "mode": mode }));
            }
            let mode = std::fs::metadata(dir).expect("meta").permissions().mode() & 0o777;
            assert_eq!(json!(mode), case["dirMode"]);
        }
        assert_eq!(json!(files), case["files"]);
    }
}

// ---- Ported from test/ultron-autoreview.test.ts, "discovery and the daemon" ----

type Outcomes = Arc<Mutex<Vec<(u64, String)>>>;

fn daemon_with(context: &Context, accounts: Vec<Account>) -> (Daemon, Outcomes) {
    let outcomes: Outcomes = Arc::new(Mutex::new(Vec::new()));
    let sink = outcomes.clone();
    let daemon = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts,
        sleep: Some(Arc::new(|_| {})),
        on_outcome: Some(Arc::new(move |candidate, outcome: &Reported| {
            sink.lock()
                .expect("outcomes")
                .push((candidate.pull.number, outcome.kind().to_string()));
        })),
    });
    (daemon, outcomes)
}

fn daemon(context: &Context) -> (Daemon, Outcomes) {
    daemon_with(context, vec![context.account.clone()])
}

fn outcomes_of(outcomes: &Outcomes) -> Vec<(u64, String)> {
    outcomes.lock().expect("outcomes").clone()
}

fn pair(number: u64, kind: &str) -> (u64, String) {
    (number, kind.to_string())
}

fn add_numbered(context: &Context, number: u64, requested: bool) {
    let mut pull = FakePull::new("o", "r", number);
    pull.head_sha = number.to_string().repeat(40);
    if requested {
        pull.requested_reviewers = vec![BOT.into()];
    }
    context.hub.add_pull(pull);
}

fn query_of(path: &str) -> String {
    percent_decode(path.split("q=").nth(1).expect("q"))
}

#[test]
fn review_daemon_notifications_find_requests_and_mentions() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    let mut mentioned = FakePull::new("o", "r", 2);
    mentioned.body = format!("ping @{BOT}");
    hub.add_pull(mentioned);
    hub.add_pull(FakePull::new("o", "r", 3));
    {
        let mut state = hub.lock();
        state.notifications = vec![
            Notification::new("review_requested", 1),
            Notification::new("mention", 2),
            Notification::new("subscribed", 3),
            Notification {
                kind: Some("Issue".into()),
                ..Notification::new("mention", 9)
            },
        ];
        state.poll_interval = Some(60);
    }
    let (instance, outcomes) = daemon(&context);
    instance.once();
    let mut found = outcomes_of(&outcomes);
    found.sort();
    assert_eq!(found, vec![pair(1, "posted"), pair(2, "posted")]);
    let account = context.deps.store.read().accounts["github.com/ultron-bot"].clone();
    assert_eq!(account.last_poll_at.as_deref(), Some("2026-10-04T10:00:00.000Z"));
    assert_eq!(account.last_modified.as_deref(), Some("Sun, 04 Oct 2026 10:00:00 GMT"));
    assert_eq!(account.poll_interval.map(|n| n.0), Some(60.0));
    // The first poll looks back a day; the next one sends If-Modified-Since and overlaps the last poll.
    let first = hub.api("GET notifications")[0].path.clone();
    assert!(first.contains("since=2026-10-03T10%3A00%3A00.000Z"), "{first}");
    hub.lock().not_modified = true;
    instance.once();
    let second = hub
        .calls()
        .into_iter()
        .rfind(|call| call.argv.iter().any(|arg| arg.starts_with("notifications")))
        .expect("second poll");
    assert!(
        second
            .argv
            .contains(&"If-Modified-Since: Sun, 04 Oct 2026 10:00:00 GMT".to_string())
    );
    assert!(second.argv.join(" ").contains("2026-10-04T09%3A58%3A00.000Z"));
    assert_eq!(outcomes_of(&outcomes).len(), 2);
}

#[test]
fn review_daemon_adopts_a_longer_poll_interval() {
    let context = setup(json!({ "pollSeconds": 60 }), None);
    let (instance, _) = daemon(&context);
    context.hub.lock().poll_interval = Some(30);
    instance.once();
    assert_eq!(instance.poll_seconds(), 60.0);
    context.hub.lock().poll_interval = Some(600);
    instance.once();
    assert_eq!(instance.poll_seconds(), 600.0);
}

#[test]
fn review_daemon_search_fallback_first_and_every_fifth_cycle() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    let mut mentioned = FakePull::new("o", "r", 2);
    mentioned.body = format!("@{BOT}");
    hub.add_pull(mentioned);
    hub.set_search(&[("review-requested:", &[1]), ("mentions:", &[2])]);
    let (instance, outcomes) = daemon(&context);
    instance.once();
    let mut numbers: Vec<u64> = outcomes_of(&outcomes).iter().map(|(n, _)| *n).collect();
    numbers.sort();
    assert_eq!(numbers, vec![1, 2]);
    let queries = || -> Vec<String> { hub.api("GET search/issues").iter().map(|c| query_of(&c.path)).collect() };
    assert_eq!(
        queries(),
        vec![
            "is:open is:pr review-requested:ultron-bot archived:false",
            "is:pr mentions:ultron-bot updated:>=2026-10-02 archived:false",
        ]
    );
    for _ in 0..4 {
        instance.once();
    }
    assert_eq!(queries().len(), 2);
    instance.once();
    assert_eq!(queries().len(), 4);
    // Found again by the search, but already reviewed: skipped, no second review.
    assert_eq!(
        outcomes_of(&outcomes)
            .iter()
            .filter(|(_, kind)| kind == "posted")
            .count(),
        2
    );
}

/// Reviews in the engine at once, the most seen, how many started, and the most the daemon ran at once.
#[derive(Default)]
struct Gate {
    active: usize,
    max_active: usize,
    started: usize,
    max_running: usize,
}

/// The scripted engine behind a gate: a review stays open until every decision is done and a second review runs
/// beside it (the fifth runs alone). FakeEngine's own hook runs under its lock, one review at a time.
struct GatedEngine {
    inner: Arc<FakeEngine>,
    gate: Mutex<Gate>,
    changed: Condvar,
    daemon: std::sync::OnceLock<std::sync::Weak<Daemon>>,
}

impl review::engine::ReviewEngine for GatedEngine {
    fn review(&self, spec: &Value, signal: Option<&kernel::Signal>) -> Result<Value, String> {
        let daemon = self.daemon.get().and_then(std::sync::Weak::upgrade).expect("daemon");
        wait_until("every decision", || daemon.deciding() == 0);
        let mut gate = self.gate.lock().expect("gate");
        gate.active += 1;
        gate.started += 1;
        gate.max_active = gate.max_active.max(gate.active);
        gate.max_running = gate.max_running.max(daemon.running());
        self.changed.notify_all();
        let (mut gate, timeout) = self
            .changed
            .wait_timeout_while(gate, GUARD, |gate| gate.active < 2 && gate.started < 5)
            .expect("gate");
        assert!(!timeout.timed_out(), "a review waited alone");
        gate.active -= 1;
        drop(gate);
        self.inner.review(spec, signal)
    }
}

#[test]
fn review_daemon_concurrency_limit_and_one_pull_once() {
    let engine = FakeEngine::new(Vec::new());
    let mut context = setup(json!({ "concurrency": 2 }), Some(engine.clone()));
    for number in 1..=5 {
        add_numbered(&context, number, true);
        // The same pull request, found twice.
        let mut state = context.hub.lock();
        state.notifications.push(Notification::new("review_requested", number));
        state.notifications.push(Notification::new("mention", number));
    }
    let gated = Arc::new(GatedEngine {
        inner: engine.clone(),
        gate: Mutex::new(Gate::default()),
        changed: Condvar::new(),
        daemon: std::sync::OnceLock::new(),
    });
    context.deps.engine = gated.clone();
    let (instance, outcomes) = daemon(&context);
    let instance = Arc::new(instance);
    let _ = gated.daemon.set(Arc::downgrade(&instance));
    instance.once();
    let gate = gated.gate.lock().expect("gate");
    assert_eq!(gate.max_active, 2);
    assert_eq!(gate.max_running, 2);
    assert_eq!(engine.specs().len(), 5);
    assert_eq!(
        outcomes_of(&outcomes)
            .iter()
            .filter(|(_, kind)| kind == "posted")
            .count(),
        5
    );
}

#[test]
fn review_daemon_rate_limit_pauses_account() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    hub.fail(
        "GET notifications",
        403,
        &[("Retry-After", "300")],
        "secondary rate limit",
    );
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![]);
    let account = context.deps.store.read().accounts["github.com/ultron-bot"].clone();
    assert_eq!(account.paused_until.as_deref(), Some("2026-10-04T10:05:00.000Z"));
    assert_eq!(
        account.last_error.as_deref(),
        Some("GitHub rate limit: secondary rate limit")
    );
    assert!(context.logs().contains(
        "github.com/ultron-bot: GitHub rate limit: secondary rate limit; paused until 2026-10-04T10:05:00.000Z"
    ));
    let sent = hub.calls().len();
    instance.once();
    assert_eq!(hub.calls().len(), sent);
    hub.set_now("2026-10-04T10:06:00Z");
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "posted")]);
    // A successful poll clears the pause and the error.
    let account = context.deps.store.read().accounts["github.com/ultron-bot"].clone();
    assert_eq!((account.paused_until, account.last_error), (None, None));
}

#[test]
fn review_daemon_every_due_pull_acknowledged_at_once() {
    let engine = FakeEngine::new(Vec::new());
    let context = setup(json!({ "concurrency": 1 }), Some(engine.clone()));
    let hub = context.hub.clone();
    for number in 1..=3 {
        add_numbered(&context, number, true);
        hub.lock().notifications.push(Notification {
            updated_at: Some("2026-10-04T09:59:56Z".into()),
            ..Notification::new("review_requested", number)
        });
    }
    let acked = Arc::new(Mutex::new(Vec::new()));
    let (hook_hub, hook_acked) = (hub.clone(), acked.clone());
    let acknowledged = move |hub: &FakeHub| {
        hub.lock()
            .pulls
            .values()
            .filter(|p| p.issue_comments.len() == 1)
            .count()
    };
    // Whenever a review runs, every pull request has its acknowledgement: the decisions did not wait for the one
    // review slot (were they limited by it, this would wait for an acknowledgement that never comes).
    *engine.on_review.lock().expect("hook") = Some(Box::new(move || {
        wait_until("three acknowledgements", || acknowledged(&hook_hub) == 3);
        hook_acked.lock().expect("acked").push(acknowledged(&hook_hub));
        hook_hub.set_now("2026-10-04T10:00:50Z");
    }));
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(*acked.lock().expect("acked"), vec![3, 3, 3]);
    assert_eq!(
        outcomes_of(&outcomes)
            .iter()
            .filter(|(_, kind)| kind == "posted")
            .count(),
        3
    );
    // One acknowledgement each: the review did not post another.
    let comments: Vec<usize> = hub.lock().pulls.values().map(|p| p.issue_comments.len()).collect();
    assert_eq!(comments, vec![1, 1, 1]);
    // The order on GitHub: three acknowledgements, then the first review.
    let posts: Vec<String> = hub
        .api("^POST repos")
        .iter()
        .map(|c| c.path.replace("repos/o/r/", ""))
        .collect();
    let mut first = posts[..3].to_vec();
    first.sort();
    assert_eq!(
        first,
        vec!["issues/1/comments", "issues/2/comments", "issues/3/comments"]
    );
    assert!(
        regex::Regex::new(r"^pulls/[123]/reviews$")
            .expect("re")
            .is_match(&posts[3]),
        "{posts:?}"
    );
    let recent = context.deps.store.read().recent;
    assert_eq!(recent[0].tag_to_ack_ms.map(|n| n.0), Some(4_000.0));
    assert_eq!(recent[0].ack_to_post_ms.map(|n| n.0), Some(50_000.0));
    let logs = context.logs();
    assert!(
        logs.contains("acknowledged 1111111 (review requested), 4.0 s after the notification"),
        "{logs}"
    );
    assert!(
        regex::Regex::new(r"posted APPROVE for [123]{7}: .* 50\.0 s after the acknowledgement")
            .expect("re")
            .is_match(&logs),
        "{logs}"
    );
}

#[test]
fn review_daemon_bot_pull_skipped_unless_setting_off() {
    for (settings, kind) in [(json!({}), "skipped"), (json!({ "skipBots": false }), "posted")] {
        let context = setup(settings, None);
        let mut pull = FakePull::new("o", "r", 1).requested(&[BOT]);
        pull.head_sha = old();
        pull.author = "dependabot[bot]".into();
        context.hub.add_pull(pull);
        context.hub.lock().notifications = vec![Notification::new("review_requested", 1)];
        let (instance, outcomes) = daemon(&context);
        instance.once();
        assert_eq!(outcomes_of(&outcomes), vec![pair(1, kind)]);
        if kind == "skipped" {
            assert!(context.engine.specs().is_empty());
            assert!(context.logs().contains(
                "github.com/o/r#1 as ultron-bot: skipped (the author dependabot[bot] is a bot (autoreview.skipBots))"
            ));
        }
    }
}

#[test]
fn review_daemon_blocked_pulls_on_search_cycles_and_prune() {
    let engine = FakeEngine::new(vec![
        Ok(engine_result(json!({ "findings": [major()] }))),
        Ok(engine_result(json!({}))),
    ]);
    let context = setup(json!({}), Some(engine));
    let hub = &context.hub;
    let mut pull = FakePull::new("o", "r", 1).requested(&[BOT]);
    pull.head_sha = old();
    hub.add_pull(pull);
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(
        blocked_pulls(&context.deps.store.read(), &context.account),
        vec![reference()]
    );
    // A push, and no notification for it: found on the next search cycle because this account requested changes.
    hub.lock().notifications.clear();
    hub.with_pull(|pull| {
        pull.head_sha = new();
        pull.ancestors = vec![old()];
    });
    hub.set_now("2026-10-04T11:00:00Z");
    for _ in 0..4 {
        instance.once();
    }
    assert_eq!(outcomes_of(&outcomes).len(), 1);
    instance.once();
    let kinds: Vec<String> = outcomes_of(&outcomes).into_iter().map(|(_, kind)| kind).collect();
    assert_eq!(kinds, vec!["posted", "posted"]);
    let states: Vec<String> = hub.pull().reviews.iter().map(|r| r.state.clone()).collect();
    assert_eq!(states, vec!["CHANGES_REQUESTED", "APPROVED"]);
    // Approved now: no longer watched.
    assert_eq!(blocked_pulls(&context.deps.store.read(), &context.account), vec![]);

    let paths = &context.deps.paths;
    std::fs::create_dir_all(&paths.dry_run).expect("dry run");
    std::fs::create_dir_all(&paths.logs).expect("logs");
    let old_file = Path::new(&paths.dry_run).join("old.md");
    let old_log = Path::new(&paths.logs).join("autoreview-2026-09-01.log");
    let fresh = Path::new(&paths.logs).join("autoreview-2026-10-03.log");
    for file in [&old_file, &old_log, &fresh] {
        std::fs::write(file, "x").expect("write");
    }
    let day = 24 * 60 * 60 * 1000;
    set_mtime(&old_file, hub.now() - 15 * day);
    set_mtime(&old_log, hub.now() - 15 * day);
    set_mtime(&fresh, hub.now() - 13 * day);
    let missing = context.dir.path().join("missing").to_string_lossy().into_owned();
    assert_eq!(
        prune_old(&[&paths.dry_run, &paths.logs, &missing], hub.now(), RETENTION_DAYS),
        2
    );
    assert_eq!(
        [old_file.exists(), old_log.exists(), fresh.exists()],
        [false, false, true]
    );
}

#[test]
fn review_daemon_prunes_at_most_daily() {
    let context = setup(json!({}), None);
    let (instance, _) = daemon(&context);
    let logs = Path::new(&context.deps.paths.logs).to_path_buf();
    std::fs::create_dir_all(&logs).expect("logs");
    let day = 24 * 60 * 60 * 1000;
    let stale = |name: &str| {
        let path = logs.join(name);
        std::fs::write(&path, "x").expect("write");
        set_mtime(&path, context.hub.now() - 20 * day);
        path
    };
    let first = stale("first.log");
    instance.prune();
    assert!(!first.exists());
    assert!(
        context
            .logs()
            .contains("removed 1 dry-run and log files older than 14 days")
    );
    // Within the day: not pruned again.
    let second = stale("second.log");
    context.hub.advance(day - 1);
    instance.prune();
    assert!(second.exists());
    context.hub.advance(1);
    instance.prune();
    assert!(!second.exists());
}

#[test]
fn review_daemon_each_account_reviews_with_its_own_token() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    let second = hub.add_account("second", "github.com", false);
    hub.add_pull(FakePull::new("o", "r", 1).requested(&["second"]));
    hub.set_search(&[("review-requested:second", &[1])]);
    let (instance, _) = daemon_with(
        &context,
        vec![context.account.clone(), Account::new("second", "github.com")],
    );
    instance.once();
    let users: Vec<String> = hub.pull().reviews.iter().map(|r| r.user.clone()).collect();
    assert_eq!(users, vec!["second"]);
    let posts = hub.api(r"^POST repos/o/r/pulls/1/reviews$");
    assert_eq!(posts[0].env.get("GH_TOKEN"), Some(&second));
}

#[test]
fn review_daemon_log_lines_mask_a_token() {
    let context = setup(json!({}), None);
    let secrets = Arc::new(Mutex::new(Vec::<String>::new()));
    let register = secrets.clone();
    let tokens = TokenStore::new(context.hub.clone()).with_secret_sink(Arc::new(move |secret: &str| {
        register.lock().expect("secrets").push(secret.into())
    }));
    tokens.env(&context.account).expect("token");
    let written = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = written.clone();
    let clock = context.hub.clone();
    let log = create_logger(
        Some(context.deps.paths.logs.clone()),
        Arc::new(move || clock.now()),
        Arc::new(move |text: &str| sink.lock().expect("written").push(text.into())),
        Arc::new(move |line: &str| {
            let mut out = line.to_string();
            for secret in secrets.lock().expect("secrets").iter() {
                out = out.replace(secret.as_str(), "[REDACTED:github-token]");
            }
            out
        }),
    );
    log(&format!("gh failed with {} in its output", context.token));
    let path = Path::new(&context.deps.paths.logs).join("autoreview-2026-10-04.log");
    let file = std::fs::read_to_string(&path).expect("log file");
    assert!(file.contains("gh failed with [REDACTED:"));
    assert!(!file.contains(&context.token));
    assert_eq!(written.lock().expect("written").join(""), file);
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// ---- Failures, retries and the recorded queue ----

#[test]
fn review_daemon_failed_decision_retried_after_a_minute() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    hub.fail("GET repos/o/r/pulls/1", 500, &[], "boom");
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed")]);
    assert!(
        context
            .logs()
            .contains("github.com/o/r#1 as ultron-bot: the decision failed: GitHub GET repos/o/r/pulls/1: 500 boom")
    );
    // Waiting for its retry: recorded as found, not yet decided.
    assert_eq!(
        context.deps.store.read().queue,
        Some(vec!["github.com/ultron-bot github.com/o/r#1".to_string()])
    );
    assert_eq!(instance.queued(), 1);
    hub.lock().notifications.clear();
    hub.advance(59_999);
    instance.once();
    assert_eq!(outcomes_of(&outcomes).len(), 1);
    hub.advance(1);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed"), pair(1, "posted")]);
    assert_eq!(context.deps.store.read().queue, Some(vec![]));
}

#[test]
fn review_daemon_rate_limited_review_waits_until_resume() {
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    hub.fail("GET repos/o/r/pulls/1", 429, &[("Retry-After", "120")], "slow down");
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed")]);
    let account = context.deps.store.read().accounts["github.com/ultron-bot"].clone();
    assert_eq!(account.paused_until.as_deref(), Some("2026-10-04T10:02:00.000Z"));
    hub.lock().notifications.clear();
    hub.advance(119_999);
    let sent = hub.calls().len();
    instance.once();
    // Paused: no poll, no decision.
    assert_eq!(hub.calls().len(), sent);
    hub.advance(1);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed"), pair(1, "posted")]);
}

#[test]
fn review_daemon_last_error_cut_to_300_characters() {
    let context = setup(json!({}), None);
    context.hub.fail("GET notifications", 500, &[], &"x".repeat(1000));
    let (instance, _) = daemon(&context);
    instance.once();
    let account = context.deps.store.read().accounts["github.com/ultron-bot"].clone();
    let error = account.last_error.expect("last error");
    assert_eq!(error.chars().count(), 300);
    assert!(error.starts_with("GitHub GET notifications?all=true"));
    assert_eq!(account.paused_until, None);
}

#[test]
fn review_daemon_records_reviewing_and_waiting_queue_lines() {
    let engine = FakeEngine::new(Vec::new());
    let context = setup(json!({ "concurrency": 1 }), Some(engine.clone()));
    for number in 1..=2 {
        add_numbered(&context, number, true);
        context
            .hub
            .lock()
            .notifications
            .push(Notification::new("review_requested", number));
    }
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let hook_release = release.clone();
    let (started, starts) = mpsc::channel::<()>();
    let started = Mutex::new(started);
    *engine.on_review.lock().expect("hook") = Some(Box::new(move || {
        let _ = started.lock().expect("started").send(());
        let (lock, changed) = &*hook_release;
        let (open, timeout) = changed
            .wait_timeout_while(lock.lock().expect("release"), GUARD, |open| !*open)
            .expect("release");
        assert!(*open && !timeout.timed_out());
    }));
    let (instance, outcomes) = daemon(&context);
    let store = context.deps.store.clone();
    let worker = std::thread::spawn(move || instance.once());
    starts.recv_timeout(GUARD).expect("a review started");
    wait_until("the queue lines", || {
        let queue = store.read().queue.unwrap_or_default();
        queue.len() == 2 && queue[0].ends_with("(reviewing)") && queue[1].ends_with("(acknowledged, waiting)")
    });
    let queue = store.read().queue.expect("queue");
    assert!(queue[0].starts_with("github.com/ultron-bot github.com/o/r#"));
    *release.0.lock().expect("release") = true;
    release.1.notify_all();
    worker.join().expect("once");
    assert_eq!(outcomes_of(&outcomes).len(), 2);
    assert_eq!(store.read().queue, Some(vec![]));
}

// ---- run ----

/// A sleep that never ends before the test opens it: the daemon can only move on by an event.
fn blocked_sleep() -> (Arc<(Mutex<bool>, Condvar)>, review::daemon::Sleep) {
    let open = Arc::new((Mutex::new(false), Condvar::new()));
    let sleeper = open.clone();
    (
        open,
        Arc::new(move |_ms| {
            let (lock, changed) = &*sleeper;
            let _ = changed
                .wait_timeout_while(lock.lock().expect("sleep"), GUARD, |open| !*open)
                .expect("sleep");
        }),
    )
}

fn open(gate: &Arc<(Mutex<bool>, Condvar)>) {
    *gate.0.lock().expect("gate") = true;
    gate.1.notify_all();
}

#[test]
fn review_daemon_run_wakes_when_a_review_finishes_and_stops() {
    let context = setup(json!({ "concurrency": 1 }), None);
    for number in 1..=2 {
        add_numbered(&context, number, true);
        context
            .hub
            .lock()
            .notifications
            .push(Notification::new("review_requested", number));
    }
    let (sleep_gate, sleep) = blocked_sleep();
    let (told, tells) = mpsc::channel::<(u64, String)>();
    let told = Mutex::new(told);
    let instance = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts: vec![context.account.clone()],
        sleep: Some(sleep),
        on_outcome: Some(Arc::new(move |candidate, outcome: &Reported| {
            let _ = told
                .lock()
                .expect("told")
                .send((candidate.pull.number, outcome.kind().to_string()));
        })),
    });
    let stopper = instance.stopper();
    let worker = std::thread::spawn(move || instance.run());
    // With one review slot and a sleep that never ends, the second review starts only because the first one's end
    // wakes the loop.
    let mut seen: Vec<(u64, String)> = (0..2).map(|_| tells.recv_timeout(GUARD).expect("an outcome")).collect();
    seen.sort();
    assert_eq!(seen, vec![pair(1, "posted"), pair(2, "posted")]);
    stopper.stop();
    worker.join().expect("run");
    assert!(stopper.stopped());
    assert_eq!(context.hub.api("GET notifications").len(), 1);
    assert_eq!(context.deps.store.read().queue, Some(vec![]));
    open(&sleep_gate);
}

#[test]
fn review_daemon_run_lets_a_running_review_finish_after_stop() {
    let engine = FakeEngine::new(Vec::new());
    let context = setup(json!({}), Some(engine.clone()));
    context.hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    context.hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    let review_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let hook_gate = review_gate.clone();
    let (started, starts) = mpsc::channel::<()>();
    let started = Mutex::new(started);
    *engine.on_review.lock().expect("hook") = Some(Box::new(move || {
        let _ = started.lock().expect("started").send(());
        let (lock, changed) = &*hook_gate;
        let _ = changed
            .wait_timeout_while(lock.lock().expect("gate"), GUARD, |open| !*open)
            .expect("gate");
    }));
    let (sleep_gate, sleep) = blocked_sleep();
    let outcomes: Outcomes = Arc::new(Mutex::new(Vec::new()));
    let sink = outcomes.clone();
    let instance = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts: vec![context.account.clone()],
        sleep: Some(sleep),
        on_outcome: Some(Arc::new(move |candidate, outcome: &Reported| {
            sink.lock()
                .expect("outcomes")
                .push((candidate.pull.number, outcome.kind().to_string()));
        })),
    });
    let stopper = instance.stopper();
    let (finished, finishes) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        instance.run();
        let _ = finished.send(());
    });
    starts.recv_timeout(GUARD).expect("the review started");
    stopper.stop();
    // Stopped, but the review is still running: run has not returned.
    assert!(finishes.try_recv().is_err());
    open(&review_gate);
    finishes.recv_timeout(GUARD).expect("run returned");
    worker.join().expect("run");
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "posted")]);
    assert_eq!(context.deps.store.read().queue, Some(vec![]));
    open(&sleep_gate);
}

#[test]
fn review_daemon_run_polls_again_after_the_poll_interval() {
    let context = setup(json!({ "pollSeconds": 20 }), None);
    let polls = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (hub, record) = (context.hub.clone(), polls.clone());
    let instance = Arc::new(Mutex::new(None::<review::daemon::Stopper>));
    let stop_slot = instance.clone();
    // The sleep advances the fake clock by what was asked; the third poll stops the daemon.
    let sleep: review::daemon::Sleep = Arc::new(move |ms| {
        record.lock().expect("polls").push(ms);
        hub.advance(ms as i64);
        if hub.api("GET notifications").len() >= 3
            && let Some(stopper) = stop_slot.lock().expect("stopper").as_ref()
        {
            stopper.stop();
        }
    });
    let daemon = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts: vec![context.account.clone()],
        sleep: Some(sleep),
        on_outcome: None,
    });
    *instance.lock().expect("stopper") = Some(daemon.stopper());
    daemon.run();
    assert_eq!(context.hub.api("GET notifications").len(), 3);
    // 20 s between polls, slept in chunks of at most 5 s.
    let slept = polls.lock().expect("polls").clone();
    assert_eq!(&slept[..4], &[5_000, 5_000, 5_000, 5_000]);
    assert_eq!(
        parse_iso_ms(
            context.deps.store.read().accounts["github.com/ultron-bot"]
                .last_poll_at
                .as_deref()
                .expect("polled")
        ),
        Some(parse_iso_ms("2026-10-04T10:00:40Z").expect("date"))
    );
}

#[test]
fn review_daemon_found_again_waits_no_longer_than_the_new_find() {
    // A failed decision waits a minute; a notification for the same pull request on the next poll keeps the
    // earliest pickup (now) and merges into the waiting candidate.
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    hub.fail("GET repos/o/r/pulls/1", 500, &[], "boom");
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed")]);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed"), pair(1, "posted")]);
    assert_eq!(instance.queued(), 0);
}

#[test]
fn review_daemon_a_finished_decision_starts_its_review() {
    // Like the TypeScript's `finally(pump)`: a decision that ends starts the review itself, without the loop.
    let context = setup(json!({}), None);
    context.hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    context.hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    let (told, tells) = mpsc::channel::<(u64, String)>();
    let told = Mutex::new(told);
    let instance = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts: vec![context.account.clone()],
        sleep: None,
        on_outcome: Some(Arc::new(move |candidate, outcome: &Reported| {
            let _ = told
                .lock()
                .expect("told")
                .send((candidate.pull.number, outcome.kind().to_string()));
        })),
    });
    instance.poll();
    instance.prepare();
    assert_eq!(tells.recv_timeout(GUARD).expect("an outcome"), pair(1, "posted"));
}

#[test]
fn review_daemon_paused_account_decides_nothing() {
    // A candidate waiting for its retry is not decided while its account is paused by a rate limit on the poll.
    let context = setup(json!({}), None);
    let hub = &context.hub;
    hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    hub.fail("GET repos/o/r/pulls/1", 500, &[], "boom");
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed")]);
    hub.lock().notifications.clear();
    hub.advance(61_000);
    hub.fail(
        "GET notifications",
        403,
        &[("Retry-After", "300")],
        "secondary rate limit",
    );
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed")]);
    assert_eq!(hub.api("GET repos/o/r/pulls/1$").len(), 1);
    assert_eq!(instance.queued(), 1);
    hub.advance(300_000);
    instance.once();
    assert_eq!(outcomes_of(&outcomes), vec![pair(1, "failed"), pair(1, "posted")]);
}

/// Reviews in the engine at once by label, and the most large ones seen together.
#[derive(Default)]
struct SizeGate {
    active: Vec<String>,
    started: usize,
    max_active: usize,
    max_large: usize,
}

/// A review stays open until two have run at once (the third runs alone). On the high-water mark, not the count
/// now: the second may finish before the first wakes.
struct SizeGatedEngine {
    inner: Arc<FakeEngine>,
    large: Vec<String>,
    gate: Mutex<SizeGate>,
    changed: Condvar,
    daemon: std::sync::OnceLock<std::sync::Weak<Daemon>>,
}

impl review::engine::ReviewEngine for SizeGatedEngine {
    fn review(&self, spec: &Value, signal: Option<&kernel::Signal>) -> Result<Value, String> {
        let daemon = self.daemon.get().and_then(std::sync::Weak::upgrade).expect("daemon");
        wait_until("every decision", || daemon.deciding() == 0);
        let label = s(&spec["label"]).to_string();
        let mut gate = self.gate.lock().expect("gate");
        gate.active.push(label.clone());
        gate.started += 1;
        gate.max_active = gate.max_active.max(gate.active.len());
        let large = gate.active.iter().filter(|label| self.large.contains(label)).count();
        gate.max_large = gate.max_large.max(large);
        self.changed.notify_all();
        let (mut gate, timeout) = self
            .changed
            .wait_timeout_while(gate, GUARD, |gate| gate.max_active < 2 && gate.started < 3)
            .expect("gate");
        assert!(!timeout.timed_out(), "a review waited alone");
        gate.active.retain(|active| *active != label);
        drop(gate);
        self.inner.review(spec, signal)
    }
}

#[test]
fn review_daemon_large_reviews_leave_a_slot_to_the_rest() {
    // 2026-10-16: three large pull requests held every slot while a 40-second review waited 71 minutes.
    let engine = FakeEngine::new(Vec::new());
    let mut context = setup(json!({ "concurrency": 2 }), Some(engine.clone()));
    for number in 1..=3 {
        add_numbered(&context, number, true);
        context.hub.with_pull_n(number, |pull| {
            if number < 3 {
                pull.additions = 43_060;
                pull.deletions = 2_638;
            } else {
                pull.additions = 12;
            }
        });
        context
            .hub
            .lock()
            .notifications
            .push(Notification::new("review_requested", number));
    }
    let gated = Arc::new(SizeGatedEngine {
        inner: engine.clone(),
        large: vec!["o/r#1".into(), "o/r#2".into()],
        gate: Mutex::new(SizeGate::default()),
        changed: Condvar::new(),
        daemon: std::sync::OnceLock::new(),
    });
    context.deps.engine = gated.clone();
    let (instance, outcomes) = daemon(&context);
    let instance = Arc::new(instance);
    let _ = gated.daemon.set(Arc::downgrade(&instance));
    instance.once();
    let gate = gated.gate.lock().expect("gate");
    // Two at a time, never both large ones: the small one ran beside a large one.
    assert_eq!((gate.max_active, gate.max_large), (2, 1));
    assert_eq!(
        outcomes_of(&outcomes)
            .iter()
            .filter(|(_, kind)| kind == "posted")
            .count(),
        3
    );
}

#[test]
fn review_daemon_large_slots_keep_one_for_the_rest() {
    use review::daemon::{LARGE_PULL_LINES, is_large, large_slots};
    assert_eq!([1, 2, 3, 8].map(large_slots), [1, 1, 2, 7]);
    let pull = |additions, deletions| review::github::PullRequest {
        additions,
        deletions,
        ..Default::default()
    };
    assert!(is_large(&pull(LARGE_PULL_LINES - 1, 1)));
    assert!(!is_large(&pull(LARGE_PULL_LINES - 2, 1)));
    // GitHub did not say: not large.
    assert!(!is_large(&pull(0, 0)));
}

#[test]
fn review_daemon_a_cycle_that_finds_nothing_new_leaves_the_state_file_alone() {
    use std::os::unix::fs::MetadataExt;
    // 2026-10-21: each poll's decisions rewrote the whole state file once per pull request it saw.
    let context = setup(json!({}), Some(FakeEngine::new(Vec::new())));
    for number in 1..=5 {
        add_numbered(&context, number, true);
    }
    let notify = || {
        let mut state = context.hub.lock();
        state.notifications = (1..=5)
            .map(|number| Notification::new("review_requested", number))
            .collect();
    };
    notify();
    let (instance, outcomes) = daemon(&context);
    instance.once();
    assert_eq!(outcomes_of(&outcomes).len(), 5);
    let path = context.deps.store.path.clone();
    let inode = || std::fs::metadata(&path).expect("state").ino();
    let before = inode();
    // The same five again, already reviewed: decided, skipped, and nothing in the state changes.
    notify();
    instance.once();
    assert_eq!(
        outcomes_of(&outcomes)
            .iter()
            .filter(|(_, kind)| kind == "skipped")
            .count(),
        5
    );
    assert_eq!(inode(), before, "the state file was rewritten");
}
