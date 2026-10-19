//! A sandboxed Host's launcher runs from the host's base environment (the session's, `Host::with_base_env`): the
//! launcher binary is looked up on that PATH and started with only that PATH and DOCKER_HOST. The command is built,
//! never spawned; Docker is never run. New in Rust: literal expectations.

use host::{AllowAll, Host};
use sandbox::launcher::Mechanism;
use serde_json::json;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;

#[test]
fn host_w17e_a_sandboxed_hosts_launcher_uses_the_base_env_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (project, bin) = (root.join("project"), root.join("bin"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("bwrap");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = bin.display().to_string();
    let host = Host::new(&project, project.join(".jobs"), &project, Box::new(AllowAll))
        .with_base_env(vec![
            ("PATH".into(), path.clone()),
            ("HOME".into(), root.display().to_string()),
            ("DOCKER_HOST".into(), "unix:///w17e.sock".into()),
        ])
        .sandboxed_with_mechanism(
            &json!({"sandbox": {"mode": "required"}}),
            &root,
            None,
            Some(Mechanism::Bwrap),
        );
    let launcher = host.launcher().unwrap();
    assert_eq!(
        launcher.policy.launch,
        vec![
            ("PATH".to_string(), path.clone()),
            ("DOCKER_HOST".to_string(), "unix:///w17e.sock".to_string())
        ]
    );
    let command = launcher.command(&["true".into()], &project).unwrap();
    assert_eq!(command.get_program(), fake.as_os_str());
    let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
    assert_eq!(
        envs,
        vec![
            (OsStr::new("DOCKER_HOST"), Some(OsStr::new("unix:///w17e.sock"))),
            (OsStr::new("PATH"), Some(OsStr::new(path.as_str()))),
        ]
    );
}
