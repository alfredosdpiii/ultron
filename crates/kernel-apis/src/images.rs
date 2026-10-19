//! `images.generate` / `images.models` (`rlm.images` of images_api.py): image generation through the ported OpenRouter
//! images API ([`ai::images`], TS `api/openrouter-images.ts`). The host makes the requests, saves every image to a
//! file and answers with the paths, so no image bytes reach the cell or the model's context.
//!
//! The model is the request's `model`, else the `images.model` setting, else [`DEFAULT_IMAGE_MODEL`]; a model is
//! `provider/id` or a bare catalog id. `size` is an aspect ratio (`16:9`) or a resolution (`1K`, `2K`, `4K`), sent as
//! OpenRouter's `image_config`. `images.baseUrl` replaces the provider's base URL (a proxy, or a test's fake).

use crate::util::fields;
use crate::{ApiModule, Request};
use ai::http::Transport;
use ai::images::{
    AssistantImages, ImagesContent, ImagesContext, ImagesModel, ImagesOptions, ImagesPayloadHook, ImagesStopReason,
    image_model, image_models, image_providers,
};
use ai::types::TextContent;
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The image model when neither the request nor the `images.model` setting names one.
pub const DEFAULT_IMAGE_MODEL: &str = "openrouter/google/gemini-2.5-flash-image";
/// At most this many images per call.
pub const MAX_IMAGES: u64 = 4;
const RESOLUTIONS: [&str; 3] = ["1K", "2K", "4K"];

/// The API key of an image provider: `Ok(None)` when none is configured.
pub type KeyLookup = Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// What the module needs from the session.
#[derive(Clone)]
pub struct ImagesConfig {
    /// The `images.model` setting.
    pub default_model: Option<String>,
    /// The `images.baseUrl` setting.
    pub base_url: Option<String>,
    /// Where images go without `out` (the session's output directory).
    pub out_dir: PathBuf,
    /// What a relative `out` is relative to (the session's cwd).
    pub cwd: PathBuf,
    pub api_key: KeyLookup,
    pub transport: Arc<dyn Transport>,
}

pub struct ImagesModule {
    config: ImagesConfig,
}

impl ImagesModule {
    pub fn new(config: ImagesConfig) -> Self {
        Self { config }
    }
}

/// `provider/id` of a catalog model.
fn model_name(model: &ImagesModel) -> String {
    format!("{}/{}", model.provider, model.id)
}

/// A catalog model by `provider/id` or by its bare id.
pub fn resolve_model(name: &str) -> Result<&'static ImagesModel, String> {
    let qualified = image_providers().find_map(|provider| {
        name.strip_prefix(provider)
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|id| image_model(provider, id))
    });
    qualified
        .or_else(|| image_providers().find_map(|provider| image_model(provider, name)))
        .ok_or_else(|| format!("Unknown image model: {name} (rlm.images.models() lists them)"))
}

/// OpenRouter's `image_config` for a `size`.
pub fn image_config(size: &str) -> Result<Value, String> {
    if RESOLUTIONS.contains(&size) {
        return Ok(json!({ "image_size": size }));
    }
    let ratio = size.split_once(':').filter(|(w, h)| {
        [w, h]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) && !part.starts_with('0'))
    });
    match ratio {
        Some(_) => Ok(json!({ "aspect_ratio": size })),
        None => Err(format!(
            "Unsupported image size: {size}; use an aspect ratio such as \"16:9\" or one of 1K, 2K, 4K"
        )),
    }
}

/// The file extension of an image MIME type.
pub fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        _ => "img",
    }
}

/// Where the images go: `count` paths for images of `mimes`. Without `out`, or with `out` an existing directory,
/// `image-<k>.<ext>` with the first unused numbers; otherwise `out` itself (one image; its extension added when it has
/// none) or `<stem>-<i>.<ext>` (several).
pub fn output_paths(out: Option<&Path>, default_dir: &Path, mimes: &[&str]) -> Vec<PathBuf> {
    let directory = match out {
        None => Some(default_dir.to_path_buf()),
        Some(path) if path.is_dir() => Some(path.to_path_buf()),
        Some(_) => None,
    };
    if let Some(directory) = directory {
        let mut next = 1;
        return mimes
            .iter()
            .map(|mime| {
                loop {
                    let path = directory.join(format!("image-{next}.{}", extension(mime)));
                    next += 1;
                    if !path.exists() {
                        break path;
                    }
                }
            })
            .collect();
    }
    let out = out.unwrap_or(default_dir);
    let given = out.extension().map(|e| e.to_string_lossy().into_owned());
    if mimes.len() == 1 {
        return vec![match given {
            Some(_) => out.to_path_buf(),
            None => out.with_extension(extension(mimes[0])),
        }];
    }
    let stem = out.with_extension("");
    mimes
        .iter()
        .enumerate()
        .map(|(index, mime)| {
            let ext = given.clone().unwrap_or_else(|| extension(mime).to_string());
            PathBuf::from(format!("{}-{}.{ext}", stem.display(), index + 1))
        })
        .collect()
}

fn text_field<'a>(request: &'a Request<'_>, key: &str) -> Result<Option<&'a str>, String> {
    match request.payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text)),
        Some(_) => Err(format!("images.generate {key} must be a non-empty string")),
    }
}

impl ImagesModule {
    fn models(&self) -> Value {
        Value::Array(
            image_providers()
                .flat_map(image_models)
                .map(|model| json!({ "model": model_name(model), "name": model.name }))
                .collect(),
        )
    }

    fn generate(&self, request: &Request<'_>) -> Result<Value, String> {
        fields(request.payload, &["prompt", "model", "size", "n", "out"])?;
        let prompt = text_field(request, "prompt")?.ok_or("images.generate prompt must be a non-empty string")?;
        let count = match request.payload.get("n") {
            None | Some(Value::Null) => 1,
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=MAX_IMAGES).contains(n))
                .ok_or(format!("images.generate n must be an integer from 1 to {MAX_IMAGES}"))?,
        } as usize;
        let name = text_field(request, "model")?
            .or(self.config.default_model.as_deref())
            .unwrap_or(DEFAULT_IMAGE_MODEL);
        let catalog = resolve_model(name)?;
        let hook: Option<ImagesPayloadHook> = match text_field(request, "size")? {
            Some(size) => {
                let config = image_config(size)?;
                Some(Arc::new(move |params: &Value, _: &ImagesModel| {
                    let mut params = params.clone();
                    params["image_config"] = config.clone();
                    Some(params)
                }))
            }
            None => None,
        };
        let out = text_field(request, "out")?.map(|out| self.config.cwd.join(out));
        let api_key = (self.config.api_key)(&catalog.provider)?.ok_or_else(|| {
            format!(
                "No API key for {} image models: set OPENROUTER_API_KEY or log in with /login",
                catalog.provider
            )
        })?;
        let mut model = catalog.clone();
        if let Some(base_url) = &self.config.base_url {
            model.base_url = base_url.clone();
        }
        let images = self.request_images(&model, prompt, count, api_key, hook, request.signal)?;
        let mimes: Vec<&str> = images.iter().map(|(mime, _)| mime.as_str()).collect();
        let paths = output_paths(out.as_deref(), &self.config.out_dir, &mimes);
        for (path, (_, bytes)) in paths.iter().zip(&images) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("images.generate: {}: {e}", parent.display()))?;
            }
            std::fs::write(path, bytes).map_err(|e| format!("images.generate: {}: {e}", path.display()))?;
        }
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        Ok(json!({ "paths": paths, "model": model_name(&model) }))
    }

    /// One request after another until `count` images came back; the first failure fails the call.
    fn request_images(
        &self,
        model: &ImagesModel,
        prompt: &str,
        count: usize,
        api_key: String,
        hook: Option<ImagesPayloadHook>,
        signal: &kernel::Signal,
    ) -> Result<Vec<(String, Vec<u8>)>, String> {
        let abort = ai::AbortSignal::new();
        let options = ImagesOptions {
            api_key: Some(api_key),
            signal: Some(abort.clone()),
            on_payload: hook,
            ..Default::default()
        };
        let context = ImagesContext {
            input: vec![ImagesContent::Text(TextContent {
                text: prompt.to_string(),
                text_signature: None,
            })],
        };
        // The cell's signal aborts the request (a cancelled or timed-out cell).
        let (cell, done) = (signal.clone(), kernel::Signal::new());
        let finished = done.clone();
        let watcher = std::thread::spawn(move || {
            while !finished.is_aborted() {
                if cell.wait(Duration::from_millis(200)) {
                    abort.abort();
                    return;
                }
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string());
        let result = runtime.and_then(|runtime| {
            let mut images = Vec::new();
            while images.len() < count {
                let answer = runtime.block_on(ai::images::generate_images(
                    model,
                    &context,
                    &options,
                    self.config.transport.as_ref(),
                ))?;
                let decoded = decode(answer)?;
                images.extend(decoded);
            }
            images.truncate(count);
            Ok(images)
        });
        done.abort("done");
        let _ = watcher.join();
        result
    }
}

/// The images of one answer, decoded; an error answer, or one without images, is the call's error.
fn decode(answer: AssistantImages) -> Result<Vec<(String, Vec<u8>)>, String> {
    match answer.stop_reason {
        ImagesStopReason::Aborted => return Err("images.generate was cancelled".into()),
        ImagesStopReason::Error => {
            return Err(format!(
                "Image generation failed: {}",
                answer.error_message.unwrap_or_else(|| "unknown error".into())
            ));
        }
        ImagesStopReason::Stop => {}
    }
    let mut images = Vec::new();
    let mut text = Vec::new();
    for item in answer.output {
        match item {
            ImagesContent::Image(image) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(image.data.as_bytes())
                    .map_err(|e| format!("Image generation failed: the image is not valid base64: {e}"))?;
                images.push((image.mime_type, bytes));
            }
            ImagesContent::Text(part) => text.push(part.text),
        }
    }
    if images.is_empty() {
        let said = text.join(" ");
        return Err(if said.trim().is_empty() {
            format!("Image generation failed: {} returned no image", answer.model)
        } else {
            format!("Image generation failed: {} returned no image: {said}", answer.model)
        });
    }
    Ok(images)
}

impl ApiModule for ImagesModule {
    fn prefixes(&self) -> &[&str] {
        &["images."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "images.generate" => self.generate(request),
            "images.models" => Ok(self.models()),
            other => Err(format!("Unknown images request: {other}")),
        }
    }
}
