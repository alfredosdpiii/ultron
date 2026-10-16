//! RFC 6455 framing with the RFC's own literal examples (section 1.3 handshake, section 5.7 frames), plus the
//! decoder's protocol checks.

use protocol::websocket::{Message, MessageDecoder, accept_key, base64, encode_frame, encode_message, sha1};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn protocol_websocket_accept_key_is_the_rfc_example() {
    assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        hex(&sha1(&[b'a'; 1_000_000])),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
}

#[test]
fn protocol_websocket_frames_are_the_rfc_examples() {
    assert_eq!(
        hex(&encode_message(&Message::Text("Hello".into()), None)),
        "810548656c6c6f"
    );
    assert_eq!(
        hex(&encode_message(
            &Message::Text("Hello".into()),
            Some([0x37, 0xfa, 0x21, 0x3d])
        )),
        "818537fa213d7f9f4d5158"
    );
    assert_eq!(
        hex(&encode_message(&Message::Ping(b"Hello".to_vec()), None)),
        "890548656c6c6f"
    );
    assert_eq!(&encode_frame(0x2, &[0; 256], None)[..4], &[0x82, 0x7e, 0x01, 0x00]);
    assert_eq!(
        &encode_frame(0x2, &[0; 65536], None)[..10],
        &[0x82, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]
    );
    assert_eq!(
        hex(&encode_message(&Message::Close(Some(1000), "bye".into()), None)),
        "880503e8627965"
    );
    assert_eq!(hex(&encode_message(&Message::Close(None, String::new()), None)), "8800");
}

#[test]
fn protocol_websocket_decoder_reassembles_and_unmasks() {
    let mut client = MessageDecoder::client(1 << 20);
    // A fragmented unmasked text message ("Hel" + "lo"), split mid-frame, with a ping between the fragments.
    let bytes = [
        0x01, 0x03, 0x48, 0x65, 0x6c, 0x89, 0x00, 0x80, 0x02, 0x6c, 0x6f, 0x88, 0x02, 0x03, 0xe8,
    ];
    assert_eq!(client.push(&bytes[..4]).unwrap(), vec![]);
    assert_eq!(
        client.push(&bytes[4..]).unwrap(),
        vec![
            Message::Ping(vec![]),
            Message::Text("Hello".into()),
            Message::Close(Some(1000), String::new()),
        ]
    );
    let mut server = MessageDecoder::server(1 << 20);
    assert_eq!(
        server
            .push(&encode_message(&Message::Binary(vec![1, 2, 3]), Some([9, 8, 7, 6])))
            .unwrap(),
        vec![Message::Binary(vec![1, 2, 3])]
    );
}

#[test]
fn protocol_websocket_decoder_rejects_protocol_violations() {
    let push = |mut decoder: MessageDecoder, bytes: &[u8]| decoder.push(bytes).unwrap_err();
    // A server must not mask; a client must.
    assert!(
        push(
            MessageDecoder::client(64),
            &encode_message(&Message::Text("x".into()), Some([1, 2, 3, 4]))
        )
        .contains("mask")
    );
    assert!(
        push(
            MessageDecoder::server(64),
            &encode_message(&Message::Text("x".into()), None)
        )
        .contains("mask")
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0xc1, 0x00]),
        "Reserved bits must be clear"
    );
    assert_eq!(push(MessageDecoder::client(64), &[0x83, 0x00]), "Unknown opcode 3");
    assert_eq!(
        push(MessageDecoder::client(64), &[0x80, 0x00]),
        "Unexpected continuation frame"
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0x01, 0x00, 0x81, 0x00]),
        "Expected a continuation frame"
    );
    assert_eq!(push(MessageDecoder::client(64), &[0x09, 0x00]), "Invalid control frame");
    assert_eq!(
        push(MessageDecoder::client(4), &[0x82, 0x05, 1, 2, 3, 4, 5]),
        "Message too large"
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0x88, 0x01, 0x03]),
        "Invalid close payload"
    );
}
