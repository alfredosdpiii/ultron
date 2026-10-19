//! `images.generate` / `images.models` (kernel_apis::images) against a 127.0.0.1 fake of OpenRouter's chat-completions
//! images endpoint, through the real HTTP transport: the request the ported images API sends, the files the module
//! saves and the errors a cell sees.

use base64::Engine;
use kernel::{HostHandler, Signal};
use kernel_apis::images::{DEFAULT_IMAGE_MODEL, extension, image_config, output_paths, resolve_model};
use kernel_apis::{ImagesConfig, ImagesModule, KernelApis};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One recorded request: the path, the headers (lowercase names) and the JSON body.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Replies (status, JSON body) in order; past the end, the last one again.
fn fake(replies: Vec<(u16, Value)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}/api/v1", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        length = v.parse().unwrap();
                    }
                    headers.push((k, v));
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let index = {
                let mut seen = record.lock().unwrap();
                seen.push(Seen {
                    path,
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                seen.len() - 1
            };
            let (status, reply) = replies[index.min(replies.len() - 1)].clone();
            let reply = reply.to_string();
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (base, seen)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// An OpenRouter images answer carrying `images` as (mime, bytes) data URLs, with optional text.
fn answer(images: &[(&str, &[u8])], text: Option<&str>) -> Value {
    let images: Vec<Value> = images
        .iter()
        .map(|(mime, bytes)| json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{}", b64(bytes))}}))
        .collect();
    json!({"id": "gen-1", "choices": [{"message": {"role": "assistant", "content": text, "images": images}}]})
}

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    apis: KernelApis,
}

impl Setup {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let payload: Map<String, Value> = payload.as_object().cloned().unwrap();
        self.apis.handle(kind, payload, &Signal::new())
    }
}

fn setup(base_url: Option<String>, default_model: Option<&str>, key: Option<&str>) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("cwd")).unwrap();
    let key = key.map(str::to_string);
    let module = ImagesModule::new(ImagesConfig {
        default_model: default_model.map(str::to_string),
        base_url,
        out_dir: root.join("out/images"),
        cwd: root.join("cwd"),
        api_key: Arc::new(move |provider: &str| Ok(key.clone().filter(|_| provider == "openrouter"))),
        transport: Arc::new(ai::ReqwestTransport::new(None)),
    });
    let inner = Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| Err(format!("unhandled {kind}")));
    Setup {
        _dir: dir,
        root,
        apis: KernelApis::new(inner).with(Arc::new(module)),
    }
}

#[test]
fn kernel_apis_images_generate_sends_the_openrouter_request_and_saves_the_image() {
    let (base, seen) = fake(vec![(200, answer(&[("image/png", b"PNG-BYTES")], Some("Here it is")))]);
    let s = setup(Some(base), None, Some("or-key"));
    let result = s
        .call("images.generate", json!({"prompt": "a red fox", "size": "16:9"}))
        .unwrap();
    let path = s.root.join("out/images/image-1.png");
    assert_eq!(
        result,
        json!({"paths": [path.display().to_string()], "model": DEFAULT_IMAGE_MODEL})
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"PNG-BYTES");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/api/v1/chat/completions");
    assert_eq!(seen[0].header("authorization"), Some("Bearer or-key"));
    assert_eq!(
        seen[0].body,
        json!({
            "model": "google/gemini-2.5-flash-image",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "a red fox"}]}],
            "stream": false,
            "modalities": ["image", "text"],
            "image_config": {"aspect_ratio": "16:9"},
        })
    );
}

#[test]
fn kernel_apis_images_n_requests_until_enough_images_and_out_names_the_files() {
    let (base, seen) = fake(vec![
        (200, answer(&[("image/jpeg", b"one")], None)),
        (200, answer(&[("image/webp", b"two"), ("image/webp", b"three")], None)),
    ]);
    // The `images.model` setting (an image-only model) and a resolution size; `out` relative to the cwd.
    let s = setup(Some(base), Some("openai/gpt-image-1"), Some("k"));
    let result = s
        .call(
            "images.generate",
            json!({"prompt": "logo", "n": 3, "size": "2K", "out": "art/logo"}),
        )
        .unwrap();
    let cwd = s.root.join("cwd");
    let paths = ["art/logo-1.jpg", "art/logo-2.webp", "art/logo-3.webp"].map(|p| cwd.join(p));
    assert_eq!(
        result,
        json!({"paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(), "model": "openrouter/openai/gpt-image-1"})
    );
    let contents: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(contents, [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "two requests gave three images");
    for request in seen.iter() {
        assert_eq!(request.body["model"], "openai/gpt-image-1");
        assert_eq!(request.body["modalities"], json!(["image"]));
        assert_eq!(request.body["image_config"], json!({"image_size": "2K"}));
    }
    drop(seen);
    // An explicit model (bare catalog id) beats the setting; no size sends no image_config; a second call into the
    // default directory takes the next free numbers.
    let (base, seen) = fake(vec![(200, answer(&[("image/png", b"a")], None))]);
    let s2 = Setup {
        apis: KernelApis::new(Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
            Err(format!("unhandled {kind}"))
        }))
        .with(Arc::new(ImagesModule::new(ImagesConfig {
            default_model: Some("openai/gpt-image-1".into()),
            base_url: Some(base),
            out_dir: s.root.join("out/images"),
            cwd: cwd.clone(),
            api_key: Arc::new(|_: &str| Ok(Some("k".into()))),
            transport: Arc::new(ai::ReqwestTransport::new(None)),
        }))),
        ..s
    };
    std::fs::create_dir_all(s2.root.join("out/images")).unwrap();
    std::fs::write(s2.root.join("out/images/image-1.png"), "taken").unwrap();
    let result = s2
        .call(
            "images.generate",
            json!({"prompt": "x", "model": "black-forest-labs/flux.2-pro"}),
        )
        .unwrap();
    assert_eq!(result["model"], "openrouter/black-forest-labs/flux.2-pro");
    assert_eq!(
        result["paths"],
        json!([s2.root.join("out/images/image-2.png").display().to_string()])
    );
    let body = seen.lock().unwrap()[0].body.clone();
    assert_eq!(body["model"], "black-forest-labs/flux.2-pro");
    assert!(body.get("image_config").is_none(), "{body}");
}

#[test]
fn kernel_apis_images_errors_reach_the_cell_and_nothing_is_written() {
    let (base, _) = fake(vec![(
        400,
        json!({"error": {"message": "Prompt rejected by moderation", "code": 400}}),
    )]);
    let s = setup(Some(base), None, Some("k"));
    let error = s.call("images.generate", json!({"prompt": "x"})).unwrap_err();
    assert!(
        error.starts_with("Image generation failed: ") && error.contains("Prompt rejected by moderation"),
        "{error}"
    );
    assert!(!s.root.join("out").exists());

    let (base, _) = fake(vec![(200, answer(&[], Some("I cannot draw that")))]);
    let s = setup(Some(base), None, Some("k"));
    assert_eq!(
        s.call("images.generate", json!({"prompt": "x"})).unwrap_err(),
        "Image generation failed: google/gemini-2.5-flash-image returned no image: I cannot draw that"
    );
    // n=2 where the second request fails: the first image is not written either.
    let (base, _) = fake(vec![
        (200, answer(&[("image/png", b"a")], None)),
        (200, answer(&[], None)),
    ]);
    let s = setup(Some(base), None, Some("k"));
    assert_eq!(
        s.call("images.generate", json!({"prompt": "x", "n": 2})).unwrap_err(),
        "Image generation failed: google/gemini-2.5-flash-image returned no image"
    );
    assert!(!s.root.join("out").exists());

    // Refused before any request.
    let (base, seen) = fake(vec![(200, answer(&[("image/png", b"a")], None))]);
    let s = setup(Some(base.clone()), None, None);
    assert_eq!(
        s.call("images.generate", json!({"prompt": "x"})).unwrap_err(),
        "No API key for openrouter image models: set OPENROUTER_API_KEY or log in with /login"
    );
    let s = setup(Some(base), None, Some("k"));
    let cases = [
        (json!({}), "images.generate prompt must be a non-empty string"),
        (
            json!({"prompt": "  "}),
            "images.generate prompt must be a non-empty string",
        ),
        (
            json!({"prompt": "x", "n": 0}),
            "images.generate n must be an integer from 1 to 4",
        ),
        (
            json!({"prompt": "x", "n": 5}),
            "images.generate n must be an integer from 1 to 4",
        ),
        (
            json!({"prompt": "x", "n": 1.5}),
            "images.generate n must be an integer from 1 to 4",
        ),
        (
            json!({"prompt": "x", "model": 3}),
            "images.generate model must be a non-empty string",
        ),
        (
            json!({"prompt": "x", "model": "openai/gpt-4o"}),
            "Unknown image model: openai/gpt-4o (rlm.images.models() lists them)",
        ),
        (
            json!({"prompt": "x", "size": "1024x1024"}),
            "Unsupported image size: 1024x1024; use an aspect ratio such as \"16:9\" or one of 1K, 2K, 4K",
        ),
        (
            json!({"prompt": "x", "quality": "hd"}),
            "Unknown payload field: quality",
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            s.call("images.generate", payload.clone()).unwrap_err(),
            expected,
            "{payload}"
        );
    }
    assert_eq!(
        s.call("images.nope", json!({})).unwrap_err(),
        "Unknown images request: images.nope"
    );
    assert!(seen.lock().unwrap().is_empty(), "no request was sent");
}

#[test]
fn kernel_apis_images_models_lists_the_catalog_as_provider_slash_id() {
    let s = setup(None, None, None);
    let models = s.call("images.models", json!({})).unwrap();
    let models = models.as_array().unwrap();
    let catalog: Vec<String> = ai::images::image_models("openrouter")
        .map(|m| format!("openrouter/{}", m.id))
        .collect();
    assert_eq!(
        models
            .iter()
            .map(|m| m["model"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        catalog
    );
    assert!(
        models.contains(&json!({"model": DEFAULT_IMAGE_MODEL, "name": "Google: Nano Banana (Gemini 2.5 Flash Image)"}))
    );
}

#[test]
fn kernel_apis_images_helpers_resolve_sizes_models_and_paths() {
    assert_eq!(
        resolve_model("openrouter/openai/gpt-image-1").unwrap().id,
        "openai/gpt-image-1"
    );
    assert_eq!(resolve_model("openai/gpt-image-1").unwrap().id, "openai/gpt-image-1");
    assert!(resolve_model("openrouter/").is_err());
    assert_eq!(image_config("4K").unwrap(), json!({"image_size": "4K"}));
    assert_eq!(image_config("21:9").unwrap(), json!({"aspect_ratio": "21:9"}));
    for bad in ["4k", "16:", ":9", "0:1", "16:9:1", "a:b", "3K"] {
        assert!(image_config(bad).is_err(), "{bad}");
    }
    assert_eq!(
        [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "image/svg+xml",
            "image/x"
        ]
        .map(extension),
        ["png", "jpg", "webp", "gif", "svg", "img"]
    );
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    assert_eq!(
        output_paths(Some(&base.join("cat.jpeg")), Path::new("/unused"), &["image/png"]),
        [base.join("cat.jpeg")]
    );
    assert_eq!(
        output_paths(Some(&base.join("cat")), Path::new("/unused"), &["image/png"]),
        [base.join("cat.png")]
    );
    assert_eq!(
        output_paths(
            Some(&base.join("cat.jpeg")),
            Path::new("/unused"),
            &["image/png", "image/png"]
        ),
        [base.join("cat-1.jpeg"), base.join("cat-2.jpeg")]
    );
    // An existing directory gets numbered names.
    assert_eq!(
        output_paths(Some(base), Path::new("/unused"), &["image/gif"]),
        [base.join("image-1.gif")]
    );
}
