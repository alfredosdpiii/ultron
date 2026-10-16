//! The `ultron` binary: composition root over the `cli` library and the daemon's internal processes
//! (`ultron __internal <role>`: the coordinator, a server generation, a session worker).

use std::path::PathBuf;

fn main() {
    // Before any thread starts: SIGINT/SIGTERM/SIGHUP are read on a thread of their own (crates/daemon `signals`);
    // without a handler they act as they would have.
    if let Err(error) = daemon::signals::install() {
        eprintln!("Signal handling unavailable: {error}");
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
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
    // The package files (runtime.py and its modules, skills, docs): embedded, installed where first needed
    // (cli::assets). Here only their location is known; a session worker always needs them.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let source = match cli::assets::source(&|k| std::env::var(k).ok(), &home) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let ready_docs = || match source.ready() {
        Ok(dir) => dir.join("docs"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let env = cli::Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        var: std::sync::Arc::new(|k| std::env::var(k).ok()),
        docs: source.dir().join("docs"),
        home,
    };
    match daemon::process::role(&argv) {
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
        Ok(Some((role, args))) => {
            let result = match role {
                daemon::process::Role::Coordinator => daemon::coordinator::run(args).map_err(|e| e.to_string()),
                // A detached server runs its relay too (opt-out: PI_OFFLINE), with the stored Radius credential.
                daemon::process::Role::Server => {
                    daemon::server::run_server_process(args, Some(cli::radius_relay::relay_config(&env)))
                }
                daemon::process::Role::SessionWorker => daemon::worker::run(args, ready_docs()),
            };
            if let Err(message) = result {
                eprintln!("{message}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Ok(None) => {}
    }
    // Unlocked handles: threads of the interactive TUI (its terminal writer) and of RPC mode write too.
    let code = cli::run(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
