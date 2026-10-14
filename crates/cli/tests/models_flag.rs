//! `--models` on the native command: parsed (args.ts `models`), but TypeScript's `runNativeUltronCommand` starts the
//! foreground server with model, provider, apiKey and thinking only (native-command.ts `startForegroundServer`), and
//! the session worker resolves its model with `scopedModels: []`. The Rust session flags carry no scope either, so
//! only the settings' `enabledModels` choose a scope (Ctrl+P, and a new session's model).

use cli::args::parse_args;
use cli::print_mode::flags;

#[test]
fn cli_models_flag_is_parsed_but_not_forwarded_to_the_session() {
    let args: Vec<String> = ["-p", "--models", "sonnet,gpt-4o:high", "--model", "a/b", "hi"]
        .into_iter()
        .map(String::from)
        .collect();
    let parsed = parse_args(&args);
    assert_eq!(
        parsed.models,
        Some(vec!["sonnet".to_string(), "gpt-4o:high".to_string()])
    );
    let session = flags(&parsed);
    assert_eq!(session.model.as_deref(), Some("a/b"));
    assert_eq!(session.models, None);
}
