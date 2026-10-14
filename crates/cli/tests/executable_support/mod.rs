//! Writing a script that a test then executes by path (a fake `claude`, `gh` or linter on PATH) without "Text file
//! busy" (ETXTBSY) under parallel tests. Shared by the cli, review and loki test binaries.
//!
//! Problem: exec of a file fails with ETXTBSY while any process holds it open for writing. Test threads share one
//! process, and every `Command::spawn` forks it. A thread that writes a script with `std::fs::write` holds a write
//! descriptor; a fork in another thread at that moment copies it, and the copy lives until that child's own exec
//! (O_CLOEXEC only closes it there). Exec'ing the script in that window fails:
//!
//!   thread A: open("bin/claude", O_WRONLY|O_CLOEXEC) = 5; write(5, ...)
//!   thread B: clone()   -> child B' holds fd 5 until its execve
//!   thread A: close(5); chmod 755; spawn ultron -> execve("bin/claude") = ETXTBSY (B' still has it open)
//!
//! Writing to a temporary name and renaming does not help: the copied descriptor refers to the same inode.
//!
//! Fix: this process never opens the file for writing. A child `sh` creates it from stdin and exits before
//! [`install`] returns, so the only write descriptor ever lives in that child (which forks nothing while holding
//! it) and is closed by the time anything executes the file. Our forks can only inherit the pipe to `sh`, which
//! delays its end-of-input by at most one fork-to-exec window and never touches the file.

#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Create (or replace, by rename, so a copy still running is untouched) the executable `path` (mode 0755) holding
/// `text`; it is complete and has no writer when this returns.
pub fn install(path: &Path, text: &str) {
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            r#"cat > "$1.part" && chmod 755 "$1.part" && mv -f "$1.part" "$1""#,
            "sh",
        ])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("sh starts");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(text.as_bytes()).expect("the script reaches sh");
    drop(stdin);
    let status = child.wait().expect("sh ends");
    assert!(status.success(), "writing {} failed: {status}", path.display());
}
