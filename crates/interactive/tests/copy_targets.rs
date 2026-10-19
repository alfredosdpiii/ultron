//! `/copy` targets: replays fixtures/omp-copy-parity.json (omp copy-targets.ts and copy-selector.ts helpers,
//! recorded by fixtures/generate-omp-copy.mjs), the ported omp copy-targets.test.ts cases with their literal
//! expectations, and Ultron's `rlm` cells as commands.

use interactive::copy_targets::{
    BlockKind, CommandKind, LastCommand, LinkTarget, MessageBlock, TranscriptEntry, collect_blocks,
    command_from_tool_call, extract_blocks, extract_links, target_copy, tool_call_label,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/omp-copy-parity.json")).unwrap()
}

fn command_json(c: &Option<LastCommand>) -> Value {
    match c {
        None => Value::Null,
        Some(c) => json!({
            "kind": match c.kind { CommandKind::Bash => "bash", CommandKind::Eval => "eval", CommandKind::Rlm => "rlm" },
            "code": c.code,
            "language": c.language,
        }),
    }
}

fn links_json(links: &[LinkTarget]) -> Value {
    Value::Array(links.iter().map(|l| json!({"text": l.text, "href": l.href})).collect())
}

#[test]
fn interactive_copy_blocks_and_links_match_omp() {
    let f = fixture();
    for case in f["blocks"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let blocks: Vec<Value> = extract_blocks(text)
            .into_iter()
            .map(|b| match b {
                MessageBlock::Code { lang, code } => json!({"kind": "code", "lang": lang, "code": code}),
                MessageBlock::Quote { text } => json!({"kind": "quote", "text": text}),
            })
            .collect();
        assert_eq!(Value::Array(blocks), case["blocks"], "{text:?}");
        assert_eq!(links_json(&extract_links(text)), case["links"], "{text:?}");
    }
}

#[test]
fn interactive_copy_commands_and_last_targets_match_omp() {
    let f = fixture();
    for case in f["calls"].as_array().unwrap() {
        let got = command_from_tool_call(case["name"].as_str().unwrap(), &case["args"]);
        assert_eq!(command_json(&got), case["command"], "{case}");
    }
}

#[test]
fn interactive_copy_turn_blocks_payload_and_summary_match_omp() {
    let f = fixture();
    for case in f["turns"].as_array().unwrap() {
        let entries: Vec<TranscriptEntry> = case["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| TranscriptEntry::new(e["id"].as_str().unwrap(), "2026-10-11T10:00:00Z", e["message"].clone()))
            .collect();
        let indices: Vec<usize> = (0..entries.len()).collect();
        let blocks = collect_blocks(&entries, &indices);
        let got: Vec<Value> = blocks
            .iter()
            .map(|b| {
                let mut v = json!({"label": b.label, "content": b.content, "entry": b.entry});
                if let Some(kind) = b.kind {
                    v["kind"] = json!(match kind {
                        BlockKind::Code => "code",
                        BlockKind::Quote => "quote",
                        BlockKind::Command => "command",
                    });
                }
                if let Some(language) = &b.language {
                    v["language"] = json!(language);
                }
                if let Some(href) = &b.href {
                    v["href"] = json!(href);
                }
                v
            })
            .collect();
        assert_eq!(Value::Array(got), case["blocks"], "{case}");
        let (content, label) = target_copy(&entries, 0, &blocks);
        assert_eq!(json!({"content": content, "label": label}), case["copy"], "{case}");
    }
    for case in f["labels"].as_array().unwrap() {
        assert_eq!(
            tool_call_label(case[0].as_str().unwrap(), &case[1]),
            case[2].as_str().unwrap()
        );
    }
}

// Ported from omp packages/tui/test/copy-targets.test.ts.

#[test]
fn interactive_copy_ported_extract_links() {
    let l = |text: &str, href: &str| LinkTarget {
        text: text.into(),
        href: href.into(),
    };
    let same = |href: &str| l(href, href);
    assert_eq!(
        extract_links("(https://en.wikipedia.org/wiki/Foo_(bar))."),
        vec![same("https://en.wikipedia.org/wiki/Foo_(bar)")]
    );
    assert_eq!(
        extract_links(
            "Run `curl https://example.com/in-code` then:\n```bash\ncurl https://example.com/fenced\n```\nDocs: https://example.com/prose"
        ),
        vec![same("https://example.com/prose")]
    );
    assert!(extract_links("[file](file:///tmp/x) [mail](mailto:a@b.c) ftp://x.y/z a@b.c").is_empty());
    assert_eq!(
        extract_links("[ref link][r] and www.example.com/www\n\n[r]: https://example.com/ref"),
        vec![
            l("ref link", "https://example.com/ref"),
            l("www.example.com/www", "http://www.example.com/www")
        ]
    );
}

#[test]
fn interactive_copy_rlm_cells_are_python_commands() {
    let rlm = command_from_tool_call("rlm", &json!({"code": "x = rlm.spawn('task')\nprint(x)"})).unwrap();
    assert_eq!(rlm.kind, CommandKind::Rlm);
    assert_eq!(rlm.language, "python");
    assert_eq!(command_from_tool_call("rlm", &json!({"code": ""})), None);
    let entries = vec![TranscriptEntry::new(
        "a",
        "t",
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "Running a cell."},
            {"type": "toolCall", "id": "c", "name": "rlm", "arguments": {"code": "print(1)"}}
        ]}),
    )];
    let blocks = collect_blocks(&entries, &[0]);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].label, "rlm code");
    assert_eq!(blocks[0].content, "print(1)");
    assert_eq!(blocks[0].kind, Some(BlockKind::Command));
}
