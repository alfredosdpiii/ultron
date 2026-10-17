//! `convertToPng` (utils/image-convert.ts) for inline tool images on Kitty, whose graphics protocol takes PNG only:
//! decode (EXIF orientation applied, as `applyExifOrientation`) and encode RGBA PNG, on a converter thread so the
//! paint path never decodes. A result that is not ready yet, or a failed conversion, leaves the image undrawn, as in
//! TypeScript.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

/// `convertToPng`: a PNG stays as it is; another format is decoded (orientation applied) and encoded as PNG;
/// `None` when it cannot be decoded.
pub fn convert_to_png(b64: &str, mime: &str) -> Option<(String, String)> {
    if mime == "image/png" {
        return Some((b64.to_string(), mime.to_string()));
    }
    let bytes = B64.decode(b64).ok()?;
    let mut decoder = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    let mut out = Cursor::new(Vec::new());
    image.to_rgba8().write_to(&mut out, ImageFormat::Png).ok()?;
    Some((B64.encode(out.into_inner()), "image/png".into()))
}

/// A source image: its MIME type and data, hashed.
pub fn image_key(data: &str, mime: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    (mime, data).hash(&mut hasher);
    hasher.finish()
}

type Results = Arc<Mutex<HashMap<u64, Option<String>>>>;

/// Converts images on its own thread; the chat asks with [`PngConverter::get`] while rendering and renders again
/// when [`PngConverter::take_ready`] says a result arrived.
pub struct PngConverter {
    jobs: Sender<(u64, String, String)>,
    results: Results,
    requested: HashSet<u64>,
    ready: Arc<AtomicBool>,
}

impl Default for PngConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl PngConverter {
    pub fn new() -> PngConverter {
        let (jobs, queue) = mpsc::channel::<(u64, String, String)>();
        let results: Results = Arc::default();
        let ready = Arc::new(AtomicBool::new(false));
        let (shared, done) = (Arc::clone(&results), Arc::clone(&ready));
        let _ = std::thread::Builder::new().name("png-convert".into()).spawn(move || {
            while let Ok((key, data, mime)) = queue.recv() {
                let png = convert_to_png(&data, &mime).map(|(png, _)| png);
                shared.lock().unwrap_or_else(|e| e.into_inner()).insert(key, png);
                done.store(true, Ordering::SeqCst);
            }
        });
        PngConverter {
            jobs,
            results,
            requested: HashSet::new(),
            ready,
        }
    }

    /// The PNG data of a converted image: `Some(Some(png))` when converted, `Some(None)` when it failed, `None`
    /// while it is pending (the first call asks for it).
    pub fn get(&mut self, data: &str, mime: &str) -> Option<Option<String>> {
        let key = image_key(data, mime);
        if let Some(result) = self.results.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Some(result.clone());
        }
        if self.requested.insert(key) {
            let _ = self.jobs.send((key, data.to_string(), mime.to_string()));
        }
        None
    }

    /// Whether a conversion finished since the last call.
    pub fn take_ready(&self) -> bool {
        self.ready.swap(false, Ordering::SeqCst)
    }
}
