//! `view_image` in the session (`ultron/rlm/view-image.ts` as session-worker.ts wires it): a cell's
//! `rlm.view_image` requests are answered by the cell's own image collector ([`host::view_image::CellImages`],
//! normalized for the lane's current model and Pi's `images.autoResize` setting), and the `rlm` tool result carries the
//! images as image blocks after its text. The image budget (`ultron/image-budget.ts`, `ULTRON_CONTEXT_IMAGES`) keeps
//! the oldest tool-result images of a request out of it as a note.

use ai::Model;
use base64::Engine;
use host::view_image::{CellImages, ImageModel, ResizeLimits, VIEW_IMAGE_REQUEST, ViewImageOptions};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The model a lane's images go to, as the host's normalizer reads it (`input`, `inputLimits.images.resize`).
pub fn image_model(model: &Model) -> ImageModel {
    let resize = model
        .input_limits
        .as_ref()
        .and_then(|limits| limits["images"]["resize"].as_object().cloned());
    ImageModel {
        provider: model.provider.clone(),
        id: model.id.clone(),
        accepts_images: model.input.iter().any(|i| i == "image"),
        resize: resize.map(|r| ResizeLimits {
            max_width: r.get("maxWidth").and_then(Value::as_u64).map(|n| n as u32),
            max_height: r.get("maxHeight").and_then(Value::as_u64).map(|n| n as u32),
            max_bytes: r.get("maxBytes").and_then(Value::as_f64),
            jpeg_quality: r.get("jpegQuality").and_then(Value::as_u64).map(|n| n as u8),
        }),
    }
}

/// Every lane's images of the cell it runs now, and the model each lane's results go to.
pub struct ViewImages {
    cells: Mutex<HashMap<String, CellImages>>,
    models: Mutex<HashMap<String, Model>>,
    /// The session's model: a lane without a model of its own sends its results there.
    default_model: Mutex<Model>,
    /// Where `images.autoResize` is read (per cell, as TypeScript reads it).
    cwd: PathBuf,
    agent_dir: PathBuf,
}

impl ViewImages {
    pub fn new(model: Model, cwd: PathBuf, agent_dir: PathBuf) -> ViewImages {
        ViewImages {
            cells: Mutex::default(),
            models: Mutex::default(),
            default_model: Mutex::new(model),
            cwd,
            agent_dir,
        }
    }

    /// The model `lane`'s results go to from now on (`None`: the root lane, whose model is the session's).
    pub fn set_model(&self, lane: Option<&str>, model: Model) {
        match lane {
            Some(lane) => {
                lock(&self.models).insert(lane.to_string(), model);
            }
            None => *lock(&self.default_model) = model,
        }
    }

    /// Pi's `images.autoResize` (default on).
    fn auto_resize(&self) -> bool {
        let settings = config::settings::SettingsManager::create(&self.cwd, &self.agent_dir, false);
        settings.settings().get("images").and_then(|i| i.get("autoResize")) != Some(&Value::Bool(false))
    }

    fn options(&self, lane: &str) -> ViewImageOptions {
        let model = lock(&self.models)
            .get(lane)
            .cloned()
            .unwrap_or_else(|| lock(&self.default_model).clone());
        ViewImageOptions {
            auto_resize: self.auto_resize(),
            model: Some(image_model(&model)),
        }
    }

    /// A cell starts on `lane`: its images start empty.
    pub fn begin(&self, lane: &str) {
        let options = self.options(lane);
        lock(&self.cells).insert(lane.to_string(), CellImages::new(options));
    }

    /// One `rlm.view_image` request of `lane`'s running cell.
    pub fn attach(&self, lane: &str, payload: &Map<String, Value>) -> Result<Value, String> {
        if !lock(&self.cells).contains_key(lane) {
            self.begin(lane);
        }
        let mut cells = lock(&self.cells);
        let images = cells.get_mut(lane).expect("begun");
        images.attach(payload)
    }

    /// The images `lane`'s cell attached (its cell ended).
    pub fn take(&self, lane: &str) -> Option<CellImages> {
        lock(&self.cells).remove(lane)
    }
}

/// The tool result content of a cell (`CellImages.content`): the text (with the non-vision note when images are
/// attached), then one image block per image.
pub fn cell_content(images: Option<&CellImages>, text: String) -> Vec<ai::UserContent> {
    let Some(images) = images.filter(|i| !i.images.is_empty()) else {
        return vec![crate::lane::text_block(text)];
    };
    let mut content = vec![crate::lane::text_block(images.text(&text))];
    content.extend(images.images.iter().map(|image| {
        ai::UserContent::Image(ai::ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&image.data),
            mime_type: image.mime_type.to_string(),
        })
    }));
    content
}

/// `rlm.view_image` answered for the lane's running cell; everything else goes on.
pub struct ViewImageLayer {
    pub inner: Arc<dyn HostHandler>,
    pub images: Arc<ViewImages>,
    pub lane: String,
}

impl HostHandler for ViewImageLayer {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind == VIEW_IMAGE_REQUEST {
            return self.images.attach(&self.lane, &payload);
        }
        self.inner.handle(kind, payload, signal)
    }
}

/// `DEFAULT_CONTEXT_IMAGES`.
pub const DEFAULT_CONTEXT_IMAGES: usize = 8;
/// `OFFLOADED_IMAGE_TEXT`.
pub const OFFLOADED_IMAGE_TEXT: &str =
    "[image omitted from context to stay within the image budget; call view_image again to see it]";

/// `contextImageBudget`: `ULTRON_CONTEXT_IMAGES`, default 8; 0 disables the budget.
pub fn context_image_budget(value: Option<&str>) -> usize {
    let raw = value.map(js_trim).unwrap_or_default();
    if raw.is_empty() {
        return DEFAULT_CONTEXT_IMAGES;
    }
    // `Number(raw)`: a whole number, possibly written with a fraction of zero, an exponent, or as a hex, octal or
    // binary literal.
    match js_number(raw) {
        Some(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => n as usize,
        _ => DEFAULT_CONTEXT_IMAGES,
    }
}

/// JavaScript's `String.prototype.trim`: white space, line terminators and the byte order mark.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// JavaScript's `Number(text)` for trimmed, non-empty `text` (`None`: `NaN`): a decimal literal (sign, fraction,
/// exponent, `Infinity`), or an unsigned `0x`, `0o` or `0b` literal.
fn js_number(text: &str) -> Option<f64> {
    let radix = match text.get(..2) {
        Some("0x" | "0X") => 16,
        Some("0o" | "0O") => 8,
        Some("0b" | "0B") => 2,
        _ => 0,
    };
    if radix != 0 {
        let digits = &text[2..];
        if digits.is_empty() {
            return None;
        }
        return digits.chars().try_fold(0f64, |value, c| {
            c.to_digit(radix).map(|d| value * f64::from(radix) + f64::from(d))
        });
    }
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    if unsigned == "Infinity" {
        return Some(if text.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    // Rust also reads `inf` and `nan` spellings, which JavaScript does not: only digits, a point and an exponent.
    if !unsigned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        return None;
    }
    text.parse::<f64>().ok()
}

/// `imagesToDrop`: how many of `total` images (oldest first) to drop under budget `max`.
pub fn images_to_drop(total: usize, max: usize) -> usize {
    if max == 0 || total <= max {
        return 0;
    }
    let step = (max / 2).max(1);
    total.min((total - step) / step * step)
}

/// `withImageBudget`: the messages with the oldest tool-result images replaced by a note; `None` when nothing
/// changes.
pub fn with_image_budget(messages: &[ai::Message], max: usize) -> Option<Vec<ai::Message>> {
    let is_image = |c: &ai::UserContent| matches!(c, ai::UserContent::Image(_));
    let total: usize = messages
        .iter()
        .map(|m| match m {
            ai::Message::ToolResult(result) => result.content.iter().filter(|c| is_image(c)).count(),
            _ => 0,
        })
        .sum();
    let mut drop = images_to_drop(total, max);
    if drop == 0 {
        return None;
    }
    Some(
        messages
            .iter()
            .map(|message| match message {
                ai::Message::ToolResult(result) if drop > 0 && result.content.iter().any(is_image) => {
                    let mut result = result.clone();
                    for part in &mut result.content {
                        if drop > 0 && is_image(part) {
                            drop -= 1;
                            *part = crate::lane::text_block(OFFLOADED_IMAGE_TEXT.to_string());
                        }
                    }
                    ai::Message::ToolResult(result)
                }
                other => other.clone(),
            })
            .collect(),
    )
}

/// The budget applied to a request's messages (`installImageBudget`: off at 0).
pub fn apply_image_budget(messages: &mut Vec<ai::Message>, max: usize) {
    if max == 0 {
        return;
    }
    if let Some(kept) = with_image_budget(messages, max) {
        *messages = kept;
    }
}
