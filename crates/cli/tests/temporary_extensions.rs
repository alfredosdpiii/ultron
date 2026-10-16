//! `-e npm:` / `-e git:` on the command line: the CLI installs the package for this run under the agent's temp
//! folder (as `resolveExtensionSources(sources, { temporary: true })` does, see package_checks.rs for the recorded
//! TypeScript behavior) and hands the session the install directory, where the extension discovery reads its
//! manifests. A fake npm on the injected PATH (no environment of its own) installs a package holding an
//! `extension.json`; nothing reaches a registry.

use cli::print_mode::session_flags;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

const TEMP_NPM: &str = "agent/tmp/extensions/npm/f35b2129";

fn fake_npm(root: &Path, log: &Path) -> String {
    format!(
        "#!/bin/sh\necho \"$*\" >> '{log}'\ncase \"$1\" in\n  install)\n    prefix=$4\n    mkdir -p \"$prefix/node_modules/foo\"\n    echo '{{\"name\":\"foo\",\"version\":\"1.0.0\"}}' > \"$prefix/node_modules/foo/package.json\"\n    echo '{{\"name\":\"foo-ext\",\"command\":\"true\"}}' > \"$prefix/node_modules/foo/extension.json\"\n    ;;\n  *) exit 3 ;;\nesac\n",
        log = log.display()
    )
    .replace("{root}", &root.display().to_string())
}

fn env(root: &Path, extra: &[(&str, &str)]) -> cli::Env {
    let mut vars: HashMap<String, String> = [
        (
            "PATH".to_string(),
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        ),
        (
            "ULTRON_CODING_AGENT_DIR".to_string(),
            root.join("agent").display().to_string(),
        ),
    ]
    .into();
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    cli::Env {
        cwd: root.join("work"),
        home: root.join("home"),
        var: Arc::new(move |k| vars.get(k).cloned()),
        docs: root.join("docs"),
    }
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    for d in ["work", "agent", "home", "bin"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let npm = root.join("bin/npm");
    std::fs::write(&npm, fake_npm(&root, &root.join("npm.log"))).unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    (temp, root)
}

#[test]
fn cli_temporary_extension_source_installs_once_and_replaces_the_source_with_its_directory() {
    let (_temp, root) = setup();
    let parsed = cli::args::parse_args(&["-e".into(), "./local-ext".into(), "-e".into(), "npm:foo".into()]);
    let flags = session_flags(&parsed, &env(&root, &[])).unwrap();
    let install = root.join(TEMP_NPM).join("node_modules/foo");
    assert_eq!(
        flags.extension_paths,
        vec![
            root.join("work/local-ext").display().to_string(),
            install.display().to_string()
        ]
    );
    assert!(install.join("extension.json").exists());
    let log = std::fs::read_to_string(root.join("npm.log")).unwrap();
    assert_eq!(
        log,
        format!(
            "install foo --prefix {} --legacy-peer-deps\n",
            root.join(TEMP_NPM).display()
        )
    );
    // The session's discovery finds the package's manifest there.
    let discovery =
        extensions::manifest::discover_cli_paths(&flags.extension_paths[1..], &root.join("work"), &root.join("home"));
    assert_eq!(discovery.manifests.len(), 1, "{:?}", discovery.diagnostics);
    // A second start finds it installed and runs nothing.
    let again = session_flags(&parsed, &env(&root, &[])).unwrap();
    assert_eq!(again.extension_paths, flags.extension_paths);
    assert_eq!(std::fs::read_to_string(root.join("npm.log")).unwrap(), log);
}

#[test]
fn cli_temporary_extension_source_is_left_out_offline_and_a_failed_install_fails_the_start() {
    let (_temp, root) = setup();
    let parsed = cli::args::parse_args(&["-e".into(), "npm:foo".into()]);
    let flags = session_flags(&parsed, &env(&root, &[("PI_OFFLINE", "1")])).unwrap();
    assert!(flags.extension_paths.is_empty());
    assert!(!root.join("npm.log").exists(), "offline runs no npm");
    let parsed = cli::args::parse_args(&["-e".into(), "npm:other".into()]);
    std::fs::write(root.join("bin/npm"), "#!/bin/sh\nexit 1\n").unwrap();
    let error = session_flags(&parsed, &env(&root, &[])).unwrap_err();
    assert_eq!(
        error,
        format!(
            "npm install other --prefix {} --legacy-peer-deps failed with code 1",
            root.join(TEMP_NPM).display()
        )
    );
}
