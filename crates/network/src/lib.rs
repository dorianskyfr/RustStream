//! Network transport for RustStream.
//!
//! * [`transport`]: QUIC endpoints (Quinn) with mutual TLS from
//!   `ruststream-security`, tuned for real-time traffic.
//! * [`stream`]: length-prefixed message I/O on reliable streams.
//! * [`video`]: sending/receiving encoded frames as QUIC datagrams.
//! * [`discovery`]: LAN discovery beacons (UDP broadcast).

pub mod discovery;
pub mod stream;
pub mod transport;
pub mod video;

pub use quinn::{Connection, Endpoint, RecvStream, SendStream};

/// Errors of the network layer.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection failed: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection lost: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("stream write failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("stream read failed: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("stream closed: {0}")]
    ClosedStream(#[from] quinn::ClosedStream),
    #[error("datagram send failed: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("protocol error: {0}")]
    Protocol(#[from] ruststream_protocol::ProtocolError),
    #[error("security error: {0}")]
    Security(#[from] ruststream_security::SecurityError),
    #[error("TLS configuration has no QUIC-compatible cipher suite")]
    NoCipherSuite,
    #[error("peer did not present a certificate")]
    NoPeerIdentity,
    #[error("datagrams are not supported by the peer")]
    DatagramsUnsupported,
}

pub type Result<T> = std::result::Result<T, NetError>;
