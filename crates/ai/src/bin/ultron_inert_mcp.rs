//! `ultron-inert-mcp <manifest>`: the inert MCP inventory server of the `claude-subscription` provider, standalone
//! for this crate's tests (the product runs it as `ultron __internal inert-mcp`).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = ai::claude_subscription::inert_mcp::run(&args) {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
