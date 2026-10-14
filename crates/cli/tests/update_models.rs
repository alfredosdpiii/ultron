//! `ultron update --models` (`refreshModelCatalogs`): with no provider credentials nothing is fetched and the
//! command reports success as TypeScript does (`console.log(chalk.green("Model catalogs refreshed"))`, plain when
//! not a terminal). The per-provider refresh is replayed against loopback servers in
//! crates/session-core/tests/model_refresh.rs.

use std::process::Command;

#[test]
fn cli_update_models_without_credentials_refreshes_nothing_and_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("work")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["update", "--models"])
        .current_dir(tmp.path().join("work"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", tmp.path())
        .env("ULTRON_CODING_AGENT_DIR", tmp.path().join("agent"))
        .env("ULTRON_SERVER_DIR", tmp.path().join("server"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Model catalogs refreshed\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(output.status.code(), Some(0));
    assert!(!tmp.path().join("agent/models-store.json").exists());
}
