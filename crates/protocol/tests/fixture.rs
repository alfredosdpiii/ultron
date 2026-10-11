//! Replays `fixtures/protocol.json`, recorded from the TypeScript `packages/protocol` by
//! `fixtures/generate.mjs`, and requires identical bytes, values and error messages.

use protocol::{
    CborOptions, ClientMessageDecoder, DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, ServerMessageDecoder, Value,
    decode_cbor, encode_cbor, encode_client_message, encode_frame, encode_server_message, parse_client_message,
    parse_server_message,
};
use serde_json::{Value as Json, json};

fn fixture() -> Json {
    serde_json::from_str(include_str!("../fixtures/protocol.json")).expect("fixture parses")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn from_spec(spec: &Json) -> Value {
    match spec {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().expect("number")),
        },
        Json::String(s) => Value::text(s.as_str()),
        Json::Array(items) => Value::Array(items.iter().map(from_spec).collect()),
        Json::Object(map) => {
            if let Some(Json::String(h)) = map.get("$bytes") {
                return Value::Bytes(unhex(h));
            }
            if let Some(Json::String(n)) = map.get("$num") {
                return Value::Float(match n.as_str() {
                    "-0" => -0.0,
                    "NaN" => f64::NAN,
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    other => panic!("unknown $num {other}"),
                });
            }
            Value::Map(map.iter().map(|(k, v)| (k.clone(), from_spec(v))).collect())
        }
    }
}

fn options(spec: &Json) -> CborOptions {
    if spec.is_null() {
        return CborOptions::default();
    }
    let field = |k: &str| spec[k].as_u64().expect("limit") as usize;
    CborOptions {
        max_byte_length: field("max_byte_length"),
        max_container_length: field("max_container_length"),
        max_depth: field("max_depth"),
    }
}

fn max(spec: &Json) -> usize {
    spec.as_u64().map_or(DEFAULT_MAX_FRAME_LENGTH, |m| m as usize)
}

/// The recorded outcome of one case: `{ok}` or `{error: "Name: message"}`.
fn expected(case: &Json) -> Result<&Json, &str> {
    match case.get("error") {
        Some(error) => Err(error.as_str().expect("error text")),
        None => Ok(&case["ok"]),
    }
}

#[test]
fn protocol_cbor_encoding_matches_typescript() {
    let fixture = fixture();
    let cases = fixture["cborEncode"].as_array().expect("cases");
    assert!(cases.len() > 60);
    for case in cases {
        let actual = encode_cbor(&from_spec(&case["value"]), &options(&case["options"]))
            .map(|bytes| json!(hex(&bytes)))
            .map_err(|e| format!("CborError: {e}"));
        match expected(case) {
            Ok(ok) => assert_eq!(actual.as_ref(), Ok(ok), "{case}"),
            Err(error) => assert_eq!(actual.as_ref().map_err(String::as_str), Err(error), "{case}"),
        }
    }
}

#[test]
fn protocol_cbor_decoding_matches_typescript() {
    let fixture = fixture();
    let cases = fixture["cborDecode"].as_array().expect("cases");
    assert!(cases.len() > 90);
    for case in cases {
        let actual = decode_cbor(&unhex(case["hex"].as_str().expect("hex")), &options(&case["options"]));
        match expected(case) {
            Ok(ok) => {
                let value = actual.unwrap_or_else(|e| panic!("{case}: {e}"));
                assert_eq!(value, from_spec(ok), "{case}");
                // Integral numbers decode as the same JavaScript value whether int or float on the wire.
                assert_eq!(
                    matches!(value, Value::Float(_)),
                    ok.is_f64() || ok.get("$num").is_some(),
                    "{case}"
                );
            }
            Err(error) => assert_eq!(
                actual.map_err(|e| format!("CborError: {e}")),
                Err(error.to_owned()),
                "{case}"
            ),
        }
    }
}

#[test]
fn protocol_frame_decoder_scripts_match_typescript() {
    let fixture = fixture();
    for script in fixture["frames"].as_array().expect("scripts") {
        let results = script["results"].as_array().expect("results");
        if results[0]["op"] == "encode:aabbcc" {
            assert_eq!(
                json!([hex(&encode_frame(&unhex("aabbcc")).expect("frame"))]),
                results[0]["ok"]
            );
            continue;
        }
        if results[0]["op"] == "new" {
            let error = FrameDecoder::new(max(&script["max"])).expect_err("rejected limit");
            assert_eq!(
                format!("RangeError: {error}"),
                results[0]["error"].as_str().expect("error")
            );
            continue;
        }
        let mut decoder = FrameDecoder::new(max(&script["max"])).expect("decoder");
        for result in results {
            let op = result["op"].as_str().expect("op");
            let actual = if op == "end" {
                decoder.end().map(|()| Json::Null)
            } else {
                decoder
                    .push(&unhex(op))
                    .map(|frames| json!(frames.iter().map(|f| hex(f)).collect::<Vec<_>>()))
            }
            .map_err(|e| format!("FrameError: {e}"));
            match expected(result) {
                Ok(ok) => assert_eq!(actual.as_ref(), Ok(ok), "{script}"),
                Err(error) => assert_eq!(actual.as_ref().map_err(String::as_str), Err(error), "{script}"),
            }
        }
    }
}

#[test]
fn protocol_message_validation_and_encoding_match_typescript() {
    let fixture = fixture();
    let cases = fixture["messages"].as_array().expect("cases");
    assert!(cases.len() > 60);
    for case in cases {
        let value = from_spec(&case["value"]);
        let limit = max(&case["max"]);
        let actual = if case["side"] == "client" {
            parse_client_message(&value).and_then(|m| encode_client_message(&m, limit))
        } else {
            parse_server_message(&value).and_then(|m| encode_server_message(&m, limit))
        }
        .map(|bytes| json!(hex(&bytes)))
        .map_err(|e| format!("ProtocolValidationError: {e}"));
        match expected(case) {
            Ok(ok) => assert_eq!(actual.as_ref(), Ok(ok), "{case}"),
            Err(error) => assert_eq!(actual.as_ref().map_err(String::as_str), Err(error), "{case}"),
        }
    }
}

#[test]
fn protocol_message_decoder_scripts_match_typescript() {
    let fixture = fixture();
    for script in fixture["decoders"].as_array().expect("scripts") {
        let limit = max(&script["max"]);
        let mut client = ClientMessageDecoder::new(limit).expect("decoder");
        let mut server = ServerMessageDecoder::new(limit).expect("decoder");
        for result in script["results"].as_array().expect("results") {
            let op = result["op"].as_str().expect("op");
            let actual: Result<Vec<Value>, _> = match (script["side"].as_str(), op) {
                (Some("client"), "end") => client.end().map(|()| Vec::new()),
                (Some("client"), _) => client
                    .push(&unhex(op))
                    .map(|ms| ms.iter().map(|m| m.to_value()).collect()),
                (_, "end") => server.end().map(|()| Vec::new()),
                _ => server
                    .push(&unhex(op))
                    .map(|ms| ms.iter().map(|m| m.to_value()).collect()),
            };
            match expected(result) {
                Ok(Json::Null) => assert_eq!(actual, Ok(Vec::new()), "{script}"),
                Ok(ok) => {
                    let want: Vec<Value> = ok.as_array().expect("messages").iter().map(from_spec).collect();
                    assert_eq!(actual, Ok(want), "{script}");
                }
                Err(error) => {
                    assert_eq!(
                        actual.map_err(|e| format!("ProtocolValidationError: {e}")),
                        Err(error.to_owned())
                    );
                }
            }
        }
    }
}
