//! The project's interpreter reaches the post-write engine (loki.ts: `projectPython` passed as `LOKI_PYTHON`;
//! "the project's interpreter reaches the engine as LOKI_PYTHON" in ultron-loki.test.ts): a newly imported module
//! is reported unresolved exactly when that interpreter says it is missing. The interpreter is a fake script that
//! answers the engine's import probe and records that it ran.

use host::file_hooks::{CellChanges, FileWriteGuard};
use host::loki_guard::{LokiGuard, LokiMode};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A fake project interpreter whose import probe reports `missing` and leaves `ran` behind.
fn fake_python(dir: &Path, missing: &str) -> String {
    let path = dir.join(format!("python-{}", missing.len()));
    let script = format!("#!/bin/sh\n: > '{}.ran'\necho '{missing}'\n", path.display());
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn report(python: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("app")).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join(".loki")).unwrap();
    std::fs::write(root.join(".loki/loki.json"), "{}\n").unwrap();
    std::fs::write(root.join("app/a.py"), "").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    std::fs::write(root.join("app/a.py"), "import os\nimport nonexistent_pkg_q\n").unwrap();
    let guard = LokiGuard::new(&root, LokiMode::On)
        .with_post_write(true)
        .with_project_python(Some(python.to_string()));
    let changes = CellChanges {
        files: vec![root.join("app/a.py")],
        complete: true,
        ..Default::default()
    };
    let report = guard.after_cell_changes(&changes, &root, "main").unwrap();
    assert!(
        Path::new(&format!("{python}.ran")).exists(),
        "the project interpreter was asked"
    );
    report.and_then(|r| r.message).unwrap_or_default()
}

#[test]
fn host_loki_project_python_decides_unresolved_imports() {
    let bin = tempfile::tempdir().unwrap();
    let missing = report(&fake_python(bin.path(), r#"["nonexistent_pkg_q"]"#));
    assert!(
        missing.contains("app/a.py:2: python-ast: unresolved import nonexistent_pkg_q"),
        "{missing}"
    );
    let found = report(&fake_python(bin.path(), "[]"));
    assert!(!found.contains("unresolved import"), "{found}");
}
