//! Ported from packages/coding-agent/test (settings-manager, settings-manager-bug, trust-manager, auth-storage)
//! with their literal expectations.

use std::fs;
use std::path::PathBuf;

use config::auth::AuthStorage;
use config::settings::{Scope, SettingsManager};
use config::trust::ProjectTrustStore;
use serde_json::{Value, json};

fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let agent = dir.path().join("agent");
    let project = dir.path().join("project");
    fs::create_dir_all(&agent).expect("agent");
    fs::create_dir_all(project.join(".pi")).expect("project");
    (dir, agent, project)
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn config_ported_stale_autoreview_worker_keeps_other_process_keys() {
    let (_tmp, agent, project) = dirs();
    let path = agent.join("settings.json");
    fs::write(&path, r#"{"autoreview":{"model":"p/m"}}"#).expect("seed");
    let mut picker = SettingsManager::create(&project, &agent, true);
    let mut stale = SettingsManager::create(&project, &agent, true);
    picker.set_autoreview_settings(&[
        ("enabled", Some(true.into())),
        ("accounts", Some(json!(["github.com/me"]))),
    ]);
    stale.set_autoreview_settings(&[("ack", Some(false.into()))]);
    assert_eq!(
        read_json(&path)["autoreview"],
        json!({ "model": "p/m", "enabled": true, "accounts": ["github.com/me"], "ack": false })
    );
    picker.set_autoreview_settings(&[("accounts", None)]);
    assert_eq!(
        read_json(&path)["autoreview"],
        json!({ "model": "p/m", "enabled": true, "ack": false })
    );
}

#[test]
fn config_ported_invalid_file_keeps_previous_settings_and_reports_path() {
    let (_tmp, agent, project) = dirs();
    let path = agent.join("settings.json");
    fs::write(&path, r#"{"theme":"dark"}"#).expect("seed");
    let mut manager = SettingsManager::create(&project, &agent, true);
    fs::write(&path, "{ invalid json").expect("corrupt");
    manager.reload();
    assert_eq!(manager.theme().as_deref(), Some("dark"));
    let errors = manager.drain_errors();
    assert_eq!(
        errors.iter().map(|e| (e.scope, e.path.clone())).collect::<Vec<_>>(),
        vec![(Scope::Global, path)]
    );
}

#[test]
fn config_ported_external_project_changes_survive_unrelated_project_write() {
    let (_tmp, agent, project) = dirs();
    let path = project.join(".pi").join("settings.json");
    fs::write(&path, r#"{"extensions":["./a"]}"#).expect("seed");
    let mut manager = SettingsManager::create(&project, &agent, true);
    fs::write(&path, r#"{"extensions":["./a"],"prompts":["./p"]}"#).expect("external");
    manager
        .set_project("packages", Some(json!(["npm:x"])))
        .expect("trusted");
    assert_eq!(
        read_json(&path),
        json!({ "extensions": ["./a"], "prompts": ["./p"], "packages": ["npm:x"] })
    );
}

#[test]
fn config_ported_untrusted_project_refuses_writes_and_skips_settings() {
    let (_tmp, agent, project) = dirs();
    fs::write(project.join(".pi/settings.json"), r#"{"theme":"project"}"#).expect("seed");
    let mut manager = SettingsManager::create(&project, &agent, false);
    assert_eq!(manager.project_settings(), &serde_json::Map::new());
    assert_eq!(
        manager.set_project("theme", Some("x".into())),
        Err("Project is not trusted; refusing to write project settings".to_string())
    );
    manager.set_project_trusted(true);
    assert_eq!(manager.theme().as_deref(), Some("project"));
}

#[test]
fn config_ported_trust_decisions_inherit_from_parents() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = ProjectTrustStore::new(&tmp.path().join("agent"));
    let parent = tmp.path().join("trusted-parent");
    let child = parent.join("project");
    fs::create_dir_all(&child).expect("mkdir");
    assert_eq!(store.get(&child), Ok(None));
    store.set(&parent, Some(true)).expect("set");
    assert_eq!(store.get(&child), Ok(Some(true)));
    store.set(&child, Some(false)).expect("set");
    assert_eq!(store.get(&child), Ok(Some(false)));
    store.set(&child, None).expect("set");
    assert_eq!(store.get(&child), Ok(Some(true)));
}

#[test]
fn config_ported_auth_modify_preserves_external_edits_and_malformed_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("auth.json");
    let mut storage = AuthStorage::create(&path);
    fs::write(&path, r#"{"external":{"type":"api_key","key":"ext"}}"#).expect("external");
    let credential = json!({ "type": "api_key", "key": "stored" });
    assert_eq!(
        storage.modify("anthropic", |_| Some(credential.clone())),
        Ok(Some(credential.clone()))
    );
    assert_eq!(
        read_json(&path),
        json!({ "external": { "type": "api_key", "key": "ext" }, "anthropic": credential })
    );
    fs::write(&path, "{ malformed").expect("corrupt");
    assert!(
        storage
            .modify("anthropic", |_| Some(json!({ "type": "api_key", "key": "new" })))
            .is_err()
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), "{ malformed");
}

#[test]
fn config_ported_settings_lock_held_blocks_write_and_stale_lock_is_taken_over() {
    let (_tmp, agent, project) = dirs();
    let path = agent.join("settings.json");
    fs::write(&path, r#"{"theme":"dark"}"#).expect("seed");
    let lock = agent.join("settings.json.lock");
    let mut manager = SettingsManager::create(&project, &agent, true);
    fs::create_dir(&lock).expect("held lock");
    manager.set_global("theme", Some("light".into()));
    assert_eq!(manager.drain_errors().len(), 1);
    assert_eq!(read_json(&path), json!({ "theme": "dark" }));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    fs::File::open(&lock)
        .expect("open lock")
        .set_modified(old)
        .expect("age lock");
    manager.set_global("theme", Some("light".into()));
    assert_eq!(manager.drain_errors(), vec![]);
    assert_eq!(read_json(&path), json!({ "theme": "light" }));
    assert!(!lock.exists());
}

#[test]
fn config_ported_settings_write_reads_the_file_only_after_taking_the_lock() {
    let (_tmp, agent, project) = dirs();
    let path = agent.join("settings.json");
    fs::write(&path, r#"{"theme":"dark"}"#).expect("seed");
    let lock = agent.join("settings.json.lock");
    // Another process holds the lock, writes a key, then releases it inside the 200 ms retry window. Under load
    // that process can be starved past the window; the write then fails with the lock held, which is right, so
    // that attempt checks nothing was written and the test tries again until the other process kept its pace.
    for _attempt in 0..50 {
        fs::write(&path, r#"{"theme":"dark"}"#).expect("seed");
        let mut manager = SettingsManager::create(&project, &agent, true);
        fs::create_dir(&lock).expect("held lock");
        let other = {
            let (path, lock) = (path.clone(), lock.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(60));
                fs::write(&path, r#"{"theme":"dark","packages":["npm:other"]}"#).expect("other write");
                fs::remove_dir(&lock).expect("release");
            })
        };
        manager.set_global("theme", Some("light".into()));
        other.join().expect("other");
        if manager.drain_errors().is_empty() {
            assert_eq!(read_json(&path), json!({ "theme": "light", "packages": ["npm:other"] }));
            return;
        }
        assert_eq!(read_json(&path), json!({ "theme": "dark", "packages": ["npm:other"] }));
    }
    panic!("the other writer never released inside the retry window in 50 attempts");
}
