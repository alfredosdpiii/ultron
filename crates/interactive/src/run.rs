//! The interactive TUI's event loop: one thread owns the [`App`] and the terminal. Key input (read on its own
//! thread) and lane events arrive on one channel; each batch of events is applied and then one frame is painted on
//! the alternate screen. Nothing here waits on the network or the disk.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use tui::keyboard_protocol::KeyboardProtocol;
use tui::screen::AltScreen;
use tui::terminal::Terminal;
use tui::tty_writer::TtyWriter;
use tui::viewport::{GateDecision, RenderGate, ViewportRenderer};

use crate::app::App;
use crate::lane::LaneEvent;

/// What the UI thread receives.
pub enum UiEvent {
    Input(String),
    Lane(LaneEvent),
    /// Stdin closed.
    InputClosed,
    /// Something finished off the UI thread (an autocomplete answer): poll and paint.
    Wake,
}

/// Spinner frame interval (`Loader`).
const ANIMATION: Duration = Duration::from_millis(80);
/// How often an idle loop looks at the terminal size.
const IDLE: Duration = Duration::from_millis(100);
/// The editor's debounce for `@` and trigger-character completion.
const DEBOUNCE: Duration = Duration::from_millis(tui::editor::AUTOCOMPLETE_DEBOUNCE_MS);

/// How long exit waits for the writer thread to drain.
const EXIT_FLUSH: Duration = Duration::from_secs(1);

/// Stdin that stops reading while `paused` (an external editor owns the terminal): it polls before each read, so
/// no keystroke meant for the editor is taken.
struct GatedStdin {
    paused: Arc<AtomicBool>,
}

impl std::io::Read for GatedStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let stdin = std::io::stdin();
        loop {
            if self.paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => continue,
                Ok(_) => {
                    if self.paused.load(Ordering::SeqCst) {
                        continue;
                    }
                    return std::io::Read::read(&mut stdin.lock(), buf);
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Forward stdin input sequences into `tx`, not reading while `paused`.
pub fn spawn_gated_input(tx: Sender<UiEvent>, paused: Arc<AtomicBool>) {
    let input = tui::terminal::spawn_input_pump(GatedStdin { paused });
    std::thread::spawn(move || {
        for data in input {
            if tx.send(UiEvent::Input(data)).is_err() {
                return;
            }
        }
        let _ = tx.send(UiEvent::InputClosed);
    });
}

/// Run `command` on a temporary `prompt.md` holding `content` with the terminal handed over (`editInExternalEditor`):
/// the edited text without a trailing newline, or `None` when the editor failed.
pub fn edit_externally(command: &str, content: &str) -> Option<String> {
    let dir = std::env::temp_dir().join(format!(
        "pi-editor-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("prompt.md");
    let result = (|| {
        std::fs::write(&path, content).ok()?;
        let mut parts = command.split(' ');
        let editor = parts.next()?;
        let mut out = std::io::stdout();
        let _ = write!(
            out,
            "Launching external editor: {command}\nPi will resume when the editor exits.\n"
        );
        let _ = out.flush();
        let status = std::process::Command::new(editor)
            .args(parts)
            .arg(&path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        Some(text.strip_suffix('\n').unwrap_or(text).to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Pi's Ctrl+Z (`suspendTui`): stop this process with SIGTSTP; the shell's `fg` (SIGCONT) continues it and the call
/// returns. Only the product's run loop installs it ([`run_stdout`]); tests inject their own hook.
fn suspend_process() -> Result<(), String> {
    if cfg!(windows) {
        return Err("Suspend to background is not supported on Windows".into());
    }
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TSTP).map_err(|e| e.to_string())
}

/// Forward stdin input sequences into `tx`.
pub fn spawn_input(tx: Sender<UiEvent>) {
    let input = tui::terminal::spawn_stdin_pump();
    std::thread::spawn(move || {
        for data in input {
            if tx.send(UiEvent::Input(data)).is_err() {
                return;
            }
        }
        let _ = tx.send(UiEvent::InputClosed);
    });
}

/// Run until the app exits. `size` reports the terminal (columns, rows).
pub fn run<W: Write>(
    app: &mut App,
    events: &Receiver<UiEvent>,
    terminal: &mut Terminal<W>,
    size: &dyn Fn() -> (u16, u16),
) -> std::io::Result<()> {
    run_with(app, events, terminal, size, None)
}

/// The product entry point on the process terminal: starts the background services of the enabled additions
/// ([`crate::qol::install_services`]) and paints through the writer thread when `qol.tty_writer` is on.
pub fn run_stdout(app: &mut App, events: &Receiver<UiEvent>, size: &dyn Fn() -> (u16, u16)) -> std::io::Result<()> {
    crate::qol::install_services(app, std::env::var("PATH").ok());
    app.set_suspend_hook(Box::new(suspend_process));
    // Fullscreen owns the mouse (wheel, scrollbar, selection); the tracking mode follows the terminal multiplexer.
    app.set_mouse_tracking(Some(tui::fullscreen::mouse_tracking(&|name| std::env::var(name).ok())));
    app.set_right_click_paste(tui::right_click_paste::right_click_paste_enabled(
        tui::clipboard_copy::platform(),
        &|name| std::env::var(name).ok(),
    ));
    let mut terminal = Terminal::new(std::io::stdout());
    if app.qol().tty_writer {
        let tty = TtyWriter::stdout()?;
        run_with(app, events, &mut terminal, size, Some(&tty))
    } else {
        run(app, events, &mut terminal, size)
    }
}

/// [`run`], with frames written through `tty` (the terminal writer thread) instead of `terminal` when given; raw
/// mode and bracketed paste still go through `terminal`.
pub fn run_with<W: Write>(
    app: &mut App,
    events: &Receiver<UiEvent>,
    terminal: &mut Terminal<W>,
    size: &dyn Fn() -> (u16, u16),
    tty: Option<&TtyWriter>,
) -> std::io::Result<()> {
    terminal.start(true)?;
    terminal.start_keyboard_protocol()?;
    match tty {
        None => {
            let mut write = |data: &str| {
                terminal.out.write_all(data.as_bytes())?;
                terminal.out.flush()
            };
            paint_loop_with(app, events, &mut write, size, None, &mut terminal.keyboard)?
        }
        Some(tty) => {
            let result = paint_loop_with(
                app,
                events,
                &mut |data| {
                    tty.write(data);
                    if tty.dead() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "terminal writer died",
                        ));
                    }
                    Ok(())
                },
                size,
                Some(&|| tty.pending()),
                &mut terminal.keyboard,
            );
            tty.flush_sync(EXIT_FLUSH);
            result?;
        }
    }
    terminal.stop()
}

/// The event and paint loop between terminal start and stop: every byte for the terminal goes through `out`.
/// `pending` reports the writer's backlog (frames are deferred while it is too deep, with `qol.render_gate`).
pub fn paint_loop(
    app: &mut App,
    events: &Receiver<UiEvent>,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    size: &dyn Fn() -> (u16, u16),
    pending: Option<&dyn Fn() -> usize>,
) -> std::io::Result<()> {
    paint_loop_with(app, events, out, size, pending, &mut KeyboardProtocol::new())
}

/// [`paint_loop`] reading the keyboard protocol replies of `keyboard` (queried at terminal start) out of the input:
/// they set the app's Kitty state or the modifyOtherKeys fallback and never reach the components; leaving the
/// terminal for an external editor or a suspend pops the protocol and queries it again on return.
pub fn paint_loop_with(
    app: &mut App,
    events: &Receiver<UiEvent>,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
    size: &dyn Fn() -> (u16, u16),
    pending: Option<&dyn Fn() -> usize>,
    keyboard: &mut KeyboardProtocol,
) -> std::io::Result<()> {
    let split = app.qol().viewport_split;
    let gated = app.qol().render_gate;
    let mut screen = AltScreen::new();
    let mut viewport = ViewportRenderer::new();
    let mut gate = RenderGate::new();
    let mouse = app.mouse_tracking().filter(|_| !split);
    // `beforeTerminalStart` / `beforeTerminalStop` with the mouse captured.
    let enter = |screen: &mut AltScreen, app: &mut App| {
        app.reset_viewport();
        screen.enter_with_mouse(mouse.unwrap_or(""))
    };
    let release = if mouse.is_some() {
        AltScreen::release_mouse()
    } else {
        String::new()
    };
    if !split {
        out(&enter(&mut screen, app))?;
    }
    let mut last_size = (0, 0);
    let mut dirty = true;
    loop {
        let (cols, rows) = size();
        if (cols, rows) != last_size {
            last_size = (cols, rows);
            dirty = true;
        }
        if app.poll_services() {
            dirty = true;
        }
        if app.poll_autocomplete() {
            dirty = true;
        }
        if let Some((command, content)) = app.take_edit_request() {
            // Pi's Ctrl+G: the TUI stops (input paused, alternate screen left, cooked mode) while the editor runs.
            let gate = app.input_gate();
            gate.store(true, Ordering::SeqCst);
            if !release.is_empty() {
                out(&release)?;
            }
            out(leave_bytes(app, split))?;
            leave_keyboard(app, keyboard, out)?;
            let raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
            if raw {
                let _ = crossterm::terminal::disable_raw_mode();
            }
            let edited = edit_externally(&command, &content);
            if raw {
                let _ = crossterm::terminal::enable_raw_mode();
            }
            out("\x1b[?2004h")?;
            out(keyboard.query())?;
            gate.store(false, Ordering::SeqCst);
            app.finish_edit(edited);
            app.reset_display = true;
        }
        if app.take_suspend_request() && app.can_suspend() {
            // Pi's Ctrl+Z: the TUI stops (input paused, alternate screen left, cooked mode), the process suspends,
            // and on continue everything is painted again.
            let gate = app.input_gate();
            gate.store(true, Ordering::SeqCst);
            if !release.is_empty() {
                out(&release)?;
            }
            out(leave_bytes(app, split))?;
            leave_keyboard(app, keyboard, out)?;
            let raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
            if raw {
                let _ = crossterm::terminal::disable_raw_mode();
            }
            app.suspend();
            if raw {
                let _ = crossterm::terminal::enable_raw_mode();
            }
            out("\x1b[?2004h")?;
            out(keyboard.query())?;
            gate.store(false, Ordering::SeqCst);
            app.reset_display = true;
        }
        if app.poll_rlm() {
            dirty = true;
        }
        if let Some(fragment) = keyboard.poll(Instant::now()) {
            // A held reply fragment that never completed is input after all.
            app.handle_input(&fragment);
            dirty = true;
        }
        if !split && app.poll_viewport(Instant::now()) {
            dirty = true;
        }
        if app.take_reset_display() {
            // Alt+L: forget what the terminal shows and paint everything again; the split's Kitty images (in
            // scrollback too) are deleted before the clear, as a full redraw deletes them.
            let images = if split { viewport.delete_images() } else { String::new() };
            out(&format!("{images}\x1b[2J\x1b[3J\x1b[H"))?;
            screen = AltScreen::new();
            viewport = ViewportRenderer::new();
            if !split {
                out(&enter(&mut screen, app))?;
            }
            dirty = true;
        }
        let mut deferred = None;
        if dirty {
            let paint = !gated || {
                gate.request();
                match gate.poll(Instant::now(), pending.map(|p| p())) {
                    GateDecision::Render => true,
                    GateDecision::Wait(d) => {
                        deferred = Some(d);
                        false
                    }
                    GateDecision::Idle => false,
                }
            };
            if paint {
                let started = Instant::now();
                for write in app.terminal_writes.drain(..) {
                    out(&write)?;
                }
                screen.show_hardware_cursor = app.show_hardware_cursor;
                screen.image_protocol = app.image_protocol;
                screen.wezterm = app.wezterm;
                viewport.show_hardware_cursor = app.show_hardware_cursor;
                viewport.clear_on_shrink = app.clear_on_shrink;
                if split {
                    let plan = app.frame_plan(cols as usize, rows as usize);
                    out(&viewport.render(&plan, cols as usize, rows as usize).bytes)?;
                } else {
                    let frame = app.render(cols as usize, rows as usize);
                    out(&screen.paint(&frame, cols as usize, rows as usize))?;
                }
                dirty = false;
                gate.rendered(started.elapsed());
            }
        }
        if app.exit {
            break;
        }
        let mut wait = if app.fire_autocomplete_pending() {
            DEBOUNCE
        } else if app.animating() {
            ANIMATION
        } else {
            IDLE
        };
        if let Some(d) = deferred {
            wait = wait.min(d);
        }
        if let Some(at) = app.viewport_deadline().filter(|_| !split) {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        if let Some(at) = keyboard.flush_deadline() {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        match events.recv_timeout(wait) {
            Ok(event) => {
                handle(app, event, keyboard, out)?;
                while let Ok(event) = events.try_recv() {
                    handle(app, event, keyboard, out)?;
                }
                dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                if app.fire_autocomplete() {
                    dirty = true;
                }
                if app.animating() {
                    app.tick();
                    dirty = true;
                }
            }
            Err(RecvTimeoutError::Disconnected) => app.exit = true,
        }
    }
    let width = last_size.0 as usize;
    app.stop_terminal_notifications();
    if split {
        for write in app.terminal_writes.drain(..) {
            out(&write)?;
        }
        let plan = app.exit_plan(width);
        out(&viewport.render(&plan, width, last_size.1 as usize).bytes)?;
        return out("\r\n\x1b[?25h");
    }
    for write in app.terminal_writes.drain(..) {
        out(&write)?;
    }
    let document = app.exit_document(width);
    if !release.is_empty() {
        out(&release)?;
    }
    out(&screen.exit(&document, width))
}

/// What leaving the TUI for a while (Ctrl+G, Ctrl+Z) writes: bracketed paste and the hidden cursor off, and on the
/// alternate screen its Kitty images deleted (`beforeTerminalStop`) before it is left.
fn leave_bytes(app: &App, split: bool) -> &'static str {
    match (split, app.image_protocol) {
        (true, _) => "\x1b[?2004l\x1b[?25h\r\n",
        (false, Some(tui::image::ImageProtocol::Kitty)) => "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?2004l\x1b[?25h\x1b[?1049l",
        (false, _) => "\x1b[?2004l\x1b[?25h\x1b[?1049l",
    }
}

/// `stop()` while the TUI leaves the terminal: pop the Kitty flags, reset modifyOtherKeys, Kitty matching off.
fn leave_keyboard(
    app: &mut App,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let bytes = keyboard.stop();
    app.set_kitty_protocol_active(false);
    if bytes.is_empty() { Ok(()) } else { out(&bytes) }
}

fn handle(
    app: &mut App,
    event: UiEvent,
    keyboard: &mut KeyboardProtocol,
    out: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    match event {
        UiEvent::Input(data) => {
            let negotiated = keyboard.input(&data, Instant::now());
            if !negotiated.write.is_empty() {
                out(&negotiated.write)?;
            }
            if negotiated.kitty_activated {
                app.set_kitty_protocol_active(true);
            }
            for data in negotiated.forward {
                app.handle_input(&data);
            }
        }
        UiEvent::Lane(event) => app.apply(event),
        UiEvent::InputClosed => app.exit = true,
        UiEvent::Wake => {}
    }
    Ok(())
}
