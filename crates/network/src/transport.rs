use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, IdleTimeout, TransportConfig, VarInt};
use rustls::pki_types::CertificateDer;
use ruststream_protocol::ALPN;
use ruststream_security::{DeviceIdentity, Fingerprint, SERVER_NAME, ServerPolicy};

use crate::{NetError, Result};

/// Application close codes (QUIC `CONNECTION_CLOSE` error codes).
pub mod close_code {
    pub const NORMAL: u32 = 0;
    pub const REJECTED: u32 = 1;
    pub const PROTOCOL_ERROR: u32 = 2;
    pub const HOST_SHUTDOWN: u32 = 3;
}

/// Transport parameters shared by host and client.
///
/// * Keep-alive well under the idle timeout so NAT bindings stay open and a
///   dead peer is detected within a few seconds.
/// * Datagram buffers sized for a few frames only: when the network falls
///   behind, Quinn drops the *oldest* queued datagrams, which matches the
///   "newest frame wins" policy instead of building latency.
pub fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    // 10 s fits comfortably in a VarInt of milliseconds.
    t.max_idle_timeout(IdleTimeout::try_from(Duration::from_secs(10)).ok());
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_concurrent_bidi_streams(VarInt::from_u32(4));
    t.max_concurrent_uni_streams(VarInt::from_u32(4));
    t.datagram_receive_buffer_size(Some(4 * 1024 * 1024));
    t.datagram_send_buffer_size(2 * 1024 * 1024);
    t
}

/// Creates the host endpoint listening on `bind`.
pub fn server_endpoint(bind: SocketAddr, identity: &DeviceIdentity) -> Result<Endpoint> {
    let tls = ruststream_security::server_config(identity, ALPN)?;
    let quic = QuicServerConfig::try_from(tls).map_err(|_| NetError::NoCipherSuite)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    cfg.transport_config(Arc::new(transport_config()));
    // Validate source addresses before doing expensive crypto for unknown peers.
    Ok(Endpoint::server(cfg, bind)?)
}

/// Creates a client-only endpoint bound to an ephemeral port.
pub fn client_endpoint() -> Result<Endpoint> {
    Ok(Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?)
}

/// Connects to a host. With [`ServerPolicy::Pinned`], the TLS handshake fails
/// unless the host presents the expected certificate.
pub async fn connect(
    endpoint: &Endpoint,
    addr: SocketAddr,
    identity: &DeviceIdentity,
    policy: ServerPolicy,
) -> Result<Connection> {
    let tls = ruststream_security::client_config(identity, policy, ALPN)?;
    let quic = QuicClientConfig::try_from(tls).map_err(|_| NetError::NoCipherSuite)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(Arc::new(transport_config()));
    let conn = endpoint.connect_with(cfg, addr, SERVER_NAME)?.await?;
    Ok(conn)
}

/// Fingerprint of the certificate the peer authenticated with.
pub fn peer_fingerprint(conn: &Connection) -> Result<Fingerprint> {
    let any = conn.peer_identity().ok_or(NetError::NoPeerIdentity)?;
    let certs = any
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| NetError::NoPeerIdentity)?;
    ruststream_security::peer_fingerprint(&certs).ok_or(NetError::NoPeerIdentity)
}

/// Snapshot of connection-level network statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinkStats {
    pub rtt: Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub congestion_window: u64,
}

pub fn link_stats(conn: &Connection) -> LinkStats {
    let s = conn.stats();
    LinkStats {
        rtt: s.path.rtt,
        lost_packets: s.path.lost_packets,
        sent_packets: s.path.sent_packets,
        congestion_window: s.path.cwnd,
    }
}
