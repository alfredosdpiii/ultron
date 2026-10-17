//! System clipboard off the paint path: an [`arboard`] backend (text) with `wl-paste`/`xclip` for image reads,
//! driven by a [`ClipboardService`] thread. Copies are fire-and-forget; reads answer on a channel.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// A clipboard image (`ClipboardImage`): its bytes and base MIME type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

/// Clipboard access. Implementations may block (X11/Wayland round trips); only the service thread calls them.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Result<String, String>;
    fn set_text(&mut self, text: &str) -> Result<(), String>;
    /// PNG bytes of a clipboard image.
    fn get_image_png(&mut self) -> Result<Vec<u8>, String>;
    /// A clipboard image (`readClipboardImage`): PNG, JPEG, WebP or GIF as read, other types converted to PNG.
    /// By default the PNG of [`Clipboard::get_image_png`].
    fn get_image(&mut self) -> Result<ClipboardImage, String> {
        self.get_image_png().map(|bytes| ClipboardImage {
            bytes,
            mime_type: "image/png".to_string(),
        })
    }
    /// A user-visible copy (`copyToClipboard`): the OSC 52 sequence to write when one is due, else why it failed.
    fn copy_checked(&mut self, text: &str) -> Result<Option<String>, String> {
        self.set_text(text).map(|()| None)
    }
}

/// The desktop clipboard through arboard; images through `wl-paste` or `xclip`.
pub struct SystemClipboard {
    inner: Option<arboard::Clipboard>,
    /// The PATH clipboard tools are looked up on (the injected environment's; `None`: no tool runs).
    path: Option<String>,
    /// The environment the copy decisions read (`DISPLAY`, `SSH_CONNECTION`, ...).
    vars: Vec<(String, String)>,
}

impl SystemClipboard {
    /// The process environment's clipboard (tools on its PATH).
    pub fn new() -> Self {
        Self::with_env(std::env::var("PATH").ok(), std::env::vars().collect())
    }

    /// A clipboard whose tools come from `path` and whose decisions read `vars`. The desktop clipboard connection
    /// opens on the first text access (an image read only runs the tools).
    pub fn with_env(path: Option<String>, vars: Vec<(String, String)>) -> Self {
        Self {
            inner: None,
            path,
            vars,
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

/// Run a clipboard tool found on `path`: its stdout when it exited 0, even empty (`runClipboardCommand`).
pub fn tool_run(path: Option<&str>, program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let program = std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())?;
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

impl Clipboard for SystemClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        self.board()?.get_text().map_err(|e| e.to_string())
    }

    fn copy_checked(&mut self, text: &str) -> Result<Option<String>, String> {
        let vars = &self.vars;
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let env = crate::clipboard_copy::CopyEnv {
            platform: crate::clipboard_copy::platform(),
            var: &var,
            proc_version: std::fs::read_to_string("/proc/version").ok(),
        };
        let mut io = crate::clipboard_copy::SystemCopyIo::new(self.path.clone());
        crate::clipboard_copy::copy_to_clipboard(text, &env, &mut io)
    }

    fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.board()?.set_text(text.to_string()).map_err(|e| e.to_string())
    }

    /// A PNG [`Clipboard::get_image`].
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        match self.get_image()? {
            image if image.mime_type == "image/png" => Ok(image.bytes),
            _ => Err("no image on the clipboard".to_string()),
        }
    }

    /// `readClipboardImage` (crate::clipboard_image): `wl-paste` in a Wayland session or WSL, then `xclip`.
    fn get_image(&mut self) -> Result<ClipboardImage, String> {
        let path = self.path.clone();
        let vars = &self.vars;
        let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let proc_version = std::fs::read_to_string("/proc/version").ok();
        let mut run = |program: &str, args: &[&str]| tool_run(path.as_deref(), program, args);
        match crate::clipboard_image::read_clipboard_image(&var, proc_version.as_deref(), &mut run) {
            crate::clipboard_image::ImageRead::Image { bytes, mime_type } => Ok(ClipboardImage { bytes, mime_type }),
            _ => Err("no image on the clipboard".to_string()),
        }
    }
}

/// What a read produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardReply {
    Text(String),
    /// A clipboard image: standard base64 of its bytes and its MIME type (PNG, JPEG, WebP or GIF).
    Image {
        data: String,
        mime_type: String,
    },
    Error(String),
    /// A [`ClipboardService::copy_checked`] finished: the OSC 52 sequence to write when one is due, or why the
    /// copy failed.
    Copied(Result<Option<String>, String>),
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
    CopyChecked(String),
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
                        Job::CopyChecked(text) => Ok(ClipboardReply::Copied(clipboard.copy_checked(&text))),
                        Job::Read(ClipboardRead::Text) => clipboard.get_text().map(ClipboardReply::Text),
                        Job::Read(ClipboardRead::Image) => clipboard.get_image().map(image_reply),
                        Job::Read(ClipboardRead::ImageOrText) => clipboard
                            .get_image()
                            .map(image_reply)
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

    /// A user-visible copy (`/copy`, the copy keys): answered by a [`ClipboardReply::Copied`]. Never blocks.
    pub fn copy_checked(&self, text: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::CopyChecked(text.to_string()));
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

fn image_reply(image: ClipboardImage) -> ClipboardReply {
    ClipboardReply::Image {
        data: B64.encode(image.bytes),
        mime_type: image.mime_type,
    }
}

/// The bytes of a [`ClipboardReply::Image`] (standard base64).
pub fn image_bytes(data: &str) -> Option<Vec<u8>> {
    B64.decode(data).ok()
}
