//! A sandboxed Host without a base environment builds its sandbox policy from an empty environment plus
//! `PATH=/usr/local/bin:/usr/bin:/bin` (`sandbox::launcher::SANDBOX_PATH`) and `HOME` = the sandbox's home, never from
//! this process's environment: neither the sandbox's own environment nor its launcher's sees the process's PATH,
//! HOME or allowlisted variables.
//! Pure checks on the computed policy; nothing is spawned. New in Rust (wave 41): literal expectations.

use host::sandboxed::sandbox_host_env;
use host::{AllowAll, Host};
use sandbox::launcher::Mechanism;
use serde_json::json;

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn host_w18s_sandbox_host_env_is_the_base_env_else_only_the_sandbox_path() {
    let home = std::path::Path::new("/sandbox/home");
    assert_eq!(
        sandbox_host_env(None, home),
        env(&[("PATH", "/usr/local/bin:/usr/bin:/bin"), ("HOME", "/sandbox/home")])
    );
    let base = env(&[("PATH", "/session/bin"), ("HOME", "/session/home")]);
    assert_eq!(sandbox_host_env(Some(&base), home), base);
    assert_eq!(sandbox_host_env(Some(&[]), home), Vec::<(String, String)>::new());
}

#[test]
fn host_w18s_a_sandboxed_host_without_a_base_env_never_reads_the_process_env() {
    // The test process has CARGO_MANIFEST_DIR (cargo sets it) and a PATH other than the sandbox's: an allowlist that
    // names them must still not pass the process's values in.
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let host = Host::new(&project, project.join(".jobs"), &project, Box::new(AllowAll)).sandboxed_with_mechanism(
        &json!({"sandbox": {"mode": "required", "env": ["CARGO_MANIFEST_DIR", "HOME"]}}),
        &root,
        None,
        Some(Mechanism::Bwrap),
    );
    let policy = &host.launcher().unwrap().policy;
    assert_eq!(policy.launch, env(&[("PATH", "/usr/local/bin:/usr/bin:/bin")]));
    let home = root.display().to_string();
    assert_ne!(std::env::var("HOME").ok().as_deref(), Some(home.as_str()));
    assert_eq!(
        policy.env,
        env(&[
            ("PATH", "/usr/local/bin:/usr/bin:/bin"),
            ("HOME", &home),
            ("TMPDIR", "/tmp")
        ])
    );
}
