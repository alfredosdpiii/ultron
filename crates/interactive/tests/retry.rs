//! Retry (`app.retry`, F5): the retryable-tail rule of omp turn-recovery.ts and the idle hint row advertising the
//! live binding.

use std::rc::Rc;

use interactive::retry::{failed_turn_tail, retry_hint_rows};
use serde_json::json;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

fn assistant(stop: &str) -> serde_json::Value {
    json!({"role": "assistant", "content": [], "stopReason": stop})
}

fn result(error: bool) -> serde_json::Value {
    json!({"role": "toolResult", "toolName": "rlm", "content": [], "isError": error})
}

#[test]
fn interactive_retry_failed_tail_rule() {
    let user = json!({"role": "user", "content": "go"});
    assert!(failed_turn_tail(&[user.clone(), assistant("error")]));
    assert!(
        failed_turn_tail(&[user.clone(), assistant("aborted"), result(true)]),
        "placeholders trail the failed turn"
    );
    assert!(
        failed_turn_tail(&[user.clone(), assistant("toolUse"), result(true)]),
        "Esc landed during the tool call"
    );
    assert!(
        !failed_turn_tail(&[user.clone(), assistant("toolUse"), result(false)]),
        "a tool still in flight"
    );
    assert!(!failed_turn_tail(&[user.clone(), assistant("toolUse")]));
    assert!(
        !failed_turn_tail(&[user.clone(), assistant("stop")]),
        "a settled turn leaves nothing to retry"
    );
    assert!(!failed_turn_tail(&[user]));
    assert!(!failed_turn_tail(&[]));
}

#[test]
fn interactive_retry_hint_names_the_live_binding() {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let failed = [json!({"role": "user", "content": "go"}), assistant("error")];
    let keys = KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    );
    let rows = retry_hint_rows(&theme, &keys, &failed, false).unwrap();
    assert_eq!(
        rows.iter().map(|r| strip_terminal_sequences(r)).collect::<Vec<_>>(),
        ["", " f5 to retry"]
    );
    assert_eq!(
        retry_hint_rows(&theme, &keys, &failed, true),
        None,
        "never while streaming"
    );
    let rebound = KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        vec![("app.retry".into(), Some(vec!["ctrl+shift+r".into()]))],
        Default::default(),
    );
    let rows = retry_hint_rows(&theme, &rebound, &failed, false).unwrap();
    assert_eq!(strip_terminal_sequences(&rows[1]), " ctrl+shift+r to retry");
    let unbound = KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        vec![("app.retry".into(), Some(vec![]))],
        Default::default(),
    );
    assert_eq!(retry_hint_rows(&theme, &unbound, &failed, false), None);
}
