//! `host/exec` as `execCommand` (core/exec.ts) runs a command for `pi.exec`: no shell, output collected whole, the
//! exit code (0 when a signal ended it), code 1 with empty output when it cannot start, `killed` after a timeout or
//! a cancel (SIGTERM to the one child it spawned). Literal expectations from execCommand's code.

use extensions::exec::exec;
use extensions::rpc::CancelToken;
use serde_json::json;

#[test]
fn extensions_exec_collects_output_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let out = exec(
        &json!({ "command": "/bin/sh", "args": ["-c", "pwd; echo err >&2; exit 3"], "cwd": "sub" }),
        dir.path(),
        &CancelToken::new(),
    )
    .unwrap();
    let cwd = dir.path().join("sub").canonicalize().unwrap();
    assert_eq!(
        out,
        json!({ "stdout": format!("{}\n", cwd.display()), "stderr": "err\n", "code": 3, "killed": false })
    );
    // No shell: `$HOME` is passed through literally.
    let out = exec(
        &json!({ "command": "/bin/echo", "args": ["$HOME", "a b"] }),
        dir.path(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out["stdout"], "$HOME a b\n");
    assert_eq!(out["code"], 0);
}

#[test]
fn extensions_exec_missing_command_is_code_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(
        &json!({ "command": "/nonexistent/tool" }),
        dir.path(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(out, json!({ "stdout": "", "stderr": "", "code": 1, "killed": false }));
    let bad = exec(&json!({ "args": [] }), dir.path(), &CancelToken::new()).unwrap_err();
    assert!(bad.to_string().contains("\"command\""));
}

#[test]
fn extensions_exec_timeout_and_cancel_kill_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let out = exec(
        &json!({ "command": "/bin/sh", "args": ["-c", "echo started; exec sleep 600"], "timeoutMs": 300 }),
        dir.path(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(
        out,
        json!({ "stdout": "started\n", "stderr": "", "code": 0, "killed": true })
    );
    let cancel = CancelToken::new();
    cancel.cancel(Some("stop"));
    let out = exec(
        &json!({ "command": "/bin/sleep", "args": ["600"] }),
        dir.path(),
        &cancel,
    )
    .unwrap();
    assert_eq!(out["killed"], true);
    assert_eq!(out["code"], 0);
}
