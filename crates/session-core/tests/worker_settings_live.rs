//! The settings a TypeScript session worker applies (session-worker.ts, worker-settings.ts, native-host.ts), through
//! a session actor against a scripted Anthropic server on loopback (p_session_support):
//! - the worker's project trust (`workerProjectTrusted`): a trusted project's `.pi/settings.json` overrides the
//!   thinking level and budgets, compaction and the request options; `defaultProjectTrust: "never"` keeps it out;
//! - `providerRequestOptions` (`retry.provider.*`, else the idle timeout) and `transport` as the operation's
//!   `streamOptions`;
//! - `rlm.frameModel` / `rlm.frameThinking` / `rlm.childModel` / `review.model` read per frame and per spawn, so a
//!   changed setting applies without a restart (a `claude-code/*` child model is dropped);
//! - the `worktrees` setting as every worktree child's default setup, and `ULTRON_KEEP_WORKTREES`.
//!
//! TS values come from crates/config/fixtures/worker-settings.json (generate-worker-settings.mjs) where recorded;
//! the rest are literal expectations from the cited TypeScript.

mod p_session_support;

use p_session_support::{HANG, Setup, actor, text, tool, tool_results};
use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The first user text that is not a clock line (subagent lanes open with one, clock-context.ts).
fn first_user_text(body: &Value) -> String {
    let texts = body["messages"].as_array().into_iter().flatten().map(|message| {
        let content = &message["content"];
        match content {
            Value::String(text) => text.clone(),
            _ => content[0]["text"].as_str().unwrap_or_default().to_string(),
        }
    });
    texts
        .into_iter()
        .find(|text| !text.starts_with("[clock] "))
        .unwrap_or_default()
}

/// Waits (hang guard only) until `found` returns something for the requests so far.
fn wait_for<T>(a: &p_session_support::Actor, what: &str, found: impl Fn(&[Value]) -> Option<T>) -> T {
    let deadline = Instant::now() + HANG;
    loop {
        if let Some(value) = found(&a.requests()) {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        other => other[0]["text"].as_str().unwrap_or_default().to_string(),
    }
}

fn stream_options(a: &p_session_support::Actor) -> Vec<Value> {
    a.writes()
        .iter()
        .filter_map(|w| w["value"]["generationContext"].get("streamOptions").cloned())
        .collect()
}

/// The `streamOptions` of every durable generation request checkpoint (`phase: "request"`) the journal holds.
fn checkpoint_stream_options(a: &p_session_support::Actor) -> Vec<Value> {
    fn walk(value: &Value, out: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                if map.get("phase") == Some(&json!("request"))
                    && map.contains_key("cutoff")
                    && let Some(options) = map.get("streamOptions")
                {
                    out.push(options.clone());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for write in a.writes() {
        walk(&write, &mut out);
    }
    out
}

fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(cwd: &Path, home: &Path) {
    git(cwd, home, &["init", "-q"]);
    std::fs::write(cwd.join("README.md"), "repo\n").unwrap();
    git(cwd, home, &["add", "README.md"]);
    let identity = ["-c", "user.email=t@t", "-c", "user.name=t"];
    git(cwd, home, &[&identity[..], &["commit", "-q", "-m", "init"]].concat());
}

/// The project's settings of the fixture's trust scenarios, with budgets for the thinking levels.
fn project_settings() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("../../config/fixtures/worker-settings.json")).unwrap();
    let mut project = fixture["project"].clone();
    project["thinkingBudgets"] = json!({"medium": 1111, "high": 2222});
    project
}

#[test]
fn session_core_trusted_project_settings_reach_the_session_and_untrusted_ones_do_not() {
    let fixture: Value = serde_json::from_str(include_str!("../../config/fixtures/worker-settings.json")).unwrap();
    let scenario = |name: &str| {
        fixture["trust"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()
            .clone()
    };
    for (name, global) in [
        ("undecided-ask", json!({})),
        ("default-never", json!({"defaultProjectTrust": "never"})),
    ] {
        let expected = scenario(name);
        let trusted = expected["trusted"] == true;
        let a = actor(
            p_session_support::in_order(vec![text("done")]),
            Setup {
                settings: Some(global),
                files: vec![(".pi/settings.json", project_settings().to_string().into_bytes())],
                ..Default::default()
            },
        );
        let end = a.prompt("go");
        assert!(end["error"].is_null(), "{name}: {end}");
        let state = a.call(Op::State).unwrap().unwrap();
        // defaultThinkingLevel and compaction.enabled of the project (TS getDefaultThinkingLevel / getCompactionSettings).
        let level = expected["defaultThinkingLevel"]["value"].as_str().unwrap_or("medium");
        assert_eq!(state["thinkingLevel"], level, "{name}");
        assert_eq!(
            state["autoCompactionEnabled"], expected["compaction"]["value"]["enabled"],
            "{name}"
        );
        let request = &a.requests()[0];
        // The project's thinkingBudgets: 2222 for its "high"; without trust, Pi's own medium budget.
        let budget = &request["thinking"]["budget_tokens"];
        if trusted {
            assert_eq!(budget, 2222, "{name}: {}", request["thinking"]);
        } else {
            assert!(budget != 2222 && budget != 1111, "{name}: {}", request["thinking"]);
        }
        // providerRequestOptions as the operation's streamOptions, with the project's transport when trusted.
        let mut options = expected["providerRequestOptions"]["value"].clone();
        if expected["transport"]["value"] != "auto" {
            options["transport"] = expected["transport"]["value"].clone();
        }
        let recorded = stream_options(&a);
        assert!(!recorded.is_empty(), "{name}");
        assert!(
            recorded.iter().all(|o| *o == options),
            "{name}: {recorded:?} != {options}"
        );
        // The durable generation's request checkpoint pins the same options.
        let pinned = checkpoint_stream_options(&a);
        assert!(!pinned.is_empty(), "{name}");
        assert!(pinned.iter().all(|o| *o == options), "{name}: {pinned:?} != {options}");
    }
}

const MODELS_CELL: &str =
    "import json\nm = await rlm.host_request('rlm.models', {})\nprint(json.dumps(m, sort_keys=True))";

#[test]
fn session_core_model_settings_are_read_per_frame_and_per_spawn() {
    let cells = [
        ("first", "toolu_models_1", MODELS_CELL.to_string()),
        (
            "second",
            "toolu_models_2",
            format!("{MODELS_CELL}\nh = await rlm.spawn('child task', name='scout')\nprint('spawned')"),
        ),
        // Within one cell, `/settings -> Models` changes the profile between two frames.
        (
            "third",
            "toolu_models_3",
            format!(
                "{MODELS_CELL}\nopen('../agent/settings.json', 'w').write(json.dumps({{'review': {{'model': 'anthropic/claude-opus-4-5'}}}}))\nm = await rlm.host_request('rlm.models', {{}})\nprint(json.dumps(m, sort_keys=True))"
            ),
        ),
    ];
    // The parent answers its latest prompt with that prompt's cell, then (its result seen) with text.
    let script = Box::new(move |body: &Value, _| {
        if first_user_text(body).contains("child task") {
            return text("child done");
        }
        let serialized = body["messages"].to_string();
        let latest = cells
            .iter()
            .rev()
            .find(|(prompt, _, _)| serialized.contains(&format!("\"text\":\"{prompt}\"")));
        match latest {
            Some((_, id, _)) if serialized.contains(&format!("\"tool_use_id\":\"{id}\"")) => text("ok"),
            Some((_, id, code)) => tool(id, code),
            None => text("(no more)"),
        }
    });
    let settings = json!({
        "rlm": {"frameModel": "anthropic/claude-haiku-4-5", "childModel": "claude-code/sonnet", "frameThinking": "bogus"},
        "review": {"model": "anthropic/claude-opus-4-5"},
        "reviewModel": "anthropic/claude-sonnet-4-6",
    });
    let a = actor(
        script,
        Setup {
            settings: Some(settings),
            ..Default::default()
        },
    );
    let models_text = |id: &str| -> String {
        wait_for(&a, id, |requests| {
            requests.iter().find_map(|r| {
                let blocks = r["messages"].as_array()?;
                blocks.iter().find_map(|m| {
                    m["content"].as_array()?.iter().find_map(|b| {
                        (b["type"] == "tool_result" && b["tool_use_id"] == id).then(|| result_text(&b["content"]))
                    })
                })
            })
        })
    };
    let models = |id: &str| -> Value {
        let text = models_text(id);
        serde_json::from_str(text.lines().next().unwrap()).unwrap_or_else(|_| panic!("{text}"))
    };
    assert!(a.prompt("first")["error"].is_null());
    // `review.model` is the /review frame model (not the old `reviewModel` key); a claude-code child model is dropped.
    assert_eq!(
        models("toolu_models_1"),
        json!({
            "frame": {"model": "anthropic/claude-haiku-4-5", "source": "setting"},
            "review": {"model": "anthropic/claude-opus-4-5", "source": "setting"},
            "child": {"source": "session"},
            "frameThinking": null,
        })
    );
    // `/settings -> Models` writes the profile's settings; the next frame and spawn read them.
    let agent = a.root.join("agent");
    std::fs::write(
        agent.join("settings.json"),
        json!({"rlm": {"childModel": "anthropic/claude-haiku-4-5", "frameThinking": "low"}}).to_string(),
    )
    .unwrap();
    assert!(a.prompt("second")["error"].is_null());
    assert_eq!(
        models("toolu_models_2"),
        json!({
            "frame": {"source": "session"},
            "review": {"source": "session"},
            "child": {"model": "anthropic/claude-haiku-4-5", "source": "setting"},
            "frameThinking": "low",
        })
    );
    // The spawn named no model: the child runs on rlm.childModel as it is now.
    let child = wait_for(&a, "the child's request", |requests| {
        requests
            .iter()
            .find(|r| first_user_text(r).contains("child task"))
            .cloned()
    });
    assert_eq!(child["model"], "claude-haiku-4-5", "{}", child["model"]);
    assert!(a.prompt("third")["error"].is_null());
    let text = models_text("toolu_models_3");
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("{text}")))
        .collect();
    assert_eq!(lines[0]["review"], json!({"source": "session"}), "{text}");
    assert_eq!(
        lines[1]["review"],
        json!({"model": "anthropic/claude-opus-4-5", "source": "setting"}),
        "{text}"
    );
}

/// A worktree child's lane: its cell reads what the `worktrees` setup command left in its worktree.
fn worktree_actor(keep: bool) -> p_session_support::Actor {
    let parent = [
        tool(
            "toolu_spawn",
            "h = await rlm.spawn('child task', name='builder', worktree=True)\nprint('spawned')",
        ),
        text("waiting"),
    ];
    let child = [
        tool(
            "toolu_child",
            "print(open('setup-mark.txt').read().strip())\nawait write('work.md', 'work')",
        ),
        text("child done"),
    ];
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let script = Box::new(move |body: &Value, _| {
        let mut counters = counters.lock().unwrap();
        if first_user_text(body).contains("child task") {
            counters.1 += 1;
            child.get(counters.1 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        } else {
            counters.0 += 1;
            parent.get(counters.0 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        }
    });
    let mut vars = Vec::new();
    if keep {
        vars.push(("ULTRON_KEEP_WORKTREES", "1".to_string()));
    }
    let a = actor(
        script,
        Setup {
            settings: Some(json!({"worktrees": {"setupCommand": "  echo from-setup > setup-mark.txt  ", "link": []}})),
            vars,
            ..Default::default()
        },
    );
    init_repo(&a.cwd, &a.root);
    a
}

/// The worktree directories Ultron has in the repository at `cwd` (`.git/ultron-worktrees/<id>`).
fn worktree_dirs(cwd: &Path) -> Vec<std::path::PathBuf> {
    let dir = cwd.join(".git").join(delegation::worktrees::WORKTREES_DIR);
    let mut dirs: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn close(a: &p_session_support::Actor) {
    let (done, closed) = std::sync::mpsc::channel();
    a.tx.send(Input::Close(done)).unwrap();
    closed.recv_timeout(HANG).unwrap();
}

#[test]
fn session_core_worktrees_setting_sets_up_children_and_keep_worktrees_keeps_them() {
    for keep in [false, true] {
        let a = worktree_actor(keep);
        assert!(a.prompt("go")["error"].is_null());
        // The setup command (trimmed setupCommand) ran in the child's worktree before the child started.
        let seen = wait_for(&a, "the child's cell result", |requests| {
            requests
                .iter()
                .filter(|r| first_user_text(r).contains("child task"))
                .flat_map(tool_results)
                .map(|c| result_text(&c))
                .next()
        });
        assert_eq!(seen.lines().next(), Some("from-setup"), "{seen}");
        // The child's end re-invokes the root: its worktree has ended unmerged.
        wait_for(&a, "the root's next turn", |requests| {
            (requests
                .iter()
                .filter(|r| !first_user_text(r).contains("child task"))
                .count()
                >= 3)
                .then_some(())
        });
        assert_eq!(worktree_dirs(&a.cwd).len(), 1, "keep={keep}");
        close(&a);
        // On close an ended child's directory goes (its branch keeps the work), unless ULTRON_KEEP_WORKTREES.
        assert_eq!(worktree_dirs(&a.cwd).len(), usize::from(keep), "keep={keep}");
    }
}

/// A pid that is certainly not running: a child this test spawned and reaped.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn session_core_opening_a_session_prunes_orphan_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (agent, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    init_repo(&cwd, &root);
    // A worktree of an earlier session whose process died: its record names a dead owner.
    let record = delegation::worktrees::create_child_worktree(&delegation::worktrees::CreateWorktreeOptions {
        cwd: &cwd,
        session_id: "crashed",
        name: "orphan",
        setup: None,
        now: 0.0,
    })
    .unwrap();
    let record_file = cwd
        .join(".git")
        .join(delegation::worktrees::WORKTREES_DIR)
        .join(format!("{}.json", record.id));
    let mut saved: delegation::worktrees::WorktreeRecord =
        serde_json::from_str(&std::fs::read_to_string(&record_file).unwrap()).unwrap();
    saved.owner.pid = dead_pid();
    std::fs::write(&record_file, delegation::worktrees::record_json(&saved)).unwrap();
    assert!(Path::new(&record.path).exists());
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.display().to_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    let env = session_core::RunEnv {
        cwd: cwd.clone(),
        home: root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = session_core::compose::Runtime::new(
        &env,
        "rpc",
        root.join("sessions"),
        session_core::compose::Services::default(),
    )
    .unwrap();
    let model = session_core::lane::catalog_model("anthropic", "claude-sonnet-4-5", &env).unwrap();
    let _stack = runtime.stack(&model, None, "fresh").unwrap();
    // Pruned in the background: the directory goes, the record with it.
    let deadline = Instant::now() + HANG;
    while Path::new(&record.path).exists() || record_file.exists() {
        assert!(Instant::now() < deadline, "the orphan worktree was never pruned");
        std::thread::sleep(Duration::from_millis(10));
    }
}
