//! The session's snapshot signing key (session-worker.ts `loadSnapshotKey(getAgentDir())`) follows
//! `ULTRON_RLM_SNAPSHOT_KEY_STORE`: with `auto` and a `secret-tool` on the session PATH (a fake script; the real
//! keyring is never touched) the key the composed session signs with lives in that keyring and the profile keeps
//! only the marker; by default it is the profile file.

use session_core::RunEnv;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

fn run_env(dir: &Path, vars: &[(&str, String)]) -> RunEnv {
    let agent = dir.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    RunEnv {
        cwd: dir.to_path_buf(),
        home: dir.join("home"),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: dir.join("docs"),
    }
}

fn fake_secret_tool(bin: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let vault = bin.join("vault");
    let script = format!(
        "#!/bin/sh\nvault='{}'\ncase \"$1\" in\n  lookup) [ -f \"$vault\" ] || exit 1; cat \"$vault\" ;;\n  store) cat > \"$vault\" ;;\nesac\n",
        vault.display()
    );
    std::fs::write(bin.join("secret-tool"), script).unwrap();
    std::fs::set_permissions(bin.join("secret-tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
    vault
}

fn hex(key: &[u8]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn session_core_snapshot_key_auto_store_uses_the_session_path_keyring() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let vault = fake_secret_tool(&bin);
    let env = run_env(
        dir.path(),
        &[
            ("PATH", format!("{}:/usr/bin:/bin", bin.display())),
            ("ULTRON_RLM_SNAPSHOT_KEY_STORE", "auto".into()),
        ],
    );
    let config = session_core::compose::Config::from_env(&env);
    let key = config.snapshot_key.expect("a key");
    assert_eq!(std::fs::read_to_string(&vault).unwrap(), hex(&key));
    let agent = env.agent_dir();
    assert!(!agent.join("rlm-snapshot.key").exists());
    assert!(agent.join("rlm-snapshot.key.keyring").exists());
    // The next session reads the same key back from the keyring.
    assert_eq!(session_core::compose::Config::from_env(&env).snapshot_key, Some(key));
}

#[test]
fn session_core_snapshot_key_defaults_to_the_profile_file() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let vault = fake_secret_tool(&bin);
    let env = run_env(dir.path(), &[("PATH", format!("{}:/usr/bin:/bin", bin.display()))]);
    let key = session_core::compose::Config::from_env(&env)
        .snapshot_key
        .expect("a key");
    let file = std::fs::read_to_string(env.agent_dir().join("rlm-snapshot.key")).unwrap();
    assert_eq!(file.trim(), hex(&key));
    assert!(!vault.exists());
}
