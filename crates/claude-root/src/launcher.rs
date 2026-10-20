//! `ultron claude [options] [claude args...]` (launcher.ts): Claude Code as the root agent of Ultron. It finds
//! `claude`, checks the login with `claude auth status --json` (it never touches credential files), checks the flags
//! it needs, and runs Claude Code with Ultron's REPL as its one MCP server (`--strict-mcp-config --mcp-config`), no
//! built-in tools (`--tools "" --allowedTools mcp__ultron__rlm`), the Claude guide as its system prompt, no user or
//! project Claude Code settings (`--setting-sources ""`), Ultron's hooks (`--settings`) and
//! `--dangerously-skip-permissions` (see [`crate::claude_cli::skip_permissions_args`]). Other arguments go to
//! `claude` unchanged. `--print-config` prints the command instead of running it, with the configs inline.

use crate::claude_cli::{self, ClaudeAuthStatus, DEFAULT_CLAUDE_MODEL, REQUIRED_CLAUDE_FLAGS, resolve_claude_binary};
use crate::control::socket_path_for;
use crate::guide::{
    Audience, CLAUDE_RLM_TOOL, GuideOptions, PromptResources, ResourceFlags, claude_system_prompt,
    describe_prompt_resources,
};
use crate::self_cmd::{SelfCommand, shell_command, shell_quote};
use crate::{Ctx, UsageError};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const HELP: &str = "Usage: ultron claude [options] [claude args...]

Run Claude Code as the root agent of Ultron: its only tool is Ultron's REPL (rlm).

Options (before any claude args, or anywhere before --):
  --print-config         Print the claude command and its config instead of running it
  --keep-mcp             Also load your own Claude Code MCP servers (default: only Ultron's)
  --keep-settings        Load your user/project Claude Code settings and CLAUDE.md (default: none)
  --no-hooks             No Ultron hooks (no automatic memory, events only in rlm results)
  --no-context-files, -nc  Leave AGENTS.md / CLAUDE.md out of the system prompt (included by default, as in ultron)
  --no-skills, -ns       Leave your skills out of the system prompt (listed by default, as in ultron)
  --watch                Open `ultron watch` in a tmux split (inside tmux)
  --frame-model <p/m>    Model of rlm.map/rlm.infer frames (default claude-code/claude-opus-5-5)
  --children <mode>      rlm.spawn subagents: claude (Claude Code processes, default) or ultron
  --child-model <alias>  Claude Code model of claude subagents (default claude-opus-5-5)
  --help                 This help

Claude Code itself runs claude-opus-5-5 unless you pass --model (or set ULTRON_CLAUDE_MODEL).
Everything else is passed to claude (for example -p \"prompt\", --model sonnet, --resume <id>).";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LauncherOptions {
    pub print_config: bool,
    pub keep_mcp: bool,
    pub keep_settings: bool,
    pub watch: bool,
    pub hooks: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
    pub frame_model: Option<String>,
    pub children: Option<String>,
    pub child_model: Option<String>,
    /// Claude Code model of the root when neither `--model` nor ULTRON_CLAUDE_MODEL names one (`claudeCode.model`).
    pub root_model: Option<String>,
    pub claude_args: Vec<String>,
}

/// Split `ultron claude` arguments into Ultron's options and claude's; `None` for `--help`.
pub fn parse_launcher_args(args: &[String]) -> Result<Option<LauncherOptions>, UsageError> {
    let mut options = LauncherOptions {
        hooks: true,
        ..LauncherOptions::default()
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || -> Result<String, UsageError> {
            let next = args
                .get(index + 1)
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))?;
            index += 1;
            Ok(next.clone())
        };
        match arg {
            "--" => {
                options.claude_args.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--help" | "-h" => return Ok(None),
            "--print-config" => options.print_config = true,
            "--keep-mcp" => options.keep_mcp = true,
            "--keep-settings" => options.keep_settings = true,
            "--watch" => options.watch = true,
            "--no-hooks" => options.hooks = false,
            "--no-context-files" | "-nc" => options.no_context_files = true,
            "--no-skills" | "-ns" => options.no_skills = true,
            "--frame-model" => options.frame_model = Some(value()?),
            "--child-model" => options.child_model = Some(value()?),
            "--children" => {
                let mode = value()?;
                if mode != "claude" && mode != "ultron" {
                    return Err(UsageError("--children must be claude or ultron".into()));
                }
                options.children = Some(mode);
            }
            other => options.claude_args.push(other.to_string()),
        }
        index += 1;
    }
    Ok(Some(options))
}

/// Everything `claude` is started with.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeLaunch {
    pub claude: String,
    pub args: Vec<String>,
    /// Environment added to the process's own.
    pub env: Vec<(String, String)>,
    /// Files the args refer to, by path, in the order written.
    pub files: Vec<(PathBuf, String)>,
    pub socket: String,
    pub watch_command: String,
    /// The system prompt's length in UTF-16 code units (JS `length`).
    pub prompt_chars: usize,
}

/// What [`build_claude_launch`] needs.
pub struct LaunchInput<'a> {
    pub claude: &'a str,
    pub flags: &'a BTreeSet<String>,
    pub self_cmd: &'a SelfCommand,
    pub options: &'a LauncherOptions,
    pub cwd: &'a str,
    /// The session's private temp directory (files inline when `None`).
    pub dir: Option<&'a Path>,
    pub socket: &'a str,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub date: String,
    pub platform: &'a str,
    pub resources: &'a PromptResources,
    pub bundled_skills: Option<&'a Path>,
    pub hints: bool,
    pub skip_permissions: Vec<String>,
}

fn stringify(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}

/// `buildClaudeLaunch`.
pub fn build_claude_launch(input: &LaunchInput) -> ClaudeLaunch {
    let options = input.options;
    let self_cmd = input.self_cmd;
    let mut mcp_args: Vec<String> = self_cmd.args.clone();
    mcp_args.extend(["mcp", "--no-instructions", "--socket", input.socket].map(String::from));
    for (flag, value) in [
        ("--frame-model", &options.frame_model),
        ("--children", &options.children),
        ("--child-model", &options.child_model),
    ] {
        if let Some(value) = value {
            mcp_args.extend([flag.to_string(), value.clone()]);
        }
    }
    // Subagents get the same context files and skills as the root.
    if options.no_context_files {
        mcp_args.push("--no-context-files".into());
    }
    if options.no_skills {
        mcp_args.push("--no-skills".into());
    }
    // Claude Code ends async hooks with a print run (`-p`): only an interactive root is woken.
    let print = options.claude_args.iter().any(|a| a == "-p" || a == "--print");
    let wake_hooks = options.hooks && !print;
    if crate::guide::wakes(input.var, wake_hooks) {
        mcp_args.push("--wake".into());
    }
    let mcp_config = stringify(&json!({
        "mcpServers": {"ultron": {"type": "stdio", "command": self_cmd.command, "args": mcp_args}}
    }));
    let hook = |event: &str, timeout: u64| json!({"hooks": [{"type": "command", "command": shell_command(self_cmd, &["hook", event, "--socket", input.socket]), "timeout": timeout}]});
    let mut settings = Map::new();
    settings.insert("permissions".into(), json!({"allow": [CLAUDE_RLM_TOOL]}));
    if options.hooks {
        let mut hooks = json!({
            "SessionStart": [hook("session-start", 30)],
            "UserPromptSubmit": [hook("user-prompt", 60)],
            "Stop": [hook("stop", 30)],
        });
        // After every rlm call, a background waiter wakes the model when work it started ends while Claude Code is
        // idle (crate::hooks::WAIT_EVENTS); a newer waiter replaces the older one. Not under TypeScript's guide.
        if crate::guide::wakes(input.var, wake_hooks) {
            hooks["PostToolUse"] = json!([{"matcher": CLAUDE_RLM_TOOL, "hooks": [{
                "type": "command",
                "command": shell_command(self_cmd, &["hook", crate::hooks::WAIT_EVENTS, "--socket", input.socket]),
                "async": true,
                "asyncRewake": true,
                "timeout": crate::hooks::WAIT_EVENTS_TIMEOUT.as_secs(),
            }]}]);
        }
        // Inside herdr, the events only its agents panel needs (crate::herdr_hook).
        if herdr::detect(input.var, None).is_some() {
            for (command, event) in crate::herdr_hook::HERDR_HOOK_EVENTS {
                hooks[event] = json!([hook(command, 10)]);
            }
        }
        settings.insert("hooks".into(), hooks);
    }
    let settings = stringify(&Value::Object(settings));
    let prompt = claude_system_prompt(
        Audience::Claude,
        &GuideOptions {
            cwd: Some(input.cwd.to_string()),
            platform: Some(input.platform.to_string()),
            date: Some(input.date.clone()),
            allowance: None,
            resources: Some(input.resources),
            bundled_skills: input.bundled_skills,
            wake: crate::guide::wakes(input.var, wake_hooks),
        },
        input.hints,
    );
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut file = |name: &str, content: &str| -> Option<String> {
        let path = input.dir?.join(name);
        files.push((path.clone(), content.to_string()));
        Some(path.display().to_string())
    };
    let mcp_path = file("mcp.json", &mcp_config);
    let settings_path = file("settings.json", &settings);
    let prompt_path = if input.flags.contains("system-prompt-file") {
        file("system-prompt.md", &prompt)
    } else {
        None
    };
    let mut args: Vec<String> = Vec::new();
    if !options.keep_mcp {
        args.push("--strict-mcp-config".into());
    }
    args.extend(["--mcp-config".into(), mcp_path.unwrap_or(mcp_config)]);
    args.extend(["--tools", "", "--allowedTools", CLAUDE_RLM_TOOL].map(String::from));
    match prompt_path {
        Some(path) => args.extend(["--system-prompt-file".into(), path]),
        None => args.extend(["--system-prompt".into(), prompt.clone()]),
    }
    args.extend(["--settings".into(), settings_path.unwrap_or(settings)]);
    if !options.keep_settings {
        args.extend(["--setting-sources".into(), String::new()]);
    }
    if input.flags.contains("dangerously-skip-permissions") {
        args.extend(input.skip_permissions.iter().cloned());
    }
    if !options
        .claude_args
        .iter()
        .any(|a| a == "--model" || a.starts_with("--model="))
    {
        let model = (input.var)("ULTRON_CLAUDE_MODEL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| options.root_model.clone())
            .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into());
        args.extend(["--model".into(), model]);
    }
    args.extend(options.claude_args.iter().cloned());
    let mut env = Vec::new();
    // A cell may wait on subagents for a long time; Claude Code's MCP tool timeout must not cut it off.
    if (input.var)("MCP_TOOL_TIMEOUT").is_none_or(|v| v.is_empty()) {
        env.push(("MCP_TOOL_TIMEOUT".to_string(), (2 * 60 * 60 * 1000).to_string()));
    }
    // Claude Code moves an MCP call still running after 120 s to the background; the model then calls `rlm` again
    // to wait, and every copy later returns the same result. Ultron backgrounds cells itself (cell_done events), so a
    // call blocks until it ends unless the user chose otherwise.
    if (input.var)("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS").is_none_or(|v| v.is_empty()) {
        env.push(("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(), "0".to_string()));
    }
    ClaudeLaunch {
        claude: input.claude.to_string(),
        args,
        env,
        files,
        socket: input.socket.to_string(),
        watch_command: shell_command(self_cmd, &["watch", "--socket", input.socket]),
        prompt_chars: prompt.encode_utf16().count(),
    }
}

/// `requireClaude`: the binary and its flags, or why `ultron claude` cannot use it.
fn require_claude(var: &dyn Fn(&str) -> Option<String>) -> Result<(String, BTreeSet<String>), String> {
    let claude = resolve_claude_binary(var).ok_or(
        "Claude Code (`claude`) is not on PATH. Install it (https://claude.com/claude-code) or set ULTRON_CLAUDE_BIN.",
    )?;
    let flags = claude_cli::claude_flags(&claude);
    let missing: Vec<String> = REQUIRED_CLAUDE_FLAGS
        .iter()
        .filter(|f| !flags.contains(**f))
        .map(|f| format!("--{f}"))
        .collect();
    let version = claude_cli::claude_version(&claude);
    if !missing.is_empty() {
        return Err(format!(
            "{claude}{} lacks {}; update Claude Code (`claude update`).",
            version.map(|v| format!(" {v}")).unwrap_or_default(),
            missing.join(", ")
        ));
    }
    Ok((claude, flags))
}

/// A launch id: `L-<ms base 36>-<8 hex>`.
fn launch_id() -> String {
    format!(
        "L-{}-{}",
        base36(session_core::lane::now_ms() as u64),
        crate::random_hex(4)
    )
}

pub fn base36(mut n: u64) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

/// `ultron claude ...`; returns the exit code (or replaces this process with Claude Code).
pub fn run(
    argv: &[String],
    ctx: &Ctx,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<i32, String> {
    let Some(mut parsed) = parse_launcher_args(argv).map_err(|e| e.0)? else {
        writeln!(out, "{HELP}").map_err(|e| e.to_string())?;
        return Ok(0);
    };
    let var = &*ctx.env.var;
    let agent_dir = ctx.env.agent_dir();
    let claude_settings = crate::settings::ClaudeCodeSettings::load(&ctx.env.cwd, &agent_dir);
    if parsed.root_model.is_none() {
        parsed.root_model = claude_settings.model.clone();
    }
    let (claude, flags) = require_claude(var)?;
    let auth: ClaudeAuthStatus = claude_cli::claude_auth_status(&claude)?;
    if !auth.logged_in && !parsed.print_config {
        return Err("Claude Code is not logged in. Run `claude auth login` (or `claude`, then /login) first.".into());
    }
    let self_cmd = crate::self_cmd::self_command(var)?;
    let cwd = ctx.env.cwd.display().to_string();
    let socket = socket_path_for(&launch_id(), &crate::control::control_dir(var)?);
    let bundled = ctx.bundled_skills();
    let resources = crate::guide::load_prompt_resources(
        &ctx.env,
        bundled.as_deref(),
        ResourceFlags {
            no_context_files: parsed.no_context_files,
            no_skills: parsed.no_skills,
        },
    );
    let skip = claude_cli::skip_permissions_args(var, claude_cli::current_uid(), claude_settings.skip_permissions);
    let dir = if parsed.print_config {
        None
    } else {
        Some(crate::temp_dir("ultron-claude-", var)?)
    };
    let launch = build_claude_launch(&LaunchInput {
        claude: &claude,
        flags: &flags,
        self_cmd: &self_cmd,
        options: &parsed,
        cwd: &cwd,
        dir: dir.as_deref(),
        socket: &socket.display().to_string(),
        var,
        date: crate::guide::utc_date(),
        platform: crate::guide::platform(),
        resources: &resources,
        bundled_skills: bundled.as_deref(),
        hints: crate::guide::hints_enabled(var("ULTRON_HINTS").as_deref()),
        skip_permissions: skip,
    });
    let Some(dir) = dir else {
        let env_prefix: String = launch
            .env
            .iter()
            .map(|(k, v)| format!("{k}={} ", shell_quote(v)))
            .collect();
        let mut system_prompt = Map::new();
        system_prompt.insert("chars".into(), launch.prompt_chars.into());
        if let Value::Object(described) = describe_prompt_resources(&resources) {
            system_prompt.extend(described);
        }
        let command = std::iter::once(claude.as_str())
            .chain(launch.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let printed = json!({
            "claude": claude,
            "auth": {"loggedIn": auth.logged_in, "method": auth.describe()},
            "args": launch.args,
            "env": launch.env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>(),
            "socket": socket.display().to_string(),
            "watch": launch.watch_command,
            "systemPrompt": system_prompt,
            "command": format!("{env_prefix}{command}"),
        });
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&printed).expect("JSON values serialize")
        )
        .map_err(|e| e.to_string())?;
        return Ok(0);
    };
    for (path, content) in &launch.files {
        crate::write_private(path, content).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let interactive = !parsed.claude_args.iter().any(|a| a == "-p" || a == "--print");
    if parsed.watch && var("TMUX").is_some_and(|v| !v.is_empty()) {
        let _ = std::process::Command::new("tmux")
            .args(["split-window", "-h", "-d", &launch.watch_command])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    } else if interactive || parsed.watch {
        writeln!(err, "Ultron RLM view: {}", launch.watch_command).map_err(|e| e.to_string())?;
    }
    let _ = err.flush();
    let _ = out.flush();
    Ok(exec_claude(&launch, &dir, err))
}

/// Replace this process with Claude Code (so Ctrl-C and the exit status are Claude Code's own, as TypeScript gets by
/// ignoring SIGINT while it waits), after starting a detached helper that removes the session's temp directory
/// once this process id is gone. Returns only when Claude Code could not be started.
fn exec_claude(launch: &ClaudeLaunch, dir: &Path, err: &mut dyn std::io::Write) -> i32 {
    use std::os::unix::process::CommandExt;
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; rm -rf -- \"$2\"",
            "ultron-claude-cleanup",
            &pid,
            &dir.display().to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    let error = std::process::Command::new(&launch.claude)
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .exec();
    let _ = std::fs::remove_dir_all(dir);
    let _ = writeln!(err, "ultron claude: {error}");
    1
}
