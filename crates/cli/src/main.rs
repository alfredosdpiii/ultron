//! The `ultron` binary: composition root over the `cli` library and the daemon's internal processes
//! (`ultron __internal <role>`: the coordinator, a server generation, a session worker).

use std::path::{Path, PathBuf};

fn main() {
    // Before any thread starts: SIGINT/SIGTERM/SIGHUP are read on a thread of their own (crates/daemon `signals`);
    // without a handler they act as they would have.
    if let Err(error) = daemon::signals::install() {
        eprintln!("Signal handling unavailable: {error}");
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let docs =
        config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"));
    // The claude-subscription provider's inert MCP inventory server (Claude Code spawns it; it executes nothing).
    if let [internal, role, args @ ..] = &argv[..]
        && internal == "__internal"
        && role == "inert-mcp"
    {
        if let Err(message) = cli::inert_mcp::run(args) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    match daemon::process::role(&argv) {
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
        Ok(Some((role, args))) => {
            let result = match role {
                daemon::process::Role::Coordinator => daemon::coordinator::run(args).map_err(|e| e.to_string()),
                daemon::process::Role::Server => daemon::server::run_server_process(args),
                daemon::process::Role::SessionWorker => daemon::worker::run(args, docs),
            };
            if let Err(message) = result {
                eprintln!("{message}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Ok(None) => {}
    }
    let env = cli::Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        home: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
        var: std::sync::Arc::new(|k| std::env::var(k).ok()),
        docs,
    };
    // Unlocked handles: threads of the interactive TUI (its terminal writer) and of RPC mode write too.
    let code = cli::run(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
