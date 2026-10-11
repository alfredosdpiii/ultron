//! Lost-server messages (experimental/lost-server.ts): a transport-level failure means the server or the session
//! worker went away; the session file is durable, so the message says how to resume.

pub const RESUME_HINT: &str = "your session is saved — run `ultron -c` to resume";

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        boundary(before) && boundary(after)
    })
}

/// `isLostServerError` on an error's message. Rust's own I/O wording for a reset, broken or refused socket counts
/// like Node's `ECONNRESET`/`EPIPE`/`ECONNREFUSED`.
pub fn is_lost(message: &str) -> bool {
    let lower = message.to_lowercase();
    let worker = lower
        .find("session worker ")
        .map(|i| &lower[i + "session worker ".len()..])
        .is_some_and(|rest| {
            ["disconnected unexpectedly", "exited unexpectedly"]
                .iter()
                .any(|tail| rest.contains(tail))
        });
    worker
        || [
            "binding is closed",
            "byte transport closed",
            "unix connection is closed",
            "session worker disconnected during",
            "lost its coordinator",
            "experimental server was replaced",
            "experimental server is shutting down",
            "experimental server detached",
            "broken pipe",
            "connection reset by peer",
            "connection refused",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
        || ["ECONNRESET", "EPIPE", "ECONNREFUSED"]
            .iter()
            .any(|w| contains_word(message, w))
}

/// `lostServerMessage`.
pub fn lost_server_message(reason: Option<&str>) -> String {
    let detail = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map_or(String::new(), |r| format!(" ({r})"));
    format!("The Ultron server stopped{detail}; {RESUME_HINT}")
}

/// `describeLostServer`: the clear message for a lost server or worker; `None` for any other error.
pub fn describe(message: &str) -> Option<String> {
    if !is_lost(message) {
        return None;
    }
    Some(if message.contains(RESUME_HINT) {
        message.to_string()
    } else {
        lost_server_message(Some(message))
    })
}

/// `friendlyStatus`: a status line's `Error: …` text, made clear when it reports a lost server.
pub fn friendly_status(text: &str) -> String {
    if text.contains(RESUME_HINT) {
        return text.to_string();
    }
    for prefix in ["Error: ", "Reconnect error: "] {
        if let Some(rest) = text.strip_prefix(prefix)
            && is_lost(rest)
        {
            return format!("Error: {}", lost_server_message(Some(rest)));
        }
    }
    text.to_string()
}
