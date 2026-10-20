//! `diagram.mermaid` (diagram_api.py, kernel_apis::diagram, session_core::session_diagrams) in the product's session:
//! a root cell renders Mermaid through a fake `node` on the session environment's PATH (a shell script printing a
//! canned Pele answer), gets back the saved files, and the tool result carries the PNG for the user only
//! (`details.displayImages`) with one `diagram: <path> (WxH)` line for the model; `to_model=True` puts the image in
//! the model's request. Without node on the session PATH the cell sees the one-line error. New in Rust: literal
//! expectations.

mod p_session_support;

use base64::Engine;
use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A Pele-shaped SVG (root `--_*` definitions over `--pele-*` colours).
const SVG: &str = concat!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60" viewBox="0 0 120 60" "##,
    r##"style="--_bg:var(--pele-bg,#fff);--_s:var(--pele-surface,#f3f3f3);--_b:var(--pele-border,#8a8a8a)">"##,
    r##"<rect x="10" y="10" width="40" height="40" fill="var(--_s)" stroke="var(--_b)"/></svg>"##
);

/// `dir/node`: reads stdin, prints the canned answer (absolute tools only: the session PATH holds just `dir`).
fn fake_node(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let answer =
        json!({"ok": true, "type": "flowchart", "mermaidType": "flowchart-v2", "svg": SVG, "width": 120, "height": 60});
    let script = format!("#!/bin/sh\n/usr/bin/cat > /dev/null\n/usr/bin/cat <<'EOF'\n{answer}\nEOF\n");
    std::fs::write(dir.join("node"), script).unwrap();
    std::fs::set_permissions(dir.join("node"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every toolResult message the session committed, in order.
fn committed_results(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("role") == Some(&json!("toolResult")) {
                out.push(value.clone());
                return;
            }
            map.values().for_each(|v| committed_results(v, out));
        }
        Value::Array(items) => items.iter().for_each(|v| committed_results(v, out)),
        _ => {}
    }
}

fn lines(output: &str, prefix: &str) -> Vec<Value> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{prefix} ")))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn session_core_diagram_mermaid_shows_the_png_to_the_user_and_a_line_to_the_model() {
    let bin = tempfile::tempdir().unwrap();
    let bin = bin.path().canonicalize().unwrap().join("bin");
    fake_node(&bin);
    let first = [
        "import json",
        "d = await diagram.mermaid('flowchart LR\\n  a --> b')",
        "print('result', json.dumps(d))",
        "for call in [lambda: diagram.mermaid(''), lambda: diagram.mermaid('graph TD', theme='neon')]:",
        "    try:",
        "        await call()",
        "    except Exception as e:",
        "        print('error', json.dumps([type(e).__name__, str(e)]))",
        "print('doc', json.dumps('Mermaid' in (type(diagram).mermaid.__doc__ or '')))",
    ]
    .join("\n");
    let second = "import json\nd = await diagram.mermaid('flowchart LR\\n  a --> b', to_model=True, path='docs/flow')\nprint('result', json.dumps(d))";
    let script = in_order(vec![tool("toolu_1", &first), tool("toolu_2", second), text("done")]);
    let a = actor(
        script,
        Setup {
            settings: Some(json!({"theme": "light"})),
            vars: vec![("PATH", bin.display().to_string())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("draw it");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let requests = a.requests();
    let results: Vec<Value> = requests.last().map(p_session_support::tool_results).unwrap();
    assert_eq!(results.len(), 2, "{results:?}");
    // A text-only result is a string; one with images a block list.
    let texts = |content: &Value| -> String {
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        content
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let images = |content: &Value| {
        content
            .as_array()
            .map_or(0, |blocks| blocks.iter().filter(|b| b["type"] == "image").count())
    };
    // The first cell: the files under the session's diagrams directory, the light theme (the setting).
    let output = texts(&results[0]);
    let result = &lines(&output, "result")[0];
    let png_path = result["png_path"].as_str().unwrap();
    let outputs = a.root.join("agent/rlm-output");
    assert!(
        png_path.starts_with(&outputs.display().to_string()) && png_path.ends_with("/diagrams/flowchart-1.png"),
        "{png_path}"
    );
    assert_eq!(
        (
            result["width"].clone(),
            result["height"].clone(),
            result["type"].clone()
        ),
        (json!(240), json!(120), json!("flowchart"))
    );
    let svg = std::fs::read_to_string(result["svg_path"].as_str().unwrap()).unwrap();
    assert!(svg.contains(r##"fill="#e8e8f0""##) && !svg.contains("var("), "{svg}");
    // The model got the line, not the image.
    assert!(output.contains(&format!("\ndiagram: {png_path} (240x120)")), "{output}");
    assert_eq!(
        images(&results[0]),
        0,
        "to_model=False keeps the image out of the request"
    );
    assert_eq!(
        lines(&output, "error"),
        vec![
            json!(["ValueError", "diagram.mermaid text must be a non-empty string"]),
            json!([
                "ValueError",
                "diagram.mermaid theme must be \"auto\", \"dark\" or \"light\""
            ]),
        ]
    );
    assert_eq!(lines(&output, "doc"), vec![json!(true)]);
    // The second: to_model=True puts the image in the request, saved where path= says.
    let output = texts(&results[1]);
    let result = &lines(&output, "result")[0];
    assert_eq!(result["png_path"], a.cwd.join("docs/flow.png").display().to_string());
    assert_eq!(images(&results[1]), 1, "{output}");
    assert!(!output.contains("diagram: "), "{output}");
    // The committed results: the first carries the PNG for the TUI in details, the second in its content.
    let mut committed = Vec::new();
    for write in a.writes() {
        committed_results(&write, &mut committed);
    }
    let by_call = |id: &str| committed.iter().rev().find(|r| r["toolCallId"] == id).unwrap().clone();
    let first = by_call("toolu_1");
    let shown = first["details"]["displayImages"]
        .as_array()
        .expect("displayImages")
        .clone();
    assert_eq!(shown.len(), 1, "{first}");
    assert_eq!(shown[0]["mimeType"], "image/png");
    let data = base64::engine::general_purpose::STANDARD
        .decode(shown[0]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(data, std::fs::read(png_path).unwrap());
    assert!(first["content"].as_array().unwrap().iter().all(|b| b["type"] == "text"));
    let second = by_call("toolu_2");
    assert!(
        second["details"].get("displayImages").is_none(),
        "{}",
        second["details"]
    );
    assert_eq!(images(&second["content"]), 1);
    // The guide points at the API.
    assert!(
        requests[0]["system"]
            .to_string()
            .contains("await diagram.mermaid(text)"),
        "the guide names diagram.mermaid"
    );
}

#[test]
fn session_core_diagram_mermaid_without_node_on_the_session_path_is_one_line() {
    let empty = tempfile::tempdir().unwrap();
    let code = "try:\n    await diagram.mermaid('graph TD\\n  a-->b')\nexcept Exception as e:\n    print('error', type(e).__name__, str(e))";
    let a = actor(
        in_order(vec![tool("toolu_1", code), text("done")]),
        Setup {
            vars: vec![("PATH", empty.path().display().to_string())],
            ..Setup::default()
        },
    );
    let ended = a.prompt("draw it");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = p_session_support::tool_results(a.requests().last().unwrap());
    let output = results[0].to_string();
    assert!(
        output.contains("error RuntimeError diagram.mermaid needs Node.js on PATH; the source stays in the transcript"),
        "{output}"
    );
    assert!(!a.root.join("agent/rlm-output").join("diagrams").exists());
}

#[test]
fn session_core_diagram_display_images_add_lines_and_details_within_the_size_cap() {
    use session_core::view_images::{DisplayImage, MAX_DISPLAY_IMAGE_BYTES, with_display_details, with_display_lines};
    let small = DisplayImage {
        png: b"PNG".to_vec(),
        line: "diagram: /d/a-1.png (2x2)".into(),
    };
    let huge = DisplayImage {
        png: vec![0; MAX_DISPLAY_IMAGE_BYTES + 1],
        line: "diagram: /d/b-1.png (9x9)".into(),
    };
    assert_eq!(with_display_lines("out".into(), &[]), "out");
    assert_eq!(
        with_display_lines("out".into(), &[small.clone(), huge.clone()]),
        "out\ndiagram: /d/a-1.png (2x2)\ndiagram: /d/b-1.png (9x9)"
    );
    assert_eq!(with_display_details(json!({}), &[]), json!({}));
    assert_eq!(
        with_display_details(json!({"fileChanges": [1]}), &[small, huge.clone()]),
        json!({"fileChanges": [1], "displayImages": [{"type": "image", "data": "UE5H", "mimeType": "image/png"}]})
    );
    // Too large to keep in the transcript: only its line remains.
    assert_eq!(with_display_details(json!({}), &[huge]), json!({}));
}

/// A cell left running in the background can still show a diagram after its result went out: the next cell of the
/// lane starts without it (its result must not carry a stale image).
#[test]
fn session_core_diagram_display_images_start_empty_with_each_cell() {
    use session_core::view_images::ViewImages;
    let model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let tmp = tempfile::tempdir().unwrap();
    let images = ViewImages::new(model, tmp.path().to_path_buf(), tmp.path().join("agent"), false);
    images.display("main", b"late".to_vec(), "diagram: late".into());
    images.display("worker", b"other".to_vec(), "diagram: other".into());
    images.begin("main");
    assert!(images.take_display("main").is_empty());
    assert_eq!(images.take_display("worker").len(), 1, "other lanes keep theirs");
    images.begin("main");
    images.display("main", b"now".to_vec(), "diagram: now".into());
    let taken = images.take_display("main");
    assert_eq!(
        taken.iter().map(|i| i.line.as_str()).collect::<Vec<_>>(),
        ["diagram: now"]
    );
    assert!(images.take_display("main").is_empty(), "taken once");
}

/// `diagram.mermaid`'s `auto` theme follows the terminal appearance the attached TUI reported (OSC 11, sent as
/// `setTerminalTheme`) over the session's `COLORFGBG`; a pair setting picks that side.
#[test]
fn session_core_diagram_auto_theme_follows_the_terminal_the_tui_reported() {
    let bin = tempfile::tempdir().unwrap();
    let bin = bin.path().canonicalize().unwrap().join("bin");
    fake_node(&bin);
    let cell = "import json\nd = await diagram.mermaid('flowchart LR\\n  a --> b')\nprint('result', json.dumps(d))";
    let script = in_order(vec![
        tool("toolu_1", cell),
        text("one"),
        tool("toolu_2", cell),
        text("two"),
    ]);
    let a = actor(
        script,
        Setup {
            settings: Some(json!({"theme": "light/dark"})),
            // COLORFGBG says dark (background 0).
            vars: vec![("PATH", bin.display().to_string()), ("COLORFGBG", "15;0".into())],
            ..Setup::default()
        },
    );
    // The node surface colour of the `n`th saved diagram: dark #282832, light #e8e8f0.
    let surface = |a: &p_session_support::Actor, n: usize| {
        let mut found = Vec::new();
        let mut stack = vec![a.root.join("agent/rlm-output")];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if path.ends_with(format!("diagrams/flowchart-{n}.svg")) {
                found.push(std::fs::read_to_string(&path).unwrap());
            }
        }
        assert_eq!(found.len(), 1);
        ["#282832", "#e8e8f0"]
            .into_iter()
            .find(|c| found[0].contains(&format!("fill=\"{c}\"")))
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(a.prompt("draw it")["error"], Value::Null);
    assert_eq!(surface(&a, 1), "#282832", "COLORFGBG's dark side");
    // The TUI's OSC 11 answer said light: as a client sends it over the worker's control service.
    let op = session_core::core::Op::from_json(&json!({"op": "setTerminalTheme", "theme": "light"})).unwrap();
    assert_eq!(op.to_json(), json!({"op": "setTerminalTheme", "theme": "light"}));
    assert_eq!(a.call(op), Ok(None));
    assert_eq!(
        a.call(session_core::core::Op::SetTerminalTheme { theme: "sepia".into() }),
        Err("theme must be light or dark, not \"sepia\"".into())
    );
    assert_eq!(a.prompt("draw it again")["error"], Value::Null);
    assert_eq!(surface(&a, 2), "#e8e8f0", "the reported light side");
}
