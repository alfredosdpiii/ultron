//! `/review` (packages/coding-agent/src/ultron/review.ts): the typed command becomes a short prompt that asks the
//! model to run Ultron's built-in review in one REPL cell and relay its report. The review itself is Python
//! (`review_api`); nothing about it is in the system prompt of ordinary turns.
//!
//! The worker expands it where every client's text enters the session (agent-controller-provider.ts `toTextPrompt`:
//! `prompt`, `steer`, `followUp` and `nextRun`), so the TUI, queued messages, `ultron -p "/review"` and RPC clients
//! behave the same. Clients send `/review <args>` as typed.

/// JavaScript's `\s` (WhiteSpace and LineTerminator), which is also what `String.prototype.trim` removes. It differs
/// from Rust's `char::is_whitespace`: it includes U+FEFF and excludes U+0085.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(js_space)
}

/// `reviewPrompt`: the model-facing prompt for `/review <args>`.
pub fn review_prompt(args: &str) -> String {
    let trimmed = js_trim(args);
    // A JSON string literal is a valid Python string literal for any argument text.
    let literal = serde_json::to_string(trimmed).unwrap_or_else(|_| "\"\"".into());
    let mut lines = vec![
        if trimmed.is_empty() {
            "/review".to_string()
        } else {
            format!("/review {trimmed}")
        },
        String::new(),
        "Run Ultron's built-in code review in one REPL cell, exactly:".into(),
        String::new(),
        "import review_api".into(),
        format!("review = await review_api.run(rlm, {literal})"),
        "print(review.report)".into(),
        String::new(),
        "Then reply with the printed report as it is. Do not review the code yourself, edit files, or run the review again unless the cell raised an error.".into(),
    ];
    if posts(trimmed) {
        lines.push(String::new());
        lines.push("If the report says a post is pending, ask me whether to post it to the pull request. Post only after I answer yes in a later message, with `await review_api.post(review, confirm=True)`; without that answer, never post.".into());
    }
    lines.join("\n")
}

/// `/(^|\s)--post(\s|=|$)/`.
fn posts(args: &str) -> bool {
    args.match_indices("--post").any(|(at, flag)| {
        let before = args[..at].chars().next_back();
        let after = args[at + flag.len()..].chars().next();
        before.is_none_or(js_space) && after.is_none_or(|c| js_space(c) || c == '=')
    })
}

/// `expandReviewCommand`: `/review [args]` (after trimming; `/^\/review(?:\s+([\s\S]*))?$/`) becomes the review
/// prompt; any other text is returned unchanged.
pub fn expand_review_command(text: &str) -> String {
    let Some(rest) = js_trim(text).strip_prefix("/review") else {
        return text.to_string();
    };
    if rest.is_empty() {
        return review_prompt("");
    }
    if rest.starts_with(js_space) {
        return review_prompt(rest);
    }
    text.to_string()
}

/// A client's operation as the worker takes it: the text of a prompt or a queued message (`steer`, `followUp`,
/// `nextRun`) with `/review` expanded; every other operation unchanged.
pub fn expand_client_op(op: crate::core::Op) -> crate::core::Op {
    use crate::core::Op;
    match op {
        Op::Prompt { message, images, queue } => Op::Prompt {
            message: expand_review_command(&message),
            images,
            queue,
        },
        Op::PromptAndWait { message, images } => Op::PromptAndWait {
            message: expand_review_command(&message),
            images,
        },
        Op::Enqueue {
            kind,
            message,
            images,
            entry_id,
        } => Op::Enqueue {
            kind,
            message: expand_review_command(&message),
            images,
            entry_id,
        },
        other => other,
    }
}
