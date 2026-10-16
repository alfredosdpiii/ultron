//! `@` file completion runs `fd` off the input/paint thread (TS `getSuggestions(..., { signal })` is async): a fake
//! `fd` blocks until the test releases it, and meanwhile typing still edits and renders; a newer request aborts the
//! one in flight (its `fd` is killed, TS `child.kill("SIGKILL")`) and the worker serves requests in order; an answer
//! for text or a cursor that changed meanwhile is dropped (TS `isAutocompleteRequestCurrent`); the answer that is
//! applied is exactly what the synchronous provider gives for the same input.

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use support::latch::HANG_GUARD;
use tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider, Completion,
};
use tui::editor::Editor;
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;

/// What every fake `fd` prints (directories end in `/`, as `fd` prints them).
const LISTING: &str = "alpha.txt\nbeta/\nbeta/alpine/\ndata.md\nzeta.rs\n";

/// A temp dir with two fake `fd`s: `blocking` reports each start on the `started` FIFO and waits for one line on
/// the `go` FIFO (bounded by `read -t`, a hang guard); `instant` prints at once. Both log to `fd.log`.
struct Fake {
    dir: PathBuf,
    started: Receiver<()>,
    go: std::fs::File,
}

impl Fake {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tui-ac-async-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        for fifo in ["started", "go"] {
            let status = std::process::Command::new("mkfifo")
                .arg(dir.join(fifo))
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo {fifo}");
        }
        let script = |name: &str, wait: &str| {
            let path = dir.join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/bash\necho start >> '{log}'\n{wait}echo end >> '{log}'\nprintf '{LISTING}'\n",
                    log = dir.join("fd.log").display(),
                    LISTING = LISTING.replace('\n', "\\n"),
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        };
        // Read-write opens never block on a FIFO, so neither side waits for the other to open it.
        script(
            "blocking",
            &format!(
                "exec 4<>'{started}'\necho x >&4\nexec 3<>'{go}'\nread -t 120 line <&3 || {{ echo timeout >> '{log}'; exit 1; }}\n",
                started = dir.join("started").display(),
                go = dir.join("go").display(),
                log = dir.join("fd.log").display(),
            ),
        );
        script("instant", "");
        let open = |name: &str| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join(name))
                .unwrap()
        };
        let reader = BufReader::new(open("started"));
        let (tx, started) = mpsc::channel();
        std::thread::spawn(move || {
            for line in reader.lines() {
                if line.is_err() || tx.send(()).is_err() {
                    return;
                }
            }
        });
        Fake {
            go: open("go"),
            started,
            dir,
        }
    }

    fn fd(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Wait until a blocking `fd` has started and is held.
    fn started(&self) {
        self.started.recv_timeout(HANG_GUARD).expect("fd never started");
    }

    /// Let one held `fd` finish.
    fn release(&mut self) {
        self.go.write_all(b"go\n").unwrap();
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("fd.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// Answers only through `get_suggestions` (the trait's default `suggestions_job`): the synchronous reference.
struct Sync(CombinedAutocompleteProvider);

impl AutocompleteProvider for Sync {
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        self.0.get_suggestions(lines, line, col, force)
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        self.0.apply_completion(lines, line, col, item, prefix)
    }
}

fn provider(base: &Path, fd: PathBuf) -> CombinedAutocompleteProvider {
    CombinedAutocompleteProvider::new(Vec::new(), base, Some(fd))
}

fn editor(provider: Box<dyn AutocompleteProvider>) -> Editor {
    let keys = KeybindingsManager::new(tui_keybindings(), Vec::new(), KeyContext::default());
    let mut e = Editor::new(keys, KeyContext::default(), Box::new(|s| s.to_string()), 0);
    e.set_autocomplete_provider(provider);
    e
}

/// Wait for wake-ups until the running request is answered; true when its answer changed the editor. An aborted
/// request wakes too (with nothing to apply), so one wake-up is not enough.
fn await_answer(e: &mut Editor, woken: &Receiver<()>) -> bool {
    loop {
        woken.recv_timeout(HANG_GUARD).expect("the waker never ran");
        if e.poll_autocomplete() {
            return true;
        }
        if !e.is_autocomplete_running() {
            return false;
        }
    }
}

/// Type `keys`, firing the debounce after each as the run loop would.
fn type_keys(e: &mut Editor, keys: &[&str]) {
    for k in keys {
        e.handle_input(k);
        e.fire_autocomplete_debounce();
    }
}

#[test]
fn tui_editor_at_completion_runs_fd_off_the_input_thread() {
    let mut fake = Fake::new("typing");
    let mut e = editor(Box::new(provider(&fake.dir, fake.fd("blocking"))));
    let (wake, woken) = mpsc::channel();
    e.set_autocomplete_waker(move || {
        let _ = wake.send(());
    });

    e.handle_input("@");
    assert!(e.has_pending_autocomplete(), "`@` is debounced");
    e.fire_autocomplete_debounce();
    assert!(e.is_autocomplete_running());
    fake.started();
    // The first `fd` is held: typing still edits and paints, with no picker yet.
    e.handle_input("a");
    let frame = e.render(40).join("\n");
    assert!(frame.contains("@a") && !frame.contains("alpha.txt"), "{frame}");
    assert!(!e.is_showing_autocomplete());
    // Scheduling the debounced request for `@a` already aborted the held one (TS `cancelAutocompleteRequest`):
    // its `fd` is killed; firing starts the next.
    assert!(e.has_pending_autocomplete() && !e.is_autocomplete_running());
    e.fire_autocomplete_debounce();
    fake.started();
    assert!(!e.poll_autocomplete(), "nothing answered yet");
    fake.release();
    fake.started();
    fake.release();
    assert!(await_answer(&mut e, &woken));
    assert!(e.is_showing_autocomplete() && !e.is_autocomplete_running());
    // The held `fd` never finished; the two walks of `@a` (depth 1, then recursive) did, in order.
    assert_eq!(fake.log(), ["start", "start", "end", "start", "end"]);

    let mut reference = editor(Box::new(Sync(provider(&fake.dir, fake.fd("instant")))));
    type_keys(&mut reference, &["@", "a"]);
    assert!(reference.is_showing_autocomplete());
    let frame = e.render(40);
    assert_eq!(frame, reference.render(40));
    assert!(frame.iter().any(|l| l.contains("alpha.txt")), "{frame:?}");

    // Accepting works as with the synchronous answer.
    e.handle_input("\t");
    reference.handle_input("\t");
    assert_eq!(e.text(), reference.text());
    assert_eq!(e.text(), "@alpha.txt ");
}

#[test]
fn tui_editor_at_completion_drops_an_answer_for_moved_cursor() {
    let mut fake = Fake::new("stale");
    let mut e = editor(Box::new(provider(&fake.dir, fake.fd("blocking"))));
    let (wake, woken) = mpsc::channel();
    e.set_autocomplete_waker(move || {
        let _ = wake.send(());
    });
    type_keys(&mut e, &["x", " ", "@"]);
    assert!(e.is_autocomplete_running());
    fake.started();
    // Left moves the cursor without a new request: the answer in flight is for another position.
    e.handle_input("\x1b[D");
    assert!(e.is_autocomplete_running());
    fake.release();
    fake.started();
    fake.release();
    assert!(!await_answer(&mut e, &woken));
    assert!(!e.is_showing_autocomplete() && !e.is_autocomplete_running());
    assert_eq!(fake.log(), ["start", "end", "start", "end"]);
    assert_eq!(e.text(), "x @");
}

#[test]
fn tui_editor_at_completion_with_one_match_on_tab_applies_it_off_thread() {
    let mut fake = Fake::new("tab");
    let mut e = editor(Box::new(provider(&fake.dir, fake.fd("blocking"))));
    // Tab forces file completion; `@zet` matches one entry, which is applied without a picker (TS explicit Tab).
    for k in ["@", "z", "e", "t"] {
        e.handle_input(k);
    }
    e.handle_input("\t");
    assert!(e.is_autocomplete_running() && !e.has_pending_autocomplete());
    fake.started();
    fake.release();
    fake.started();
    fake.release();
    assert!(e.wait_autocomplete(HANG_GUARD));
    assert_eq!(e.text(), "@zeta.rs ");
    assert!(!e.is_showing_autocomplete());
    let mut reference = editor(Box::new(Sync(provider(&fake.dir, fake.fd("instant")))));
    for k in ["@", "z", "e", "t", "\t"] {
        reference.handle_input(k);
    }
    assert_eq!(reference.text(), e.text());
}
