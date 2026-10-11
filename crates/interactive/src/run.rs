//! The interactive TUI's event loop: one thread owns the [`App`] and the terminal. Key input (read on its own
//! thread) and lane events arrive on one channel; each batch of events is applied and then one frame is painted on
//! the alternate screen. Nothing here waits on the network or the disk.

use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

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
}

/// Spinner frame interval (`Loader`).
const ANIMATION: Duration = Duration::from_millis(80);
/// How often an idle loop looks at the terminal size.
const IDLE: Duration = Duration::from_millis(100);
/// The editor's debounce for `@` and trigger-character completion.
const DEBOUNCE: Duration = Duration::from_millis(tui::editor::AUTOCOMPLETE_DEBOUNCE_MS);

/// How long exit waits for the writer thread to drain.
const EXIT_FLUSH: Duration = Duration::from_secs(1);

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
    match tty {
        None => paint_loop(app, events, &mut |data| terminal.write(data), size, None)?,
        Some(tty) => {
            let result = paint_loop(
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
    let split = app.qol().viewport_split;
    let gated = app.qol().render_gate;
    let mut screen = AltScreen::new();
    let mut viewport = ViewportRenderer::new();
    let mut gate = RenderGate::new();
    if !split {
        out(&screen.enter())?;
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
        match events.recv_timeout(wait) {
            Ok(event) => {
                handle(app, event);
                while let Ok(event) = events.try_recv() {
                    handle(app, event);
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
    if split {
        for write in app.terminal_writes.drain(..) {
            out(&write)?;
        }
        let plan = app.exit_plan(width);
        out(&viewport.render(&plan, width, last_size.1 as usize).bytes)?;
        return out("\r\n\x1b[?25h");
    }
    let document = app.exit_document(width);
    out(&screen.exit(&document, width))
}

fn handle(app: &mut App, event: UiEvent) {
    match event {
        UiEvent::Input(data) => app.handle_input(&data),
        UiEvent::Lane(event) => app.apply(event),
        UiEvent::InputClosed => app.exit = true,
    }
}
