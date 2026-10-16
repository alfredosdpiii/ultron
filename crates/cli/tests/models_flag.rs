//! `--models` on the native command: parsed (args.ts `models`) and, unlike TypeScript's `runNativeUltronCommand`
//! (which drops it), forwarded to the session worker, where it scopes a new session's model as Pi's
//! `buildSessionOptions` does (decisions: scoped models choose a new session's model); without it the settings'
//! `enabledModels` apply.

use cli::args::parse_args;
use cli::print_mode::flags;

#[test]
fn cli_models_flag_is_parsed_and_forwarded_to_the_session() {
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
    assert_eq!(session.models, parsed.models);
}
