//! The footer's git branch, `(sub)` and Claude Code usage through the App: replays fixtures/footer-data.json,
//! recorded from the client TUI's `NativeFooter` (Pi's FooterComponent over FooterDataProvider) by
//! fixtures/generate-footer-data.mjs. Each case writes its files under a private temp root (also the home), starts
//! the footer branch watcher there (`git` from the case's PATH only), opens the transcript, and the rendered screen
//! must hold the TypeScript footer lines exactly. Plus the branch following a `HEAD` change, and the Rust root's
//! `claude-subscription` provider read as Claude Code (literal expectations).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use interactive::app::{App, AppOptions};
use interactive::footer_data::{FooterBranch, claude_code_usage_text, find_git_paths, read_branch};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

struct Quiet(RefCell<Vec<String>>);

impl LaneClient for Quiet {
    fn prompt(&self, text: &str) {
        self.0.borrow_mut().push(text.into());
    }
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, _: &str) -> String {
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
}

struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn root(tag: &str) -> Root {
    let dir = std::env::temp_dir().join(format!("footer-data-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Root(dir)
}

/// Run a script this test just wrote until it starts: a thread that forked while the file was open for writing
/// keeps the write descriptor until its own exec, and an exec in that window fails with ETXTBSY.
fn wait_executable(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match std::process::Command::new(path).output() {
            Err(e) if e.raw_os_error() == Some(26) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => return,
        }
    }
}

fn write_files(root: &Path, files: &Value) {
    for (path, content) in files.as_object().unwrap() {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, content.as_str().unwrap()).unwrap();
        if path.starts_with("bin/") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            wait_executable(&file);
        }
    }
}

fn model_info(m: &Value) -> ModelInfo {
    ModelInfo {
        provider: m["provider"].as_str().unwrap().into(),
        id: m["id"].as_str().unwrap().into(),
        name: m["name"].as_str().unwrap().into(),
        reasoning: m["reasoning"] == true,
        context_window: m["contextWindow"].as_u64().unwrap(),
        thinking_levels: Vec::new(),
    }
}

fn app(cwd: &Path, home: &Path, model: ModelInfo, models: Vec<ModelInfo>) -> App {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let cwd = cwd.display().to_string();
    App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: cwd.clone(),
            home: Some(home.display().to_string()),
            server_id: "server".into(),
            models,
            model,
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, &cwd),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::parity(),
        },
        Box::new(Quiet(RefCell::default())),
    )
}

fn wait_read(branch: &FooterBranch) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !branch.has_read() {
        assert!(Instant::now() < deadline, "the footer branch was never read");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `want` as consecutive rows of `screen`.
fn assert_rows(screen: &[String], want: &[String], label: &str) {
    let found = screen.windows(want.len()).any(|w| w == want);
    assert!(
        found,
        "{label}: footer differs from TypeScript\n--- want\n{}\n--- screen\n{}",
        want.join("\n"),
        screen.join("\n")
    );
}

#[test]
fn interactive_footer_data_branch_sub_and_usage_match_the_ts_native_footer() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/footer-data.json")).unwrap();
    let width = fixture["width"].as_u64().unwrap() as usize;
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let root = root(name);
        write_files(&root.0, &case["files"]);
        let cwd = root.0.join(case["cwd"].as_str().unwrap());
        let path_var = case["path"].as_str().map(|p| root.0.join(p).display().to_string());
        let messages = case["messages"].as_array().unwrap().clone();
        assert_eq!(
            claude_code_usage_text(&messages).map_or(Value::Null, Value::from),
            case["usageText"],
            "{name}: usage text"
        );
        let models: Vec<ModelInfo> = case["models"].as_array().unwrap().iter().map(model_info).collect();
        let mut app = app(&cwd, &root.0, model_info(&case["model"]), models);
        let branch = FooterBranch::spawn(cwd.clone(), path_var, || {});
        wait_read(&branch);
        app.set_footer_branch(branch);
        app.poll_services();
        app.apply(LaneEvent::Opened(SessionOpened {
            session_id: "sess".into(),
            session_file: None,
            name: case["sessionName"].as_str().map(str::to_string),
            messages,
        }));
        let want: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        assert_rows(&app.render(width, 30), &want, name);
    }
}

#[test]
fn interactive_footer_data_git_paths_like_find_git_paths() {
    let root = root("paths");
    write_files(
        &root.0,
        &json!({
            "repo/.git/HEAD": "ref: refs/heads/main\n",
            "repo/.git/worktrees/wt/HEAD": "ref: refs/heads/topic\n",
            "repo/.git/worktrees/wt/commondir": "../..\n",
            "wt/a/b/.keep": "",
            "wt/.git": "gitdir: ../repo/.git/worktrees/wt\n",
        }),
    );
    let paths = find_git_paths(&root.0.join("wt/a/b")).unwrap();
    assert_eq!(paths.repo_dir, root.0.join("wt"));
    assert_eq!(paths.head_path, root.0.join("repo/.git/worktrees/wt/HEAD"));
    assert_eq!(paths.common_git_dir, root.0.join("repo/.git"));
    assert_eq!(read_branch(Some(&paths), None).as_deref(), Some("topic"));
    let paths = find_git_paths(&root.0.join("repo")).unwrap();
    assert_eq!(paths.common_git_dir, root.0.join("repo/.git"));
    assert_eq!(read_branch(None, None), None);
}

/// `HEAD` rewritten (a checkout): the watcher reads the new branch and the footer shows it.
#[test]
fn interactive_footer_data_branch_follows_a_checkout() {
    let root = root("follow");
    write_files(&root.0, &json!({"repo/.git/HEAD": "ref: refs/heads/main\n"}));
    let cwd = root.0.join("repo");
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let tx = std::sync::Mutex::new(tx);
    let branch = FooterBranch::spawn(cwd.clone(), None, move || {
        let _ = tx.lock().unwrap().send(());
    });
    rx.recv_timeout(Duration::from_secs(120)).expect("first read");
    assert_eq!(branch.branch().as_deref(), Some("main"));
    // A rename over HEAD, as git writes it.
    std::fs::write(cwd.join(".git/HEAD.lock"), "ref: refs/heads/feature-long-name\n").unwrap();
    std::fs::rename(cwd.join(".git/HEAD.lock"), cwd.join(".git/HEAD")).unwrap();
    rx.recv_timeout(Duration::from_secs(120))
        .expect("the checkout is noticed");
    assert_eq!(branch.branch().as_deref(), Some("feature-long-name"));
    let mut app = app(
        &cwd,
        &root.0,
        model_info(
            &json!({"provider": "anthropic", "id": "m", "name": "M", "reasoning": false, "contextWindow": 1000}),
        ),
        Vec::new(),
    );
    app.set_footer_branch(branch);
    assert!(app.poll_services());
    let screen = app.render(80, 20).join("\n");
    assert!(screen.contains("~/repo (feature-long-name)"), "{screen}");
}

/// The Rust root under `--claude` runs on `claude-subscription`: its responses carry Claude Code's usage and its
/// costs are the plan's, as the TypeScript footer treats `claude-code`.
#[test]
fn interactive_footer_data_claude_subscription_reads_as_claude_code() {
    let root = root("subscription");
    let usage = json!({"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}});
    let messages = vec![
        json!({"role": "assistant", "provider": "claude-subscription", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": "ok"}], "usage": usage, "stopReason": "stop", "timestamp": 1,
        "diagnostics": [{"type": "claude_code_usage", "timestamp": 0,
            "details": {"status": "rejected", "windows": {"five_hour": {"utilization": 1}, "seven_day": {"utilization": 0.42}}}}]}),
    ];
    assert_eq!(
        claude_code_usage_text(&messages).as_deref(),
        Some("Claude Code 5h 100% · 7d 42% (limit reached)")
    );
    let model = model_info(
        &json!({"provider": "claude-subscription", "id": "claude-opus-5-5", "name": "Opus",
        "reasoning": false, "contextWindow": 200000}),
    );
    let mut app = app(&root.0, &root.0, model.clone(), vec![model]);
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages,
    }));
    let screen: Vec<String> = app
        .render(100, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect();
    assert!(
        screen.iter().any(|l| l.starts_with("↑10 ↓5 $0.000 (sub) ")),
        "{screen:?}"
    );
    assert_eq!(
        screen.last().map(String::as_str),
        Some("Claude Code 5h 100% · 7d 42% (limit reached)")
    );
}
