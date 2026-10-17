//! Opening links from the TUI (tui-renderer.ts passes `openUrl: openBrowser` to the fullscreen screen): a primary
//! click on an OSC 8 link in the fullscreen transcript opens it, and so does a link chosen in the `/copy` picker.
//! The launcher is a fake `xdg-open` (`open` on macOS) on a PATH the test injects, which records its arguments and
//! environment; the real PATH, DISPLAY and browser are never used. The click rule itself is replayed against the
//! TypeScript in crates/tui/tests/fullscreen_clicks.rs.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::render_settings::{self, RenderSettings};
use interactive::url_opener::{browser_command, find_launcher, launcher_name, url_opener};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

const HANG_GUARD: Duration = Duration::from_secs(120);
const URL: &str = "https://example.com/docs?q=1";
const W: usize = 60;
const H: usize = 16;

/// A private directory, removed at the end of the test.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("link-click-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Quiet;

impl LaneClient for Quiet {
    fn prompt(&self, _: &str) {}
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
    fn save_setting(&self, _: &str, _: Value) {}
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

/// An app on one turn whose answer is `text`.
fn app(qol: QolSettings, text: &str) -> App {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "off".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: vec![
            json!({"role": "user", "content": "where are the docs?"}),
            json!({"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": text}]}),
        ],
    }));
    app
}

/// A PATH holding only the fake launcher, which writes its arguments and some of its environment to `out` (shell
/// builtins only: nothing else is on that PATH), ending with `end`.
fn fake_launcher(dir: &Path) -> (String, PathBuf) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let out = dir.join("opened.txt");
    let script = bin.join(launcher_name());
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n{{ for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; printf 'PATH=%s\\nHOME=%s\\nDISPLAY=%s\\nCARGO_PKG_NAME=%s\\nend\\n' \"$PATH\" \"$HOME\" \"${{DISPLAY-unset}}\" \"${{CARGO_PKG_NAME-unset}}\"; }} > '{0}'\n",
            out.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (bin.display().to_string(), out)
}

/// The launcher's record, once it has written all of it.
fn wait_for(out: &Path) -> Vec<String> {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        if let Ok(text) = std::fs::read_to_string(out)
            && let Some(record) = text.strip_suffix("end\n")
        {
            return record.lines().map(str::to_string).collect();
        }
        assert!(Instant::now() < deadline, "the launcher never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn screen(app: &mut App) -> Vec<String> {
    app.render(W, H)
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect()
}

#[test]
fn interactive_fullscreen_click_on_a_markdown_link_opens_it_with_the_session_launcher() {
    render_settings::set(RenderSettings {
        hyperlinks: true,
        ..RenderSettings::default()
    });
    let dir = TempDir::new("markdown");
    let (path, out) = fake_launcher(dir.path());
    let mut app = app(QolSettings::parity(), &format!("Read [the manual]({URL}) first."));
    app.set_url_opener(url_opener(Some(path.clone()), Some("/h".into())));
    let rows = screen(&mut app);
    let row = rows.iter().position(|l| l.contains("the manual")).unwrap();
    let col = rows[row].find("the manual").unwrap() + 2;
    // A drag over the link selects text and opens nothing; a click opens it.
    app.handle_input(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    app.handle_input(&format!("\x1b[<32;{};{}M", col + 3, row + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", col + 3, row + 1));
    screen(&mut app);
    assert!(!out.exists());
    app.handle_input(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", col + 1, row + 1));
    // Only the session's PATH and the given HOME reach the launcher: nothing of this process's environment (cargo
    // gives the test process CARGO_PKG_NAME).
    assert_eq!(
        wait_for(&out),
        [
            format!("arg={URL}"),
            format!("PATH={path}"),
            "HOME=/h".into(),
            "DISPLAY=unset".into(),
            "CARGO_PKG_NAME=unset".into()
        ]
    );
    render_settings::set(RenderSettings::default());
}

#[test]
fn interactive_copy_picker_opens_a_link_with_the_session_launcher() {
    let dir = TempDir::new("picker");
    let (path, out) = fake_launcher(dir.path());
    let mut app = app(QolSettings::default(), &format!("Docs at {URL}"));
    app.set_url_opener(url_opener(Some(path), None));
    for data in ["/copy", "\r"] {
        app.handle_input(data);
    }
    app.render(W, H);
    // Into the turn's blocks: the link is the only one; `o` opens it.
    app.handle_input("\x1b[C");
    app.render(W, H);
    app.handle_input("o");
    assert_eq!(wait_for(&out)[0], format!("arg={URL}"));
    let rows = screen(&mut app);
    assert!(rows.iter().any(|l| l.contains(&format!("Opened {URL}"))), "{rows:#?}");
}

#[test]
fn interactive_copy_picker_without_an_opener_opens_nothing() {
    let mut app = app(QolSettings::default(), &format!("Docs at {URL}"));
    for data in ["/copy", "\r"] {
        app.handle_input(data);
    }
    app.render(W, H);
    app.handle_input("\x1b[C");
    app.render(W, H);
    app.handle_input("o");
    let rows = screen(&mut app);
    assert!(rows.iter().any(|l| l.contains("Error: no browser opener")), "{rows:#?}");
}

/// `openBrowser`'s launcher per platform, looked up on the given PATH only (first match wins, `None` finds none).
#[test]
fn interactive_url_opener_finds_the_launcher_on_the_given_path_only() {
    assert_eq!(browser_command("darwin", URL), ("open".into(), vec![URL.to_string()]));
    assert_eq!(
        browser_command("win32", URL),
        (
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), URL.to_string()]
        )
    );
    assert_eq!(
        browser_command("linux", URL),
        ("xdg-open".into(), vec![URL.to_string()])
    );
    let dir = TempDir::new("lookup");
    let (first, second, empty) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));
    for d in [&first, &second, &empty] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(first.join("xdg-open"), "").unwrap();
    std::fs::write(second.join("xdg-open"), "").unwrap();
    std::fs::create_dir_all(empty.join("xdg-open")).unwrap();
    let joined = |dirs: &[&PathBuf]| std::env::join_paths(dirs).unwrap().into_string().unwrap();
    assert_eq!(
        find_launcher(Some(&joined(&[&empty, &second, &first])), "xdg-open"),
        Some(second.join("xdg-open")),
        "a directory of that name is not a launcher"
    );
    assert_eq!(find_launcher(Some(&joined(&[&empty])), "xdg-open"), None);
    assert_eq!(find_launcher(None, "xdg-open"), None);
}
