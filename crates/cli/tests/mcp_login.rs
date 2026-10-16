//! Replays `fixtures/mcp_login.json` (recorded from the TypeScript CLI by `fixtures/mcp_login.mjs`): `ultron mcp list`,
//! `login` and `logout` against crates/mcp/fixtures/fake_oauth_server.py, every step in one profile. The browser is a
//! fake `xdg-open`, the only program on the run's PATH, that follows the authorization redirect to the CLI's
//! loopback callback (or ignores it); nothing real is ever opened.

use serde_json::Value;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn fake_browser() -> String {
    format!(
        "#!{} -I\nimport os, sys, urllib.request\nlog = os.path.join(os.path.dirname(os.path.abspath(__file__)), \"opened.log\")\nwith open(log, \"a\") as f:\n    f.write(\"opened\\n\")\nif os.path.exists(os.path.join(os.path.dirname(os.path.abspath(__file__)), \"ignore\")):\n    sys.exit(0)\nurllib.request.urlopen(sys.argv[1]).read()\n",
        python()
    )
}

/// The generator's `normalizeText`.
fn normalize(text: &str, origin: &str, tmp: &str) -> String {
    let text = text.replace(origin, "<ORIGIN>").replace(tmp, "<TMP>");
    let marker = "<ORIGIN>/authorize?";
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find(marker) {
        out.push_str(&rest[..i + marker.len()]);
        out.push_str("<QUERY>");
        let after = &rest[i + marker.len()..];
        rest = &after[after.find(char::is_whitespace).unwrap_or(after.len())..];
    }
    out.push_str(rest);
    out
}

struct FakeServer(std::process::Child);

impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cli_mcp_login_logout_and_list_match_typescript() {
    use std::os::unix::fs::PermissionsExt;
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/mcp_login.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (agent, bin, cwd) = (base.join("agent"), base.join("bin"), base.join("cwd"));
    for dir in [&agent, &bin, &cwd] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let browser = bin.join("xdg-open");
    std::fs::write(&browser, fake_browser()).unwrap();
    std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mcp/fixtures/fake_oauth_server.py");
    let mut child = Command::new(python())
        .arg("-I")
        .arg(script)
        .arg("--resources")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let server = FakeServer(child);
    let mut port = String::new();
    std::io::BufReader::new(stdout).read_line(&mut port).unwrap();
    let origin = format!("http://127.0.0.1:{}", port.trim());
    std::fs::write(
        agent.join("mcp.json"),
        serde_json::json!({"mcpServers": {"issues": {"url": format!("{origin}/mcp")}}}).to_string(),
    )
    .unwrap();
    let base_text = base.display().to_string();
    for (n, step) in fixture["steps"].as_array().unwrap().iter().enumerate() {
        if step["ignoreBrowser"] == true {
            std::fs::write(bin.join("ignore"), "").unwrap();
        }
        let args: Vec<&str> = step["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(&args)
            .env_clear()
            .current_dir(&cwd)
            .env("PATH", &bin)
            .env("HOME", &base)
            .env("ULTRON_CODING_AGENT_DIR", &agent)
            .env("ULTRON_SERVER_DIR", base.join("server"))
            .env("NO_COLOR", "1")
            .env("PI_OFFLINE", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let what = format!("step {n} {args:?}");
        let text = |bytes: &[u8]| normalize(&String::from_utf8_lossy(bytes), &origin, &base_text);
        assert_eq!(text(&output.stdout), step["stdout"].as_str().unwrap(), "{what} stdout");
        assert_eq!(text(&output.stderr), step["stderr"].as_str().unwrap(), "{what} stderr");
        assert_eq!(
            output.status.code(),
            step["code"].as_i64().map(|c| c as i32),
            "{what} code"
        );
        let opened = std::fs::read_to_string(bin.join("opened.log")).map_or(0, |t| t.lines().count());
        assert_eq!(opened as u64, step["browserOpened"].as_u64().unwrap(), "{what} browser");
        let signed_in = std::fs::read_to_string(agent.join("mcp-auth.json")).is_ok_and(|t| t.contains("access-"));
        assert_eq!(signed_in, step["signedIn"] == true, "{what} stored tokens");
    }
    let log: Value = {
        let url = format!("{origin}/__log");
        let output = Command::new(python())
            .args([
                "-I",
                "-c",
                "import sys, urllib.request; print(urllib.request.urlopen(sys.argv[1]).read().decode())",
            ])
            .arg(url)
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let log: Vec<String> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|l| normalize(l.as_str().unwrap(), &origin, &base_text))
        .collect();
    let want: Vec<String> = fixture["serverLog"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    assert_eq!(log, want, "server log");
    drop(server);
}
