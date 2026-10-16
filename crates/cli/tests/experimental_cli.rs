//! Replays `fixtures/experimental_cli.json` (recorded from the TypeScript experimental CLI's `cli.parse(argv)` by
//! `fixtures/experimental_cli.mjs`): `ultron server` / `ultron client` command lines, `--auth-token(-file)` and
//! `--connect` transport addresses, each parsed exactly as TypeScript does (the command's keys in its order, or its
//! errors in its order).

use serde_json::{Value, json};

#[test]
fn cli_experimental_command_lines_parse_like_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/experimental_cli.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 100);
    for case in cases {
        let argv: Vec<String> = case["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let parsed = cli::experimental::parse(&argv).expect("server or client");
        let got = match parsed {
            Ok(command) => json!({"ok": true, "command": command.to_json()}),
            Err(errors) => json!({"ok": false, "errors": errors}),
        };
        // Compared as text: the keys' order is part of the answer.
        assert_eq!(got.to_string(), case["result"].to_string(), "argv {argv:?}");
    }
}

#[test]
fn cli_experimental_dispatches_only_server_and_client() {
    assert!(cli::experimental::parse(&["list".to_string()]).is_none());
    assert!(cli::experimental::parse(&[]).is_none());
}
