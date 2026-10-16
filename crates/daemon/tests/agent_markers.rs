//! `PI_CODING_AGENT` / `AI_AGENT` in every session process: replays fixtures/agent-markers.json (cli/setup.ts
//! `setupCli` and process.ts's `{ ...process.env, ...options.env }`, recorded by fixtures/generate-agent-markers.mjs)
//! through `daemon::process::spawn` with a stand-in executable that writes the environment it got.

use std::time::{Duration, Instant};

use daemon::process::{AGENT_MARKERS, Role, set_executable, spawn};
use serde_json::Value;

fn env_of(text: &str, key: &str) -> Option<String> {
    text.split('\0')
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}

#[test]
fn daemon_internal_processes_carry_the_agent_markers_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/agent-markers.json")).unwrap();
    let markers: Vec<(String, String)> = AGENT_MARKERS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let want: Vec<(String, String)> = fixture["markers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    assert_eq!(markers, want);

    let dir = tempfile::tempdir().unwrap();
    // The stand-in "ultron": `__internal <role> <out>` writes its environment to <out>.
    let exe = dir.path().join("fake-ultron");
    std::fs::write(
        &exe,
        "#!/bin/sh\n/usr/bin/env -0 > \"$3.tmp\" && /bin/mv \"$3.tmp\" \"$3\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    set_executable(exe);
    let run = |name: &str, env: &[(String, String)]| -> String {
        let out = dir.path().join(name);
        let mut child = spawn(Role::Server, &[out.display().to_string()], env).unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while !out.exists() {
            assert!(Instant::now() < deadline, "hang guard: {name}");
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.wait();
        std::fs::read_to_string(out).unwrap()
    };
    let plain = run("plain.env", &[]);
    for (key, value) in &want {
        assert_eq!(env_of(&plain, key).as_deref(), Some(value.as_str()), "{key}");
    }
    // `options.env` still wins over the markers.
    let overridden = run("override.env", &[("AI_AGENT".into(), "from-options".into())]);
    for (key, value) in fixture["withOptionsEnv"].as_object().unwrap() {
        assert_eq!(env_of(&overridden, key).as_deref(), value.as_str(), "{key}");
    }
}
