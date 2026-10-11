//! Process tests that try real escapes under every backend available on this machine. A backend that is not
//! installed or does not work here is skipped with the reason printed (Docker also needs a local image named by
//! ULTRON_SANDBOX_TEST_IMAGE; images are never pulled).
//!
//! The scratch tree lives under the cargo target dir (inside /home on a usual checkout), not /tmp, so the read-only
//! home and /home rules are exercised rather than hidden by a private /tmp:
//!   base/home                fake HOME: notes.txt, .ssh/id_test and .netrc (secrets, hidden), .cache (writable)
//!   base/home/work/project   the project (writable), with `link` -> base/outside
//!   base/outside             a directory outside every writable path

use sandbox::launcher::{Launcher, Mechanism, Policy};
use sandbox::settings::{Mode, Settings};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Tree {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    outside: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::Builder::new()
        .prefix("sandbox-escape-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let project = home.join("work/project");
    let outside = base.join("outside");
    for path in [&project, &outside, &home.join(".ssh"), &home.join(".cache")] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(home.join("notes.txt"), "original\n").unwrap();
    std::fs::write(home.join(".ssh/id_test"), "SECRET-KEY-MATERIAL\n").unwrap();
    std::fs::write(home.join(".netrc"), "machine x login SECRET-NETRC\n").unwrap();
    std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
    Tree {
        _dir: dir,
        home,
        project,
        outside,
    }
}

fn policy(tree: &Tree, network: bool) -> Policy {
    let settings = Settings {
        mode: Mode::Required,
        writable: vec!["~/.cache".into()],
        readable: Vec::new(),
        hidden: vec!["~/.ssh".into(), "~/.netrc".into()],
        network,
        env: Vec::new(),
        image: std::env::var("ULTRON_SANDBOX_TEST_IMAGE").ok(),
    };
    let host_env: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), tree.home.to_string_lossy().into_owned()),
        ("SECRET_TOKEN".into(), "leaked-token".into()),
        ("AWS_SECRET_ACCESS_KEY".into(), "leaked-aws".into()),
    ];
    Policy::new(&settings, &tree.project, &tree.home, &[], None, &host_env)
}

/// The backends that work here; prints why the others are skipped.
fn backends() -> Vec<Mechanism> {
    let tree = tree();
    let mut out = Vec::new();
    for mechanism in Mechanism::STRONGEST_FIRST {
        let policy = policy(&tree, false);
        if mechanism == Mechanism::Docker && policy.image.is_none() {
            println!("skip docker: no ULTRON_SANDBOX_TEST_IMAGE (a local image; never pulled)");
            continue;
        }
        let launcher = Launcher::new(Mode::Required, policy);
        if launcher.probe(mechanism) {
            out.push(mechanism);
        } else {
            println!(
                "skip {}: not installed or not permitted on this machine",
                mechanism.name()
            );
        }
    }
    out
}

fn run(policy: &Policy, mechanism: Mechanism, script: &str) -> String {
    let launcher = Launcher::new(Mode::Required, policy.clone()).with_mechanism(Some(mechanism));
    let argv: Vec<String> = vec!["bash".into(), "-c".into(), script.into()];
    let output = launcher
        .command(&argv, &policy.cwd)
        .unwrap()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

#[test]
fn sandbox_escape_writes_outside_writable_paths_never_reach_the_host() {
    for mechanism in backends() {
        let tree = tree();
        let policy = policy(&tree, false);
        let script = format!(
            r#"
echo in > inproject.txt
echo cache > "$HOME/.cache/c.txt"
echo clobbered > "$HOME/notes.txt"
echo x > "$HOME/new.txt"
echo x > "{outside}/direct.txt"
echo x > link/viasymlink.txt
echo x > ../escape.txt
mkdir -p "$HOME/.config/autostart" && echo x > "$HOME/.config/autostart/evil.desktop"
cp /bin/sh "$HOME/sh-copy"
true
"#,
            outside = tree.outside.display()
        );
        let output = run(&policy, mechanism, &script);
        let name = mechanism.name();
        assert_eq!(
            std::fs::read_to_string(tree.project.join("inproject.txt")).unwrap(),
            "in\n",
            "{name}: {output}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.home.join(".cache/c.txt")).unwrap(),
            "cache\n",
            "{name}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.home.join("notes.txt")).unwrap(),
            "original\n",
            "{name}"
        );
        for escaped in [
            tree.home.join("new.txt"),
            tree.outside.join("direct.txt"),
            tree.outside.join("viasymlink.txt"),
            tree.home.join("work/escape.txt"),
            tree.home.join(".config"),
            tree.home.join("sh-copy"),
        ] {
            assert!(!escaped.exists(), "{name}: {} was written: {output}", escaped.display());
        }
    }
}

#[test]
fn sandbox_escape_hidden_credentials_and_variables_stay_out() {
    for mechanism in backends() {
        let tree = tree();
        let policy = policy(&tree, false);
        let output = run(
            &policy,
            mechanism,
            r#"cat "$HOME/.ssh/id_test" "$HOME/.netrc"; ls -A "$HOME/.ssh"; env; echo "token=${SECRET_TOKEN:-absent}""#,
        );
        let name = mechanism.name();
        assert!(!output.contains("SECRET-"), "{name}: {output}");
        assert!(!output.contains("leaked"), "{name}: {output}");
        assert!(output.contains("token=absent"), "{name}: {output}");
        assert!(
            output.contains(&format!("HOME={}", tree.home.display())),
            "{name}: {output}"
        );
    }
}

/// A nested user and mount namespace gets full capabilities over its own copy of the mounts, but the mounts it
/// inherited are locked: it can neither uncover a hidden path nor make the home writable.
#[test]
fn sandbox_escape_nested_namespaces_cannot_undo_the_mounts() {
    for mechanism in backends() {
        let tree = tree();
        let policy = policy(&tree, false);
        let script = r#"
unshare --user --map-root-user --mount sh -c '
  echo "nested-uid=$(id -u)"
  umount -l "$HOME/.ssh" 2>&1; umount -l "$HOME/.netrc" 2>&1
  cat "$HOME/.ssh/id_test" "$HOME/.netrc" 2>&1
  mount -o remount,rw "$HOME" 2>&1; mount -o remount,bind,rw "$HOME" 2>&1
  echo x > "$HOME/nested.txt" 2>&1
  mount --bind "$PWD" "$HOME" 2>&1
' 2>&1
echo done
"#;
        let output = run(&policy, mechanism, script);
        let name = mechanism.name();
        assert!(output.contains("done"), "{name}: {output}");
        assert!(
            output.contains("nested-uid=0"),
            "{name}: the nested namespace was not entered: {output}"
        );
        assert!(!output.contains("SECRET-"), "{name}: {output}");
        assert!(!tree.home.join("nested.txt").exists(), "{name}: {output}");
    }
}

#[test]
fn sandbox_escape_network_off_cannot_reach_the_host_and_on_can() {
    for mechanism in backends() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then echo reachable; else echo unreachable; fi"
        );
        let tree = tree();
        let name = mechanism.name();
        let off = run(&policy(&tree, false), mechanism, &script);
        assert_eq!(off.trim(), "unreachable", "{name}");
        if mechanism != Mechanism::Docker {
            // Docker's network is a bridge: the host's loopback is not the container's.
            let on = run(&policy(&tree, true), mechanism, &script);
            assert_eq!(on.trim(), "reachable", "{name}");
        }
    }
}

#[test]
fn sandbox_escape_host_processes_and_tmp_are_invisible() {
    for mechanism in backends() {
        let tree = tree();
        let host_tmp = std::env::temp_dir().join(format!("ultron-sandbox-escape-{}", std::process::id()));
        std::fs::write(&host_tmp, "host\n").unwrap();
        let script = format!(
            "kill -0 {pid} 2>/dev/null && echo pid-visible; [ -e {tmp} ] && echo tmp-visible; echo x > /tmp/ultron-sandbox-inside-{pid}; echo pid=$$",
            pid = std::process::id(),
            tmp = host_tmp.display()
        );
        let output = run(&policy(&tree, false), mechanism, &script);
        let _ = std::fs::remove_file(&host_tmp);
        let name = mechanism.name();
        assert!(!output.contains("pid-visible"), "{name}: {output}");
        assert!(!output.contains("tmp-visible"), "{name}: {output}");
        let inside = PathBuf::from(format!("/tmp/ultron-sandbox-inside-{}", std::process::id()));
        assert!(!inside.exists(), "{name}: a write to /tmp reached the host's /tmp");
    }
}

/// The sandbox dies with its parent: killing the process that started the launcher (not its group) stops the
/// sandboxed command.
#[test]
fn sandbox_escape_sandbox_dies_with_its_parent() {
    for mechanism in backends().into_iter().filter(|m| *m != Mechanism::Docker) {
        let tree = tree();
        let policy = policy(&tree, false);
        let launcher = Launcher::new(Mode::Required, policy.clone()).with_mechanism(Some(mechanism));
        let argv: Vec<String> = vec![
            "bash".into(),
            "-c".into(),
            "while :; do date +%s%N > tick; sleep 0.05; done".into(),
        ];
        let inner = launcher.command(&argv, &policy.cwd).unwrap();
        let mut parent = Command::new("sh");
        parent
            .arg("-c")
            .arg(r#""$@" & wait"#)
            .arg("sh")
            .arg(inner.get_program())
            .args(inner.get_args());
        parent
            .env_clear()
            .envs(inner.get_envs().filter_map(|(k, v)| Some((k, v?))))
            .current_dir(&policy.cwd);
        let mut child = parent
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let tick = tree.project.join("tick");
        let started = Instant::now();
        while !tick.exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tick.exists(), "{}: the sandboxed loop never started", mechanism.name());
        child.kill().unwrap();
        child.wait().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let before = std::fs::read_to_string(&tick).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(400));
        let after = std::fs::read_to_string(&tick).unwrap_or_default();
        assert_eq!(
            before,
            after,
            "{}: the sandboxed command outlived its parent",
            mechanism.name()
        );
    }
}

#[test]
fn sandbox_escape_doctor_self_check_holds_on_every_backend() {
    for mechanism in backends() {
        let tree = tree();
        for network in [false, true] {
            let report = sandbox::doctor::self_check(&policy(&tree, network), Some(mechanism), Some(&tree.outside));
            assert_eq!(report["ok"], true, "{} network={network}: {report}", mechanism.name());
            assert_eq!(report["home"], "read-only");
            assert_eq!(report["exposed"], serde_json::json!([]));
        }
    }
}

/// The doctor is not vacuous: without isolation the same probe fails, naming what leaked.
#[test]
fn sandbox_escape_doctor_catches_an_unisolated_run() {
    let tree = tree();
    let report = sandbox::doctor::self_check(&policy(&tree, false), None, Some(&tree.outside));
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["project"], "writable");
    assert_eq!(report["home"], "writable");
    assert_eq!(report["outside"], "writable");
    assert_eq!(report["token"], "present");
    assert_eq!(report["network"], "reachable");
    assert_eq!(report["hostTmp"], "visible");
    assert_eq!(report["exposed"].as_array().unwrap().len(), 2, "{report}");
}

#[test]
fn sandbox_escape_required_mode_refuses_without_a_sandbox_and_auto_runs_plain() {
    let tree = tree();
    let argv: Vec<String> = vec!["true".into()];
    let required = Launcher::new(Mode::Required, policy(&tree, false)).with_mechanism(None);
    let error = required.command(&argv, &tree.project).unwrap_err();
    assert!(
        error.starts_with("sandbox.mode is required but no sandbox works here"),
        "{error}"
    );
    let auto = Launcher::new(Mode::Auto, policy(&tree, false)).with_mechanism(None);
    assert_eq!(auto.command(&argv, &tree.project).unwrap().get_program(), "true");
    let off = Launcher::new(Mode::Off, policy(&tree, false)).with_mechanism(Some(Mechanism::Bwrap));
    assert_eq!(off.mechanism(), None);
    assert_eq!(off.command(&argv, &tree.project).unwrap().get_program(), "true");
}

/// A session dir inside a hidden directory (the agent dir under `~/.ultron`) stays writable, and readable paths there
/// readable, while the rest of the hidden directory stays hidden: the most specific rule wins.
#[test]
fn sandbox_escape_writable_dirs_inside_hidden_ones_stay_usable_and_siblings_hidden() {
    for mechanism in backends() {
        let tree = tree();
        let session = tree.home.join(".ssh/session");
        let skills = tree.home.join(".ssh/skills");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("skill.py"), "visible\n").unwrap();
        let settings = Settings {
            mode: Mode::Required,
            writable: Vec::new(),
            readable: vec!["~/.ssh/skills".into()],
            hidden: vec!["~/.ssh".into()],
            network: false,
            env: Vec::new(),
            image: std::env::var("ULTRON_SANDBOX_TEST_IMAGE").ok(),
        };
        let host_env = vec![("HOME".to_string(), tree.home.to_string_lossy().into_owned())];
        let policy = Policy::new(
            &settings,
            &tree.project,
            &tree.home,
            std::slice::from_ref(&session),
            None,
            &host_env,
        );
        let script = format!(
            r#"echo saved > "{session}/state.txt"
echo x > "{skills}/new.txt" 2>/dev/null
cat "{skills}/skill.py"
cat "$HOME/.ssh/id_test" 2>/dev/null
ls -A "$HOME/.ssh""#,
            session = session.display(),
            skills = skills.display()
        );
        let output = run(&policy, mechanism, &script);
        let name = mechanism.name();
        assert_eq!(
            std::fs::read_to_string(session.join("state.txt")).unwrap_or_default(),
            "saved\n",
            "{name}: {output}"
        );
        assert!(!skills.join("new.txt").exists(), "{name}: {output}");
        assert!(output.starts_with("visible\n"), "{name}: {output}");
        assert!(
            !output.contains("SECRET") && !output.contains("id_test"),
            "{name}: {output}"
        );
    }
}
