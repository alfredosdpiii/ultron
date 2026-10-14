//! `ultron autoreview install | uninstall` (`autoreview/service.ts`): a user service that runs `ultron autoreview
//! run` and restarts it. Linux: a systemd user unit; macOS: a launchd agent. The files are written (or removed) and
//! the commands that enable them are printed, never run: enabling is left to the user.

use std::collections::HashMap;
use std::path::Path;

use crate::js::path_join;

pub const SERVICE_NAME: &str = "ultron-autoreview";
pub const LAUNCHD_LABEL: &str = "com.ultron.autoreview";

/// How this Ultron CLI is started again (`SelfCommand` of claude/self.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfCommand {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFile {
    pub path: String,
    pub content: String,
    /// Commands that enable the service (printed, never run).
    pub enable: Vec<String>,
    pub disable: Vec<String>,
}

fn safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
}

/// `shellQuote` of claude/self.ts.
pub fn shell_quote(value: &str) -> String {
    if safe(value) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn systemd_quote(value: &str) -> String {
    if safe(value) {
        value.to_string()
    } else {
        let escaped: String = value
            .chars()
            .flat_map(|c| match c {
                '"' | '\\' => vec!['\\', c],
                other => vec![other],
            })
            .collect();
        format!("\"{escaped}\"")
    }
}

fn xml(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The service file for `platform` (`linux`, `darwin`), or None where neither systemd nor launchd applies. `env`
/// gives PATH (carried into the unit: the service starts without a login shell), ULTRON_CODING_AGENT_DIR and
/// XDG_CONFIG_HOME.
pub fn service_file(
    self_command: &SelfCommand,
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> Option<ServiceFile> {
    let command: Vec<&str> = std::iter::once(self_command.command.as_str())
        .chain(self_command.args.iter().map(String::as_str))
        .chain(["autoreview", "run"])
        .collect();
    let path = env.get("PATH").map_or("/usr/local/bin:/usr/bin:/bin", String::as_str);
    let agent_dir = env.get("ULTRON_CODING_AGENT_DIR").filter(|dir| !dir.is_empty());
    match platform {
        "linux" => {
            let config = env
                .get("XDG_CONFIG_HOME")
                .map(|value| crate::js::trim(value))
                .filter(|value| !value.is_empty())
                .map_or_else(|| path_join(&[home, ".config"]), str::to_string);
            let mut lines = vec![
                "[Unit]".to_string(),
                "Description=Ultron automated pull-request reviewer".into(),
                "After=network-online.target".into(),
                "Wants=network-online.target".into(),
                String::new(),
                "[Service]".into(),
                format!(
                    "ExecStart={}",
                    command
                        .iter()
                        .map(|part| systemd_quote(part))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                format!("Environment={}", systemd_quote(&format!("PATH={path}"))),
            ];
            if let Some(dir) = agent_dir {
                lines.push(format!(
                    "Environment={}",
                    systemd_quote(&format!("ULTRON_CODING_AGENT_DIR={dir}"))
                ));
            }
            lines.extend(
                [
                    "Restart=always",
                    "RestartSec=15",
                    "",
                    "[Install]",
                    "WantedBy=default.target",
                    "",
                ]
                .map(str::to_string),
            );
            Some(ServiceFile {
                path: path_join(&[&config, "systemd", "user", &format!("{SERVICE_NAME}.service")]),
                content: lines.join("\n"),
                enable: vec![
                    "systemctl --user daemon-reload".into(),
                    format!("systemctl --user enable --now {SERVICE_NAME}.service"),
                ],
                disable: vec![format!("systemctl --user disable --now {SERVICE_NAME}.service")],
            })
        }
        "darwin" => {
            let file = path_join(&[home, "Library", "LaunchAgents", &format!("{LAUNCHD_LABEL}.plist")]);
            let logs = path_join(&[home, "Library", "Logs", &format!("{SERVICE_NAME}.log")]);
            let mut variables = vec![("PATH", path.to_string())];
            if let Some(dir) = agent_dir {
                variables.push(("ULTRON_CODING_AGENT_DIR", dir.clone()));
            }
            let mut lines = vec![
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string(),
                "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">".into(),
                "<plist version=\"1.0\">".into(),
                "<dict>".into(),
                format!("\t<key>Label</key><string>{LAUNCHD_LABEL}</string>"),
                "\t<key>ProgramArguments</key>".into(),
                "\t<array>".into(),
            ];
            lines.extend(command.iter().map(|part| format!("\t\t<string>{}</string>", xml(part))));
            lines.push("\t</array>".into());
            lines.push("\t<key>EnvironmentVariables</key>".into());
            lines.push("\t<dict>".into());
            lines.extend(
                variables
                    .iter()
                    .map(|(name, value)| format!("\t\t<key>{name}</key><string>{}</string>", xml(value))),
            );
            lines.extend([
                "\t</dict>".to_string(),
                "\t<key>KeepAlive</key><true/>".into(),
                "\t<key>RunAtLoad</key><true/>".into(),
                format!("\t<key>StandardErrorPath</key><string>{}</string>", xml(&logs)),
                "</dict>".into(),
                "</plist>".into(),
                String::new(),
            ]);
            Some(ServiceFile {
                enable: vec![format!("launchctl load -w {}", shell_quote(&file))],
                disable: vec![format!("launchctl unload -w {}", shell_quote(&file))],
                path: file,
                content: lines.join("\n"),
            })
        }
        _ => None,
    }
}

/// Writes the service file (mode 0644), creating its directory.
pub fn install_service(file: &ServiceFile) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = Path::new(&file.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)
        .map_err(|error| format!("{}: {error}", file.path))?;
    out.write_all(file.content.as_bytes())
        .map_err(|error| format!("{}: {error}", file.path))
}

/// Removes the service file; false when there was none.
pub fn uninstall_service(file: &ServiceFile) -> bool {
    let path = Path::new(&file.path);
    if !path.exists() {
        return false;
    }
    let _ = std::fs::remove_file(path);
    true
}
