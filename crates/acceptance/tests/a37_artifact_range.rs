//! A37 (the artifact case): "range reads return exact byte ranges, including ranges that split multibyte characters"
//! of packages/coding-agent/test/ultron-a37-bounded-preview.test.ts, over `kernel_apis::artifacts`.

use kernel_apis::artifacts::{ArtifactLimits, Artifacts, MemoryKeyedDocuments};
use serde_json::json;

/// Standard base64 decoding of what `base64` produced.
fn decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let digits: Vec<u32> = text
        .bytes()
        .filter(|b| *b != b'=')
        .map(|b| ALPHABET.iter().position(|a| *a == b).unwrap() as u32)
        .collect();
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0, |n, (i, d)| n | (d << (18 - 6 * i)));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

#[test]
fn acceptance_a37_range_reads_return_exact_byte_ranges_including_ranges_that_split_multibyte_characters() {
    let services = Artifacts::new(Box::new(MemoryKeyedDocuments::default()), ArtifactLimits::default()).unwrap();
    let content = format!("{}tail", "é☃𝄞".repeat(20_000));
    let bytes = content.as_bytes();
    let put = services.handle("artifacts.put", &json!({ "text": content })).unwrap();
    assert_eq!(put["bytes"], bytes.len());
    let id = put["id"].clone();
    let n = bytes.len();
    // Every range, including ones starting and ending mid-character, is byte exact.
    for (offset, length) in [
        (0, 1),
        (1, 3),
        (2, 5),
        (3, 7),
        (n - 6, 6),
        (n - 2, 100),
        (n, 10),
        (n + 5, 10),
    ] {
        let read = services
            .handle(
                "artifacts.read",
                &json!({ "id": id, "options": { "offset": offset, "length": length } }),
            )
            .unwrap();
        let expected = &bytes[offset.min(n)..(offset + length).min(n)];
        assert_eq!(decode(read["base64"].as_str().unwrap()), expected, "{offset}+{length}");
        assert_eq!(
            (read["offset"].clone(), read["length"].clone(), read["total"].clone()),
            (json!(offset), json!(expected.len()), json!(n))
        );
    }
    // Sequential chunked reads reassemble the full value exactly.
    let mut chunks = Vec::new();
    for offset in (0..n).step_by(65_537) {
        let read = services
            .handle(
                "artifacts.read",
                &json!({ "id": id, "options": { "offset": offset, "length": 65_537 } }),
            )
            .unwrap();
        chunks.extend(decode(read["base64"].as_str().unwrap()));
    }
    assert_eq!(chunks, bytes);
    // Ranges are bounded; an oversized request fails instead of returning everything.
    assert_eq!(
        services
            .handle(
                "artifacts.read",
                &json!({ "id": id, "options": { "offset": 0, "length": 1_048_577 } })
            )
            .unwrap_err(),
        "Invalid artifact range"
    );
}
