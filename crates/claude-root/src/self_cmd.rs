//! How to start this same Ultron CLI again (self.ts): for Claude Code's MCP config, its hooks and subagent servers.

/// A command and its leading arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfCommand {
    pub command: String,
    pub args: Vec<String>,
}

/// `ULTRON_SELF_COMMAND` (a JSON array: command and leading args) overrides it; otherwise this executable.
pub fn self_command(var: &dyn Fn(&str) -> Option<String>) -> Result<SelfCommand, String> {
    if let Some(configured) = var("ULTRON_SELF_COMMAND")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        let parsed: serde_json::Value =
            serde_json::from_str(&configured).map_err(|e| format!("ULTRON_SELF_COMMAND: {e}"))?;
        let parts: Option<Vec<String>> = parsed
            .as_array()
            .filter(|a| !a.is_empty())
            .and_then(|a| a.iter().map(|p| p.as_str().map(str::to_string)).collect());
        let parts = parts.ok_or("ULTRON_SELF_COMMAND must be a JSON array of strings")?;
        return Ok(SelfCommand {
            command: parts[0].clone(),
            args: parts[1..].to_vec(),
        });
    }
    let exe = std::env::current_exe().map_err(|_| "cannot tell how this Ultron CLI was started".to_string())?;
    Ok(SelfCommand {
        command: exe.display().to_string(),
        args: Vec::new(),
    })
}

/// A shell command line for `self` plus `args` (hook commands are run by a shell).
pub fn shell_command(command: &SelfCommand, args: &[&str]) -> String {
    std::iter::once(command.command.as_str())
        .chain(command.args.iter().map(String::as_str))
        .chain(args.iter().copied())
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote `value` unless it is made only of shell-safe characters.
pub fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
