//! Process tests that try real escapes under every backend available on this machine. A backend that is not
//! installed or does not work here is skipped with a `SKIPPED` line and the reason (`backends_support`: Docker runs
//! on a local image, ULTRON_SANDBOX_TEST_IMAGE or a known base image already present; images are never pulled;
//! ULTRON_SANDBOX_TEST_REQUIRE turns a skip into a failure).
//!
//! The scratch tree lives under the cargo target dir (inside /home on a usual checkout), not /tmp, so the read-only
//! home and /home rules are exercised rather than hidden by a private /tmp:
//!   base/fakehome                fake HOME: notes.txt, .ssh/id_test and .netrc (secrets, hidden), .cache (writable)
//!   base/fakehome/work/project   the project (writable), with `link` -> base/outside
//!   base/outside                 a directory outside every writable path

mod backends_support;

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
    let home = base.join("fakehome");
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
        image: backends_support::docker_image().ok(),
    };
    let host_env: Vec<(String, String)> = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), tree.home.to_string_lossy().into_owned()),
        ("SECRET_TOKEN".into(), "leaked-token".into()),
        ("AWS_SECRET_ACCESS_KEY".into(), "leaked-aws".into()),
    ];
    Policy::new(&settings, &tree.project, &tree.home, &[], None, &host_env)
}

/// The backends this machine runs (probed with the bare tools, so a launcher bug fails instead of skipping).
fn backends() -> Vec<Mechanism> {
    backends_support::machine_backends()
        .into_iter()
        .filter_map(Mechanism::parse)
        .collect()
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
/// inherited are locked: it can neither uncover a hidden path nor make the home writable. Docker's seccomp profile
/// refuses the nested namespace outright, which leaves the mounts as they are too.
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
        if mechanism == Mechanism::Docker {
            assert!(
                output.contains("nested-uid=0") || output.contains("unshare failed: Operation not permitted"),
                "{name}: neither entered nor refused: {output}"
            );
        } else {
            assert!(
                output.contains("nested-uid=0"),
                "{name}: the nested namespace was not entered: {output}"
            );
        }
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
/// sandboxed command. For Docker that means the container, which is no process of ours, stops too.
#[test]
fn sandbox_escape_sandbox_dies_with_its_parent() {
    for mechanism in backends() {
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
        while !tick.exists() && started.elapsed() < Duration::from_secs(120) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tick.exists(), "{}: the sandboxed loop never started", mechanism.name());
        child.kill().unwrap();
        child.wait().unwrap();
        // The loop ticks every 50 ms while it lives: wait until the tick holds still for 400 ms. Teardown can take
        // a while under load; a loop that outlived its parent never holds still (hang guard 120 s).
        let killed = Instant::now();
        loop {
            let before = std::fs::read_to_string(&tick).unwrap_or_default();
            std::thread::sleep(Duration::from_millis(400));
            let after = std::fs::read_to_string(&tick).unwrap_or_default();
            if before == after {
                break;
            }
            assert!(
                killed.elapsed() < Duration::from_secs(120),
                "{}: the sandboxed command outlived its parent",
                mechanism.name()
            );
        }
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
            image: backends_support::docker_image().ok(),
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

/// Under Docker the command's environment is the policy's allowlist only, not that plus the image's own ENV (the
/// python images define GPG_KEY, PYTHON_VERSION, PYTHON_SHA256). Needs a local image that defines extra variables.
#[test]
fn sandbox_escape_docker_clears_the_images_environment() {
    let image =
        match backends_support::local_image(&["python:3.12-slim", "python:3.13-slim", "python:3.12", "python:3.13"]) {
            Ok(image) => image,
            Err(reason) => return backends_support::skip("docker", &reason),
        };
    let tree = tree();
    let mut policy = policy(&tree, false);
    policy.image = Some(image.clone());
    if let Err(reason) = backends_support::machine_runs("docker", Some(&image)) {
        return backends_support::skip("docker", &reason);
    }
    let output = run(&policy, Mechanism::Docker, "env | sed 's/=.*//' | sort");
    let mut names: Vec<&str> = output
        .lines()
        .filter(|name| !["PWD", "SHLVL", "_"].contains(name))
        .collect();
    names.sort_unstable();
    let mut want: Vec<&str> = policy.env.iter().map(|(name, _)| name.as_str()).collect();
    want.sort_unstable();
    assert_eq!(names, want, "{image}: {output}");
}

/// The real layout, with `which` answered from one directory: a machine whose PATH has no `setpriv`.
struct PathLayout(PathBuf);
impl sandbox::launcher::Layout for PathLayout {
    fn entry(&self, path: &std::path::Path) -> sandbox::launcher::Entry {
        sandbox::launcher::RealLayout.entry(path)
    }
    fn which(&self, name: &str) -> bool {
        self.0.join(name).exists()
    }
}

/// Wait until `tick` (rewritten every 50 ms while its loop lives) holds still for 400 ms (hang guard 120 s).
fn wait_until_still(what: &str, tick: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let before = std::fs::read_to_string(tick).unwrap_or_default();
        std::thread::sleep(Duration::from_millis(400));
        if before == std::fs::read_to_string(tick).unwrap_or_default() {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: the command outlived its stop");
    }
}

/// Docker on a machine without `setpriv`: the client's stdin holder polls the client instead of dying with it. With
/// a PATH holding only bash, docker and sleep, the command runs and keeps its exit status, a killed client's
/// container stops, and the holder ends after its client.
#[test]
fn sandbox_escape_docker_without_setpriv_ties_the_container_to_its_client() {
    let image = match backends_support::docker_image() {
        Ok(image) => image,
        Err(reason) => return backends_support::skip("docker", &reason),
    };
    if let Err(reason) = backends_support::machine_runs("docker", Some(&image)) {
        return backends_support::skip("docker", &reason);
    }
    let tree = tree();
    let bin = tree.outside.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["bash", "docker", "sleep"] {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(tool))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("{tool} on PATH"));
        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
    }
    let layout = PathLayout(bin.clone());
    let mut policy = policy(&tree, false);
    policy.image = Some(image);
    let run = |script: &str| {
        let argv: Vec<String> = vec!["bash".into(), "-c".into(), script.into()];
        let wrapped = policy.wrap(Mechanism::Docker, &layout, &argv, &policy.cwd);
        assert!(!wrapped.iter().any(|part| part == "setpriv"), "{wrapped:?}");
        let mut command = Command::new(&wrapped[0]);
        command
            .args(&wrapped[1..])
            .env_clear()
            .env("PATH", &bin)
            .current_dir(&policy.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.spawn().unwrap()
    };
    // Runs to its end with its status; afterwards nothing of its process group is left: the stdin holder
    // (`while kill -0 $$ ...`) saw its client gone and ended.
    let mut client = run("sleep 1; echo done > done.txt; exit 6");
    let group = client.id();
    assert_eq!(client.wait().unwrap().code(), Some(6));
    assert_eq!(
        std::fs::read_to_string(tree.project.join("done.txt")).unwrap(),
        "done\n"
    );
    let in_group = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                // /proc/<pid>/stat: `pid (comm) state ppid pgrp ...`, skipping zombies.
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                let fields: Vec<&str> = stat
                    .rsplit_once(')')
                    .map_or("", |(_, rest)| rest)
                    .split_whitespace()
                    .collect();
                fields.first() != Some(&"Z") && fields.get(2) == Some(&group.to_string().as_str())
            })
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while in_group() > 0 {
        assert!(Instant::now() < deadline, "the stdin holder outlived its client");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A killed client's container stops.
    let mut client = run("while :; do date +%s%N > tick; sleep 0.05; done");
    let tick = tree.project.join("tick");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !tick.exists() {
        assert!(Instant::now() < deadline, "the loop never ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = rustix::process::Pid::from_raw(client.id() as i32).unwrap();
    rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).unwrap();
    client.wait().unwrap();
    wait_until_still("docker without setpriv", &tick);
}
