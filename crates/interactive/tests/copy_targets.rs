//! `/copy` targets: replays fixtures/omp-copy-parity.json (omp copy-targets.ts and copy-selector.ts helpers,
//! recorded by fixtures/generate-omp-copy.mjs), the ported omp copy-targets.test.ts cases with their literal
//! expectations, and Ultron's `rlm` cells as commands.

use interactive::copy_targets::{
    BlockKind, CommandKind, LastCommand, LinkTarget, MessageBlock, TranscriptEntry, assistant_text, collect_blocks,
    command_from_tool_call, extract_blocks, extract_code_blocks, extract_last_code_block, extract_last_command,
    extract_last_link, extract_links, extract_quote_blocks, target_copy, tool_call_label, turn_summary,
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
    for case in f["lasts"].as_array().unwrap() {
        let messages = case["messages"].as_array().unwrap();
        assert_eq!(command_json(&extract_last_command(messages)), case["command"]);
        let link = extract_last_link(messages).map(|l| json!({"text": l.text, "href": l.href}));
        assert_eq!(link.unwrap_or(Value::Null), case["link"]);
        let code = extract_last_code_block(messages).map(|(lang, code)| json!({"lang": lang, "code": code}));
        assert_eq!(code.unwrap_or(Value::Null), case["code"]);
        let texts: Vec<Value> = messages
            .iter()
            .map(|m| assistant_text(m).map_or(Value::Null, Value::String))
            .collect();
        assert_eq!(Value::Array(texts), case["text"]);
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
        assert_eq!(
            turn_summary(&entries[0]),
            case["summary"]["label"].as_str().unwrap(),
            "{case}"
        );
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
fn interactive_copy_ported_extract_code_and_quote_blocks() {
    assert_eq!(
        extract_code_blocks("intro\n```ts\nconst x = 1;\n```\ntail"),
        vec![("ts".to_string(), "const x = 1;".to_string())]
    );
    let blocks = extract_code_blocks("```\nplain\n```\n\n```py\nprint(1)\n```");
    assert_eq!(blocks.iter().map(|b| b.0.as_str()).collect::<Vec<_>>(), ["", "py"]);
    assert_eq!(
        blocks.iter().map(|b| b.1.as_str()).collect::<Vec<_>>(),
        ["plain", "print(1)"]
    );
    assert_eq!(
        extract_quote_blocks("intro\n> line one\n> line two\ntail"),
        ["line one\nline two"]
    );
    assert_eq!(
        extract_quote_blocks("> first\n>\n> second\n\nbreak\n> later"),
        ["first\n\nsecond", "later"]
    );
    assert_eq!(
        extract_quote_blocks("> real quote\n```\n> not a quote\n```"),
        ["real quote"]
    );
}

#[test]
fn interactive_copy_ported_extract_last_command() {
    let calls = |c: Value| json!({"role": "assistant", "content": c});
    let messages = vec![
        calls(json!([{"type": "toolCall", "id": "0", "name": "bash", "arguments": {"command": "echo old"}}])),
        calls(json!([{"type": "toolCall", "id": "0", "name": "read", "arguments": {"path": "x"}}])),
        calls(json!([
            {"type": "toolCall", "id": "0", "name": "bash", "arguments": {"command": "echo a"}},
            {"type": "toolCall", "id": "1", "name": "bash", "arguments": {"command": "echo b"}}
        ])),
    ];
    assert_eq!(
        extract_last_command(&messages),
        Some(LastCommand {
            kind: CommandKind::Bash,
            code: "echo b".into(),
            language: "bash".into()
        })
    );
    let py = [calls(
        json!([{"type": "toolCall", "id": "0", "name": "eval", "arguments": {"language": "py", "code": "print(1)"}}]),
    )];
    assert_eq!(extract_last_command(&py).unwrap().language, "python");
    let js = [calls(
        json!([{"type": "toolCall", "id": "0", "name": "eval", "arguments": {"language": "js", "code": "log(1)"}}]),
    )];
    assert_eq!(extract_last_command(&js).unwrap().language, "javascript");
    let cells = [calls(json!([{"type": "toolCall", "id": "0", "name": "eval",
        "arguments": {"cells": [{"language": "py", "code": "print(1)"}, {"code": "print(2)"}]}}]))];
    assert_eq!(extract_last_command(&cells).unwrap().code, "print(1)\n\nprint(2)");
}

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
    let messages = [
        json!({"role": "assistant", "content": [{"type": "text", "text": "old https://example.com/old"}]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "a https://example.com/a and b https://example.com/b"}]}),
        json!({"role": "user", "content": "thanks https://example.com/user-link"}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "no links here"}]}),
    ];
    assert_eq!(extract_last_link(&messages), Some(same("https://example.com/b")));
    assert_eq!(extract_last_link(&messages[3..]), None);
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
