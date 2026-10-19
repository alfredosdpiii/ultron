//! `ultron trace check <file>`: read a request trace (`ULTRON_REQUEST_TRACE`, session_core::request_trace) and
//! report every request whose context was not an append of its lane's previous one (a prompt-cache reset), with its
//! reason. Exit 0 when every request appended, 1 when any reset, 2 for a usage error or an unreadable trace.

use session_core::request_trace::check;
use std::io::Write;

const USAGE: &str = "Usage: ultron trace check <file>";

pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let io = |result: std::io::Result<()>| result.map_or(2, |()| 0);
    let file = match args {
        [command, file] if command == "check" => file,
        _ => return io(writeln!(err, "{USAGE}")).max(2),
    };
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) => return io(writeln!(err, "Error: {file}: {error}")).max(2),
    };
    let report = match check(&text) {
        Ok(report) => report,
        Err(error) => return io(writeln!(err, "Error: {file}: {error}")).max(2),
    };
    for reset in &report.resets {
        if io(writeln!(
            out,
            "{file}:{}: session {} lane {} request {}: reset ({}) at message {}",
            reset.line, reset.session, reset.lane, reset.request, reset.reason, reset.at
        )) != 0
        {
            return 2;
        }
    }
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if io(writeln!(
        out,
        "{} in {}, {}",
        plural(report.requests, "request"),
        plural(report.lanes, "lane"),
        plural(report.resets.len(), "reset")
    )) != 0
    {
        return 2;
    }
    i32::from(!report.resets.is_empty())
}
