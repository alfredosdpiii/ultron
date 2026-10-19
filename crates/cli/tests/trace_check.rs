//! `ultron trace check <file>` (Rust-only): every reset of a request trace (`ULTRON_REQUEST_TRACE`) with its reason,
//! a summary line, and the exit status: 0 when every request appended, 1 with a reset, 2 for a usage error or an
//! unreadable trace. Runs the CLI entry (`cli::run`) with a private environment.

use std::path::Path;
use std::sync::Arc;

fn run(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let root = cwd.to_path_buf();
    let env = cli::Env {
        cwd: root.clone(),
        home: root.clone(),
        var: Arc::new(move |key| match key {
            "HOME" => Some(root.display().to_string()),
            "ULTRON_CODING_AGENT_DIR" => Some(root.join("agent").display().to_string()),
            "ULTRON_SERVER_DIR" => Some(root.join("server").display().to_string()),
            "PI_OFFLINE" => Some("1".into()),
            _ => None,
        }),
        docs: cwd.join("docs"),
    };
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&argv, &env, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn cli_trace_check_reports_each_reset_with_its_reason() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("calls.jsonl");
    std::fs::write(
        &file,
        [
            r#"{"session":"s1","lane":"main","request":1,"kind":"start","base":0,"messages":[]}"#,
            r#"{"session":"s1","lane":"main","request":2,"kind":"append","base":2,"messages":[]}"#,
            r#"{"session":"s1","lane":"conversation:2","request":1,"kind":"start","base":0,"messages":[]}"#,
            r#"{"session":"s1","lane":"main","request":3,"kind":"reset","reason":"compaction","at":1,"messages":[]}"#,
            r#"{"session":"s1","lane":"conversation:2","request":2,"kind":"reset","reason":"context_edit","at":4,"messages":[]}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let path = file.display().to_string();
    let (code, out, err) = run(&["trace", "check", &path], dir.path());
    assert_eq!(err, "");
    assert_eq!(
        out,
        format!(
            "{path}:4: session s1 lane main request 3: reset (compaction) at message 1\n\
             {path}:5: session s1 lane conversation:2 request 2: reset (context_edit) at message 4\n\
             5 requests in 2 lanes, 2 resets\n"
        )
    );
    assert_eq!(code, 1);
}

#[test]
fn cli_trace_check_passes_an_append_only_trace_and_rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("calls.jsonl");
    std::fs::write(
        &file,
        r#"{"session":"s1","lane":"main","request":1,"kind":"start","base":0,"messages":[]}"#,
    )
    .unwrap();
    let path = file.display().to_string();
    assert_eq!(
        run(&["trace", "check", &path], dir.path()),
        (0, "1 request in 1 lane, 0 resets\n".to_string(), String::new())
    );
    assert_eq!(
        run(&["trace"], dir.path()),
        (2, String::new(), "Usage: ultron trace check <file>\n".to_string())
    );
    std::fs::write(&file, "not json\n").unwrap();
    let (code, out, err) = run(&["trace", "check", &path], dir.path());
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.starts_with(&format!("Error: {path}: line 1: not a trace record (")),
        "{err}"
    );
    let missing = dir.path().join("missing.jsonl").display().to_string();
    let (code, _, err) = run(&["trace", "check", &missing], dir.path());
    assert_eq!(code, 2);
    assert!(err.starts_with(&format!("Error: {missing}: ")), "{err}");
}
