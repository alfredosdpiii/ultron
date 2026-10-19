//! The shell commands run under (utils/shell.ts `getShellConfig` on Unix): the `shellPath` setting when it names an
//! existing file (else the command fails with "Custom shell path not found: <path>"), else `/bin/bash`, else `bash`
//! found on PATH, else `sh`. Every shell runs the command as `<shell> -c <command>`.

use std::path::Path;

/// The shell and the arguments before the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
}

impl ShellConfig {
    fn bash(shell: &str) -> Self {
        ShellConfig {
            shell: shell.to_string(),
            args: vec!["-c".into()],
        }
    }

    /// The argument vector that runs `command`.
    pub fn argv(&self, command: &str) -> Vec<String> {
        let mut argv = vec![self.shell.clone()];
        argv.extend(self.args.iter().cloned());
        argv.push(command.to_string());
        argv
    }
}

/// `getShellConfig(customShellPath)`; `exists` checks a path, `which` finds an executable on PATH.
pub fn shell_config(
    custom: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
    which: &dyn Fn(&str) -> Option<String>,
) -> Result<ShellConfig, String> {
    if let Some(custom) = custom.filter(|c| !c.is_empty()) {
        if exists(Path::new(custom)) {
            return Ok(ShellConfig::bash(custom));
        }
        return Err(format!("Custom shell path not found: {custom}"));
    }
    if exists(Path::new("/bin/bash")) {
        return Ok(ShellConfig::bash("/bin/bash"));
    }
    if let Some(bash) = which("bash") {
        return Ok(ShellConfig::bash(&bash));
    }
    Ok(ShellConfig {
        shell: "sh".into(),
        args: vec!["-c".into()],
    })
}

/// [`shell_config`] on this machine: the file system and this process's PATH.
pub fn system_shell_config(custom: Option<&str>) -> Result<ShellConfig, String> {
    let path = std::env::var("PATH").ok();
    shell_config_on_path(custom, path.as_deref())
}

/// [`shell_config`] on this machine's file system with `bash` looked up on `path` (a session's PATH).
pub fn shell_config_on_path(custom: Option<&str>, path: Option<&str>) -> Result<ShellConfig, String> {
    let which = |name: &str| {
        std::env::split_paths(path?)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
            .map(|p| p.to_string_lossy().into_owned())
    };
    shell_config(custom, &|p: &Path| p.exists(), &which)
}

/// A user `!` command under the `shellCommandPrefix` setting (session-control-provider.ts `bash`): the prefix, a
/// newline, then the command; an absent or empty prefix leaves the command alone.
pub fn prefixed_command(prefix: Option<&str>, command: &str) -> String {
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    }
}
