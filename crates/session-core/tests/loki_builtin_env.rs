//! Loki in a composed session (session-worker.ts: `if (lokiMode(...) !== "off") process.env.ULTRON_LOKI_BUILTIN =
//! "1"`): extensions get `ULTRON_LOKI_BUILTIN=1` when the guard is on or advising (so a project's own Loki extension
//! defers to the built-in integration), not when it is off.

use serde_json::json;
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use std::path::Path;
use std::sync::Arc;

fn env_for(root: &Path, cwd: &Path, extra: &[(&str, String)]) -> RunEnv {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let mut vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
    ];
    vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
    RunEnv {
        cwd: cwd.to_path_buf(),
        home,
        var: Arc::new(move |key| vars.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    }
}

/// An extension that only records its `ULTRON_LOKI_BUILTIN` and exits (its load then fails, harmlessly).
fn recording_extension(env: &RunEnv, out: &Path) {
    let dir = env.agent_dir().join("extensions/recorder");
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "printf '%s' \"${{ULTRON_LOKI_BUILTIN-unset}}\" > '{}'; exit 1",
        out.display()
    );
    std::fs::write(
        dir.join("extension.json"),
        json!({"command": "/bin/sh", "args": ["-c", script]}).to_string(),
    )
    .unwrap();
}

fn builtin_seen(loki: Option<&str>) -> String {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let extra: Vec<(&str, String)> = loki.map(|v| ("ULTRON_LOKI", v.to_string())).into_iter().collect();
    let env = env_for(root.path(), &cwd, &extra);
    let out = root.path().join("seen");
    recording_extension(&env, &out);
    let _runtime = Runtime::new(&env, "print", root.path().join("sessions"), Services::default()).unwrap();
    std::fs::read_to_string(&out).unwrap_or_else(|e| panic!("{}: {e}", out.display()))
}

#[test]
fn session_core_loki_builtin_reaches_extensions_when_loki_is_on() {
    assert_eq!(builtin_seen(None), "1");
    assert_eq!(builtin_seen(Some("advise")), "1");
    assert_eq!(builtin_seen(Some("off")), "unset");
}
