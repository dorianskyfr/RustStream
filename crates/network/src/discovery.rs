//! LAN discovery with UDP broadcast beacons.
//!
//! A host that accepts connections broadcasts a small beacon every second.
//! Beacons are **not authenticated**: they are only hints used to show nearby
//! computers and to refresh the IP address of a saved computer. Authentication
//! always happens in the TLS handshake (fingerprint pinning or pairing), so a
//! forged beacon can at worst make a connection attempt fail.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use ruststream_protocol::{DISCOVERY_PORT, MAX_NAME_LEN, PROTOCOL_VERSION};
use ruststream_security::Fingerprint;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util_lite::CancelFlag;

use crate::Result;

const MAGIC: [u8; 4] = *b"RSTM";
const MAX_BEACON: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    pub magic: [u8; 4],
    pub protocol_version: u16,
    pub name: String,
    pub fingerprint: Fingerprint,
    /// QUIC port of the host.
    pub port: u16,
}

impl Beacon {
    pub fn new(name: &str, fingerprint: Fingerprint, port: u16) -> Self {
        let mut name = name.to_string();
        if name.len() > MAX_NAME_LEN {
            let mut cut = MAX_NAME_LEN;
            while !name.is_char_boundary(cut) {
                cut -= 1;
            }
            name.truncate(cut);
        }
        Self {
            magic: MAGIC,
            protocol_version: PROTOCOL_VERSION,
            name,
            fingerprint,
            port,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_allocvec(self).map_err(ruststream_protocol::ProtocolError::from)?)
    }

    /// Parses and validates a beacon; returns `None` for anything unexpected.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() > MAX_BEACON {
            return None;
        }
        let b: Self = postcard::from_bytes(data).ok()?;
        let valid = b.magic == MAGIC
            && b.port != 0
            && b.name.len() <= MAX_NAME_LEN
            && !b.name.chars().any(char::is_control);
        valid.then_some(b)
    }
}

/// A host seen on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub name: String,
    pub fingerprint: Fingerprint,
    pub addr: SocketAddr,
    pub protocol_version: u16,
}

/// Broadcasts `beacon` every `interval` until `stop` is set.
pub async fn announce(beacon: Beacon, interval: Duration, stop: CancelFlag) -> Result<()> {
    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).await?;
    socket.set_broadcast(true)?;
    let payload = beacon.encode()?;
    let target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT));
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while !stop.is_set() {
        tick.tick().await;
        if let Err(e) = socket.send_to(&payload, target).await {
            // Typically "network unreachable" while offline; keep trying.
            tracing::debug!(error = %e, "discovery beacon not sent");
        }
    }
    Ok(())
}

/// Binds the discovery port with address reuse so several RustStream
/// processes on one machine can listen simultaneously.
fn bind_listener(port: u16) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(UdpSocket::from_std(socket.into())?)
}

/// Listens for beacons and forwards valid ones until `stop` is set or the
/// receiver is dropped.
pub async fn listen(tx: mpsc::Sender<DiscoveredHost>, stop: CancelFlag) -> Result<()> {
    listen_on(DISCOVERY_PORT, tx, stop).await
}

pub async fn listen_on(
    port: u16,
    tx: mpsc::Sender<DiscoveredHost>,
    stop: CancelFlag,
) -> Result<()> {
    let socket = bind_listener(port)?;
    let mut buf = [0u8; 2048];
    while !stop.is_set() {
        let recv = tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf));
        let (len, from) = match recv.await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                // On Windows, ICMP "port unreachable" surfaces as a recv error.
                tracing::debug!(error = %e, "discovery receive error");
                continue;
            }
            Err(_) => continue,
        };
        let Some(b) = Beacon::decode(&buf[..len]) else {
            continue;
        };
        let host = DiscoveredHost {
            name: b.name,
            fingerprint: b.fingerprint,
            addr: SocketAddr::new(from.ip(), b.port),
            protocol_version: b.protocol_version,
        };
        if tx.send(host).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Minimal shared cancellation flag (avoids pulling `tokio-util`).
pub mod tokio_util_lite {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Clone, Default)]
    pub struct CancelFlag(Arc<AtomicBool>);

    impl CancelFlag {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn set(&self) {
            self.0.store(true, Ordering::Release);
        }
        pub fn is_set(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrip_and_validation() {
        let fp = Fingerprint::of_certificate(b"host");
        let b = Beacon::new("Gaming PC", fp, 47800);
        let wire = b.encode().expect("encode");
        assert_eq!(Beacon::decode(&wire), Some(b));
        assert!(Beacon::decode(b"garbage").is_none());
        let mut bad = Beacon::new("x", fp, 1);
        bad.magic = *b"XXXX";
        assert!(Beacon::decode(&bad.encode().expect("encode")).is_none());
        let long = Beacon::new(&"é".repeat(100), fp, 1);
        assert!(long.name.len() <= MAX_NAME_LEN);
    }

    #[tokio::test]
    async fn listener_receives_unicast_beacon() {
        // Unicast to localhost exercises the receive path without relying on
        // broadcast support of the CI network.
        let port = 47_891;
        let (tx, mut rx) = mpsc::channel(4);
        let stop = CancelFlag::new();
        let task = tokio::spawn(listen_on(port, tx, stop.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let fp = Fingerprint::of_certificate(b"host");
        let wire = Beacon::new("Desk", fp, 47800).encode().expect("encode");
        let s = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        s.send_to(&wire, ("127.0.0.1", port)).await.expect("send");
        let host = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("host");
        assert_eq!(host.name, "Desk");
        assert_eq!(host.addr.port(), 47800);
        stop.set();
        task.await.expect("join").expect("listen");
    }
}
