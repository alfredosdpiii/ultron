//! `acceptance-gate lock --write|--check` and `acceptance-gate mutate` for the Rust build (A26).
//!
//! Usage:
//!   acceptance-gate lock --write|--check [--root <dir>] [--profile rust|node]
//!   acceptance-gate mutate [--root <dir>] [--profile rust] [--only <id,...>] [--timeout-ms <n>] [--runner <cmd>]
//!
//! `--profile node` (the default for `lock`) writes and checks exactly what `node scripts/acceptance-lock.mjs` does;
//! `--profile rust` (the default for `mutate`) locks crates/acceptance and runs crates/acceptance/mutations.json with
//! `cargo test` (`--runner` names the cargo command, e.g. the machine-lock wrapper; extra words are its arguments).

use acceptance_gate::{MutationOptions, Profile, check_lock, describe_lock_check, run_mutations, write_lock};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "Usage: acceptance-gate lock --write|--check [--root <dir>] [--profile rust|node]\n       acceptance-gate mutate [--root <dir>] [--only <id,...>] [--timeout-ms <n>] [--runner <cmd>]";

fn value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).map(String::as_str)
}

fn run(args: &[String]) -> Result<i32, String> {
    let root = match value(args, "--root") {
        Some(root) => PathBuf::from(root),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let profile = |default: &str| match value(args, "--profile").unwrap_or(default) {
        "node" => Ok(Profile::node()),
        "rust" => Ok(Profile::rust()),
        other => Err(format!("unknown profile {other}")),
    };
    match args.first().map(String::as_str) {
        Some("lock") => {
            let profile = profile("node")?;
            if args.iter().any(|a| a == "--write") {
                let (code, stdout, stderr) = write_lock(&root, &profile)?;
                print!("{stdout}");
                eprint!("{stderr}");
                return Ok(code);
            }
            if args.iter().any(|a| a == "--check") {
                let check = check_lock(&root, &profile)?;
                println!("{}", describe_lock_check(&check));
                return Ok(if check.ok { 0 } else { 1 });
            }
            eprintln!("{USAGE}");
            Ok(2)
        }
        Some("mutate") => {
            let profile = profile("rust")?;
            let timeout_ms = match value(args, "--timeout-ms") {
                Some(ms) => ms.parse::<u64>().map_err(|e| format!("--timeout-ms: {e}"))?,
                None => 15 * 60 * 1000,
            };
            let options = MutationOptions {
                only: value(args, "--only").map(|ids| ids.split(',').map(String::from).collect()),
                timeout: Duration::from_millis(timeout_ms),
                runner: value(args, "--runner")
                    .unwrap_or("cargo")
                    .split_whitespace()
                    .map(String::from)
                    .collect(),
            };
            let (code, log) = run_mutations(&root, &profile, &options)?;
            for line in log {
                println!("{line}");
            }
            Ok(code)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}
