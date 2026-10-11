//! Replays fixtures/vim-parity.json (recorded from omp packages/tui/src/vim.ts by generate-vim.mjs): after every
//! key the commands, mode, pending text, Visual anchor and the cursor (moves applied as omp's Editor applies them)
//! must match exactly.

use serde_json::{Value, json};
use tui::vim::*;

fn col(c: usize) -> i64 {
    if c == usize::MAX { -1 } else { c as i64 }
}

fn p(x: Pos) -> Value {
    json!([x.line, col(x.col)])
}

fn cmd(c: &VimCommand) -> Value {
    match c {
        VimCommand::Move { to } => json!({"kind": "move", "to": p(*to)}),
        VimCommand::Mode { mode } => json!({"kind": "mode", "mode": mode.name()}),
        VimCommand::Yank { from, to, linewise } => {
            json!({"kind": "yank", "from": p(*from), "to": p(*to), "linewise": linewise})
        }
        VimCommand::Delete {
            from,
            to,
            linewise,
            insert,
        } => {
            json!({"kind": "delete", "from": p(*from), "to": p(*to), "linewise": linewise, "insert": insert})
        }
        VimCommand::OpenLine { below } => json!({"kind": "openLine", "below": below}),
        VimCommand::Paste { after, count } => json!({"kind": "paste", "after": after, "count": count}),
        VimCommand::Undo => json!({"kind": "undo"}),
        VimCommand::Replace { from, to, text } => {
            json!({"kind": "replace", "from": p(*from), "to": p(*to), "text": text})
        }
        VimCommand::Join { from_line, lines } => json!({"kind": "join", "fromLine": from_line, "lines": lines}),
        VimCommand::Indent {
            from_line,
            to_line,
            out,
        } => {
            json!({"kind": "indent", "fromLine": from_line, "toLine": to_line, "out": out})
        }
    }
}

#[test]
fn tui_vim_state_machine_matches_omp() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/vim-parity.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 5000);
    let mut failures = Vec::new();
    for case in cases {
        let text = case["text"].as_str().unwrap();
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let mut vim = VimState::new();
        vim.mode = if case["mode"] == "insert" {
            VimMode::Insert
        } else {
            VimMode::Normal
        };
        let mut line = case["cursor"][0].as_u64().unwrap() as usize;
        let mut c = case["cursor"][1].as_u64().unwrap() as usize;
        for step in case["steps"].as_array().unwrap() {
            let key = step["key"].as_str().unwrap();
            let buf = VimBuffer {
                lines: &lines,
                cursor_line: line,
                cursor_col: c,
            };
            let cmds = vim.handle_key(key, &buf);
            if let Some(cmds) = &cmds {
                for cm in cmds {
                    if let VimCommand::Move { to } = cm {
                        line = to.line.min(lines.len() - 1);
                        c = to.col.min(lines[line].len());
                    }
                }
                if vim.mode == VimMode::Normal {
                    c = c.min(last_grapheme_start(&lines[line]));
                }
            }
            let got = json!({
                "key": key,
                "cmds": cmds.map(|v| v.iter().map(cmd).collect::<Vec<_>>()),
                "mode": vim.mode.name(),
                "pending": vim.pending_text(),
                "anchor": vim.anchor.map(p),
                "cursor": [line, c],
            });
            if &got != step {
                failures.push(format!(
                    "{text:?} @{} keys {}: want {step} got {got}",
                    case["cursor"], case["keys"]
                ));
                break;
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures, first:\n{}",
        failures.len(),
        failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
}
