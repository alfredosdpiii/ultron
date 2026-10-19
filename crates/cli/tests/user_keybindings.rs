//! The composition root's `KeybindingsManager.create(agentDir)`: `<agentDir>/keybindings.json` migrated and kept to
//! string bindings; a missing or malformed file is no bindings.

#[test]
fn cli_user_keybindings_load_the_agent_dir_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let linux = tui::keybindings::Platform::new("linux", false);
    assert!(cli::interactive::user_keybindings(dir, linux).is_empty());
    std::fs::write(
        dir.join("keybindings.json"),
        r#"{"app.clear": 1, "exit": "ctrl+q", "zzz": ["a"]}"#,
    )
    .unwrap();
    assert_eq!(
        cli::interactive::user_keybindings(dir, linux),
        [
            ("app.exit".to_string(), Some(vec!["ctrl+q".to_string()])),
            ("zzz".to_string(), Some(vec!["a".to_string()])),
        ]
    );
    std::fs::write(dir.join("keybindings.json"), "{broken").unwrap();
    assert!(cli::interactive::user_keybindings(dir, linux).is_empty());
}
