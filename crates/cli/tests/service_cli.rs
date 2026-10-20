//! `ultron service install|uninstall|status|restart|run` and the help texts through the binary (a Rust-only feature;
//! literal expectations). `systemctl` is a fake on the injected PATH that records its arguments (never the real one);
//! the unit goes under the injected XDG_CONFIG_HOME inside the test's temp root, never the real HOME. The durable
//! instance is the temp HOME's (`ULTRON_SERVER_DIR` unset).

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, Home, assert_none_mentioning};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

/// A bin directory holding only a fake `systemctl` that appends `$*` to `log` and answers `is-enabled` /
/// `is-active`; `fail` makes `enable` and `restart` fail as an unreachable user manager does.
fn fake_systemctl(root: &Path, log: &Path, fail: bool) -> PathBuf {
    let bin = root.join(if fail { "bin-failing" } else { "bin" });
    std::fs::create_dir_all(&bin).unwrap();
    let broken = if fail {
        "echo 'Failed to connect to bus: No medium found' >&2; exit 1"
    } else {
        ":"
    };
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$2\" in\n  enable|restart) {broken};;\n  is-enabled) echo enabled;;\n  \
         is-active) echo active;;\nesac\n",
        log.display()
    );
    let path = bin.join("systemctl");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn run(home: &Home, path: &Path, args: &[&str]) -> (i32, String, String) {
    let output = home
        .instance_command(&home.cwd(), args)
        .env("PATH", path)
        .env("XDG_CONFIG_HOME", home.root.join("xdg"))
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn units(home: &Home) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home.root.join("xdg/systemd/user"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn cli_service_installs_reports_restarts_and_removes_the_durable_unit_with_the_injected_systemctl() {
    let home = Home::new(1);
    let log = home.root.join("systemctl.log");
    let bin = fake_systemctl(&home.root, &log, false);
    let (code, out, err) = run(&home, &bin, &["service", "install"]);
    assert_eq!(code, 0, "{err}");
    let names = units(&home);
    assert_eq!(names.len(), 1, "{names:?}");
    let name = names[0].clone();
    assert!(
        name.starts_with("ultron-durable-") && name.ends_with(".service") && name.len() == 15 + 12 + 8,
        "{name}"
    );
    let path = home.root.join("xdg/systemd/user").join(&name);
    let unit = std::fs::read_to_string(&path).unwrap();
    assert!(out.starts_with(&format!("Wrote {}:\n{unit}", path.display())), "{out}");
    let exe = env!("CARGO_BIN_EXE_ultron");
    assert_eq!(
        unit,
        format!(
            "[Unit]\nDescription=Ultron durable server ({sd})\n\n[Service]\nType=simple\nWorkingDirectory={home}\n\
             Environment=\"ULTRON_CODING_AGENT_DIR={agent}\"\nEnvironmentFile=-{agent}/service.env\n\
             ExecStart=\"{exe}\" service run\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
            sd = home.root.join(".ultron/server").display(),
            home = home.root.display(),
            agent = home.agent().display(),
        )
    );
    assert!(!unit.contains("test-key"), "keys never go into the unit");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        format!("--user daemon-reload\n--user enable --now {name}\n")
    );
    assert!(!home.root.join(".config").exists());

    let (code, out, _) = run(&home, &bin, &["service", "status"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!(
            "Unit: {} (installed)\nEnabled: enabled\nActive: active\nServer: none reachable\nSchedules: 0 active\n",
            path.display()
        )
    );
    let (code, out, err) = run(&home, &bin, &["service", "restart"]);
    assert_eq!((code, out), (0, format!("Restarted {name}.\n")), "{err}");
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .ends_with(&format!("--user restart {name}\n"))
    );

    let (code, out, err) = run(&home, &bin, &["service", "uninstall"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, format!("Removed {}\n", path.display()));
    assert!(units(&home).is_empty());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.ends_with(&format!("--user disable --now {name}\n--user daemon-reload\n")),
        "{calls}"
    );
    let (code, out, _) = run(&home, &bin, &["service", "status"]);
    assert_eq!(code, 1);
    assert!(out.contains("(missing)"), "{out}");
    let (code, out, _) = run(&home, &bin, &["service", "uninstall"]);
    assert_eq!((code, out), (0, format!("Not installed ({name}).\n")));
    let (code, _, err) = run(&home, &bin, &["service", "restart"]);
    assert_eq!((code, err), (1, format!("Not installed ({name}).\n")));
}

#[test]
fn cli_service_without_systemd_or_a_user_manager_fails_and_writes_nothing() {
    let home = Home::new(1);
    let empty = home.root.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    for action in ["install", "status", "uninstall", "restart"] {
        let (code, out, err) = run(&home, &empty, &["service", action]);
        assert_eq!(code, 1, "{action}: {out}");
        assert_eq!(
            err.trim(),
            "No systemd found: systemctl is not on PATH. `ultron durable` starts the durable server on demand instead."
        );
    }
    assert!(!home.root.join("xdg").exists());
    let log = home.root.join("failing.log");
    let failing = fake_systemctl(&home.root, &log, true);
    let (code, _, err) = run(&home, &failing, &["service", "install"]);
    assert_eq!(code, 1);
    assert!(err.contains("Failed to connect to bus: No medium found"), "{err}");
    assert!(err.contains("The unit was not installed."), "{err}");
    assert!(units(&home).is_empty());
    let (code, _, err) = run(&home, &failing, &["service", "install", "now"]);
    assert_eq!(
        (code, err.trim()),
        (2, "Usage: ultron service install|uninstall|status|restart (see --help)")
    );
}

#[test]
fn cli_service_finds_systemctl_and_the_unit_dir_only_through_the_injected_env() {
    // Only the lookup runs here (nothing is executed): the injected PATH and XDG_CONFIG_HOME decide, never this
    // process's own environment.
    let home = Home::new(1);
    let bin = fake_systemctl(&home.root, &home.root.join("unused.log"), false);
    let empty = home.root.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let env = |vars: Vec<(&'static str, String)>| cli::Env {
        cwd: home.cwd(),
        home: home.root.join("injected-home"),
        var: std::sync::Arc::new(move |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())),
        docs: home.root.join("docs"),
    };
    let fake = env(vec![("PATH", format!("{}:{}", empty.display(), bin.display()))]);
    assert_eq!(cli::service_cli::systemctl(&fake), Some(bin.join("systemctl")));
    assert_eq!(
        cli::service_cli::unit_dir(&fake),
        home.root.join("injected-home/.config/systemd/user")
    );
    let none = env(vec![
        ("PATH", empty.display().to_string()),
        ("XDG_CONFIG_HOME", home.root.join("xdg").display().to_string()),
    ]);
    assert_eq!(cli::service_cli::systemctl(&none), None);
    assert_eq!(cli::service_cli::unit_dir(&none), home.root.join("xdg/systemd/user"));
    let relative = env(vec![("XDG_CONFIG_HOME", "rel".into())]);
    assert_eq!(cli::service_cli::systemctl(&relative), None);
    assert_eq!(
        cli::service_cli::unit_dir(&relative),
        home.root.join("injected-home/.config/systemd/user")
    );
}

#[test]
fn cli_service_run_serves_the_durable_instance_until_signalled_and_status_reports_it() {
    let home = Home::new(1);
    let log = home.root.join("systemctl.log");
    let bin = fake_systemctl(&home.root, &log, false);
    // What the unit's ExecStart runs, as this test's own child.
    let mut served = home
        .instance_command(&home.root, &["service", "run"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let durable = home.root.join(".ultron/server");
    let deadline = Instant::now() + HANG_GUARD;
    let server = loop {
        if let Some(server) = daemon::instance::generations(&durable).pop() {
            break server;
        }
        assert!(served.try_wait().unwrap().is_none(), "service run exited");
        assert!(Instant::now() < deadline, "the durable server did not start");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(server.pid, served.id());
    // `ultron durable start` finds it instead of starting another.
    let started = home
        .instance_command(&home.cwd(), &["durable", "start"])
        .output()
        .unwrap();
    let route = String::from_utf8(started.stdout).unwrap();
    assert!(route.starts_with(&format!("{}\t", server.server_id)), "{route}");
    assert_eq!(daemon::instance::generations(&durable).len(), 1);
    let (code, out, _) = run(&home, &bin, &["service", "status"]);
    assert_eq!(code, 1, "not installed");
    assert!(
        out.ends_with(&format!(
            "Server: {} pid {} sessions 0\nSchedules: 0 active\n",
            server.server_id, server.pid
        )),
        "{out}"
    );
    // SIGTERM (what `systemctl stop` sends) to this test's own child closes it.
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(served.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert_eq!(served.wait().unwrap().code(), Some(0));
    assert_none_mentioning(&[home.root.clone(), daemon::instance::socket_dir(&durable)], HANG_GUARD);
}

#[test]
fn cli_service_and_schedule_help_document_the_commands() {
    let home = Home::new(1);
    let empty = home.root.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    for flag in ["--help", "-h", "help"] {
        let (code, out, err) = run(&home, &empty, &["service", flag]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Usage: ultron service <command>\n"), "{out}");
        for expected in [
            "  install ",
            "$XDG_CONFIG_HOME/systemd/user",
            "  uninstall ",
            "  status ",
            "  restart ",
            "durable instance's server (`ultron durable`)",
            "<agent dir>/service.env",
        ] {
            assert!(out.contains(expected), "{expected:?} not in {out}");
        }
        let (code, out, err) = run(&home, &empty, &["schedule", flag]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Usage: ultron schedule <command>\n"), "{out}");
        for expected in ["  list ", "  wake ", "  cancel <id|name> ", "`ultron service install`"] {
            assert!(out.contains(expected), "{expected:?} not in {out}");
        }
        assert!(!out.contains("keepalive"), "{out}");
    }
    // `wake` with no schedule starts nothing and says nothing.
    let (code, out, err) = run(&home, &empty, &["schedule", "wake"]);
    assert_eq!((code, out.as_str()), (0, ""), "{err}");
    assert!(common::processes_mentioning(&home.root).is_empty());
}

#[test]
fn cli_service_release_install_restarts_the_service_only_when_its_unit_is_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // The unpacked release: install.sh beside a fake `ultron` that records its arguments.
    let release = root.join("release");
    std::fs::create_dir_all(&release).unwrap();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/rust-release/install.sh");
    std::fs::copy(script, release.join("install.sh")).unwrap();
    let log = root.join("ultron.log");
    let fake = release.join("ultron");
    std::fs::write(&fake, format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let install = || {
        let output = std::process::Command::new("/bin/sh")
            .arg(release.join("install.sh"))
            .args(["--prefix", root.join("prefix").to_str().unwrap()])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("xdg"))
            .env("ULTRON_PYTHON", "/bin/sh")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let out = install();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "--version\n");
    assert!(!out.contains("Restarted"), "{out}");
    // `ultron-s` is a relative link to the installed binary, replaced on every install.
    let link = root.join("prefix/bin/ultron-s");
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("ultron"));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("elsewhere", &link).unwrap();
    let units = root.join("xdg/systemd/user");
    std::fs::create_dir_all(&units).unwrap();
    std::fs::write(units.join("ultron-durable-0123456789ab.service"), "[Unit]\n").unwrap();
    let out = install();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "--version\n--version\nservice restart\n"
    );
    assert!(out.contains("Restarted the ultron service."), "{out}");
    assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("ultron"));
}
