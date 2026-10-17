//! The interpreter a session's project runs on (`loki.ts` `findProjectPython`), which Loki asks whether a newly
//! imported module exists (`LOKI_PYTHON` for the engine). Not the interpreter that parses Python for Loki
//! (`ULTRON_LOKI_PYTHON`), whose packages are not the project's.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Prints the interpreter's path, then 1 when it belongs to a virtual or conda environment (any Python 2.7+).
pub const ENVIRONMENT_PROBE: &str = "import os, sys\nprint(sys.executable)\nbase = getattr(sys, 'base_prefix', sys.prefix)\nconda = os.path.isdir(os.path.join(sys.prefix, 'conda-meta'))\nprint(int(base != sys.prefix or hasattr(sys, 'real_prefix') or conda))";

/// The session variables the probe runs with (TypeScript passes the whole session environment): what `python`
/// needs to resolve the same interpreter `bash()` would, through a venv, conda, pyenv or uv.
pub const PROBE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "PYENV_ROOT",
    "PYENV_VERSION",
    "PYTHONHOME",
    "PYTHONPATH",
    "UV_PROJECT_ENVIRONMENT",
    "LANG",
    "LC_ALL",
];

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `findProjectPython(cwd, env)` over the session environment `var`:
/// - `ULTRON_LOKI_PROJECT_PYTHON`;
/// - nothing when `LOKI_PYTHON` is set (left to the engine);
/// - the `python` (else `python3`) on the session's `PATH`, run in `cwd`, when it belongs to a virtual or conda
///   environment (the first one found decides; a bare system interpreter says nothing about the project);
/// - `.venv` or `venv` in `cwd`.
pub fn find_project_python(cwd: &Path, var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let set = |key: &str| var(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(named) = set("ULTRON_LOKI_PROJECT_PYTHON") {
        return Some(named);
    }
    if set("LOKI_PYTHON").is_some() {
        return None;
    }
    let env: Vec<(String, String)> = PROBE_ENV
        .iter()
        .filter_map(|name| Some((name.to_string(), var(name)?)))
        .collect();
    probe_environment(cwd, &env).or_else(|| venv_python(cwd))
}

/// The `python`/`python3` of the session's `PATH` when it is a virtual or conda environment's.
fn probe_environment(cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let path = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str())?;
    for command in ["python", "python3"] {
        let Some(executable) = find_on_path(command, path) else {
            continue;
        };
        let Some(stdout) = run_probe(&executable, cwd, env) else {
            continue;
        };
        let mut lines = stdout.trim().lines();
        let (found, environment) = (lines.next().unwrap_or_default(), lines.next().unwrap_or_default());
        let found = found.trim_end_matches('\r');
        // The first one found is what `python` means here, whichever kind it is.
        if !found.is_empty() && Path::new(found).is_absolute() && environment.trim_end_matches('\r') == "1" {
            return Some(found.to_string());
        }
        return None;
    }
    None
}

fn venv_python(cwd: &Path) -> Option<String> {
    [".venv", "venv"].iter().find_map(|directory| {
        ["bin/python", "bin/python3", "Scripts/python.exe"]
            .iter()
            .map(|relative| cwd.join(directory).join(relative))
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.display().to_string())
    })
}

fn find_on_path(name: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The probe's stdout when it exits 0 within the timeout; the child is killed otherwise.
fn run_probe(executable: &Path, cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let mut child = Command::new(executable)
        .args(["-c", ENVIRONMENT_PROBE])
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                // Our own child, which did not answer in time.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = reader.join().unwrap_or_default();
    status.filter(|s| s.success()).map(|_| text)
}
