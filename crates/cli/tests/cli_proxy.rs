//! HTTP requests the CLI process makes go through the run's proxies as Pi's HTTP stack sends them (`EnvHttpProxyAgent`
//! with `applyHttpProxySettings`): `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` from the injected environment, and the
//! global `httpProxy` setting when they are unset. Every proxied host is `.invalid`, so only the 127.0.0.1 fake proxy
//! can answer; nothing reaches a real host.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, mpsc};

/// Only a hang guard: an event that is due arrives at once.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);

/// A fake proxy on 127.0.0.1: reports each request line and answers `answer`.
fn fake_proxy(answer: String) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let _ = stream.write_all(answer.as_bytes());
            let _ = stream.flush();
            if tx.send(line.trim_end().to_string()).is_err() {
                return;
            }
        }
    });
    (port, rx)
}

fn json_answer(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn env(root: &Path, vars: &[(&str, String)]) -> cli::Env {
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    map.insert("HOME".into(), root.display().to_string());
    map.insert(
        "ULTRON_CODING_AGENT_DIR".into(),
        root.join("agent").display().to_string(),
    );
    map.insert("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string());
    cli::Env {
        cwd: root.join("work"),
        home: root.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: root.join("docs"),
    }
}

fn layout() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for dir in ["work", "agent", "server"] {
        std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }
    tmp
}

/// `ultron update --self` asks the registry for the latest version through the `httpProxy` setting's proxy.
#[test]
fn cli_proxy_setting_carries_the_self_update_version_check() {
    let tmp = layout();
    let (port, seen) = fake_proxy(json_answer(r#"{"version":"0.0.1"}"#));
    std::fs::write(
        tmp.path().join("agent/settings.json"),
        format!(r#"{{"httpProxy": " http://127.0.0.1:{port} "}}"#),
    )
    .unwrap();
    let proxied = cli::startup::proxy_env(&env(tmp.path(), &[]));
    let endpoints = cli::package_cli::Endpoints {
        latest_version_url: "http://registry.invalid/ultron-agent/latest".into(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args: Vec<String> = ["update", "--self"].iter().map(|s| s.to_string()).collect();
    let code = cli::package_cli::run_package_with(&args, &proxied, &mut out, &mut err, &endpoints).unwrap();
    assert_eq!(
        seen.recv_timeout(HANG_GUARD).unwrap(),
        "GET http://registry.invalid/ultron-agent/latest HTTP/1.1"
    );
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("ultron is already up to date"), "code {code}: {out}");
}

/// The setup wizard's fetches (the Hindsight probe, an endpoint's model list) honour `HTTP_PROXY` and `NO_PROXY`.
#[test]
fn cli_proxy_setup_fetch_goes_through_the_injected_proxy() {
    let (port, seen) = fake_proxy(json_answer(r#"{"data":[{"id":"b"},{"id":"a"}]}"#));
    let (direct_port, direct_seen) = fake_proxy(json_answer("{}"));
    let lookup = |k: &str| match k {
        "HTTP_PROXY" => Some(format!("http://127.0.0.1:{port}")),
        "NO_PROXY" => Some("127.0.0.1".into()),
        _ => None,
    };
    let fetch = cli::setup_core::http_fetch_with(ai::net::ProxyEnv::from_lookup(&lookup));
    let models = cli::setup_core::list_endpoint_models("http://models.invalid:8080/v1", Some("k"), &fetch);
    assert_eq!(models, Some(vec!["a".to_string(), "b".to_string()]));
    assert_eq!(
        seen.recv_timeout(HANG_GUARD).unwrap(),
        "GET http://models.invalid:8080/v1/models HTTP/1.1"
    );
    // A NO_PROXY host is fetched directly.
    let direct = fetch(&cli::setup_core::FetchRequest {
        url: format!("http://127.0.0.1:{direct_port}/health"),
        headers: Vec::new(),
        timeout: std::time::Duration::from_secs(120),
    });
    assert_eq!(direct.map(|r| r.status), Ok(200));
    assert_eq!(direct_seen.recv_timeout(HANG_GUARD).unwrap(), "GET /health HTTP/1.1");
    assert!(seen.try_recv().is_err(), "the proxy saw a NO_PROXY request");
}

/// Logins and token refreshes in the CLI process (`ultron auth`, the TUI's login) reach the providers through the
/// run's `HTTPS_PROXY`.
#[test]
fn cli_proxy_oauth_requests_tunnel_through_https_proxy() {
    let tmp = layout();
    let refused = "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string();
    let (port, seen) = fake_proxy(refused);
    let run_env = env(tmp.path(), &[("HTTPS_PROXY", format!("http://127.0.0.1:{port}"))]);
    for options in [
        session_core::stored_auth::OAuthOptions::for_env(&run_env),
        cli::tui_login::options(&run_env),
    ] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let sent = runtime.block_on(options.transport.send(ai::HttpRequest {
            method: "POST".into(),
            url: "https://auth.invalid/oauth/token".into(),
            headers: Default::default(),
            body: String::new(),
        }));
        assert!(sent.is_err());
        assert_eq!(
            seen.recv_timeout(HANG_GUARD).unwrap(),
            "CONNECT auth.invalid:443 HTTP/1.1"
        );
    }
}

/// `PI_OFFLINE` (or `--offline`): the version check makes no request at all, not even to the proxy.
#[test]
fn cli_proxy_offline_run_sends_nothing() {
    let tmp = layout();
    let (port, seen) = fake_proxy(json_answer(r#"{"version":"0.0.1"}"#));
    let args: Vec<String> = ["update", "--self", "--offline"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let run_env = cli::startup::offline_env(
        &args,
        &env(tmp.path(), &[("HTTP_PROXY", format!("http://127.0.0.1:{port}"))]),
    );
    let endpoints = cli::package_cli::Endpoints {
        latest_version_url: "http://registry.invalid/ultron-agent/latest".into(),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::package_cli::run_package_with(&args[..2], &run_env, &mut out, &mut err, &endpoints).unwrap();
    assert_eq!(code, 1);
    assert!(
        String::from_utf8_lossy(&err).contains("Could not determine latest ultron version."),
        "{}",
        String::from_utf8_lossy(&err)
    );
    assert!(seen.try_recv().is_err(), "an offline run reached the proxy");
}

/// npm and git, which TypeScript runs with its `process.env`, get the run's proxies: the `httpProxy` setting
/// becomes their `HTTP_PROXY`/`HTTPS_PROXY`.
#[test]
fn cli_proxy_package_installs_pass_the_proxy_to_npm() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = layout();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let npm = root.join("bin/npm");
    std::fs::write(
        &npm,
        format!(
            "#!/bin/sh\necho \"$HTTP_PROXY|$HTTPS_PROXY|$NO_PROXY\" >> '{}'\nprefix=$4\nmkdir -p \"$prefix/node_modules/foo\"\necho '{{\"name\":\"foo\",\"version\":\"1.0.0\"}}' > \"$prefix/node_modules/foo/package.json\"\n",
            root.join("npm.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("agent/settings.json"),
        r#"{"httpProxy": "http://127.0.0.1:9"}"#,
    )
    .unwrap();
    let run_env = cli::startup::proxy_env(&env(
        &root,
        &[
            ("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display())),
            ("NO_PROXY", "localhost".into()),
        ],
    ));
    let parsed = cli::args::parse_args(&["-e".into(), "npm:foo".into()]);
    cli::print_mode::session_flags(&parsed, &run_env).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("npm.log")).unwrap(),
        "http://127.0.0.1:9|http://127.0.0.1:9|localhost\n"
    );
}
