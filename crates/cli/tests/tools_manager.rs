//! `ensureTool("fd")` (packages/coding-agent/src/utils/tools-manager.ts) as the native client runs it before the
//! TUI starts: `<agentDir>/bin/fd`, then `fd` and `fdfind` on the run's PATH, then (not offline) the latest release
//! downloaded and extracted into `<agentDir>/bin`. Releases come from a fake GitHub on 127.0.0.1; `tar` from the
//! test's PATH. Literal expectations from the TypeScript source.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cli::Env;
use cli::tools_manager::{ensure_fd, fd_asset_name, fd_path, latest_version_from_redirect, offline};

fn env(vars: &[(&str, String)], agent: &Path) -> Env {
    let mut map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    map.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    Env {
        cwd: agent.to_path_buf(),
        home: agent.to_path_buf(),
        var: Arc::new(move |k| map.get(k).cloned()),
        docs: PathBuf::new(),
    }
}

fn executable(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn cli_tools_manager_fd_lookup_order_matches_ts() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let path_dir = dir.path().join("path");
    std::fs::create_dir_all(&path_dir).unwrap();
    let path = || vec![("PATH", path_dir.display().to_string())];
    assert_eq!(fd_path(&env(&path(), &agent)), None);
    // Debian's name.
    executable(&path_dir.join("fdfind"), "#!/bin/sh\n");
    assert_eq!(fd_path(&env(&path(), &agent)), Some(path_dir.join("fdfind")));
    // `fd` before `fdfind`; a file that cannot run is skipped.
    std::fs::write(path_dir.join("fd"), "").unwrap();
    assert_eq!(fd_path(&env(&path(), &agent)), Some(path_dir.join("fdfind")));
    executable(&path_dir.join("fd"), "#!/bin/sh\n");
    assert_eq!(fd_path(&env(&path(), &agent)), Some(path_dir.join("fd")));
    // The agent's bin directory first.
    std::fs::create_dir_all(agent.join("bin")).unwrap();
    std::fs::write(agent.join("bin").join("fd"), "").unwrap();
    assert_eq!(fd_path(&env(&path(), &agent)), Some(agent.join("bin").join("fd")));
    // Nothing on the PATH, offline: no download.
    std::fs::remove_file(agent.join("bin").join("fd")).unwrap();
    let none = vec![
        ("PATH", dir.path().join("none").display().to_string()),
        ("PI_OFFLINE", "1".to_string()),
    ];
    assert_eq!(ensure_fd(&env(&none, &agent), "http://127.0.0.1:9"), None);
}

#[test]
fn cli_tools_manager_offline_and_assets_match_ts() {
    let dir = tempfile::tempdir().unwrap();
    let with = |v: &str| offline(&env(&[("PI_OFFLINE", v.to_string())], dir.path()));
    assert!(with("1") && with("true") && with("TRUE") && with("yes") && with("Yes"));
    assert!(!with("0") && !with("on") && !with(""));
    assert!(!offline(&env(&[], dir.path())));
    assert_eq!(
        fd_asset_name("10.2.0", "linux", "x86_64").as_deref(),
        Some("fd-v10.2.0-x86_64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        fd_asset_name("10.2.0", "linux", "aarch64").as_deref(),
        Some("fd-v10.2.0-aarch64-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        fd_asset_name("10.3.0", "macos", "aarch64").as_deref(),
        Some("fd-v10.3.0-aarch64-apple-darwin.tar.gz")
    );
    assert_eq!(fd_asset_name("1", "freebsd", "x86_64"), None);
    assert_eq!(
        latest_version_from_redirect(302, Some("https://github.com/sharkdp/fd/releases/tag/v10.2.0")).unwrap(),
        "10.2.0"
    );
    assert_eq!(
        latest_version_from_redirect(302, Some("/sharkdp/fd/releases/tag/v9.0.0%2Bx")).unwrap(),
        "9.0.0+x"
    );
    assert_eq!(
        latest_version_from_redirect(200, None).unwrap_err(),
        "Failed to resolve latest sharkdp/fd release: HTTP 200 without redirect"
    );
    assert_eq!(
        latest_version_from_redirect(302, Some("https://github.com/sharkdp/fd/releases")).unwrap_err(),
        "Failed to resolve latest sharkdp/fd release: unexpected redirect to https://github.com/sharkdp/fd/releases"
    );
}

/// A one-thread HTTP server on 127.0.0.1: each request's `METHOD path` and User-Agent recorded, answered by `answer`.
fn serve(answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let target = line.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            let mut agent = String::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header == "\r\n" || header.is_empty() {
                    break;
                }
                if let Some(v) = header.to_lowercase().strip_prefix("user-agent:") {
                    agent = v.trim().to_string();
                }
            }
            log.lock().unwrap().push(format!("{target} ua={agent}"));
            let path = target.split(' ').nth(1).unwrap_or("").to_string();
            let _ = stream.write_all(&answer(&path));
            let _ = stream.flush();
        }
    });
    (base, seen)
}

fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn cli_tools_manager_downloads_the_latest_fd_release_from_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let asset = fd_asset_name("10.2.0", std::env::consts::OS, std::env::consts::ARCH).unwrap();
    // The release archive: fd under the versioned directory.
    let stage = dir.path().join("stage");
    let nested = stage.join(asset.trim_end_matches(".tar.gz"));
    executable(&nested.join("fd"), "#!/bin/sh\necho fake fd\n");
    let archive = dir.path().join(&asset);
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg(asset.trim_end_matches(".tar.gz"))
        .status()
        .unwrap();
    assert!(status.success());
    let mut bytes = Vec::new();
    std::fs::File::open(&archive).unwrap().read_to_end(&mut bytes).unwrap();
    let expected_asset = asset.clone();
    let failures = Arc::new(Mutex::new(1));
    let fail = Arc::clone(&failures);
    let (base, seen) = serve(move |path| match path {
        "/sharkdp/fd/releases/latest" => response(
            "302 Found",
            "Location: https://github.com/sharkdp/fd/releases/tag/v10.2.0\r\n",
            b"",
        ),
        p if p == format!("/sharkdp/fd/releases/download/v10.2.0/{expected_asset}") => {
            // One transient failure first: `fetchWithRetry` tries again.
            let mut left = fail.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return response("503 Service Unavailable", "", b"");
            }
            response("200 OK", "", &bytes)
        }
        _ => response("404 Not Found", "", b""),
    });
    // tar from the run's PATH only: a directory holding nothing but tar (no fd to find).
    let tools = dir.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    std::os::unix::fs::symlink("/usr/bin/tar", tools.join("tar")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/gzip", tools.join("gzip")).unwrap();
    let run_env = env(&[("PATH", tools.display().to_string())], &agent);
    let fd = ensure_fd(&run_env, &base).expect("downloaded");
    assert_eq!(fd, agent.join("bin").join("fd"));
    let out = std::process::Command::new(&fd).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fake fd\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&fd).unwrap().permissions().mode() & 0o777, 0o755);
    // The archive and the extraction directory are gone.
    let left: Vec<String> = std::fs::read_dir(agent.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["fd"]);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "GET /sharkdp/fd/releases/latest ua=ultron-coding-agent".to_string(),
            format!("GET /sharkdp/fd/releases/download/v10.2.0/{asset} ua="),
            format!("GET /sharkdp/fd/releases/download/v10.2.0/{asset} ua="),
        ]
    );
    // Found next time without a request.
    assert_eq!(ensure_fd(&run_env, &base), Some(fd));
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[test]
fn cli_tools_manager_failed_download_is_no_fd() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    let (base, seen) = serve(|path| match path {
        "/sharkdp/fd/releases/latest" => response(
            "302 Found",
            "Location: https://github.com/sharkdp/fd/releases/tag/v10.2.0\r\n",
            b"",
        ),
        _ => response("404 Not Found", "", b""),
    });
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let run_env = env(&[("PATH", empty.display().to_string())], &agent);
    assert_eq!(ensure_fd(&run_env, &base), None);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(
        cli::tools_manager::download_fd(&run_env, &base).unwrap_err(),
        format!(
            "Download failed with HTTP 404: {base}/sharkdp/fd/releases/download/v10.2.0/{}",
            fd_asset_name("10.2.0", std::env::consts::OS, std::env::consts::ARCH).unwrap()
        )
    );
    // Nothing left behind but the bin directory.
    assert_eq!(std::fs::read_dir(agent.join("bin")).unwrap().count(), 0);
}
