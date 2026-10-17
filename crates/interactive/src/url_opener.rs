//! `openBrowser` (packages/coding-agent/src/utils/open-browser.ts) for the TUI: a clicked hyperlink in the
//! fullscreen transcript (`openUrl` of `TuiAltScreen`, tui-renderer.ts) and a link chosen in the copy picker open
//! with the platform's launcher. The launcher is looked up only on the PATH the session was started with (never the
//! process's), and runs detached with only that PATH and HOME in its environment; a missing launcher opens nothing,
//! and a launcher that fails is ignored (the link stays on screen to open by hand).

use std::sync::Arc;

use tui::fullscreen::UrlOpener;

/// The variables a launcher needs to reach the desktop; only these pass to it besides PATH and HOME.
pub const DESKTOP_VARS: [&str; 8] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "BROWSER",
];

/// `openBrowser`'s launcher and arguments for `platform` (Node's `process.platform`): `open` on macOS, `rundll32
/// url.dll,FileProtocolHandler` on Windows, else `xdg-open`.
pub fn browser_command(platform: &str, target: &str) -> (String, Vec<String>) {
    match platform {
        "darwin" => ("open".into(), vec![target.into()]),
        "win32" => (
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), target.into()],
        ),
        _ => ("xdg-open".into(), vec![target.into()]),
    }
}

/// Node's `process.platform` for this build.
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// `name` on `path` (the session's PATH; `None` finds nothing): the first directory holding a file of that name.
pub fn find_launcher(path: Option<&str>, name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The launcher name of this platform (`xdg-open`, `open` on macOS, `rundll32` on Windows).
pub fn launcher_name() -> String {
    browser_command(node_platform(), "").0
}

/// The opener whose launcher comes from `path` (the session's PATH) and runs with only `path` and `home`.
pub fn url_opener(path: Option<String>, home: Option<String>) -> UrlOpener {
    url_opener_with(path, home, Vec::new())
}

/// [`url_opener`] that also passes `desktop` (the session's DISPLAY, WAYLAND_DISPLAY, session bus and the like)
/// to the launcher, so xdg-open can reach the desktop.
pub fn url_opener_with(path: Option<String>, home: Option<String>, desktop: Vec<(String, String)>) -> UrlOpener {
    Arc::new(move |url: &str| {
        let (name, args) = browser_command(node_platform(), url);
        let Some(launcher) = find_launcher(path.as_deref(), &name) else {
            return;
        };
        let mut command = std::process::Command::new(launcher);
        command
            .args(&args)
            .env_clear()
            .env("PATH", path.as_deref().unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(home) = &home {
            command.env("HOME", home);
        }
        command.envs(desktop.iter().map(|(name, value)| (name, value)));
        // `detached: true`: its own process group, reaped in the background.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    })
}
