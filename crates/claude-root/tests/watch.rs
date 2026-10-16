//! `ultron watch` frames against packages/coding-agent ultron/claude/watch.ts (`renderWatchFrame`), replayed from
//! fixtures/watch.json (`node crates/claude-root/fixtures/generate-watch.mjs`), and `--once --plain` end to end over
//! a fake control socket.

use std::sync::Arc;

use claude_root::control::{ControlHandler, listen};
use claude_root::watch::{render_watch_frame, run, strip_ansi};
use interactive::rlm::RlmSnapshot;
use interactive::rlm::pane::RlmPane;
use interactive::rlm::visualizer::RlmStyle;
use serde_json::{Value, json};

fn keys() -> tui::keybindings::KeybindingsManager {
    tui::keybindings::KeybindingsManager::new(
        tui::qol_keys::native_app_keybindings(tui::keybindings::Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

#[test]
fn claude_root_watch_frames_match_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/watch.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 18);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let snapshot = RlmSnapshot::from_wire(&fixture["wires"][name]);
        let status = &fixture["statuses"][case["status"].as_u64().unwrap() as usize];
        let (width, height) = (
            case["width"].as_u64().unwrap() as usize,
            case["height"].as_u64().unwrap() as usize,
        );
        let mut pane = RlmPane::new();
        let lines = render_watch_frame(
            &mut pane,
            &keys(),
            &snapshot,
            status,
            width,
            height,
            &RlmStyle::plain(),
            false,
            30,
        );
        let want: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        assert_eq!(lines, want, "{name} status {} {width}x{height}", case["status"]);
    }
}

#[test]
fn claude_root_watch_strips_csi_and_osc() {
    assert_eq!(
        strip_ansi("\x1b[1m\x1b[38;5;2mUltron\x1b[39m\x1b[22m \x1b]8;;http://x\x07link\x1b]8;;\x07"),
        "Ultron link"
    );
}

#[test]
fn claude_root_watch_once_plain_draws_the_session_over_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("watch.sock");
    let handler: ControlHandler = Arc::new(|request| match request["op"].as_str().unwrap_or("") {
        "status" => Ok(json!({"cwd": "/work/app", "cells": 1, "frameModel": "m/one"})),
        "cells" => {
            Ok(json!({"cells": [{"n": 1, "code": "print(1)", "status": "ok", "startedAt": 1.0, "endedAt": 2.0}]}))
        }
        "inspect" => match request["request"].as_str().unwrap_or("") {
            "agents.status" => Ok(json!({"tasks": []})),
            "rlm.frames" => Ok(json!({"frames": []})),
            other => Err(format!("unknown request {other}")),
        },
        other => Err(format!("unknown op {other}")),
    });
    let server = listen(&socket, handler).unwrap();
    let control = dir.path().join("runtime");
    std::fs::create_dir_all(&control).unwrap();
    let vars = move |name: &str| match name {
        "XDG_RUNTIME_DIR" => Some(control.display().to_string()),
        _ => None,
    };
    let args: Vec<String> = ["--once", "--plain", "--width", "80", "--socket"]
        .iter()
        .map(|s| s.to_string())
        .chain([socket.display().to_string()])
        .collect();
    let mut out = Vec::new();
    let code = run(&args, "/work/app", &vars, &mut out).unwrap();
    server.close();
    assert_eq!(code, 0);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("Ultron /work/app · frames m/one · 1 cells\n"),
        "{text}"
    );
    assert!(!text.contains('\x1b'), "{text}");
}
