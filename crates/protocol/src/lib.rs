//! Port of `packages/protocol`: the strict CBOR subset, 4-byte big-endian framing and the
//! validated client/server envelopes of protocol version 8.

pub mod cbor;
pub mod framing;
pub mod message;
pub mod rpc;
pub mod service;
pub mod websocket;

pub use cbor::{CborError, CborOptions, Value, decode_cbor, encode_cbor};
pub use framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, FrameError, encode_frame};
pub use message::{
    ClientMessage, ClientMessageDecoder, PROTOCOL_VERSION, ProtocolError, ProtocolValidationError, RpcTarget,
    ServerMessage, ServerMessageDecoder, SessionTarget, encode_client_message, encode_server_message, is_server_id,
    is_supported_protocol_version, parse_client_message, parse_server_message,
};
pub use service::{ServiceCall, parse_service_call};
