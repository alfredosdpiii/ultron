//! pi-mcp-adapter next to the built-in MCP gateway (extensions/mcp/coexistence.ts `preferNativeMcp`, ported from
//! ultron-native-mcp.test.ts "pi-mcp-adapter is left out with a notice"): an installed adapter, auto-discovered or
//! a settings package, is left out, and the session's start tells the client's UI once (`ADAPTER_NOTICE`, info);
//! other JavaScript extensions are still reported; without the built-in gateway (`--no-extensions`) nothing is
//! touched.

use serde_json::json;
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::lane::Lane;
use std::path::Path;
use std::sync::{Arc, Mutex};

const NOTICE: &str = "pi-mcp-adapter is installed but no longer needed: Ultron connects MCP servers itself and reads the same mcp.json. The adapter was not loaded; remove it with `ultron remove npm:pi-mcp-adapter`.";

struct Opened {
    notices: Vec<(String, String)>,
    diagnostics: Vec<String>,
    errors: Vec<String>,
}

fn open(root: &Path, no_extensions: bool) -> Opened {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    let adapter = agent.join("extensions/pi-mcp-adapter");
    std::fs::create_dir_all(&adapter).unwrap();
    std::fs::write(adapter.join("index.ts"), "export default () => {};\n").unwrap();
    let other = agent.join("extensions/other-tool");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("index.ts"), "export default () => {};\n").unwrap();
    // The adapter as a settings package too (a local package directory).
    let package = root.join("packages/node_modules/pi-mcp-adapter");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        json!({"name": "pi-mcp-adapter", "pi": {"extensions": ["./index.ts"]}}).to_string(),
    )
    .unwrap();
    std::fs::write(package.join("index.ts"), "export default () => {};\n").unwrap();
    std::fs::write(
        agent.join("settings.json"),
        json!({"sandbox": {"mode": "off"}, "packages": [package.display().to_string()]}).to_string(),
    )
    .unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let notices: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let sink = notices.clone();
    let services = Services {
        mcp_notices: Some(Arc::new(move |message: &str, level: &str| {
            sink.lock().unwrap().push((message.into(), level.into()));
        })),
        no_extensions,
        ..Services::default()
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), services).unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    let lane = Lane::open(&runtime, &model, None, "mcp-coexistence").unwrap();
    lane.shutdown();
    let notices = notices.lock().unwrap().clone();
    Opened {
        notices,
        diagnostics: runtime.diagnostics.clone(),
        errors: runtime
            .extension_errors
            .iter()
            .map(|d| d.path.display().to_string())
            .collect(),
    }
}

#[test]
fn session_core_mcp_coexistence_leaves_the_adapter_out_with_one_notice_like_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let opened = open(dir.path(), false);
    assert_eq!(opened.notices, [(NOTICE.to_string(), "info".to_string())]);
    assert!(
        !opened.diagnostics.iter().any(|d| d.contains("pi-mcp-adapter")),
        "{:?}",
        opened.diagnostics
    );
    assert!(
        !opened.errors.iter().any(|d| d.contains("pi-mcp-adapter")),
        "{:?}",
        opened.errors
    );
    // Another JavaScript extension is still reported.
    assert!(
        opened
            .diagnostics
            .iter()
            .any(|d| d.contains("other-tool") && d.contains("JavaScript/TypeScript extensions are no longer loaded")),
        "{:?}",
        opened.diagnostics
    );
}

#[test]
fn session_core_mcp_coexistence_without_the_builtin_gateway_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let opened = open(dir.path(), true);
    assert_eq!(opened.notices, []);
    // The settings package's adapter is reported as any JavaScript extension.
    assert!(
        opened
            .diagnostics
            .iter()
            .any(|d| d.contains("packages/node_modules/pi-mcp-adapter")),
        "{:?}",
        opened.diagnostics
    );
}
