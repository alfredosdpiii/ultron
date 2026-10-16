//! `!command` config values in auth.json (core/resolve-config-value.ts, auth-storage.ts:446), ported from
//! packages/coding-agent/test/resolve-config-value.test.ts and auth-storage.test.ts with their literal expectations.

use config::auth::{clear_command_cache, resolve_config_value, resolve_stored_api_key};
use serde_json::json;

fn none(_: &str) -> Option<String> {
    None
}

#[test]
fn config_command_values_run_in_a_shell_and_are_trimmed() {
    assert_eq!(
        resolve_config_value("!echo '  spaced-key  '", &none)
            .unwrap()
            .as_deref(),
        Some("spaced-key")
    );
    assert_eq!(
        resolve_config_value("!printf 'line1\\nline2'", &none)
            .unwrap()
            .as_deref(),
        Some("line1\nline2")
    );
    assert_eq!(
        resolve_config_value("!echo 'hello world' | tr ' ' '-'", &none)
            .unwrap()
            .as_deref(),
        Some("hello-world")
    );
}

#[test]
fn config_command_values_that_fail_resolve_to_nothing() {
    for command in ["!exit 1", "!nonexistent-command-12345", "!printf ''"] {
        assert_eq!(resolve_config_value(command, &none).unwrap(), None, "{command}");
    }
}

#[test]
fn config_command_values_are_cached_until_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("counter");
    std::fs::write(&counter, "0").unwrap();
    let path = counter.display().to_string();
    let success = format!("!sh -c 'count=$(cat \"{path}\"); echo $((count + 1)) > \"{path}\"; echo value'");
    let read = || std::fs::read_to_string(&counter).unwrap().trim().to_string();
    assert_eq!(resolve_config_value(&success, &none).unwrap().as_deref(), Some("value"));
    assert_eq!(resolve_config_value(&success, &none).unwrap().as_deref(), Some("value"));
    assert_eq!(read(), "1");
    let failure = format!("!sh -c 'count=$(cat \"{path}\"); echo $((count + 1)) > \"{path}\"; exit 1'");
    assert_eq!(resolve_config_value(&failure, &none).unwrap(), None);
    assert_eq!(resolve_config_value(&failure, &none).unwrap(), None);
    assert_eq!(read(), "2");
    clear_command_cache();
    assert_eq!(resolve_config_value(&success, &none).unwrap().as_deref(), Some("value"));
    assert_eq!(read(), "3");
}

#[test]
fn config_stored_command_keys_resolve_like_the_typescript_store() {
    // auth-storage.test.ts "resolves command-backed API-key credentials".
    let credential = json!({"type": "api_key", "key": "!printf 'command-key'"});
    assert_eq!(
        resolve_stored_api_key(credential, &none).unwrap(),
        Some(json!({"type": "api_key", "key": "command-key"}))
    );
    // A command that fails leaves the credential without a key (the provider then falls back as for an unset `$VAR`).
    let failing = json!({"type": "api_key", "key": "!exit 7"});
    assert_eq!(
        resolve_stored_api_key(failing, &none).unwrap(),
        Some(json!({"type": "api_key"}))
    );
    // `$!` escapes a literal `!`: no command runs.
    let escaped = json!({"type": "api_key", "key": "$!printf nope"});
    assert_eq!(
        resolve_stored_api_key(escaped, &none).unwrap(),
        Some(json!({"type": "api_key", "key": "!printf nope"}))
    );
}
