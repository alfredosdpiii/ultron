//! Release packaging: the `ultron` binary carries the package files it reads at run time (`cli::assets`) and runs
//! from a copy outside the checkout with the checkout hidden (a `bwrap` tmpfs over the workspace), with private HOME,
//! agent and server directories: `--version`, `--help`, a print-mode run whose rlm cell runs in the Python kernel
//! started from the installed runtime, and `--export` with both built-in themes. Installation is atomic, shared by
//! concurrent first runs, never rewrites a verified directory and replaces a damaged one. Masked runs have no Node.js:
//! `PATH` holds only a few system tools and the system `node`, when there is one, is hidden too.

mod common;
mod daemon_common;

use cli::assets::{self, FILES, HASH, MARKER, RUNTIME};
use common::{
    Naming, anthropic_text, anthropic_tool, assert_private_home, mock_server, session_files, tool_result_text,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(180);

/// The workspace root: everything the binary must not need.
fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// `bwrap` on the test's PATH (masked runs get a PATH without it).
fn bwrap() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("bwrap"))
        .find(|path| path.is_file())
        .expect("bwrap is required to hide the checkout")
}

/// Wait for `command` with a hang guard (the process is killed and the test fails past it).
fn run(mut command: Command) -> Output {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(HANG_GUARD) {
        Ok(output) => output.unwrap(),
        Err(_) => {
            if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
            panic!("{command:?} did not finish within {HANG_GUARD:?}");
        }
    }
}

/// A copy of the built binary in a temp dir outside the checkout, with private HOME, agent, server and cwd dirs.
struct Outside {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    agent: PathBuf,
    cwd: PathBuf,
}

impl Outside {
    fn new() -> Outside {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert!(
            !root.starts_with(checkout()),
            "temp dir {} is inside the checkout",
            root.display()
        );
        assert_private_home(&root);
        let bin = root.join("bin").join("ultron");
        for dir in ["bin", "agent", "cwd", "home", "tools"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for tool in [
            "sh", "bash", "env", "python3", "git", "cat", "ls", "uname", "cp", "chmod", "mv", "mkdir", "dirname",
            "sed", "flock", "ln",
        ] {
            let path = Path::new("/usr/bin").join(tool);
            if path.exists() {
                std::os::unix::fs::symlink(&path, root.join("tools").join(tool)).unwrap();
            }
        }
        std::fs::copy(env!("CARGO_BIN_EXE_ultron"), &bin).unwrap();
        let (agent, cwd) = (root.join("agent"), root.join("cwd"));
        Outside {
            _tmp: tmp,
            root,
            bin,
            agent,
            cwd,
        }
    }

    /// The copied binary under `bwrap` with an empty tmpfs over the checkout, and a clean environment.
    fn masked(&self) -> Command {
        let mut command = Command::new(bwrap());
        command.args(["--dev-bind", "/", "/", "--tmpfs"]).arg(checkout());
        for node in ["/usr/bin/node", "/usr/local/bin/node"] {
            if Path::new(node).exists() {
                command.args(["--ro-bind", "/dev/null", node]);
            }
        }
        command.arg("--chdir").arg(&self.cwd).arg("--").arg(&self.bin);
        self.private(&mut command);
        command
    }

    fn private(&self, command: &mut Command) {
        command
            .env_clear()
            .current_dir(&self.cwd)
            .env("PATH", self.root.join("tools"))
            .env("HOME", self.root.join("home"))
            .env("ULTRON_CODING_AGENT_DIR", &self.agent)
            .env("ULTRON_SERVER_DIR", self.root.join("server"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("NO_COLOR", "1")
            .env("TZ", "UTC");
        if let Some(python) = std::env::var_os("ULTRON_PYTHON") {
            command.env("ULTRON_PYTHON", python);
        }
    }

    fn runtime_base(&self) -> PathBuf {
        self.agent.join("runtime")
    }

    fn installed(&self) -> PathBuf {
        assets::install_dir(&self.runtime_base())
    }
}

/// The mask hides the checkout from the process (so any read of it fails).
fn assert_mask_hides_checkout() {
    let mut probe = Command::new(bwrap());
    probe
        .args(["--dev-bind", "/", "/", "--tmpfs"])
        .arg(checkout())
        .args(["--", "/usr/bin/test", "-e"])
        .arg(checkout().join("Cargo.toml"));
    let status = run(probe).status;
    assert_eq!(
        status.code(),
        Some(1),
        "bwrap could not hide the checkout ({status}); bwrap is required"
    );
}

/// Every file a directory holds, relative to it.
fn tree(dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.insert(path.strip_prefix(dir).unwrap().to_string_lossy().to_string());
            }
        }
    }
    found
}

fn embedded() -> BTreeSet<String> {
    FILES.iter().map(|a| a.path.to_string()).collect()
}

#[test]
fn cli_package_copied_binary_runs_with_the_checkout_hidden() {
    assert_mask_hides_checkout();
    let outside = Outside::new();
    let checkout_text = checkout().display().to_string();

    // --version and --help: as the binary in the checkout prints them.
    let version = run({
        let mut c = outside.masked();
        c.arg("--version");
        c
    });
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("{}\n", cli::version())
    );
    assert_eq!(version.status.code(), Some(0));
    let help = run({
        let mut c = outside.masked();
        c.arg("--help");
        c
    });
    let reference = run({
        let mut c = Command::new(env!("CARGO_BIN_EXE_ultron"));
        outside.private(&mut c);
        c.env("ULTRON_CODING_AGENT_DIR", outside.root.join("reference-agent"))
            .arg("--help");
        c
    });
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).starts_with("ultron - AI coding agent"));
    assert_eq!(help.stdout, reference.stdout);

    // Neither installed anything: they read no package files.
    assert!(
        !outside.runtime_base().exists(),
        "--version/--help installed the runtime files"
    );
    let installed = outside.installed();

    // A print-mode run: the rlm cell runs in the kernel started from the installed runtime.py, whose sibling
    // modules and secret-patterns.json load from beside it.
    let code = "import sys, secret_patterns\nprint(sys.modules['__main__'].__file__)\nprint(secret_patterns.PATTERNS_PATH, secret_patterns.PATTERNS_PATH.is_file())";
    let (port, seen) = mock_server(
        vec![anthropic_tool("toolu_pkg", code), anthropic_text("packaged run done")],
        Naming::Title("Packaged".into()),
    );
    std::fs::write(
        outside.agent.join("models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
    let printed = run({
        let mut c = outside.masked();
        c.args(["-p", "--model", "anthropic/claude-sonnet-4-5", "run a cell"]);
        c
    });
    assert_eq!(String::from_utf8_lossy(&printed.stderr), "", "stderr of the print run");
    assert_eq!(String::from_utf8(printed.stdout).unwrap(), "packaged run done\n");
    assert_eq!(printed.status.code(), Some(0));
    // The run installed the embedded files: verified, exactly the embedded set plus the marker and the lock.
    assert!(assets::verified(&installed), "{} is not verified", installed.display());
    let mut want = embedded();
    want.extend([MARKER.to_string(), assets::LOCK.to_string()]);
    assert_eq!(tree(&installed), want);
    assert_eq!(
        std::fs::read_dir(outside.runtime_base()).unwrap().count(),
        1,
        "only the installed dir"
    );
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2);
    let result = tool_result_text(&requests[1]["body"], "toolu_pkg").expect("the cell's result went back");
    let runtime = installed.join(RUNTIME);
    let patterns = installed.join("src/ultron/rlm/secret-patterns.json");
    assert!(
        result.contains(&format!("{}\n{} True", runtime.display(), patterns.display())),
        "cell result: {result}"
    );
    // The system prompt points the model at the installed bundled skills, and nothing sent names the checkout.
    let body = requests[0]["body"].to_string();
    assert!(
        body.contains(&installed.join("skills").display().to_string()),
        "system prompt: {body}"
    );
    for request in &requests {
        assert!(
            !request.to_string().contains(&checkout_text),
            "a request names the checkout"
        );
    }

    // --export with each built-in theme, as the library renders the same session.
    let session = session_files(&outside.agent.join("experimental").join("sessions"))
        .pop()
        .expect("the run saved a session");
    let mut pages = Vec::new();
    for (name, colorfgbg) in [("dark", "15;0"), ("light", "0;15")] {
        let file = format!("{name}.html");
        let exported = run({
            let mut c = outside.masked();
            c.env("COLORFGBG", colorfgbg).arg("--export").arg(&session).arg(&file);
            c
        });
        assert_eq!(
            exported.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let page = std::fs::read_to_string(outside.cwd.join(&file)).unwrap();
        let library_cwd = outside.root.join(format!("library-{name}"));
        std::fs::create_dir_all(&library_cwd).unwrap();
        let fgbg = colorfgbg.to_string();
        let env = cli::Env {
            cwd: library_cwd.clone(),
            home: outside.root.join("home"),
            var: Arc::new(move |k| (k == "COLORFGBG").then(|| fgbg.clone())),
            docs: installed.join("docs"),
        };
        cli::export_html::export(&session.display().to_string(), Some(&file), &env).unwrap();
        assert_eq!(
            page,
            std::fs::read_to_string(library_cwd.join(&file)).unwrap(),
            "{name} page"
        );
        assert!(page.contains("marked v18.0.5") && page.contains("Highlight.js v11.9.0"));
        // Every template slot filled: theme variables, session data, scripts.
        let slots = ["{{SESSION_DATA}}", "{{THEME_VARS}}", "{{BODY_BG}}"];
        assert!(page.contains("--exportPageBg: #"), "{name} page has no theme variables");
        assert!(
            !slots.iter().any(|slot| page.contains(slot)),
            "{name} page has an unfilled slot"
        );
        pages.push(page);
    }
    assert_ne!(pages[0], pages[1], "the light theme's page differs from the dark one");

    daemon_common::assert_no_processes(&outside.root, HANG_GUARD);
}

#[test]
fn cli_package_concurrent_first_runs_install_one_directory() {
    assert_mask_hides_checkout();
    let outside = Outside::new();
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut c = outside.masked();
            c.arg("--list-models")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let pid = child.id();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let output = rx
            .recv_timeout(HANG_GUARD)
            .unwrap_or_else(|_| panic!("pid {pid} hung"))
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // One directory, no staging or stale leftovers.
    let entries: Vec<String> = std::fs::read_dir(outside.runtime_base())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries,
        [outside.installed().file_name().unwrap().to_string_lossy().to_string()]
    );
    assert!(assets::verified(&outside.installed()));
}

#[test]
fn cli_package_threads_racing_on_install_agree_on_one_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("runtime");
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let (barrier, base) = (barrier.clone(), base.clone());
            std::thread::spawn(move || {
                barrier.wait();
                assets::install(&base)
            })
        })
        .collect();
    let dirs: BTreeSet<PathBuf> = handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
    assert_eq!(dirs.into_iter().collect::<Vec<_>>(), [assets::install_dir(&base)]);
    assert_eq!(
        std::fs::read_dir(&base).unwrap().count(),
        1,
        "no staging or stale leftovers"
    );
    assert!(assets::verified(&assets::install_dir(&base)));
}

#[test]
fn cli_package_install_keeps_a_verified_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("runtime");
    let dir = assets::install(&base).unwrap();
    assert_eq!(
        dir.file_name().unwrap().to_string_lossy(),
        format!("{}-{}", cli::version(), &HASH[..16])
    );
    // A file the user added and the files' identities survive a second install: nothing is rewritten.
    std::fs::write(dir.join("user-note.txt"), "kept").unwrap();
    let inode = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    };
    let before = (inode(&dir), inode(&dir.join(RUNTIME)), inode(&dir.join(MARKER)));
    assert_eq!(assets::install(&base).unwrap(), dir);
    assert_eq!(
        (inode(&dir), inode(&dir.join(RUNTIME)), inode(&dir.join(MARKER))),
        before
    );
    assert_eq!(std::fs::read_to_string(dir.join("user-note.txt")).unwrap(), "kept");
}

#[test]
fn cli_package_install_replaces_a_damaged_directory() {
    let runtime = FILES.iter().find(|a| a.path == RUNTIME).unwrap();
    type Damage = fn(&Path);
    let damages: [(&str, Damage); 4] = [
        ("truncated runtime.py", |dir| {
            std::fs::write(dir.join(RUNTIME), "").unwrap()
        }),
        ("deleted skill file", |dir| {
            let skill = FILES.iter().find(|a| a.path.starts_with("skills/")).unwrap();
            std::fs::remove_file(dir.join(skill.path)).unwrap();
        }),
        ("marker of another build", |dir| {
            std::fs::write(dir.join(MARKER), "0000\n").unwrap()
        }),
        ("no marker (an interrupted copy)", |dir| {
            std::fs::remove_file(dir.join(MARKER)).unwrap()
        }),
    ];
    for (name, damage) in damages {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("runtime");
        let dir = assets::install(&base).unwrap();
        damage(&dir);
        assert!(!assets::verified(&dir), "{name}: still verified");
        assert_eq!(assets::install(&base).unwrap(), dir, "{name}");
        assert!(assets::verified(&dir), "{name}: not repaired");
        assert_eq!(std::fs::read(dir.join(RUNTIME)).unwrap(), runtime.bytes, "{name}");
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 1, "{name}: leftovers");
    }
}

#[test]
fn cli_package_embeds_the_runtime_skills_docs_and_changelog_byte_for_byte() {
    let package = checkout().join("packages/coding-agent");
    for asset in FILES {
        assert_eq!(
            std::fs::read(package.join(asset.path)).unwrap(),
            asset.bytes,
            "{} differs from the checkout",
            asset.path
        );
    }
    let names = embedded();
    // Every runtime module (runtime.py imports its siblings by name) and the patterns file beside them.
    for entry in std::fs::read_dir(package.join("src/ultron/rlm")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = format!("src/ultron/rlm/{name}");
        let wanted = name.ends_with(".py") || name == "secret-patterns.json";
        assert_eq!(names.contains(&path), wanted, "{path}");
    }
    // The bundled skills and docs trees in full (no caches or hidden files), and the changelog.
    for tree_name in ["skills", "docs"] {
        let on_disk: BTreeSet<String> = tree(&package.join(tree_name))
            .into_iter()
            .filter(|p| !p.split('/').any(|part| part.starts_with('.') || part == "__pycache__"))
            .map(|p| format!("{tree_name}/{p}"))
            .collect();
        let in_binary: BTreeSet<String> = names
            .iter()
            .filter(|p| p.starts_with(&format!("{tree_name}/")))
            .cloned()
            .collect();
        assert_eq!(in_binary, on_disk, "{tree_name}");
    }
    assert!(names.contains("CHANGELOG.md"));
    // Executable scripts stay executable once installed.
    let script = FILES
        .iter()
        .find(|a| a.path == "skills/pstack/show-me-your-work/scripts/log.sh")
        .unwrap();
    assert_eq!(script.mode, 0o755);
    assert_eq!(FILES.iter().find(|a| a.path == RUNTIME).unwrap().mode, 0o644);
}

#[test]
fn cli_package_dev_assets_chooses_where_the_files_come_from() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let agent = tmp.path().join("agent");
    let lookup = |dev: Option<String>| {
        let agent = agent.display().to_string();
        move |key: &str| match key {
            "ULTRON_CODING_AGENT_DIR" => Some(agent.clone()),
            "ULTRON_DEV_ASSETS" => dev.clone(),
            _ => None,
        }
    };
    let live = PathBuf::from(assets::CHECKOUT.expect("tests run a debug build"));
    // Unset or empty in a debug build: the live checkout. Nothing is installed.
    assert_eq!(assets::package_dir(&lookup(None), &home).unwrap(), live);
    assert_eq!(assets::package_dir(&lookup(Some(String::new())), &home).unwrap(), live);
    assert!(!agent.join("runtime").exists());
    // 1: the checkout too.
    assert_eq!(assets::package_dir(&lookup(Some("1".into())), &home).unwrap(), live);
    // 0: the embedded files, installed under the agent dir.
    let installed = assets::package_dir(&lookup(Some("0".into())), &home).unwrap();
    assert_eq!(installed, assets::install_dir(&agent.join("runtime")));
    assert!(assets::verified(&installed));
    // A package directory with a runtime.py, tilde-expanded; one without is an error naming it.
    let other = home.join("pkg");
    std::fs::create_dir_all(other.join("src/ultron/rlm")).unwrap();
    std::fs::write(other.join(RUNTIME), "").unwrap();
    assert_eq!(
        assets::package_dir(&lookup(Some("~/pkg".into())), &home).unwrap(),
        other
    );
    let missing = tmp.path().join("missing");
    assert_eq!(
        assets::package_dir(&lookup(Some(missing.display().to_string())), &home).unwrap_err(),
        format!("ULTRON_DEV_ASSETS={}: no {RUNTIME} there", missing.display())
    );
}

#[test]
fn cli_package_install_script_installs_into_a_prefix_without_node() {
    let outside = Outside::new();
    let release = checkout().join("scripts/rust-release");
    // An unpacked release as scripts/rust-release.sh stages it.
    let stage = outside.root.join("ultron-stage");
    std::fs::create_dir_all(stage.join("licenses")).unwrap();
    std::fs::copy(&outside.bin, stage.join("ultron")).unwrap();
    for file in ["install.sh", "README.md", "NOTICE"] {
        std::fs::copy(release.join(file), stage.join(file)).unwrap();
    }
    std::fs::copy(checkout().join("LICENSE"), stage.join("LICENSE")).unwrap();
    std::fs::write(stage.join("licenses/loki-engine.LICENSE"), "MIT").unwrap();
    let install = |args: &[&str], prefix_env: Option<&Path>| {
        let mut c = Command::new("/bin/sh");
        outside.private(&mut c);
        c.env("ULTRON_DEV_ASSETS", "0").arg(stage.join("install.sh")).args(args);
        if let Some(prefix) = prefix_env {
            c.env("PREFIX", prefix);
        }
        run(c)
    };
    let prefix = outside.root.join("prefix");
    let first = install(&["--prefix", &prefix.display().to_string()], None);
    let stdout = String::from_utf8(first.stdout).unwrap();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let bin = prefix.join("bin/ultron");
    assert_eq!(
        stdout,
        format!(
            "Installed {} to {}\nAdd {} to your PATH to run ultron.\n",
            cli::version(),
            bin.display(),
            prefix.join("bin").display()
        )
    );
    assert_eq!(std::fs::read(&bin).unwrap(), std::fs::read(&outside.bin).unwrap());
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o777, 0o755);
    }
    for file in ["LICENSE", "NOTICE", "README.md", "licenses/loki-engine.LICENSE"] {
        assert_eq!(
            std::fs::read(prefix.join("share/doc/ultron").join(file)).unwrap(),
            std::fs::read(stage.join(file)).unwrap(),
            "{file}"
        );
    }
    // Reinstalling (PREFIX from the environment) replaces the binary by rename: no temporary copy is left.
    let inode = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    };
    let before = inode(&bin);
    let second = install(&[], Some(&prefix));
    assert_eq!(second.status.code(), Some(0));
    assert_ne!(inode(&bin), before);
    let names: Vec<String> = std::fs::read_dir(prefix.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let mut names = names;
    names.sort();
    // The binary, and `ultron-s` as a relative link to it (no temporary copy left behind).
    assert_eq!(names, ["ultron", "ultron-s"]);
    assert_eq!(
        std::fs::read_link(prefix.join("bin/ultron-s")).unwrap(),
        Path::new("ultron")
    );
    // The installed binary runs from the prefix.
    let mut c = Command::new(&bin);
    outside.private(&mut c);
    let version = run({
        c.env("ULTRON_DEV_ASSETS", "0").arg("--version");
        c
    });
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("{}\n", cli::version())
    );
    // An unknown argument is refused before anything is written.
    let refused = install(&["--frobnicate"], None);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "unknown argument: --frobnicate\n"
    );
}

/// A fresh session file to export, written by a print run in its own private dirs.
fn session_to_export() -> (Outside, PathBuf) {
    let writer = Outside::new();
    let (port, _seen) = mock_server(vec![anthropic_text("for export")], Naming::Title("Export".into()));
    std::fs::write(
        writer.agent.join("models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
    let printed = run({
        let mut c = writer.masked();
        c.args(["-p", "--model", "anthropic/claude-sonnet-4-5", "hi"]);
        c
    });
    assert_eq!(
        printed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&printed.stderr)
    );
    let session = session_files(&writer.agent.join("experimental").join("sessions"))
        .pop()
        .unwrap();
    (writer, session)
}

#[test]
fn cli_package_runs_that_read_no_package_files_install_nothing() {
    assert_mask_hides_checkout();
    let (writer, session) = session_to_export();
    let outside = Outside::new();
    let exported = outside.cwd.join("page.html").display().to_string();
    let session = session.display().to_string();
    let lazy: [&[&str]; 9] = [
        &["--version"],
        &["--help"],
        &["auth", "--help"],
        &["usage", "--help"],
        &["config", "--help"],
        &["migrate", "--help"],
        &["mcp", "list"],
        &["instances"],
        &["--export", &session, &exported],
    ];
    for args in lazy {
        let output = run({
            let mut c = outside.masked();
            c.args(args);
            c
        });
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!outside.runtime_base().exists(), "{args:?} installed the runtime files");
    }
    assert!(
        std::fs::read_to_string(&exported)
            .unwrap()
            .contains("Highlight.js v11.9.0")
    );
    // The first run that reads them installs them.
    let listed = run({
        let mut c = outside.masked();
        c.arg("--list-models");
        c
    });
    assert_eq!(listed.status.code(), Some(0));
    assert!(assets::verified(&outside.installed()));
    daemon_common::assert_none_mentioning(&[outside.root.clone(), writer.root.clone()], HANG_GUARD);
}

/// A provider whose first non-naming request is answered only once `release` is sent; `received` reports it.
fn held_provider() -> (u16, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (received_tx, received) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut release_rx = Some(release_rx);
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let naming = String::from_utf8_lossy(&body).contains(common::NAMING_SYSTEM);
            let reply = if naming {
                anthropic_text("Held")
            } else {
                let _ = received_tx.send(());
                if let Some(rx) = release_rx.take() {
                    let _ = rx.recv();
                }
                anthropic_text("released")
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, received, release)
}

/// A real process holding `lock` exclusively (util-linux `flock`) until its stdin closes; returns once it holds it.
fn hold_exclusively(lock: &Path) -> std::process::Child {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new("flock")
        .arg("-x")
        .arg(lock)
        .args(["sh", "-c", "echo locked; cat >/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "locked\n");
    child
}

fn set_old(dir: &Path) {
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(dir).unwrap().set_modified(an_hour_ago).unwrap();
}

#[test]
fn cli_package_prune_spares_installs_and_staging_in_use_by_running_processes() {
    assert_mask_hides_checkout();
    let outside = Outside::new();
    let base = outside.runtime_base();
    // A running session (its CLI process and session worker hold the install in use)...
    let (port, received, release) = held_provider();
    std::fs::write(
        outside.agent.join("models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
    let session = {
        let mut c = outside.masked();
        c.args(["-p", "--model", "anthropic/claude-sonnet-4-5", "hold"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c.spawn().unwrap()
    };
    received
        .recv_timeout(HANG_GUARD)
        .expect("the session reached the provider");
    // ...whose install becomes an older version's (the rename keeps its locks): the old session still running.
    let in_use = base.join("0.0.0-0000000000000000");
    std::fs::rename(outside.installed(), &in_use).unwrap();
    // An older install nobody uses, a crashed install, an install being written, a fresh staging dir, a moved-aside one.
    let unused = base.join("0.0.1-1111111111111111");
    std::fs::rename(assets::install(&outside.root.join("spare")).unwrap(), &unused).unwrap();
    let crashed = base.join(".tmp-0.0.2-crashed");
    let writing = base.join(".tmp-0.0.2-writing");
    let fresh = base.join(".tmp-0.0.2-fresh");
    let aside = base.join(".stale-0.0.2-aside");
    for dir in [&crashed, &writing, &fresh, &aside] {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(assets::LOCK), "").unwrap();
    }
    let mut writer = hold_exclusively(&writing.join(assets::LOCK));
    for dir in [&crashed, &writing] {
        set_old(dir);
    }
    // A run that needs the files installs the current version and prunes.
    let list = || {
        let output = run({
            let mut c = outside.masked();
            c.arg("--list-models");
            c
        });
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    list();
    let names = || -> BTreeSet<String> {
        std::fs::read_dir(&base)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect()
    };
    let current = outside.installed().file_name().unwrap().to_string_lossy().to_string();
    let kept: BTreeSet<String> = [
        &current,
        "0.0.0-0000000000000000",
        ".tmp-0.0.2-writing",
        ".tmp-0.0.2-fresh",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        names(),
        kept,
        "kept: the current install, the one in use, live and fresh staging"
    );
    assert!(assets::verified(&outside.installed()));
    assert!(assets::verified(&in_use), "an install in use is left whole");

    // Once the session and the writer are gone, the next run prunes what they held.
    drop(writer.stdin.take());
    writer.wait().unwrap();
    release.send(()).unwrap();
    let mut session = session;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(session.wait());
    });
    assert!(
        rx.recv_timeout(HANG_GUARD)
            .expect("the session finished")
            .unwrap()
            .success()
    );
    assert!(
        daemon_common::wait_gone(&outside.root, HANG_GUARD).is_empty(),
        "session processes still running"
    );
    list();
    let kept: BTreeSet<String> = [&current, ".tmp-0.0.2-fresh"].into_iter().map(String::from).collect();
    assert_eq!(names(), kept);
}

#[test]
fn cli_package_concurrent_runs_prune_while_installing() {
    assert_mask_hides_checkout();
    let outside = Outside::new();
    let base = outside.runtime_base();
    let unused = ["0.0.1-1111111111111111", "0.0.2-2222222222222222"];
    for (n, name) in unused.iter().enumerate() {
        let spare = assets::install(&outside.root.join(format!("spare{n}"))).unwrap();
        std::fs::create_dir_all(&base).unwrap();
        std::fs::rename(spare, base.join(name)).unwrap();
    }
    // An older install held in use by a real process (a shared lock, as a running session holds it).
    let held = base.join("0.0.3-3333333333333333");
    std::fs::rename(assets::install(&outside.root.join("spare-held")).unwrap(), &held).unwrap();
    let mut holder = Command::new("flock")
        .arg("-s")
        .arg(held.join(assets::LOCK))
        .args(["sh", "-c", "echo locked; cat >/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line, "locked\n");
    }
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut c = outside.masked();
            c.arg("--list-models")
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let output = rx.recv_timeout(HANG_GUARD).expect("a run hung").unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let names: BTreeSet<String> = std::fs::read_dir(&base)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let current = outside.installed().file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(names, BTreeSet::from([current, "0.0.3-3333333333333333".to_string()]));
    assert!(assets::verified(&outside.installed()) && assets::verified(&held));
    drop(holder.stdin.take());
    holder.wait().unwrap();
}

/// Whether some process waits for a lock on `path`'s inode (`/proc/locks` lists waiters with `->`).
fn lock_waited_on(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let ino = std::fs::metadata(path).unwrap().ino();
    std::fs::read_to_string("/proc/locks")
        .unwrap_or_default()
        .lines()
        .any(|line| line.contains("->") && line.split_whitespace().any(|f| f.ends_with(&format!(":{ino}"))))
}

#[test]
fn cli_package_ready_reinstalls_when_pruned_while_waiting_for_its_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("runtime");
    let target = assets::install(&base).unwrap();
    let old_lock = target.join(assets::LOCK);
    // A pruner holds the install's lock exclusively...
    let pruner = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&old_lock)
        .unwrap();
    pruner.lock().unwrap();
    let ready = {
        let source = assets::Source::Embedded(base.clone());
        std::thread::spawn(move || source.ready())
    };
    // ...while a run waits for it; the pruner moves the install aside and a fresh one appears at the path.
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while !lock_waited_on(&old_lock) {
        assert!(std::time::Instant::now() < deadline, "ready() never waited on the lock");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::rename(&target, base.join(".stale-moved")).unwrap();
    assert_eq!(assets::install(&base).unwrap(), target);
    drop(pruner);
    assert_eq!(ready.join().unwrap().unwrap(), target);
    // The run holds the install now at the path, not the one moved aside: no prune can take it.
    let fresh = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target.join(assets::LOCK))
        .unwrap();
    assert!(fresh.try_lock().is_err(), "the fresh install is not held in use");
}
