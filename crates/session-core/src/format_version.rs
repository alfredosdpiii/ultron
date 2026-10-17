//! The resume-time format check (format-version.ts `assertSessionFormatsReadable`): every versioned session value
//! Ultron owns must be readable by this build. The first value written in a format this build does not know fails
//! the open, naming the value and its version, before any of it is loaded, settled or rewritten.

use session_journal::Session;

/// Session values checked when a session resumes (`SINGLE_VALUES`); artifacts are checked when read.
const SINGLE_VALUES: [(&str, &str, &str); 7] = [
    ("ultron.tasks", "root", "version"),
    ("ultron.definitions", "root", "version"),
    ("ultron.usage", "root", "version"),
    ("ultron.memory.state", "root", "version"),
    ("ultron.local", "ultron.refinements", "formatVersion"),
    ("ultron.local", "ultron.experiments", "formatVersion"),
    ("ultron.local", "ultron.artifact-refs", "formatVersion"),
];

/// Version field of each `ultron.module` document (`moduleField`).
fn module_field(key: &str) -> &'static str {
    if key == "skills" { "format" } else { "version" }
}

/// `assertSessionFormatsReadable`: the error of the first value in an unknown format version.
pub fn assert_session_formats_readable(session: &Session) -> Result<(), String> {
    for (namespace, key, field) in SINGLE_VALUES {
        kernel_apis::store::read_versioned(
            &format!("{namespace}/{key}"),
            session.value(namespace, key).cloned(),
            field,
        )?;
    }
    for stored in session.state().scan_values("ultron.module", "") {
        kernel_apis::store::read_versioned(
            &format!("ultron.module/{}", stored.key),
            Some(stored.value.clone()),
            module_field(&stored.key),
        )?;
    }
    Ok(())
}
