//! A deterministic proxy for the ignored startup benchmark (`cli_interactive_first_frame_beats_typescript_startup`
//! in interactive.rs, wall clock against the TypeScript time, so never part of the gate): the first frame does not
//! wait for the slow things startup also sets going. The provider endpoint accepts connections and never answers,
//! and the Python interpreter of the RLM kernel blocks until released; the header must still be drawn. A paint path
//! that awaited either would never draw it (the test then fails at its 120 s hang guard), whatever the machine load.

mod executable_support;
mod tui_harness;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tui_harness::Tui;

/// A provider endpoint that accepts every connection and never answers; counts them.
fn black_hole() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            count.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    (port, accepted)
}

#[test]
fn cli_first_frame_waits_for_neither_the_network_nor_the_kernel() {
    let (port, _accepted) = black_hole();
    let gate = tempfile::tempdir().unwrap();
    let (go, started) = (gate.path().join("go"), gate.path().join("started"));
    let real = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    // The kernel's interpreter: records that it was asked for, then waits for `go` before it becomes Python.
    let python = gate.path().join("python");
    executable_support::install(
        &python,
        &format!(
            "#!/bin/sh\n: > '{started}'\nwhile [ ! -e '{go}' ]; do sleep 0.05; done\nexec '{real}' \"$@\"\n",
            started = started.display(),
            go = go.display(),
        ),
    );
    let tui = Tui::start(
        port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[("ULTRON_PYTHON", &python.display().to_string())],
    );
    let screen = tui.wait("ultron v");
    assert!(screen.contains("ultron v"), "{screen}");
    // Released only now, so a first frame that waited for the kernel could not have been drawn above.
    std::fs::write(&go, "").unwrap();
    println!(
        "first frame drawn; the kernel's interpreter {} by then",
        if started.exists() {
            "had been asked for"
        } else {
            "had not been asked for (it starts lazily)"
        }
    );
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}
