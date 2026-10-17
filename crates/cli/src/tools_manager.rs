//! `ensureTool("fd")` (packages/coding-agent/src/utils/tools-manager.ts), as the native client TUI runs it before
//! it starts (client-tui.ts `runClientTui`): `<agentDir>/bin/fd` first, then `fd` and `fdfind` on the run's PATH,
//! then (not offline) the latest sharkdp/fd release downloaded from GitHub into `<agentDir>/bin`. Failures are
//! silent: the TUI then completes `@` paths without fd. The release host is a parameter so tests answer from
//! 127.0.0.1; external programs (`tar`) come from the run's PATH.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Env;

/// Where the TypeScript product downloads releases from.
pub const GITHUB: &str = "https://github.com";
const FD_REPO: &str = "sharkdp/fd";
/// `${APP_NAME}-coding-agent` with `piConfig.name` "ultron".
const USER_AGENT: &str = "ultron-coding-agent";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// `fetchWithRetry`: two immediate retries of a transport failure or one of these statuses.
const RETRYABLE_STATUS: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];
const MAX_RETRIES: usize = 2;

/// `isOfflineModeEnabled`: `PI_OFFLINE` is `1`, `true` or `yes` (any case for the words).
pub fn offline(env: &Env) -> bool {
    (env.var)("PI_OFFLINE").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `getToolPath("fd")`: `<agentDir>/bin/fd` when it exists, else the first of `fd`, `fdfind` that the run's PATH
/// can start (resolved to its path).
pub fn fd_path(env: &Env) -> Option<PathBuf> {
    let local = env.agent_dir().join("bin").join("fd");
    if local.exists() {
        return Some(local);
    }
    let path = (env.var)("PATH")?;
    ["fd", "fdfind"].iter().find_map(|name| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| executable(candidate))
    })
}

/// `ensureTool("fd")` with releases from `host` ([`GITHUB`] in the product).
pub fn ensure_fd(env: &Env, host: &str) -> Option<PathBuf> {
    if let Some(path) = fd_path(env) {
        return Some(path);
    }
    if offline(env) {
        return None;
    }
    download_fd(env, host).ok()
}

/// `getAssetName` for fd on this platform.
pub fn fd_asset_name(version: &str, os: &str, arch: &str) -> Option<String> {
    let arch = if arch == "aarch64" || arch == "arm64" {
        "aarch64"
    } else {
        "x86_64"
    };
    match os {
        "macos" | "darwin" => Some(format!("fd-v{version}-{arch}-apple-darwin.tar.gz")),
        "linux" => Some(format!("fd-v{version}-{arch}-unknown-linux-musl.tar.gz")),
        _ => None,
    }
}

/// `getLatestVersion`'s reading of the release page redirect: the tag of `/releases/tag/<tag>`, without a `v`.
pub fn latest_version_from_redirect(status: u16, location: Option<&str>) -> Result<String, String> {
    let location = location
        .filter(|_| (300..400).contains(&status))
        .ok_or_else(|| format!("Failed to resolve latest {FD_REPO} release: HTTP {status} without redirect"))?;
    let base = url::Url::parse(GITHUB).map_err(|e| e.to_string())?;
    let tag = base
        .join(location)
        .ok()
        .and_then(|u| u.path().rsplit('/').next().map(str::to_string))
        .filter(|t| !t.is_empty() && location.contains("/releases/tag/"))
        .ok_or_else(|| format!("Failed to resolve latest {FD_REPO} release: unexpected redirect to {location}"))?;
    let tag = url::form_urlencoded::parse(format!("t={}", tag.replace('+', "%2B")).as_bytes())
        .next()
        .map_or(tag, |(_, v)| v.into_owned());
    Ok(tag.strip_prefix('v').unwrap_or(&tag).to_string())
}

async fn fetch_with_retry(request: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
    let mut attempt = 0;
    loop {
        match request().send().await {
            Ok(response) if RETRYABLE_STATUS.contains(&response.status().as_u16()) && attempt < MAX_RETRIES => {}
            Ok(response) => return Ok(response),
            Err(_) if attempt < MAX_RETRIES => {}
            Err(error) => return Err(error.to_string()),
        }
        attempt += 1;
    }
}

/// `downloadTool("fd")`: resolve the latest version, download its archive into `<agentDir>/bin`, extract it with
/// `tar` (from the run's PATH) into a private directory, move the binary to `<agentDir>/bin/fd` (mode 755) and
/// remove the archive and the directory.
pub fn download_fd(env: &Env, host: &str) -> Result<PathBuf, String> {
    let host = host.trim_end_matches('/');
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let version = runtime.block_on(async {
        let latest = format!("{host}/{FD_REPO}/releases/latest");
        let response = tokio::time::timeout(
            NETWORK_TIMEOUT,
            fetch_with_retry(|| client.get(&latest).header("User-Agent", USER_AGENT)),
        )
        .await
        .map_err(|_| "The operation was aborted due to timeout".to_string())??;
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        latest_version_from_redirect(response.status().as_u16(), location.as_deref())
    })?;
    let asset = fd_asset_name(&version, std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        format!(
            "Unsupported platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let bin = env.agent_dir().join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let url = format!("{host}/{FD_REPO}/releases/download/v{version}/{asset}");
    let archive = bin.join(&asset);
    let binary = bin.join("fd");
    runtime.block_on(async {
        let response = tokio::time::timeout(DOWNLOAD_TIMEOUT, async {
            let response = fetch_with_retry(|| client.get(&url)).await?;
            if !response.status().is_success() {
                return Err(format!(
                    "Download failed with HTTP {}: {url}",
                    response.status().as_u16()
                ));
            }
            response.bytes().await.map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "The operation was aborted due to timeout".to_string())??;
        std::fs::write(&archive, &response).map_err(|e| e.to_string())
    })?;
    let unique = format!(
        "extract_tmp_fd_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let extract = bin.join(unique);
    let result = (|| {
        std::fs::create_dir_all(&extract).map_err(|e| e.to_string())?;
        let tar = (env.var)("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join("tar"))
                    .find(|p| executable(p))
            })
            .ok_or_else(|| format!("Failed to extract {asset}: tar: spawn tar ENOENT"))?;
        let output = std::process::Command::new(tar)
            .arg("xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&extract)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("Failed to extract {asset}: tar: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(format!("Failed to extract {asset}: tar: {stderr}"));
        }
        let nested = extract.join(asset.trim_end_matches(".tar.gz")).join("fd");
        let found = [nested, extract.join("fd")]
            .into_iter()
            .find(|p| p.exists())
            .or_else(|| find_recursively(&extract, "fd"))
            .ok_or_else(|| format!("Binary not found in archive: expected fd under {}", extract.display()))?;
        std::fs::rename(found, &binary).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&extract);
    result.map(|()| binary)
}

/// `findBinaryRecursively`: a file named `name` anywhere under `root`.
fn find_recursively(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            let kind = entry.file_type().ok()?;
            if kind.is_file() && entry.file_name() == name {
                return Some(path);
            }
            if kind.is_dir() {
                stack.push(path);
            }
        }
    }
    None
}
