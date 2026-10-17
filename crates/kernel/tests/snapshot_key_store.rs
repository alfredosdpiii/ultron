//! The snapshot key store, ported from `packages/coding-agent/test/ultron-snapshot-key-store.test.ts` with its
//! literal expectations: the file by default, the keyring in auto/keyring mode (taking over a file key, a marker
//! left behind), a process key once the keyring held the key but cannot be reached, and the `secret-tool` CLI (a
//! fake script on an injected PATH; the real keyring is never touched).

use kernel::load_or_create_snapshot_key;
use kernel::snapshot::SNAPSHOT_KEY_FILE;
use kernel::snapshot_store::{
    KeySource, KeyStore, Lookup, SNAPSHOT_KEYRING_MARKER, SnapshotKeyring, load_snapshot_key, secret_tool_keyring,
};
use std::cell::RefCell;
use std::path::Path;

#[derive(Default)]
struct State {
    value: Option<String>,
    stores: usize,
    unreachable: bool,
    store_fails: bool,
}

/// `fakeKeyring`.
#[derive(Default)]
struct Fake(RefCell<State>);

impl Fake {
    fn with(value: Option<&str>, unreachable: bool, store_fails: bool) -> Fake {
        Fake(RefCell::new(State {
            value: value.map(str::to_string),
            unreachable,
            store_fails,
            ..State::default()
        }))
    }
}

impl SnapshotKeyring for Fake {
    fn lookup(&self) -> Lookup {
        let state = self.0.borrow();
        (!state.unreachable).then(|| state.value.clone())
    }

    fn store(&self, hex: &str) -> bool {
        let mut state = self.0.borrow_mut();
        state.stores += 1;
        if state.unreachable || state.store_fails {
            return false;
        }
        state.value = Some(hex.into());
        true
    }
}

fn hex(key: &[u8]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn kernel_snapshot_key_store_defaults_to_the_file_without_touching_the_keyring() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = Fake::default();
    let loaded = load_snapshot_key(dir.path(), KeyStore::from_setting(None), &keyring).unwrap();
    assert_eq!(loaded.source, KeySource::File);
    assert_eq!(loaded.key, load_or_create_snapshot_key(dir.path()).unwrap());
    assert_eq!(keyring.0.borrow().stores, 0);
    assert!(!dir.path().join(SNAPSHOT_KEYRING_MARKER).exists());
    for (setting, store) in [
        (" AUTO ", KeyStore::Auto),
        ("keyring", KeyStore::Keyring),
        ("file", KeyStore::File),
        ("other", KeyStore::File),
    ] {
        assert_eq!(KeyStore::from_setting(Some(setting)), store, "{setting}");
    }
}

#[test]
fn kernel_snapshot_key_store_keyring_takes_over_an_existing_file_key() {
    let dir = tempfile::tempdir().unwrap();
    let file_key = load_or_create_snapshot_key(dir.path()).unwrap();
    let keyring = Fake::default();
    let loaded = load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap();
    assert_eq!(loaded.source, KeySource::Keyring);
    assert_eq!(loaded.key, file_key);
    assert_eq!(keyring.0.borrow().value.as_deref(), Some(hex(&file_key).as_str()));
    assert!(!dir.path().join(SNAPSHOT_KEY_FILE).exists());
    let marker = std::fs::read_to_string(dir.path().join(SNAPSHOT_KEYRING_MARKER)).unwrap();
    assert!(marker.contains("holds no secret"));
    // Stable on the next load, without storing again.
    assert_eq!(
        load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap().key,
        file_key
    );
    assert_eq!(keyring.0.borrow().stores, 1);
}

#[test]
fn kernel_snapshot_key_store_fresh_profile_with_a_keyring_writes_no_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = Fake::default();
    let loaded = load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap();
    assert_eq!(loaded.source, KeySource::Keyring);
    assert_eq!(loaded.key.len(), 32);
    assert_eq!(keyring.0.borrow().value.as_deref(), Some(hex(&loaded.key).as_str()));
    assert!(!dir.path().join(SNAPSHOT_KEY_FILE).exists());
}

#[test]
fn kernel_snapshot_key_store_unreachable_keyring_uses_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let loaded = load_snapshot_key(dir.path(), KeyStore::Auto, &Fake::with(None, true, false)).unwrap();
    assert_eq!(loaded.source, KeySource::File);
    assert_eq!(loaded.key, load_or_create_snapshot_key(dir.path()).unwrap());
    // A keyring that refuses to store also falls back to the file, keeping the same key.
    let again = load_snapshot_key(dir.path(), KeyStore::Auto, &Fake::with(None, false, true)).unwrap();
    assert_eq!(again.source, KeySource::File);
    assert_eq!(again.key, loaded.key);
    assert!(dir.path().join(SNAPSHOT_KEY_FILE).exists());
}

#[test]
fn kernel_snapshot_key_store_never_forks_a_second_persistent_key() {
    let dir = tempfile::tempdir().unwrap();
    let keyring = Fake::default();
    let stored = load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap().key;
    keyring.0.borrow_mut().unreachable = true;
    let offline = load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap();
    assert_eq!(offline.source, KeySource::Process);
    assert_eq!(
        offline.warning.as_deref(),
        Some(
            "RLM snapshot key: the OS keyring holding the key is unreachable; using a key for this process only, so its snapshots will not restore in later sessions"
        )
    );
    assert_ne!(offline.key, stored);
    assert!(!dir.path().join(SNAPSHOT_KEY_FILE).exists());
    // Back online: the original key.
    keyring.0.borrow_mut().unreachable = false;
    assert_eq!(
        load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap().key,
        stored
    );
}

#[test]
fn kernel_snapshot_key_store_disagreeing_file_is_left_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let file_key = load_or_create_snapshot_key(dir.path()).unwrap();
    let other = "ab".repeat(32);
    let keyring = Fake::with(Some(&other), false, false);
    let loaded = load_snapshot_key(dir.path(), KeyStore::Auto, &keyring).unwrap();
    assert_eq!(hex(&loaded.key), other);
    let path = dir.path().join(SNAPSHOT_KEY_FILE).display().to_string();
    assert_eq!(
        loaded.warning,
        Some(format!(
            "RLM snapshot key: {path} differs from the key in the OS keyring; the keyring key is used and the file was left in place"
        ))
    );
    assert!(dir.path().join(SNAPSHOT_KEY_FILE).exists());
    let file_mode = load_snapshot_key(dir.path(), KeyStore::from_setting(Some("file")), &keyring).unwrap();
    assert_eq!(file_mode.source, KeySource::File);
    assert_eq!(file_mode.key, file_key);
    assert_eq!(keyring.0.borrow().stores, 0);
    // Keyring mode with an unreachable keyring degrades to a process key rather than the file.
    let fresh = tempfile::tempdir().unwrap();
    let forced = load_snapshot_key(fresh.path(), KeyStore::Keyring, &Fake::with(None, true, false)).unwrap();
    assert_eq!(forced.source, KeySource::Process);
    assert!(!fresh.path().join(SNAPSHOT_KEY_FILE).exists());
    // Keyring mode whose keyring will not store: a process key too.
    let refusing = load_snapshot_key(fresh.path(), KeyStore::Keyring, &Fake::with(None, false, true)).unwrap();
    assert_eq!(refusing.source, KeySource::Process);
    assert!(
        refusing
            .warning
            .unwrap()
            .starts_with("RLM snapshot key: the OS keyring did not store the key;")
    );
}

/// A fake `secret-tool` in `dir`: lookup prints the secret or exits 1 silently (recording its attributes), store
/// reads the secret from stdin, and `vault.broken` makes it fail loudly.
fn fake_secret_tool(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let vault = dir.join("vault");
    let tool = dir.join("secret-tool");
    std::fs::write(
        &tool,
        [
            "#!/bin/sh".to_string(),
            format!("vault='{}'", vault.display()),
            r#"[ -f "$vault.broken" ] && { echo "Cannot autolaunch D-Bus" >&2; exit 1; }"#.into(),
            r#"case "$1" in"#.into(),
            r#"  lookup) shift; [ -f "$vault" ] || exit 1; printf "%s" "$(cat "$vault")"; echo "$*" > "$vault.attrs" ;;"#
                .into(),
            r#"  store) cat > "$vault" ;;"#.into(),
            "esac".into(),
        ]
        .join("\n"),
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    vault
}

#[test]
fn kernel_snapshot_key_store_secret_tool_speaks_the_cli_from_the_injected_path() {
    let bin = tempfile::tempdir().unwrap();
    let vault = fake_secret_tool(bin.path());
    let agent = tempfile::tempdir().unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let var = |name: &str| (name == "PATH").then(|| path.clone());
    let keyring = secret_tool_keyring(agent.path(), &var);
    assert_eq!(keyring.lookup(), Some(None));
    let loaded = load_snapshot_key(agent.path(), KeyStore::Auto, &keyring).unwrap();
    assert_eq!(loaded.source, KeySource::Keyring);
    assert_eq!(std::fs::read_to_string(&vault).unwrap(), hex(&loaded.key));
    let attrs = std::fs::read_to_string(bin.path().join("vault.attrs")).unwrap();
    assert!(
        attrs.contains(&format!("purpose rlm-snapshot-key profile {}", agent.path().display())),
        "{attrs}"
    );
    std::fs::write(bin.path().join("vault.broken"), "").unwrap();
    assert_eq!(keyring.lookup(), None);
    // No secret-tool on the session PATH (the process PATH is never searched): unavailable.
    let empty = tempfile::tempdir().unwrap();
    let missing = empty.path().display().to_string();
    let none = secret_tool_keyring(agent.path(), &|name: &str| (name == "PATH").then(|| missing.clone()));
    assert_eq!(none.lookup(), None);
    assert!(!none.store("00"));
}
