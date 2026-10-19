//! A host given a base environment (the session's, `Host::with_base_env`) runs its commands from it, never from this
//! process's: blocking bash and shell jobs see only its variables (plus the command's own, `with_env`), find tools on
//! its PATH, and the sandbox's allowlisted variables are taken from it too.

use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn host(root: &Path) -> Host {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("w16tool");
    std::fs::write(&tool, "#!/bin/sh\necho \"fake $W16F_SESSION_ONLY $W16F_EXTRA\"\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    Host::new(root, root.join("jobs"), root, Box::new(AllowAll))
        .with_base_env(vec![
            ("PATH".into(), bin.display().to_string()),
            ("W16F_SESSION_ONLY".into(), "session".into()),
        ])
        .with_env(vec![("W16F_EXTRA".into(), "extra".into())])
}

#[test]
fn host_w16f_blocking_bash_and_shell_jobs_run_in_the_base_env_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = host(&root);
    let signal = Signal::new();
    // CARGO_MANIFEST_DIR is in this process's environment (cargo sets it), never in the commands'.
    let command = "w16tool; command -v ls || echo no-ls; echo \"[${CARGO_MANIFEST_DIR:-unset}]\"";
    let blocking = host
        .handle("bash", payload(json!({"command": command})), &signal)
        .unwrap();
    assert_eq!(blocking["output"], "fake session extra\nno-ls\n[unset]\n");
    let job = host
        .handle(
            "shell.run",
            payload(json!({"command": command, "yield_after": 0})),
            &signal,
        )
        .unwrap();
    let done = host
        .handle("shell.result", payload(json!({"id": job["id"]})), &signal)
        .unwrap();
    assert_eq!(
        (done["status"].as_str(), done["text"].as_str()),
        (Some("completed"), Some("fake session extra\nno-ls\n[unset]\n"))
    );
}

#[test]
fn host_w16f_the_sandbox_allowlist_takes_its_values_from_the_base_env() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = host(&root).sandboxed(&json!({"sandbox": {"mode": "auto"}}), &root, None);
    let env = &host.launcher().unwrap().policy.env;
    let path = env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str());
    assert_eq!(path, Some(root.join("bin").display().to_string().as_str()));
    // HOME is allowlisted but absent from the base environment: not taken from this process.
    assert!(!env.iter().any(|(key, _)| key == "HOME"), "{env:?}");
}
