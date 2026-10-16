//! The startup package update check in the running TUI (interactive-mode.ts `checkForPackageUpdates` /
//! `showPackageUpdateNotification`; the check is replayed against TypeScript in package_checks.rs and the notice's
//! rendering in crates/interactive/tests/package_updates.rs): a configured npm package installed at 1.0.0, a fake
//! npm on the run's PATH answering `view` with 1.1.0, and the notice appears after the session starts; offline it
//! does not. The fake npm logs its arguments; no registry is reached.

mod tui_harness;

use tui_harness::{Mock, TempDir, Tui};

fn start(tmp: &TempDir, offline: bool) -> (Tui, Mock, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp.0.clone();
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = root.join("npm.log");
    std::fs::write(
        bin.join("npm"),
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = view ]; then echo '\"1.1.0\"'; exit 0; fi\nexit 3\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("npm"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let package = root.join("agent/npm/node_modules/foo");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), r#"{"name":"foo","version":"1.0.0"}"#).unwrap();
    std::fs::write(root.join("agent/settings.json"), r#"{"packages":["npm:foo"]}"#).unwrap();
    let mock = Mock::start(Vec::new(), None);
    let path = format!("{}:{}:/usr/bin:/bin", bin.display(), root.join("no-browser").display());
    let mut env = vec![
        ("ULTRON_SPLASH", "off"),
        ("ULTRON_AUTO_NAME", "off"),
        ("PATH", path.as_str()),
    ];
    if offline {
        // Any value counts here (`process.env.PI_OFFLINE`), not only the package manager's 1/true/yes.
        env.push(("PI_OFFLINE", "on"));
    }
    let tui = Tui::start_at(
        &root,
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &env,
    );
    (tui, mock, log)
}

#[test]
fn cli_interactive_startup_shows_package_updates() {
    let tmp = TempDir::new();
    let (tui, _mock, log) = start(&tmp, false);
    let screen = tui.wait("- foo");
    assert!(screen.contains("Package Updates Available"), "{screen}");
    assert!(
        screen.contains("Package updates are available. Run ultron update --extensions"),
        "{screen}"
    );
    assert!(screen.contains("Packages:"), "{screen}");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "view foo version --json\n");
}

#[test]
fn cli_interactive_startup_offline_checks_no_packages() {
    let tmp = TempDir::new();
    let (tui, _mock, log) = start(&tmp, true);
    let screen = tui.wait("Session:");
    assert!(!screen.contains("Package Updates Available"), "{screen}");
    assert!(!log.exists(), "offline runs no npm");
}
