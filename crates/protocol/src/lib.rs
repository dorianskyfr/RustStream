//! RustStream wire protocol.
//!
//! The protocol runs over QUIC (see `ruststream-network`) and is split by
//! delivery requirements:
//!
//! * **Control** — one bidirectional, reliable, ordered stream carrying
//!   [`ControlMessage`]s (handshake, pairing, keyframe requests, pings).
//! * **Input** — one client→host unidirectional reliable stream carrying
//!   [`InputEvent`]s. It is separate from control so a large control message
//!   can never delay a key press, and reliable so a key-up is never lost.
//! * **Video** — unreliable QUIC datagrams carrying fragments of encoded
//!   frames ([`video`]). A lost fragment invalidates one frame only; the client
//!   then asks for a keyframe instead of waiting for a retransmission.
//!
//! Stream messages are encoded with `postcard` and framed with a little-endian
//! `u32` length prefix ([`framing`]). Video datagrams use a fixed binary header
//! so that the hot path never goes through serde.

pub mod control;
pub mod framing;
pub mod input;
pub mod video;

pub use control::*;
pub use input::*;

/// Wire protocol version. Bump on any incompatible change to messages or framing.
pub const PROTOCOL_VERSION: u16 = 1;

/// QUIC ALPN identifier. Includes the major protocol version so incompatible
/// peers fail during the TLS handshake instead of mid-session.
pub const ALPN: &[u8] = b"ruststream/1";

/// Default UDP port of the host QUIC endpoint.
pub const DEFAULT_PORT: u16 = 47800;

/// UDP port used for LAN discovery beacons.
pub const DISCOVERY_PORT: u16 = 47801;

/// Upper bound for a single framed stream message. Anything larger is rejected
/// as malformed before any allocation happens.
pub const MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// Maximum length accepted for human-readable names (device names, reasons).
pub const MAX_NAME_LEN: usize = 64;

/// Errors produced while encoding or decoding protocol data.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message too large: {0} bytes (max {MAX_MESSAGE_SIZE})")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(&'static str),
    #[error("serialization error: {0}")]
    Codec(#[from] postcard::Error),
}
