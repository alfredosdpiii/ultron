//! The Rust-only additions of the interactive TUI (crate::qol): each one works when on and is absent in parity.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, PromptImage, SessionOpened};
use interactive::qol::{GitSource, QolSettings, StatusSymbols, from_settings};
use interactive::run::{UiEvent, paint_loop};
use serde_json::{Value, json};
use tui::clipboard::{Clipboard, ClipboardService};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::notify::{Message, TurnOutcome};
use tui::status_git::GitSnapshot;
use tui::status_line::{GitCounts, Preset, SegmentId, SeparatorStyle};
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};
use tui::tty_writer::{MAX_PENDING_OUTPUT_BYTES, TtyWriter};
use tui::utils::strip_terminal_sequences;
use tui::viewport::{HistoryKind, ViewportRenderer};
use tui::vim::VimMode;

#[derive(Default)]
struct Calls(RefCell<Vec<String>>);

struct Recorder(Rc<Calls>);

impl LaneClient for Recorder {
    fn prompt(&self, text: &str) {
        self.0.0.borrow_mut().push(format!("prompt {text}"));
    }
    fn prompt_with_images(&self, text: &str, images: Vec<PromptImage>) {
        let mimes: Vec<String> = images
            .iter()
            .map(|i| format!("{}:{}", i.mime_type, i.data.len()))
            .collect();
        self.0
            .0
            .borrow_mut()
            .push(format!("prompt_with_images {text} [{}]", mimes.join(",")));
    }
    fn steer(&self, text: &str) -> String {
        self.0.0.borrow_mut().push(format!("steer {text}"));
        "s1".into()
    }
    fn follow_up(&self, _: &str) -> String {
        "f1".into()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {
        self.0.0.borrow_mut().push("abort".into());
    }
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
}

fn theme() -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap())
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: id.into(),
        name: format!("Claude {id}"),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    }
}

fn app_in(cwd: &str, qol: QolSettings) -> (App, Rc<Calls>) {
    app_with_fd(cwd, qol, None)
}

fn app_with_fd(cwd: &str, qol: QolSettings, fd_path: Option<std::path::PathBuf>) -> (App, Rc<Calls>) {
    let keys = || {
        KeybindingsManager::new(
            app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let calls = Rc::new(Calls::default());
    let mut app = App::new(
        AppOptions {
            theme: theme(),
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: cwd.into(),
            home: None,
            server_id: "server".into(),
            models: vec![model("sonnet"), model("opus")],
            model: model("sonnet"),
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, cwd),
            fd_path,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Recorder(calls.clone())),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: Some("Fix parser".into()),
        messages: Vec::new(),
    }));
    (app, calls)
}

fn fresh(qol: QolSettings) -> (App, Rc<Calls>) {
    app_in("/nonexistent-qol", qol)
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

fn plain(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen(app: &mut App) -> String {
    plain(&app.render(100, 30))
}

fn calls(c: &Calls) -> Vec<String> {
    c.0.borrow().clone()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("interactive-qol-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A PNG header naming a `w` x `h` image (enough for the dimension probe).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

fn assistant(text: &str, stop: &str) -> Value {
    json!({"role": "assistant", "stopReason": stop, "content": [{"type": "text", "text": text}],
        "usage": {"input": 300, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": {"total": 0.5}}})
}

// ---- settings ----

/// client-tui.ts `createInteractiveTui({ tuiMode: "fullscreen" })`: fullscreen by default; only an explicit
/// "regular" keeps the scrollback split (any other value stays fullscreen).
#[test]
fn interactive_qol_tui_mode_defaults_to_fullscreen_like_ts() {
    let none = |_: &str| None;
    assert!(!from_settings(&json!({}), &none).viewport_split);
    assert!(!QolSettings::default().viewport_split);
    assert!(from_settings(&json!({"tuiMode": "regular"}), &none).viewport_split);
    assert!(!from_settings(&json!({"tuiMode": "fullscreen"}), &none).viewport_split);
    assert!(!from_settings(&json!({"tuiMode": "split"}), &none).viewport_split);
    assert!(!from_settings(&json!({"tuiMode": true}), &none).viewport_split);
}

#[test]
fn interactive_qol_settings_keys_defaults_and_parity_env() {
    let none = |_: &str| None;
    let d = from_settings(&json!({}), &none);
    assert_eq!(d, QolSettings::default());
    assert!(d.notifications && d.tty_writer && d.render_gate && d.status_line);
    assert!(d.attachments && d.highlight && d.autocomplete_emoji && d.autocomplete_models);
    assert!(!d.vim && !d.autocomplete_github && !d.status_pr);

    let q = from_settings(
        &json!({
            "notifications": "off",
            "tuiMode": "fullscreen",
            "terminal": {"writerThread": false, "frameGate": false},
            "statusLine": {"preset": "minimal", "left": ["model", "nope", "git"], "separator": "pipe",
                "symbols": "ascii", "pr": true},
            "vimMode": true,
            "images": {"blockImages": true},
            "markdown": {"syntaxHighlight": false},
            "autocomplete": {"emoji": false, "modelMentions": false, "githubRefs": true}
        }),
        &none,
    );
    assert!(!q.notifications && !q.viewport_split && !q.tty_writer && !q.render_gate);
    assert!(q.status_line && q.status_pr);
    assert_eq!(q.status_layout.preset, Some(Preset::Minimal));
    assert_eq!(q.status_layout.left, Some(vec![SegmentId::Model, SegmentId::Git]));
    assert_eq!(q.status_layout.separator, Some(SeparatorStyle::Pipe));
    assert_eq!(q.status_symbols, StatusSymbols::Ascii);
    assert!(q.vim && !q.attachments && !q.highlight);
    assert!(!q.autocomplete_emoji && !q.autocomplete_models && q.autocomplete_github);
    assert!(!from_settings(&json!({"statusLine": false}), &none).status_line);
    assert!(!from_settings(&json!({"statusLine": {"enabled": false}}), &none).status_line);

    // ULTRON_TUI_PARITY=1 wins over every setting; other values do not.
    let parity = |k: &str| (k == "ULTRON_TUI_PARITY").then(|| "1".to_string());
    assert_eq!(from_settings(&json!({"vimMode": true}), &parity), QolSettings::parity());
    let zero = |k: &str| (k == "ULTRON_TUI_PARITY").then(|| "0".to_string());
    assert!(from_settings(&json!({"vimMode": true}), &zero).vim);
    assert!(!QolSettings::parity().any() && QolSettings::default().any());
}

// ---- notifications ----

fn notified(qol: QolSettings, script: impl FnOnce(&mut App)) -> Vec<Message> {
    let (mut app, _) = fresh(qol);
    let sent = Rc::new(RefCell::new(Vec::new()));
    let sink = sent.clone();
    app.set_notify_sink(Box::new(move |m| sink.borrow_mut().push(m)));
    script(&mut app);
    sent.borrow().clone()
}

fn turn(app: &mut App, stop: &str, error: Option<&str>) {
    app.apply(LaneEvent::OperationStarted("op".into()));
    app.apply(LaneEvent::Message(assistant("done", stop)));
    app.apply(LaneEvent::OperationEnded {
        error: error.map(str::to_string),
    });
}

#[test]
fn interactive_qol_turn_end_notification_reaches_the_sink() {
    let done = notified(QolSettings::default(), |app| turn(app, "stop", None));
    assert_eq!(done, [Message::turn(Some("Fix parser"), TurnOutcome::Complete)]);
    let failed = notified(QolSettings::default(), |app| turn(app, "stop", Some("boom")));
    assert_eq!(failed, [Message::turn(Some("Fix parser"), TurnOutcome::Error)]);
    let errored = notified(QolSettings::default(), |app| turn(app, "error", None));
    assert_eq!(errored, [Message::turn(Some("Fix parser"), TurnOutcome::Error)]);
    // A turn the user aborted with Escape is not announced; the next one is.
    let aborted = notified(QolSettings::default(), |app| {
        app.apply(LaneEvent::OperationStarted("op".into()));
        app.handle_input("\x1b");
        app.apply(LaneEvent::OperationEnded { error: None });
        turn(app, "stop", None);
    });
    assert_eq!(aborted.len(), 1);
    // No turn, no notification; parity sends none.
    assert!(
        notified(QolSettings::default(), |app| app
            .apply(LaneEvent::OperationEnded { error: None }))
        .is_empty()
    );
    assert!(notified(QolSettings::parity(), |app| turn(app, "stop", None)).is_empty());
}

#[test]
fn interactive_qol_service_writes_reach_the_terminal_queue() {
    let (mut app, _) = fresh(QolSettings::default());
    app.terminal_writes.clear();
    assert!(!app.poll_services());
    app.side_writes()
        .lock()
        .unwrap()
        .push("\x1b]9;ultron: Complete\x1b\\".into());
    assert!(app.poll_services());
    assert_eq!(app.terminal_writes, ["\x1b]9;ultron: Complete\x1b\\"]);
    assert!(!app.poll_services());
}

// ---- status line ----

struct FakeGit(Arc<Mutex<GitSnapshot>>, Rc<Cell<usize>>);

impl GitSource for FakeGit {
    fn snapshot(&self) -> GitSnapshot {
        self.0.lock().unwrap().clone()
    }
    fn refresh(&self) {
        self.1.set(self.1.get() + 1);
    }
}

#[test]
fn interactive_qol_status_line_shows_model_git_context_and_session() {
    let (mut app, _) = app_in("/work/proj", QolSettings::default());
    let git = Arc::new(Mutex::new(GitSnapshot::default()));
    let refreshes = Rc::new(Cell::new(0));
    app.set_git_source(Box::new(FakeGit(git.clone(), refreshes.clone())));
    *git.lock().unwrap() = GitSnapshot {
        branch: Some("feature-x".into()),
        status: Some(GitCounts {
            staged: 1,
            unstaged: 2,
            untracked: 0,
        }),
        pr: None,
    };
    assert!(app.poll_services());
    turn(&mut app, "stop", None);
    assert_eq!(refreshes.get(), 1, "a finished turn refreshes git");
    let line = strip_terminal_sequences(&app.status_line(120).unwrap());
    for part in ["sonnet", "feature-x", "proj", "Fix parser", "32.0%"] {
        assert!(line.contains(part), "{part:?} missing from {line:?}");
    }
    assert!(strip_terminal_sequences(&app.status_line(30).unwrap()).chars().count() <= 30);
    // It is a dock row above the editor; parity has neither.
    let s = screen(&mut app);
    let status_row = s.lines().position(|l| l.contains("feature-x")).unwrap();
    let editor_top = s
        .lines()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|l| l.starts_with('─'))
        .unwrap();
    assert!(status_row < editor_top, "{s}");
    let (mut parity, _) = app_in("/work/proj", QolSettings::parity());
    assert_eq!(parity.status_line(120), None);
    assert!(!screen(&mut parity).contains("feature-x"));
}

// ---- vim ----

#[test]
fn interactive_qol_vim_mode_toggle_changes_editor_keys() {
    let vim = QolSettings {
        vim: true,
        ..QolSettings::default()
    };
    let (mut app, c) = fresh(vim);
    assert_eq!(app.vim_mode(), Some(VimMode::Insert));
    typed(&mut app, "hello world");
    app.handle_input("\x1b");
    assert_eq!(app.vim_mode(), Some(VimMode::Normal));
    assert!(strip_terminal_sequences(&app.status_line(120).unwrap()).contains("NORMAL"));
    // Normal mode: `0` to line start, `dw` deletes a word, `x` a character; nothing is typed.
    typed(&mut app, "0dwx");
    assert_eq!(app.editor_text(), "orld");
    typed(&mut app, "A!");
    assert_eq!(app.vim_mode(), Some(VimMode::Insert));
    assert_eq!(app.editor_text(), "orld!");
    // Escape in quiet Normal mode is the app's again: it aborts a running turn.
    app.apply(LaneEvent::OperationStarted("op".into()));
    app.handle_input("\x1b");
    assert_eq!(app.vim_mode(), Some(VimMode::Normal));
    assert!(calls(&c).is_empty());
    app.handle_input("\x1b");
    assert_eq!(calls(&c), ["abort"]);

    let (mut off, _) = fresh(QolSettings::default());
    assert_eq!(off.vim_mode(), None);
    typed(&mut off, "hello");
    off.handle_input("\x1b");
    typed(&mut off, "0dw");
    assert_eq!(off.editor_text(), "hello0dw");
}

// ---- attachments ----

struct FakeClipboard(Vec<u8>);

impl Clipboard for FakeClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        Ok("clip text".into())
    }
    fn set_text(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        if self.0.is_empty() {
            Err("no image".into())
        } else {
            Ok(self.0.clone())
        }
    }
}

fn poll_until(app: &mut App, done: impl Fn(&App) -> bool) {
    // The deadline only guards a hang; the reply is an event the loop waits for.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        app.poll_services();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("service reply never arrived");
}

#[test]
fn interactive_qol_pasted_image_path_becomes_an_attachment_sent_with_the_prompt() {
    let dir = temp_dir("paste");
    let path = dir.join("shot.png");
    std::fs::write(&path, png(800, 600)).unwrap();
    let paste = format!("\x1b[200~{}\x1b[201~", path.display());

    let (mut app, c) = fresh(QolSettings::default());
    typed(&mut app, "see ");
    app.handle_input(&paste);
    assert_eq!(app.editor_text(), "see [Image #1, 800x600] ");
    assert_eq!(app.attachments().images.len(), 1);
    // The chip band shows the staged image above the editor.
    assert!(screen(&mut app).contains("800x600"), "{}", screen(&mut app));
    app.handle_input("\r");
    let data_len = base64_len(png(800, 600).len());
    assert_eq!(
        calls(&c),
        [format!(
            "prompt_with_images see [Image #1, 800x600] [image/png:{data_len}]"
        )]
    );
    assert!(app.attachments().images.is_empty());

    // A path that is not an image, or a missing file, stays text; parity always inserts text.
    let (mut app, _) = fresh(QolSettings::default());
    app.handle_input(&format!("\x1b[200~{}\x1b[201~", dir.join("missing.png").display()));
    assert!(app.attachments().images.is_empty());
    assert!(app.editor_text().contains("missing.png"));
    let (mut parity, _) = fresh(QolSettings::parity());
    parity.handle_input(&paste);
    assert_eq!(parity.editor_text(), path.display().to_string());
    assert!(parity.attachments().images.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

#[test]
fn interactive_qol_clipboard_paste_image_then_text_fallback() {
    let (mut app, _) = fresh(QolSettings::default());
    app.set_clipboard(ClipboardService::spawn(FakeClipboard(png(2, 3))));
    app.handle_input("\x16");
    poll_until(&mut app, |a| !a.editor_text().is_empty());
    assert_eq!(app.editor_text(), "[Image #1, 2x3] ");
    assert_eq!(app.attachments().images[0].mime, "image/png");

    let (mut app, _) = fresh(QolSettings::default());
    app.set_clipboard(ClipboardService::spawn(FakeClipboard(Vec::new())));
    app.handle_input("\x16");
    poll_until(&mut app, |a| !a.editor_text().is_empty());
    assert_eq!(app.editor_text(), "clip text");

    // Parity (client-tui-actions.ts `clipboardPasteText`): an image is saved to a temp file and its path inserted;
    // otherwise the clipboard's text is inserted as typed text.
    let dir = std::env::temp_dir().join(format!("qol-paste-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (mut parity, _) = fresh(QolSettings::parity());
    parity.set_temp_dir(dir.clone());
    parity.set_clipboard(ClipboardService::spawn(FakeClipboard(png(2, 3))));
    parity.handle_input("\x16");
    poll_until(&mut parity, |a| !a.editor_text().is_empty());
    let path = parity.editor_text();
    assert!(
        path.starts_with(&format!("{}/ultron-clipboard-", dir.display())) && path.ends_with(".png"),
        "{path}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), png(2, 3));
    assert!(parity.attachments().images.is_empty());
    let (mut parity, _) = fresh(QolSettings::parity());
    parity.set_clipboard(ClipboardService::spawn(FakeClipboard(Vec::new())));
    parity.handle_input("\x16");
    poll_until(&mut parity, |a| !a.editor_text().is_empty());
    assert_eq!(parity.editor_text(), "clip text");
    let _ = std::fs::remove_dir_all(dir);
}

/// A clipboard holding one typed image (`readClipboardImage` returns JPEG, WebP and GIF as read).
struct TypedClipboard(tui::clipboard::ClipboardImage);

impl Clipboard for TypedClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        Ok("clip text".into())
    }
    fn set_text(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        Err("not a PNG".into())
    }
    fn get_image(&mut self) -> Result<tui::clipboard::ClipboardImage, String> {
        Ok(self.0.clone())
    }
}

/// Ctrl+V of a JPEG, WebP or GIF clipboard image (`clipboardPasteText`): saved with the extension of its type
/// (`extensionForImageMimeType`), bytes unchanged; with the attachments, an attachment of that type.
#[test]
fn interactive_qol_clipboard_paste_keeps_the_image_type_like_ts() {
    let mut jpeg = Vec::new();
    image::RgbImage::new(4, 2)
        .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .unwrap();
    let dir = std::env::temp_dir().join(format!("qol-typed-paste-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = [
        ("image/jpeg", jpeg.clone(), "jpg"),
        ("image/webp", b"RIFF\x10\0\0\0WEBPVP8 ".to_vec(), "webp"),
        ("image/gif", b"GIF89a".to_vec(), "gif"),
    ];
    for (mime_type, bytes, extension) in cases {
        let image = tui::clipboard::ClipboardImage {
            bytes: bytes.clone(),
            mime_type: mime_type.into(),
        };
        let (mut parity, _) = fresh(QolSettings::parity());
        parity.set_temp_dir(dir.clone());
        parity.set_clipboard(ClipboardService::spawn(TypedClipboard(image)));
        parity.handle_input("\x16");
        poll_until(&mut parity, |a| !a.editor_text().is_empty());
        let path = parity.editor_text();
        assert!(
            path.starts_with(&format!("{}/ultron-clipboard-", dir.display()))
                && path.ends_with(&format!(".{extension}")),
            "{path}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{mime_type}");
    }
    let (mut app, _) = fresh(QolSettings::default());
    app.set_clipboard(ClipboardService::spawn(TypedClipboard(
        tui::clipboard::ClipboardImage {
            bytes: jpeg,
            mime_type: "image/jpeg".into(),
        },
    )));
    app.handle_input("\x16");
    poll_until(&mut app, |a| !a.editor_text().is_empty());
    assert_eq!(app.editor_text(), "[Image #1, 4x2] ");
    assert_eq!(app.attachments().images[0].mime, "image/jpeg");
    let _ = std::fs::remove_dir_all(dir);
}

// ---- highlighting ----

fn rlm_transcript() -> Vec<Value> {
    vec![
        json!({"role": "assistant", "stopReason": "toolUse", "content": [
            {"type": "text", "text": "Example:\n\n```python\ndef shown(x):\n    return x\n```"},
            {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "def cell(y):\n    return y"}}
        ]}),
        json!({"role": "toolResult", "toolCallId": "c1", "toolName": "rlm", "isError": false,
            "content": [{"type": "text", "text": "ok"}]}),
    ]
}

#[test]
fn interactive_qol_highlight_styles_fenced_code_and_leaves_rlm_cells_to_the_cell_highlighter() {
    let keyword = theme().fg_ansi("syntaxKeyword").unwrap();
    let render = |qol: QolSettings| {
        let (mut app, _) = fresh(qol);
        app.apply(LaneEvent::Opened(SessionOpened {
            session_id: "s2".into(),
            session_file: None,
            name: None,
            messages: rlm_transcript(),
        }));
        app.render(100, 60)
    };
    let on = render(QolSettings {
        highlight: true,
        ..QolSettings::parity()
    });
    let off = render(QolSettings::parity());
    let line = |lines: &[String], needle: &str| {
        lines
            .iter()
            .find(|l| strip_terminal_sequences(l).contains(needle))
            .unwrap()
            .clone()
    };
    // Fenced code blocks: QoL highlighting adds syntax colors.
    assert!(
        line(&on, "shown(x)").contains(&format!("{keyword}def")),
        "{:?}",
        line(&on, "shown(x)")
    );
    assert!(
        !line(&off, "shown(x)").contains(&keyword),
        "{:?}",
        line(&off, "shown(x)")
    );
    // `rlm` cells always use the app's own cell highlighter (the product's is TS's highlight.js port), so QoL
    // highlighting does not change them.
    assert_eq!(line(&on, "cell(y)"), line(&off, "cell(y)"));
    // Same text either way: highlighting only adds styling.
    assert_eq!(plain(&on), plain(&off));
}

// ---- autocomplete ----

#[test]
fn interactive_qol_autocomplete_chain_offers_commands_files_emoji_and_models() {
    let dir = temp_dir("complete");
    std::fs::write(dir.join("alpha.txt"), "a").unwrap();
    std::fs::create_dir(dir.join("beta")).unwrap();
    let cwd = dir.display().to_string();
    let suggest = |qol: QolSettings, text: &str| {
        let (mut app, _) = app_in(&cwd, qol);
        typed(&mut app, text);
        app.fire_autocomplete();
        screen(&mut app)
    };
    // Slash commands and files still come from the command and file provider.
    assert!(suggest(QolSettings::default(), "/mo").contains("model"));
    let (mut app, _) = app_in(&cwd, QolSettings::default());
    typed(&mut app, "./al");
    app.handle_input("\t");
    assert_eq!(app.editor_text(), "./alpha.txt");
    // Emoji and model mentions are offered on their own syntax.
    let emoji = suggest(QolSettings::default(), ":smile");
    assert!(emoji.contains("smile"), "{emoji}");
    assert!(emoji.contains('😄'), "{emoji}");
    let models = suggest(QolSettings::default(), "^opu");
    assert!(models.contains("anthropic/opus"), "{models}");
    // Accepting a model mention inserts the selector.
    let (mut app, _) = app_in(&cwd, QolSettings::default());
    typed(&mut app, "ask ^opu");
    app.fire_autocomplete();
    app.handle_input("\t");
    assert_eq!(app.editor_text(), "ask ^anthropic/opus ");
    // Parity: neither extra source answers.
    assert!(!suggest(QolSettings::parity(), ":smile").contains('😄'));
    assert!(!suggest(QolSettings::parity(), "^opu").contains("anthropic/opus"));
    let _ = std::fs::remove_dir_all(dir);
}

// ---- scrollback/live split, render gate, writer thread ----

#[test]
fn interactive_qol_viewport_split_commits_finished_rows_once() {
    let (mut app, _) = fresh(QolSettings::default());
    app.apply(LaneEvent::Message(json!({"role": "user", "content": "first question"})));
    app.apply(LaneEvent::Message(assistant("first answer", "stop")));
    let plan = app.frame_plan(80, 24);
    let history = plan.history.expect("finished rows go to scrollback");
    assert_eq!(history.kind, HistoryKind::Append);
    let rows = plain(&history.rows);
    assert!(
        rows.contains("first question") && rows.contains("first answer"),
        "{rows}"
    );
    // The viewport is the dock only: nothing finished is repainted.
    let viewport = plain(&plan.viewport);
    assert!(!viewport.contains("first answer"));
    assert!(viewport.contains("Fix parser"), "footer in the viewport: {viewport}");
    assert_eq!(app.frame_plan(80, 24).history, None);

    // A streaming answer is live: in the viewport, not in history, until the turn ends.
    app.apply(LaneEvent::OperationStarted("op".into()));
    app.apply(LaneEvent::Message(
        json!({"role": "user", "content": "second question"}),
    ));
    app.apply(LaneEvent::Streaming(
        json!({"role": "assistant", "content": [{"type": "text", "text": "partial"}]}),
    ));
    let plan = app.frame_plan(80, 24);
    assert!(plain(&plan.history.expect("the new user message settled").rows).contains("second question"));
    assert!(plain(&plan.viewport).contains("partial"));
    app.apply(LaneEvent::Message(assistant("partial and done", "stop")));
    app.apply(LaneEvent::OperationEnded { error: None });
    let plan = app.frame_plan(80, 24);
    let appended = plain(&plan.history.unwrap().rows);
    assert!(
        appended.contains("partial and done") && !appended.contains("first answer"),
        "{appended}"
    );
    assert!(!plain(&plan.viewport).contains("partial"));

    // Rows already written changed (a resize re-wraps them): the whole ledger is replayed.
    let plan = app.frame_plan(60, 24);
    let replay = plan.history.unwrap();
    assert_eq!(replay.kind, HistoryKind::Replay);
    assert!(plain(&replay.rows).contains("first answer"));
}

#[test]
fn interactive_qol_viewport_keeps_the_editor_visible_under_tall_live_output() {
    let (mut app, _) = fresh(QolSettings::default());
    app.apply(LaneEvent::OperationStarted("op".into()));
    let long: String = (0..80).map(|i| format!("line {i}\n\n")).collect();
    app.apply(LaneEvent::Streaming(
        json!({"role": "assistant", "content": [{"type": "text", "text": long}]}),
    ));
    typed(&mut app, "draft text");
    let plan = app.frame_plan(80, 20);
    assert!(plan.viewport.len() > 20);
    let mut renderer = ViewportRenderer::new();
    renderer.render(&plan, 80, 20);
    let shown = plain(renderer.viewport());
    assert_eq!(renderer.viewport().len(), 20);
    assert!(shown.contains("draft text"), "{shown}");
    assert!(shown.contains("line 79"), "{shown}");
    assert!(!shown.contains("line 0\n"), "{shown}");
}

/// The scripted batches of [`run_script`]: one per loop iteration, with idle iterations before stdin closes.
fn script() -> Vec<Vec<UiEvent>> {
    let mut batches = vec![
        vec![UiEvent::Lane(LaneEvent::Message(
            json!({"role": "user", "content": "hi"}),
        ))],
        vec![UiEvent::Lane(LaneEvent::Message(assistant("hello there", "stop")))],
        vec![UiEvent::Input("abc".into())],
    ];
    batches.extend((0..12).map(|_| Vec::new()));
    batches.push(vec![UiEvent::InputClosed]);
    batches
}

/// The defaults with the scrollback split (tuiMode "regular"), which these runs measure.
fn split() -> QolSettings {
    QolSettings {
        viewport_split: true,
        ..QolSettings::default()
    }
}

fn run_script(qol: QolSettings, out: &mut dyn FnMut(&str) -> std::io::Result<()>, pending: Option<&dyn Fn() -> usize>) {
    let (mut app, _) = fresh(qol);
    let (tx, rx) = mpsc::channel();
    let batches = RefCell::new(script().into_iter());
    let size = || {
        if let Some(batch) = batches.borrow_mut().next() {
            for e in batch {
                tx.send(e).unwrap();
            }
        }
        (80, 24)
    };
    paint_loop(&mut app, &rx, out, &size, pending).unwrap();
}

#[test]
fn interactive_qol_render_gate_writes_nothing_while_the_writer_is_backlogged() {
    let backlog = Cell::new(MAX_PENDING_OUTPUT_BYTES + 1);
    let polls = Cell::new(0);
    let pending = || {
        polls.set(polls.get() + 1);
        // The terminal drains after a while.
        if polls.get() > 5 {
            backlog.set(0);
        }
        backlog.get()
    };
    let mut writes = Vec::new();
    run_script(
        split(),
        &mut |data| {
            writes.push((backlog.get(), data.to_string()));
            Ok(())
        },
        Some(&pending),
    );
    assert!(polls.get() > 5, "the gate asked for the backlog");
    let (during, after): (Vec<_>, Vec<_>) = writes.iter().partition(|(b, _)| *b > MAX_PENDING_OUTPUT_BYTES);
    assert!(during.is_empty(), "frames written while backlogged: {}", during.len());
    let all: String = after.iter().map(|(_, d)| d.as_str()).collect();
    assert!(strip_terminal_sequences(&all).contains("hello there"));

    // Without the gate every dirty iteration paints at once.
    let mut ungated = 0;
    run_script(
        QolSettings {
            render_gate: false,
            ..split()
        },
        &mut |_| {
            ungated += 1;
            Ok(())
        },
        Some(&|| MAX_PENDING_OUTPUT_BYTES + 1),
    );
    assert!(ungated > 3);
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn interactive_qol_writer_thread_delivers_the_frames_in_order() {
    let no_gate = QolSettings {
        render_gate: false,
        ..split()
    };
    let mut direct = String::new();
    run_script(
        no_gate.clone(),
        &mut |d| {
            direct.push_str(d);
            Ok(())
        },
        None,
    );
    let sink = Shared::default();
    let tty = TtyWriter::new(sink.clone()).unwrap();
    run_script(
        no_gate,
        &mut |d| {
            tty.write(d);
            Ok(())
        },
        Some(&|| tty.pending()),
    );
    // A hang guard: the writer thread flushes as soon as it runs, however loaded the machine.
    assert!(tty.flush_sync(Duration::from_secs(120)));
    let threaded = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    // With the writer's backlog check the app may paint one more frame that only moves the cursor (when the check
    // sees the writer busy); such empty frames change nothing on screen, so they are left out of the comparison.
    assert_eq!(without_empty_frames(&threaded), without_empty_frames(&direct));
    // The split paints on the main screen: no alternate screen, transcript rows above the dock, cursor shown at exit.
    assert!(!direct.contains("\x1b[?1049h"));
    assert!(strip_terminal_sequences(&direct).contains("hello there"));
    assert!(direct.ends_with("\r\n\x1b[?25h"));
}

#[test]
fn interactive_qol_fullscreen_mode_keeps_the_alternate_screen() {
    let mut out = String::new();
    run_script(
        QolSettings {
            viewport_split: false,
            ..QolSettings::default()
        },
        &mut |d| {
            out.push_str(d);
            Ok(())
        },
        None,
    );
    assert!(out.starts_with("\x1b[?1049h"));
    assert!(out.contains("\x1b[?1049l"));
}

#[test]
fn interactive_qol_dock_keeps_three_editor_rows_in_a_short_terminal() {
    let dir = temp_dir("short");
    let path = dir.join("a.png");
    std::fs::write(&path, png(4, 4)).unwrap();
    let (mut app, _) = fresh(QolSettings {
        viewport_split: false,
        ..QolSettings::default()
    });
    app.apply(LaneEvent::Status("one\ntwo\nthree".into()));
    // The status line and the attachment chips are extra dock parts; the editor still gets its three rows.
    app.handle_input(&format!("\x1b[200~{}\x1b[201~", path.display()));
    typed(&mut app, "draft");
    let frame = plain(&app.render(80, 10));
    let rows: Vec<&str> = frame.lines().collect();
    let draft = rows.iter().position(|l| l.contains("draft")).expect(&frame);
    assert!(
        rows[draft - 1].starts_with('─') && rows[draft + 1].starts_with('─'),
        "{frame}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `@` completion runs `fd` off the UI thread: while the fake `fd` is held the app keeps editing and painting, and
/// the run loop paints the picker once the answer's wake-up arrives (`UiEvent::Wake`, then `poll_autocomplete`).
#[test]
fn interactive_qol_at_completion_paints_while_fd_runs_off_the_ui_thread() {
    use std::io::{BufRead, Write};
    let dir = temp_dir("fd");
    for fifo in ["started", "go"] {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(dir.join(fifo))
                .status()
                .unwrap()
                .success()
        );
    }
    // Reports each start on `started`, then waits for one line on `go` (`read -t`: a hang guard only).
    let fd = dir.join("fd");
    std::fs::write(
        &fd,
        format!(
            "#!/bin/bash\nexec 4<>'{s}'\necho x >&4\nexec 3<>'{g}'\nread -t 120 line <&3 || exit 1\nprintf 'alpha.txt\\nbeta/\\n'\n",
            s = dir.join("started").display(),
            g = dir.join("go").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fd, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let open = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(name))
            .unwrap()
    };
    let mut go = open("go");
    let (started_tx, started) = mpsc::channel();
    let reader = std::io::BufReader::new(open("started"));
    std::thread::spawn(move || {
        for line in reader.lines() {
            if line.is_err() || started_tx.send(()).is_err() {
                return;
            }
        }
    });
    let hang_guard = Duration::from_secs(120);

    let (mut app, _) = app_with_fd(&dir.display().to_string(), QolSettings::default(), Some(fd));
    let (tx, rx) = mpsc::channel();
    let wake = Mutex::new(tx.clone());
    app.set_autocomplete_waker(move || {
        let _ = wake.lock().unwrap().send(UiEvent::Wake);
    });
    app.handle_input("@");
    assert!(app.fire_autocomplete());
    started.recv_timeout(hang_guard).expect("fd never started");
    // `fd` is held: typing still edits and paints.
    app.handle_input("x");
    app.handle_input("\x7f");
    let shown = screen(&mut app);
    assert!(shown.contains("@") && !shown.contains("alpha.txt"), "{shown}");
    assert!(!app.poll_autocomplete());
    // The backspace re-requested `@` (debounced): fire it, as the loop's timer does.
    assert!(app.fire_autocomplete());
    started.recv_timeout(hang_guard).expect("fd never restarted");

    // Release both walks, then let the run loop take the wake-up and paint the answer.
    go.write_all(b"go\ngo\n").unwrap();
    let frames = RefCell::new(String::new());
    let since = std::time::Instant::now();
    let size = || {
        let painted = strip_terminal_sequences(&frames.borrow()).contains("alpha.txt");
        if painted || since.elapsed() > hang_guard {
            tx.send(UiEvent::InputClosed).unwrap();
        }
        (100, 30)
    };
    paint_loop(
        &mut app,
        &rx,
        &mut |data| {
            frames.borrow_mut().push_str(data);
            Ok(())
        },
        &size,
        None,
    )
    .unwrap();
    assert!(
        strip_terminal_sequences(&frames.borrow()).contains("alpha.txt"),
        "the picker was never painted"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// `output` without synchronized-update frames that hold only control sequences (no visible change).
fn without_empty_frames(output: &str) -> String {
    let (begin, end) = ("\x1b[?2026h", "\x1b[?2026l");
    let mut out = String::new();
    let mut rest = output;
    while let Some(start) = rest.find(begin) {
        let Some(len) = rest[start..].find(end) else { break };
        let frame = &rest[start..start + len + end.len()];
        out.push_str(&rest[..start]);
        if !only_control_sequences(frame) {
            out.push_str(frame);
        }
        rest = &rest[start + len + end.len()..];
    }
    out.push_str(rest);
    out
}

/// Whether `frame` is nothing but CSI sequences (cursor moves, mode switches).
fn only_control_sequences(frame: &str) -> bool {
    let mut chars = frame.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' || chars.next() != Some('[') {
            return false;
        }
        // Parameters and intermediates, then one final byte in @..~.
        loop {
            match chars.next() {
                Some(b) if ('@'..='~').contains(&b) => break,
                Some(_) => {}
                None => return false,
            }
        }
    }
    true
}
