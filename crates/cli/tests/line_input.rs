//! The MCP sign-in's terminal question (`waitForRedirectUrl`): the pasted redirect URL is read line-edited from the
//! input as it arrives; Esc or Ctrl+C cancels it, and the browser's callback (the cancel token) or the timeout ends it
//! at once. Nothing keeps reading afterwards: input typed after the question ended is still there for the next reader.
//! The input here is a pipe the test writes to; the terminal mode switch is the product's `TerminalInput`.

use cli::line_input::{FdInput, LineRead, read_line};
use cli::mcp_login::redirect_answer;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const HANG_GUARD: Duration = Duration::from_secs(120);

fn read(bytes: &[u8]) -> (LineRead, String) {
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(bytes).unwrap();
    drop(writer);
    let mut echo = Vec::new();
    let result = read_line(&mut FdInput(reader), &mut echo, &|| false);
    (result, String::from_utf8_lossy(&echo).into_owned())
}

#[test]
fn cli_line_input_reads_an_edited_line_and_echoes_it() {
    let (result, echo) = read(b"http://x/cb?codf\x7fe=1\r");
    assert_eq!(result, LineRead::Line("http://x/cb?code=1".into()));
    assert_eq!(echo, "http://x/cb?codf\x08 \x08e=1\r\n");
    // Arrow keys are skipped, a UTF-8 character is erased whole, LF ends the line too.
    let (result, _) = read("a\x1b[D\x1bOBé\x7fb\n".as_bytes());
    assert_eq!(result, LineRead::Line("ab".into()));
}

#[test]
fn cli_line_input_esc_and_ctrl_c_cancel_and_the_end_closes() {
    assert_eq!(read(b"http://half\x1b").0, LineRead::Cancelled);
    assert_eq!(read(b"http://half\x03more\r").0, LineRead::Cancelled);
    assert_eq!(read(b"\x04").0, LineRead::Closed);
    assert_eq!(read(b"partial").0, LineRead::Closed);
}

/// The browser's callback cancels the question while nothing was typed: the read ends, and what is typed afterwards
/// stays in the input for whoever reads next (no reader thread left behind).
#[test]
fn cli_line_input_callback_ends_the_question_without_taking_later_input() {
    let (mut reader, mut writer) = std::io::pipe().unwrap();
    let cancel = mcp::CancelToken::new();
    let callback = cancel.clone();
    let (asked_tx, asked_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let question = std::thread::spawn(move || {
        let mut input = FdInput(reader.try_clone().unwrap());
        let mut echo = Vec::new();
        asked_tx.send(()).unwrap();
        let answer = redirect_answer(&mut input, &mut echo, &cancel, Instant::now() + HANG_GUARD);
        done_tx.send((answer, reader)).unwrap();
    });
    asked_rx.recv().unwrap();
    callback.cancel(None);
    let (answer, reader_back) = done_rx.recv_timeout(HANG_GUARD).expect("the question ended");
    question.join().unwrap();
    assert_eq!(answer, None);
    writer.write_all(b"next command\n").unwrap();
    drop(writer);
    let mut rest = String::new();
    reader = reader_back;
    reader.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "next command\n");
}

#[test]
fn cli_line_input_pasted_url_answers_and_the_deadline_gives_up() {
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"http://127.0.0.1/cb?code=c&state=s\r").unwrap();
    let cancel = mcp::CancelToken::new();
    let answer = redirect_answer(
        &mut FdInput(reader),
        &mut Vec::new(),
        &cancel,
        Instant::now() + HANG_GUARD,
    );
    assert_eq!(answer.as_deref(), Some("http://127.0.0.1/cb?code=c&state=s"));
    // A passed deadline ends the question without input (on a thread, so a question that never ends fails here).
    let (reader, writer) = std::io::pipe().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let asked = cancel.clone();
    std::thread::spawn(move || {
        let answer = redirect_answer(&mut FdInput(reader), &mut Vec::new(), &asked, Instant::now());
        let _ = done_tx.send(answer);
    });
    assert_eq!(
        done_rx
            .recv_timeout(HANG_GUARD)
            .expect("the deadline ended the question"),
        None
    );
    drop(writer);
    // Esc cancels it.
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"\x1b").unwrap();
    let answer = redirect_answer(
        &mut FdInput(reader),
        &mut Vec::new(),
        &cancel,
        Instant::now() + HANG_GUARD,
    );
    assert_eq!(answer, None);
}
