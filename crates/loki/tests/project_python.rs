//! `findProjectPython` (loki.ts), ported from ultron-loki.test.ts "the project's interpreter" with its literal
//! expectations: ULTRON_LOKI_PROJECT_PYTHON wins; an inherited LOKI_PYTHON is left to the engine; the `python` (else
//! `python3`) of the session's PATH counts only when it belongs to an environment; else `.venv`/`venv` in the
//! session directory. Fake interpreters are scripts on an injected PATH.

use loki::project::find_project_python;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn executable(path: &Path, text: &str) -> String {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.display().to_string()
}

fn fake_python(dir: &Path, name: &str, environment: bool) -> String {
    let path = dir.join(name);
    executable(
        &path,
        &format!("#!/bin/sh\necho '{}'\necho {}\n", path.display(), u8::from(environment)),
    )
}

fn find(cwd: &Path, path: &Path, extra: &[(&str, &str)]) -> Option<String> {
    let mut vars: HashMap<String, String> = extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    vars.insert("PATH".into(), path.display().to_string());
    find_project_python(cwd, &|name| vars.get(name).cloned())
}

#[test]
fn loki_project_python_named_wins_and_loki_python_is_left_to_the_engine() {
    let work = tempfile::tempdir().unwrap();
    let bin = work.path().join("bin");
    fake_python(&bin, "python", true);
    assert_eq!(
        find(work.path(), &bin, &[("ULTRON_LOKI_PROJECT_PYTHON", " /named/python ")]).as_deref(),
        Some("/named/python")
    );
    assert_eq!(find(work.path(), &bin, &[("LOKI_PYTHON", "/set/by/user")]), None);
}

#[test]
fn loki_project_python_uses_the_environment_python_bash_would_run() {
    let work = tempfile::tempdir().unwrap();
    let bin = work.path().join("bin");
    let python = fake_python(&bin, "python", true);
    assert_eq!(find(work.path(), &bin, &[]), Some(python));
    // Only python3 on PATH.
    let bin3 = work.path().join("bin3");
    let python3 = fake_python(&bin3, "python3", true);
    assert_eq!(find(work.path(), &bin3, &[]), Some(python3));
}

#[test]
fn loki_project_python_skips_a_system_interpreter_and_finds_a_venv() {
    let work = tempfile::tempdir().unwrap();
    let bin = work.path().join("bin");
    fake_python(&bin, "python", false);
    fake_python(&bin, "python3", true);
    let project = work.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    assert_eq!(find(&project, &bin, &[]), None);
    let venv = executable(&project.join("venv/bin/python3"), "#!/bin/sh\n");
    assert_eq!(find(&project, &bin, &[]), Some(venv));
    let dot_venv = executable(&project.join(".venv/bin/python"), "#!/bin/sh\n");
    assert_eq!(find(&project, &bin, &[]), Some(dot_venv));
    // No python at all.
    assert_eq!(find(work.path(), &work.path().join("empty"), &[]), None);
}

/// A `python` that fails is skipped for `python3` (TS: `if (result.error || result.status !== 0) continue`).
#[test]
fn loki_project_python_failing_python_falls_through_to_python3() {
    let work = tempfile::tempdir().unwrap();
    let bin = work.path().join("bin");
    executable(&bin.join("python"), "#!/bin/sh\nexit 3\n");
    let python3 = fake_python(&bin, "python3", true);
    assert_eq!(find(work.path(), &bin, &[]), Some(python3));
}
