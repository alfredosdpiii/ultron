//! `ultron autoreview install | uninstall` (service.ts): replays fixtures/service.json (recorded from `serviceFile` by
//! fixtures/generate_service.mjs) and the TypeScript test's literal expectations.

use review::service::{SelfCommand, install_service, service_file, uninstall_service};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/service.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn self_of(value: &Value) -> SelfCommand {
    SelfCommand {
        command: value["command"].as_str().unwrap().into(),
        args: value["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect(),
    }
}

fn env_of(value: &Value) -> HashMap<String, String> {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn review_service_files_match_recorded() {
    let recorded = fixture();
    for case in recorded["cases"].as_array().unwrap() {
        let target = &case["target"];
        let file = service_file(
            &self_of(&case["self"]),
            target["platform"].as_str().unwrap(),
            target["home"].as_str().unwrap(),
            &env_of(&target["env"]),
        );
        let got = file.map_or(
            Value::Null,
            |file| json!({"path": file.path, "content": file.content, "enable": file.enable, "disable": file.disable}),
        );
        assert_eq!(got, case["file"], "{case}");
    }
    for case in recorded["quotes"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        let file = service_file(
            &SelfCommand {
                command: value.into(),
                args: vec![],
            },
            "darwin",
            value,
            &HashMap::new(),
        );
        assert_eq!(
            file.map(|f| f.enable[0].clone()),
            case["quoted"].as_str().map(str::to_string),
            "{value}"
        );
    }
}

/// "install writes a restarting user unit and prints how to enable it; uninstall removes it" (the files part).
#[test]
fn review_service_install_and_uninstall() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().display().to_string();
    let env: HashMap<String, String> = [("PATH".to_string(), "/usr/bin:/bin".to_string())].into();
    let me = SelfCommand {
        command: "/usr/bin/ultron".into(),
        args: vec![],
    };
    let file = service_file(&me, "linux", &home, &env).unwrap();
    install_service(&file).unwrap();
    let unit = dir.path().join(".config/systemd/user/ultron-autoreview.service");
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(text.contains("ExecStart=/usr/bin/ultron autoreview run"));
    assert!(text.contains("Restart=always"));
    assert!(text.contains("Environment=PATH=/usr/bin:/bin"));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777, 0o644);
    assert!(uninstall_service(&file));
    assert!(!unit.exists());
    assert!(!uninstall_service(&file));
    let mac = service_file(
        &SelfCommand {
            command: "/opt/node".into(),
            args: vec!["/opt/ultron/cli.js".into()],
        },
        "darwin",
        "/Users/x",
        &[("PATH".to_string(), "/bin".to_string())].into(),
    )
    .unwrap();
    assert_eq!(mac.path, "/Users/x/Library/LaunchAgents/com.ultron.autoreview.plist");
    assert!(
        mac.content
            .contains("<string>/opt/ultron/cli.js</string>\n\t\t<string>autoreview</string>\n\t\t<string>run</string>")
    );
    assert!(mac.content.contains("<key>KeepAlive</key><true/>"));
    assert_eq!(service_file(&me, "win32", "/x", &HashMap::new()), None);
}
