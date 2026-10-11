//! The `loki` commands as library calls: what loki.py's `main` prints and returns, for a host that runs the engine
//! in-process instead of as `python loki.py ...`.

use crate::config::{self, Config};
use crate::{CheckError, MAX_VIOLATIONS, hook_report, protect_path, py, resolve};
use serde_json::Value;
use std::path::Path;

/// A command's exit status and output streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: u8,
    pub stdout: String,
    pub stderr: String,
}

/// A check that could not run: `loki: <error>` on stderr, the error report on stdout for `--format json`.
pub fn fail(command: &str, error: &CheckError, json: bool) -> Outcome {
    let text = error.describe();
    Outcome {
        status: 2,
        stdout: if json {
            format!("{}\n", py::dumps(&hook_report(command, &[], &[], Some(&text))))
        } else {
            String::new()
        },
        stderr: format!("loki: {text}\n"),
    }
}

/// `load_config` with loki.py's error kinds.
pub fn load(root: &Path) -> Result<Config, CheckError> {
    config::load_config(root).map_err(|e| match e {
        config::ConfigError::Invalid(m) => CheckError::Value(m),
        config::ConfigError::Guard(kind, m) => CheckError::Guard(kind, m),
    })
}

/// `loki --root ROOT protect --file F... [--preview HOST] [--format json]`: refuse protected or out-of-root
/// targets, then preview the write. `payload` reads the preview envelope (stdin), only when `preview` is set.
pub fn protect(
    root: &Path,
    files: &[String],
    preview: Option<&str>,
    json: bool,
    payload: impl FnOnce() -> Result<Value, CheckError>,
) -> Outcome {
    let root = resolve(root);
    match load(&root) {
        Ok(config) => protect_with(&root, &config, files, preview, json, payload),
        Err(error) => fail("protect", &error, json),
    }
}

/// [`protect`] with the policy already loaded from the resolved `root`.
pub fn protect_with(
    root: &Path,
    config: &Config,
    files: &[String],
    preview: Option<&str>,
    json: bool,
    payload: impl FnOnce() -> Result<Value, CheckError>,
) -> Outcome {
    let checked = || -> Result<(Vec<String>, Vec<String>), CheckError> {
        let payload = match preview {
            Some(_) => Some(payload()?),
            None => None,
        };
        let mut paths: Vec<String> = Vec::new();
        for path in files {
            crate::valid_path(path)?;
            if !paths.contains(path) {
                paths.push(path.clone());
            }
        }
        let mut messages = Vec::new();
        for path in &paths {
            messages.extend(protect_path(path, root, config)?);
        }
        let mut warnings = Vec::new();
        if messages.is_empty() {
            messages.extend(crate::preview_violations(
                root,
                &paths,
                payload.as_ref(),
                preview.unwrap_or(""),
                crate::proc::Deadline::after(20.0),
                &mut warnings,
                config,
            )?);
        }
        Ok((messages, warnings))
    };
    let (messages, mut warnings) = match checked() {
        Ok(result) => result,
        Err(error) => return fail("protect", &error, json),
    };
    let mut seen = Vec::new();
    warnings.retain(|w| {
        !seen.contains(w) && {
            seen.push(w.clone());
            true
        }
    });
    let stdout = if json {
        format!("{}\n", py::dumps(&hook_report("protect", &messages, &warnings, None)))
    } else {
        String::new()
    };
    if !messages.is_empty() {
        let advisory: Vec<&String> = warnings.iter().filter(|w| !w.starts_with("NOT CHECKED")).collect();
        let unchecked = warnings.iter().filter(|w| w.starts_with("NOT CHECKED"));
        let label = (!advisory.is_empty()).then(|| "loki: advisory (not blocking):".to_string());
        let lines: Vec<String> = messages
            .iter()
            .cloned()
            .chain(label)
            .chain(advisory.into_iter().cloned())
            .chain(unchecked.cloned())
            .take(MAX_VIOLATIONS)
            .collect();
        return Outcome {
            status: 2,
            stdout,
            stderr: format!("{}\n", lines.join("\n")),
        };
    }
    let stderr = if warnings.is_empty() {
        String::new()
    } else {
        format!("{}\n", warnings.join("\n"))
    };
    Outcome {
        status: 0,
        stdout,
        stderr,
    }
}
