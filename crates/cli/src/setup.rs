//! `ultron setup` (`cli/setup/command.ts`): help, argument checks and the answer without a terminal. The guided
//! wizard itself (provider login, model test, Hindsight install) is an interactive TUI and is not ported.

use crate::Env;
use std::io::IsTerminal;

/// Handle `setup ...`; None when `argv` is not the setup command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("setup") {
        return None;
    }
    let agent_dir = env.agent_dir();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        let _ = writeln!(
            out,
            "Usage:
  ultron setup

A guided setup: checks Node.js and python3, sets up a provider (subscription login, API key or a custom
OpenAI-compatible endpoint) and the default model with a live test, and finds or installs the Hindsight memory
server. Every step can be skipped; run it again at any time.

Files are written to {} (secrets with mode 0600).",
            agent_dir.display()
        );
        return Some(0);
    }
    if let Some(option) = argv.get(1) {
        let _ = writeln!(err, "Unknown option for \"ultron setup\": {option}");
        return Some(1);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let _ = writeln!(
            err,
            "ultron setup is interactive and needs a terminal.\nWithout one, configure {} directly: provider keys in environment variables (e.g. ANTHROPIC_API_KEY), custom endpoints in models.json, and ULTRON_HINDSIGHT_URL.",
            agent_dir.display()
        );
        return Some(1);
    }
    // ponytail: the setup wizard (cli/setup/wizard.ts, tui.ts) is interactive and not ported.
    let _ = writeln!(err, "Error: `ultron setup` is not implemented in the Rust build yet");
    Some(1)
}
