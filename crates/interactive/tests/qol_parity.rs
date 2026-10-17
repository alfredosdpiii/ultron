//! Parity guard for the Rust-only TUI additions: with every addition off (`ULTRON_TUI_PARITY=1`), the bytes the paint
//! loop writes for a representative session are exactly the bytes the TUI wrote before the additions existed.
//! Fixture: fixtures/qol-parity-frames.json, recorded from the paint loop before any addition was wired in
//! (`QOL_PARITY_RECORD=1 cargo test -p interactive --test qol_parity` rewrites it; never re-record to make a change
//! pass).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, QueueKind, Queued, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

struct Quiet;

impl LaneClient for Quiet {
    fn prompt(&self, _: &str) {}
    fn steer(&self, _: &str) -> String {
        "s1".into()
    }
    fn follow_up(&self, _: &str) -> String {
        "f1".into()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: id.into(),
        name: id.into(),
        reasoning: true,
        context_window: 200_000,
        thinking_levels: Vec::new(),
    }
}

pub fn parity_app(qol: QolSettings) -> App {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/nonexistent-qol-parity".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model("claude-a"), model("claude-b")],
            model: model("claude-a"),
            thinking_level: "medium".into(),
            splash: true,
            history: PromptHistoryStore::new(None, "/nonexistent-qol-parity"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Quiet),
    )
}

fn usage(input: u64, output: u64) -> Value {
    json!({"input": input, "output": output, "cacheRead": 10, "cacheWrite": 5, "totalTokens": 0,
        "cost": {"total": 0.0123}})
}

/// A user prompt, a thinking + fenced-code answer that calls `rlm`, the cell's output, and a final answer.
pub fn transcript() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "List the files and show me a **rust** example", "timestamp": 1}),
        json!({"role": "assistant", "stopReason": "toolUse", "usage": usage(1200, 80), "content": [
            {"type": "thinking", "thinking": "The user wants a listing."},
            {"type": "text", "text": "Here is an example:\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nNow the listing."},
            {"type": "toolCall", "id": "call_1", "name": "rlm",
                "arguments": {"code": "import os\nfor name in sorted(os.listdir('.')):\n    print(name)  # one per line"}}
        ]}),
        json!({"role": "toolResult", "toolCallId": "call_1", "toolName": "rlm", "isError": false,
            "content": [{"type": "text", "text": "Cargo.toml\ncrates\nsrc"}]}),
        json!({"role": "assistant", "stopReason": "stop", "usage": usage(1500, 120), "content": [
            {"type": "text", "text": "Three entries:\n\n- `Cargo.toml`\n- `crates`\n- `src`\n\n```python\nprint('done')\n```"}
        ]}),
    ]
}

fn input(s: &str) -> UiEvent {
    UiEvent::Input(s.to_string())
}

/// The scripted batches: each one reaches the loop on its own iteration, so every batch paints one frame.
pub fn batches() -> Vec<Vec<UiEvent>> {
    let lane = UiEvent::Lane;
    vec![
        vec![lane(LaneEvent::Opened(SessionOpened {
            session_id: "sess-1".into(),
            session_file: Some("/s/sess-1.jsonl".into()),
            name: Some("Parity run".into()),
            messages: transcript(),
        }))],
        vec![input("/"), input("m"), input("o")],
        vec![input("\x1b"), input("\x03")],
        vec![input("\x15")],
        vec![input("\x1b[200~/tmp/screenshot.png\x1b[201~")],
        vec![input("\x15"), input(":smi"), input("\x1b"), input("j"), input("k")],
        vec![input("\x15"), input("^cla")],
        vec![input("\x15"), input("look at this")],
        vec![input("\r")],
        vec![
            lane(LaneEvent::OperationStarted("op-1".into())),
            lane(LaneEvent::Streaming(json!({"role": "assistant", "content": [
                {"type": "text", "text": "Working on it:\n\n```python\nx = 1\n```"},
                {"type": "toolCall", "id": "call_2", "name": "rlm", "arguments": {"code": "x = 1\nprint(x)"}}
            ]}))),
            lane(LaneEvent::ToolStarted {
                id: "call_2".into(),
                name: "rlm".into(),
                args: json!({"code": "x = 1\nprint(x)"}),
            }),
            lane(LaneEvent::Queues(vec![Queued {
                entry_id: "f1".into(),
                kind: QueueKind::FollowUp,
                text: "and then?".into(),
                user: true,
            }])),
        ],
        vec![
            lane(LaneEvent::Message(
                json!({"role": "assistant", "stopReason": "toolUse", "usage": usage(1700, 30),
                "content": [
                    {"type": "text", "text": "Working on it:\n\n```python\nx = 1\n```"},
                    {"type": "toolCall", "id": "call_2", "name": "rlm", "arguments": {"code": "x = 1\nprint(x)"}}
                ]}),
            )),
            lane(LaneEvent::Message(
                json!({"role": "toolResult", "toolCallId": "call_2", "toolName": "rlm",
                "isError": false, "content": [{"type": "text", "text": "1"}]}),
            )),
            lane(LaneEvent::Message(
                json!({"role": "assistant", "stopReason": "stop", "usage": usage(1800, 10),
                "content": [{"type": "text", "text": "Done."}]}),
            )),
            lane(LaneEvent::OperationEnded { error: None }),
        ],
        vec![input("\x0f")],
        vec![UiEvent::InputClosed],
    ]
}

/// Every write of the paint loop for the scripted session; the terminal shrinks for one frame.
pub fn paint(app: &mut App) -> Vec<String> {
    let (tx, rx) = mpsc::channel();
    let pending = RefCell::new(batches().into_iter());
    let calls = Cell::new(0usize);
    let size = || {
        if let Some(batch) = pending.borrow_mut().next() {
            for event in batch {
                tx.send(event).unwrap();
            }
        }
        calls.set(calls.get() + 1);
        if calls.get() == 6 { (60, 20) } else { (80, 30) }
    };
    let mut writes = Vec::new();
    paint_loop(
        app,
        &rx,
        &mut |data| {
            writes.push(data.to_string());
            Ok(())
        },
        &size,
        None,
    )
    .unwrap();
    writes
}

#[test]
fn interactive_qol_parity_off_paints_the_recorded_bytes() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/qol-parity-frames.json");
    let writes = paint(&mut parity_app(QolSettings::parity()));
    if std::env::var("QOL_PARITY_RECORD").as_deref() == Ok("1") {
        std::fs::write(path, serde_json::to_string_pretty(&writes).unwrap() + "\n").unwrap();
    }
    let recorded: Vec<String> = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).unwrap();
    assert_eq!(writes.len(), recorded.len());
    for (i, (got, want)) in writes.iter().zip(&recorded).enumerate() {
        assert_eq!(got, want, "write {i} differs");
    }
}

#[test]
fn interactive_qol_parity_from_env_turns_everything_off() {
    let settings = json!({"vimMode": true, "statusLine": {"preset": "full"}, "notifications": true});
    let qol = QolSettings::from_settings(&settings, &|k| (k == "ULTRON_TUI_PARITY").then(|| "1".to_string()));
    assert_eq!(qol, QolSettings::parity());
    // Parity really paints differently from the defaults (the guard above is not vacuous).
    let defaults = paint(&mut parity_app(QolSettings::default()));
    let parity = paint(&mut parity_app(QolSettings::parity()));
    assert_ne!(defaults, parity);
}
