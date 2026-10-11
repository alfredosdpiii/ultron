//! Ported from packages/protocol/test/{framing,protocol}.test.ts and test/cbor/cbor.test.ts (literal expectations).

use protocol::{
    CborOptions, ClientMessage, ClientMessageDecoder, FrameDecoder, PROTOCOL_VERSION, RpcTarget, Value, decode_cbor,
    encode_cbor, encode_client_message, encode_frame, is_supported_protocol_version,
};

const SID: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn protocol_frames_are_prefixed_with_big_endian_length() {
    assert_eq!(
        encode_frame(&[0xaa, 0xbb, 0xcc]),
        Ok(vec![0, 0, 0, 3, 0xaa, 0xbb, 0xcc])
    );
    assert_eq!(encode_frame(&[]), Ok(vec![0, 0, 0, 0]));
}

#[test]
fn protocol_frame_decoder_handles_every_split_point() {
    let wire = encode_frame(&[10, 20, 30, 40]).expect("frame");
    for split in 0..=wire.len() {
        let mut decoder = FrameDecoder::default();
        let mut frames = decoder.push(&wire[..split]).expect("push");
        frames.extend(decoder.push(&wire[split..]).expect("push"));
        decoder.end().expect("end");
        assert_eq!(frames, vec![vec![10, 20, 30, 40]]);
    }
}

#[test]
fn protocol_frame_decoder_assembles_payloads_spanning_blocks() {
    let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let wire = encode_frame(&payload).expect("frame");
    let mut decoder = FrameDecoder::default();
    let mut frames = decoder.push(&wire[..101]).expect("push");
    frames.extend(decoder.push(&wire[101..65_541]).expect("push"));
    frames.extend(decoder.push(&wire[65_541..]).expect("push"));
    decoder.end().expect("end");
    assert_eq!(frames, vec![payload]);
}

#[test]
fn protocol_frame_decoder_rejects_oversized_then_stays_failed() {
    let mut decoder = FrameDecoder::new(3).expect("decoder");
    assert_eq!(
        decoder.push(&[0, 0, 0, 4]).expect_err("limit").0,
        "Frame length 4 exceeds configured limit of 3"
    );
    assert_eq!(decoder.push(&[1]).expect_err("failed").0, "Frame decoder has failed");
}

#[test]
fn protocol_negotiates_version_8_only() {
    assert_eq!(PROTOCOL_VERSION, 8);
    assert!(is_supported_protocol_version(8));
    assert!(!is_supported_protocol_version(7));
}

#[test]
fn protocol_cbor_rfc_vectors_round_trip() {
    let vectors = [
        (Value::Int(1_000_000_000_000), "1b000000e8d4a51000"),
        (Value::Int(-1_000_000), "3a000f423f"),
        (Value::Float(1.1), "fb3ff199999999999a"),
        (Value::Float(-0.0), "fb8000000000000000"),
        (Value::text("𐅑"), "64f0908591"),
        (
            Value::map([
                ("a", Value::Int(1)),
                ("b", Value::Array(vec![Value::Int(2), Value::Int(3)])),
            ]),
            "a26161016162820203",
        ),
    ];
    for (value, wire) in vectors {
        let encoded = encode_cbor(&value, &CborOptions::default()).expect("encode");
        let text: String = encoded.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(text, wire);
        assert_eq!(decode_cbor(&encoded, &CborOptions::default()), Ok(value));
    }
}

#[test]
fn protocol_client_decoder_reassembles_split_messages() {
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
    };
    let request = ClientMessage::Request {
        id: "request-1".into(),
        target: RpcTarget::Server { server_id: SID.into() },
        call: Value::map([
            ("serviceId", Value::text("pi.session-directory")),
            ("member", Value::text("list")),
            ("args", Value::Array(vec![])),
        ]),
    };
    let mut wire = encode_client_message(&hello, 1 << 24).expect("hello");
    wire.extend(encode_client_message(&request, 1 << 24).expect("request"));
    for split in 0..=wire.len() {
        let mut decoder = ClientMessageDecoder::default();
        let mut messages = decoder.push(&wire[..split]).expect("push");
        messages.extend(decoder.push(&wire[split..]).expect("push"));
        decoder.end().expect("end");
        assert_eq!(messages, vec![hello.clone(), request.clone()]);
    }
}
