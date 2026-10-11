//! System clipboard off the paint path: an [`arboard`] backend (text) with `wl-paste`/`xclip` for image reads,
//! driven by a [`ClipboardService`] thread. Copies are fire-and-forget; reads answer on a channel.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// Clipboard access. Implementations may block (X11/Wayland round trips); only the service thread calls them.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: &str) -> Result<(), String>;
    /// PNG bytes of a clipboard image.
    fn get_image_png(&mut self) -> Result<Vec<u8>, String>;
}

/// The desktop clipboard through arboard; images through `wl-paste` or `xclip`.
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self {
            inner: arboard::Clipboard::new().ok(),
        }
    }

    fn board(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.inner.is_none() {
            self.inner = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        self.inner.as_mut().ok_or_else(|| "clipboard unavailable".to_string())
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a clipboard tool; its stdout when it succeeded with output.
fn tool_output(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        self.board()?.get_text().map_err(|e| e.to_string())
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.board()?.set_text(text.to_string()).map_err(|e| e.to_string())
    }

    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        tool_output("wl-paste", &["--no-newline", "--type", "image/png"])
            .or_else(|| tool_output("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]))
            .filter(|b| b.starts_with(b"\x89PNG"))
            .ok_or_else(|| "no image on the clipboard".to_string())
    }
}

/// What a read produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardReply {
    Text(String),
    /// Base64 PNG.
    Image(String),
    Error(String),
}

/// Which read to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    Text,
    Image,
    /// An image when the clipboard holds one, else text.
    ImageOrText,
}

enum Job {
    Copy(String),
    Read(ClipboardRead),
    Stop,
}

/// A thread owning the clipboard: `copy` and `request` return immediately.
pub struct ClipboardService {
    tx: Option<Sender<Job>>,
    replies: Receiver<ClipboardReply>,
    worker: Option<JoinHandle<()>>,
}

impl ClipboardService {
    pub fn spawn<C: Clipboard + 'static>(mut clipboard: C) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply_tx, replies) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                for job in rx {
                    let reply = match job {
                        Job::Stop => break,
                        Job::Copy(text) => {
                            let _ = clipboard.set_text(&text);
                            continue;
                        }
                        Job::Read(ClipboardRead::Text) => clipboard.get_text().map(ClipboardReply::Text),
                        Job::Read(ClipboardRead::Image) => {
                            clipboard.get_image_png().map(|b| ClipboardReply::Image(B64.encode(b)))
                        }
                        Job::Read(ClipboardRead::ImageOrText) => clipboard
                            .get_image_png()
                            .map(|b| ClipboardReply::Image(B64.encode(b)))
                            .or_else(|_| clipboard.get_text().map(ClipboardReply::Text)),
                    };
                    if reply_tx.send(reply.unwrap_or_else(ClipboardReply::Error)).is_err() {
                        break;
                    }
                }
            })
            .ok();
        Self {
            tx: Some(tx),
            replies,
            worker,
        }
    }

    /// The desktop clipboard.
    pub fn system() -> Self {
        Self::spawn(SystemClipboard::new())
    }

    /// Copy `text` (Vim yanks, copy commands). Never blocks.
    pub fn copy(&self, text: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Copy(text.to_string()));
        }
    }

    /// Start a read; poll [`ClipboardService::try_reply`] from the event loop.
    pub fn request(&self, read: ClipboardRead) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Read(read));
        }
    }

    /// The next finished read, if any. Never blocks.
    pub fn try_reply(&self) -> Option<ClipboardReply> {
        self.replies.try_recv().ok()
    }

    /// The receiver, for event loops that select on several channels.
    pub fn replies(&self) -> &Receiver<ClipboardReply> {
        &self.replies
    }

    /// A closure for `VimEditor::on_yank`.
    pub fn yank_sink(&self) -> Box<dyn FnMut(&str)> {
        let tx = self.tx.clone();
        Box::new(move |text: &str| {
            if let Some(tx) = &tx {
                let _ = tx.send(Job::Copy(text.to_string()));
            }
        })
    }

    /// Finish queued jobs and stop (yank sinks handed out earlier become no-ops).
    pub fn shutdown(mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Job::Stop);
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for ClipboardService {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Job::Stop);
        }
    }
}
