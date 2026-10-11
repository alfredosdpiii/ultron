//! Replays crates/host/fixtures/images.json, recorded by fixtures/images.mjs from the TypeScript view_image host side
//! (view-image.ts CellImages over Photon). Inputs are the recorded bytes; replies must match exactly where the
//! output bytes are the input's, and in everything but the byte count where TypeScript re-encoded (Photon vs `image`).

use host::view_image::{CellImages, ImageModel, ResizeLimits, ViewImageOptions, detect_image_mime_type};
use serde_json::{Map, Value, json};
use std::path::Path;

fn fixture() -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/images.json");
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    data["cases"].as_array().unwrap().clone()
}

fn decode_base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = text.bytes().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &d)| n | (u32::from(d) << (18 - 6 * i)));
        out.extend(n.to_be_bytes()[1..chunk.len()].iter());
    }
    out
}

/// The description with its byte count replaced, for replies whose bytes were re-encoded.
fn without_size(description: &str) -> String {
    let (head, tail) = description.split_once(", ").unwrap();
    let rest = tail
        .split_once(' ')
        .map(|(_, unit)| unit.split_once(' ').map_or("", |(_, r)| r))
        .unwrap_or("");
    format!("{head}, <size> {rest}")
}

fn options(case: &Value) -> ViewImageOptions {
    let model = case["vision"].as_bool().map(|vision| ImageModel {
        provider: "faux".into(),
        id: "m1".into(),
        accepts_images: vision,
        resize: case["resize"].as_object().map(|r| ResizeLimits {
            max_width: r.get("maxWidth").and_then(Value::as_u64).map(|v| v as u32),
            max_height: r.get("maxHeight").and_then(Value::as_u64).map(|v| v as u32),
            max_bytes: r.get("maxBytes").and_then(Value::as_f64),
            jpeg_quality: None,
        }),
    });
    ViewImageOptions {
        auto_resize: case["auto_resize"].as_bool().unwrap_or(true),
        model,
    }
}

#[test]
fn host_view_image_replies_match_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let mut checked = 0;
    for (index, case) in fixture().iter().enumerate() {
        if case.get("replies").is_some() {
            continue;
        }
        let path = dir.path().join(format!("img-{index}"));
        if let Some(input) = case["input"].as_str() {
            std::fs::write(&path, decode_base64(input)).unwrap();
        }
        let mut payload = Map::new();
        payload.insert("path".into(), json!(path.to_str().unwrap()));
        if let Some(detail) = case["detail"].as_str() {
            payload.insert("detail".into(), json!(detail));
        }
        let mut images = CellImages::new(options(case));
        let name = case["name"].as_str().unwrap();
        match images.attach(&payload) {
            Ok(reply) => {
                let image = &images.images[0];
                assert_eq!(image.mime_type, case["mime"].as_str().unwrap(), "{name}");
                let input = decode_base64(case["input"].as_str().unwrap());
                let same = case["same_bytes"].as_bool().unwrap();
                assert_eq!(image.data == input, same, "{name}");
                let (got, want) = (
                    reply["description"].as_str().unwrap(),
                    case["reply"]["description"].as_str().unwrap(),
                );
                if same {
                    assert_eq!(got, want, "{name}");
                } else {
                    assert_eq!(without_size(got), without_size(want), "{name}");
                }
                assert_eq!(reply.get("note"), case["reply"].get("note"), "{name}");
                assert_eq!(images.text("out"), case["content"][0].as_str().unwrap(), "{name}");
            }
            Err(error) => {
                let error = error.replace(dir.path().to_str().unwrap(), "<dir>");
                assert_eq!(Some(error.as_str()), case["error"].as_str(), "{name}");
            }
        }
        checked += 1;
    }
    assert_eq!(checked, 18);
}

#[test]
fn host_view_image_refuses_the_ninth_image_like_typescript() {
    let case = fixture()
        .into_iter()
        .find(|case| case["name"] == "per cell limit")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limit");
    std::fs::write(&path, decode_base64(case["input"].as_str().unwrap())).unwrap();
    let mut payload = Map::new();
    payload.insert("path".into(), json!(path.to_str().unwrap()));
    let mut images = CellImages::new(ViewImageOptions::default());
    let replies: Vec<String> = (0..9)
        .map(|_| match images.attach(&payload) {
            Ok(reply) => reply["description"].as_str().unwrap().to_owned(),
            Err(error) => format!("error: {error}"),
        })
        .collect();
    assert_eq!(json!(replies), case["replies"]);
}

#[test]
fn host_view_image_detects_types_by_content() {
    // utils/mime.ts: a JPEG with 0xf7 (JPEG-LS) and an animated PNG are refused; a non-path payload is an error.
    assert_eq!(detect_image_mime_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
    assert_eq!(detect_image_mime_type(&[0xff, 0xd8, 0xff, 0xf7]), None);
    let mut apng = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    apng.extend(b"IHDR");
    apng.extend([0; 13 + 4]);
    let png = apng.clone();
    apng.extend([0, 0, 0, 8]);
    apng.extend(b"acTL");
    assert_eq!(detect_image_mime_type(&png), Some("image/png"));
    assert_eq!(detect_image_mime_type(&apng), None);
    assert_eq!(detect_image_mime_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    let error = CellImages::default().attach(&Map::new()).unwrap_err();
    assert_eq!(error, "view_image: path must be a non-empty string");
}
