//! `/mcp`, ported from packages/coding-agent/test/ultron-native-mcp.test.ts ("/mcp reports status, connects,
//! disconnects and disables; session start waits for no server", "a profile without MCP servers registers no gateway
//! tool") with its literal expectations. The TypeScript fake server's echo tool is the Python fake's (`beta`:
//! `echo` "Beta echo" and `beta_only`), so the tool texts name those.

use mcp::config::{Scope, ServerEntry};
use mcp::gateway::{Gateway, GatewayOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn entry(name: &str, log: &Path, extra: Value) -> ServerEntry {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_gateway_server.py");
    let mut config = json!({ "command": python(), "args": [script, "beta", log] });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    ServerEntry {
        name: name.into(),
        config: config.as_object().unwrap().clone(),
        source: "test".into(),
        scope: Scope::Global,
    }
}

fn started(log: &PathBuf) -> bool {
    std::fs::read_to_string(log).is_ok_and(|text| text.contains("initialize"))
}

fn gateway(servers: Vec<ServerEntry>, dir: &Path) -> Arc<Gateway> {
    Gateway::new(GatewayOptions {
        agent_dir: dir.to_path_buf(),
        project_trusted: true,
        servers: Some(servers),
        env: mcp::connection::ConnectionEnv {
            cwd: dir.to_path_buf(),
            home: dir.to_path_buf(),
            ..Default::default()
        },
        open_url: None,
    })
}

fn call(gateway: &Gateway, params: Value) -> Value {
    gateway.execute(params.as_object().unwrap(), None).unwrap()
}

#[test]
fn mcp_gateway_command_reports_status_connects_disconnects_and_disables() {
    let dir = tempfile::tempdir().unwrap();
    let (lazy_log, eager_log) = (dir.path().join("lazy.log"), dir.path().join("eager.log"));
    let gateway = gateway(
        vec![
            entry("lazy", &lazy_log, json!({})),
            entry("eager", &eager_log, json!({ "lifecycle": "eager" })),
        ],
        dir.path(),
    );
    gateway.start(None);
    gateway.settle();
    // Only the eager server was even started.
    assert!(started(&eager_log));
    assert!(!started(&lazy_log));
    assert!(gateway.command("").0.contains("lazy: not connected"));
    assert_eq!(gateway.command("connect lazy").0, "Connected to lazy (2 tools).");
    assert!(gateway.command("tools lazy").0.contains("lazy_echo: Beta echo"));
    assert_eq!(
        gateway.command("disconnect lazy").0,
        "Disconnected \"lazy\". The next call connects it again."
    );
    assert_eq!(gateway.command("disable lazy").0, "Disabled \"lazy\" for this session.");
    assert_eq!(
        call(&gateway, json!({ "server": "lazy" }))["details"]["error"],
        "server_disabled"
    );
    assert_eq!(gateway.command("enable lazy").0, "Enabled \"lazy\" for this session.");
    assert_eq!(call(&gateway, json!({ "server": "lazy" }))["details"]["count"], 2);
    assert_eq!(
        gateway.command("login lazy").0,
        "MCP server \"lazy\" does not use OAuth sign-in."
    );
    let usage = gateway.command("frobnicate");
    assert!(usage.0.contains("Usage: /mcp"), "{usage:?}");
    assert_eq!(usage.1, "warning");
    assert_eq!(
        gateway.command("connect nope"),
        (
            "MCP: No MCP server \"nope\". Servers: lazy, eager".to_string(),
            "error".to_string()
        )
    );
    gateway.close();
}

#[test]
fn mcp_gateway_command_enable_and_disable_edit_the_defining_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("x.log");
    let file = dir.path().join("mcp.json");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_gateway_server.py");
    std::fs::write(
        &file,
        "{\n    \"mcpServers\": {\n        \"x\": {\n            \"command\": \"@PY@\",\n            \"args\": [\"@SCRIPT@\", \"beta\", \"@LOG@\"]\n        }\n    }\n}\n"
            .replace("@PY@", &python())
            .replace("@SCRIPT@", &script.display().to_string())
            .replace("@LOG@", &log.display().to_string()),
    )
    .unwrap();
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: dir.path().to_path_buf(),
        env: mcp::connection::ConnectionEnv {
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(gateway.command("connect x").0, "Connected to x (2 tools).");
    assert_eq!(
        gateway.command("disable x").0,
        format!("Disabled \"x\" in {}.", file.display())
    );
    // Disabling disconnects: the server's stdin is closed.
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(&log).is_ok_and(|t| t.contains("eof")) {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(120),
            "the server was not closed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(gateway.command("status").0.contains("x: disabled"));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["mcpServers"]["x"]["enabled"], false);
    // Indentation is kept.
    assert!(std::fs::read_to_string(&file).unwrap().contains("\n        \"x\": {"));
    assert_eq!(
        gateway.command("enable x").0,
        format!("Enabled \"x\" in {}.", file.display())
    );
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["mcpServers"]["x"].get("enabled"), None);
    gateway.close();
}

#[test]
fn mcp_gateway_a_profile_without_mcp_servers_registers_no_gateway_tool() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(gateway(Vec::new(), dir.path()).tool(), None);
}

/// `openBrowser` of utils/open-browser.ts: the launcher per platform, never a shell.
#[test]
fn mcp_gateway_open_browser_uses_the_platform_launcher_as_typescript() {
    use mcp::gateway::browser_command;
    let url = "https://example.com/a?b=1&c=2";
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(browser_command("darwin", url), ("open".to_string(), s(&[url])));
    assert_eq!(
        browser_command("win32", url),
        ("rundll32".to_string(), s(&["url.dll,FileProtocolHandler", url]))
    );
    assert_eq!(browser_command("linux", url), ("xdg-open".to_string(), s(&[url])));
    assert_eq!(browser_command("freebsd", url), ("xdg-open".to_string(), s(&[url])));
}

/// The launcher runs detached with the URL as one argument (no shell re-parses it); a missing launcher is ignored.
#[test]
fn mcp_gateway_open_browser_passes_the_url_as_one_argument() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("args");
    let script = dir.path().join("launcher");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}.tmp && mv {}.tmp {}\n",
            out.display(),
            out.display(),
            out.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let url = "https://example.com/a?b=1&c=$(touch pwned)";
    mcp::gateway::spawn_detached(&script.display().to_string(), &[url.to_string()]);
    let start = std::time::Instant::now();
    while !out.exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(120),
            "the launcher never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(std::fs::read_to_string(&out).unwrap(), format!("{url}\n"));
    assert!(!dir.path().join("pwned").exists());
    mcp::gateway::spawn_detached("/nonexistent/launcher", &[url.to_string()]);
}

/// The session's browser opener finds `xdg-open` only on the PATH it was given (never the process's), runs it with
/// just that PATH and HOME, and opens nothing when that PATH has no launcher.
#[test]
fn mcp_gateway_browser_opener_uses_only_the_given_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let out = dir.path().join("opened");
    let launcher = bin.join("xdg-open");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$1\" \"$PATH\" \"$HOME\" > {}.tmp && mv {}.tmp {}\n",
            out.display(),
            out.display(),
            out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let open = mcp::gateway::browser_opener(Some(path.clone()), Some("/h".into()));
    open("https://example.com/signin");
    let start = std::time::Instant::now();
    while !out.exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(120),
            "the launcher never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("https://example.com/signin|{path}|/h\n")
    );
    // A PATH without a launcher, or none at all: nothing runs (the process PATH is never searched).
    std::fs::remove_file(&out).unwrap();
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    mcp::gateway::browser_opener(Some(empty.display().to_string()), None)("https://example.com/x");
    mcp::gateway::browser_opener(None, None)("https://example.com/y");
    assert!(!out.exists());
}
