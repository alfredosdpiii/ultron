//! The sandbox launcher (bwrap, unshare, setpriv, the Docker client) runs from the session's environment, never this
//! process's: its PATH and DOCKER_HOST come from the environment the policy was built from (`Policy::launch`), with
//! `SANDBOX_PATH` when that has no PATH. Pure checks on the computed commands; nothing is spawned and Docker is never
//! run. New in Rust: literal expectations.

use sandbox::launcher::{
    Launcher, Layout, Mechanism, PathLayout, Policy, RealLayout, SANDBOX_PATH, launch_env, resolve_program,
};
use sandbox::settings::{Mode, Settings};
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// A directory holding fake executables `names` (never run).
fn fake_bin(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().canonicalize().unwrap().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for name in names {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, bin)
}

fn policy(project: &Path, host_env: &[(String, String)]) -> Policy {
    Policy::new(&Settings::default(), project, project, &[], None, host_env)
}

fn command_env(command: &std::process::Command) -> Vec<(String, Option<String>)> {
    command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

#[test]
fn sandbox_w17e_launch_env_is_the_session_path_and_docker_host_else_sandbox_path() {
    assert_eq!(
        launch_env(&env(&[
            ("HOME", "/h"),
            ("PATH", "/session/bin"),
            ("DOCKER_HOST", "unix:///d.sock"),
            ("X", "1")
        ])),
        env(&[("PATH", "/session/bin"), ("DOCKER_HOST", "unix:///d.sock")])
    );
    // No PATH in the session: the fixed one, never this process's; no DOCKER_HOST: none.
    assert_eq!(launch_env(&env(&[("HOME", "/h")])), env(&[("PATH", SANDBOX_PATH)]));
    assert_eq!(SANDBOX_PATH, "/usr/local/bin:/usr/bin:/bin");
    let dir = tempfile::tempdir().unwrap();
    let built = policy(
        dir.path(),
        &env(&[("PATH", "/session/bin"), ("DOCKER_HOST", "tcp://127.0.0.1:1")]),
    );
    assert_eq!(
        built.launch,
        env(&[("PATH", "/session/bin"), ("DOCKER_HOST", "tcp://127.0.0.1:1")])
    );
    assert_eq!(policy(dir.path(), &[]).launch, env(&[("PATH", SANDBOX_PATH)]));
}

#[test]
fn sandbox_w17e_the_launcher_binary_is_found_on_the_session_path_and_started_with_only_its_env() {
    let (_dir, bin) = fake_bin(&["bwrap"]);
    let project = tempfile::tempdir().unwrap();
    let path = format!("{}:/nowhere", bin.display());
    let session = env(&[("PATH", &path), ("DOCKER_HOST", "unix:///fake.sock"), ("SECRET", "s")]);
    let launcher =
        Launcher::new(Mode::Required, policy(project.path(), &session)).with_mechanism(Some(Mechanism::Bwrap));
    let command = launcher.command(&["true".into()], project.path()).unwrap();
    assert_eq!(command.get_program(), bin.join("bwrap").as_os_str());
    assert_eq!(command.get_args().next(), Some(OsStr::new("--unshare-all")));
    // env_clear, then exactly the launcher's environment.
    assert_eq!(
        command_env(&command),
        vec![
            ("DOCKER_HOST".to_string(), Some("unix:///fake.sock".to_string())),
            ("PATH".to_string(), Some(path.clone())),
        ]
    );
    let persistent = launcher.command_persistent(&["true".into()], project.path()).unwrap();
    // `sh` of the persistent wrapper is looked up on the session PATH too (absent there: left bare).
    assert_eq!(persistent.get_program(), OsStr::new("sh"));
    assert_eq!(command_env(&persistent), command_env(&command));
}

#[test]
fn sandbox_w17e_a_launcher_only_on_the_process_path_is_not_resolved() {
    // The session PATH has no bwrap: the program stays bare, so the spawn searches the command's own PATH (the
    // session's), not this process's.
    let (_dir, bin) = fake_bin(&["other"]);
    let project = tempfile::tempdir().unwrap();
    let session = env(&[("PATH", &bin.display().to_string())]);
    let launcher =
        Launcher::new(Mode::Required, policy(project.path(), &session)).with_mechanism(Some(Mechanism::Bwrap));
    let command = launcher.command(&["true".into()], project.path()).unwrap();
    assert_eq!(command.get_program(), OsStr::new("bwrap"));
    assert_eq!(
        command_env(&command),
        vec![("PATH".to_string(), Some(bin.display().to_string()))]
    );
    assert_eq!(
        resolve_program("/abs/bwrap", &bin.display().to_string()),
        PathBuf::from("/abs/bwrap")
    );
}

#[test]
fn sandbox_w17e_setpriv_is_used_only_when_the_session_path_has_it() {
    let project = tempfile::tempdir().unwrap();
    let (_with, with_bin) = fake_bin(&["setpriv"]);
    let (_without, without_bin) = fake_bin(&["other"]);
    let argv = |bin: &Path| {
        let policy = policy(project.path(), &env(&[("PATH", &bin.display().to_string())]));
        policy.wrap(Mechanism::Unshare, &policy.layout(), &["true".into()], project.path())
    };
    assert_eq!(argv(&with_bin)[..4], ["setpriv", "--pdeathsig", "KILL", "--"]);
    assert_eq!(argv(&without_bin)[0], "unshare");
    let layout = PathLayout {
        path: with_bin.display().to_string(),
    };
    assert!(layout.which("setpriv"));
    assert!(!layout.which("other"));
    // RealLayout looks on SANDBOX_PATH only: a fake in a temp dir is never found there.
    assert!(!RealLayout.which("setpriv-w17e-not-installed"));
}
