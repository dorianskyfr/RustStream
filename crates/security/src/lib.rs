//! Security primitives for RustStream.
//!
//! Model (see `SECURITY.md`):
//!
//! * Every installation owns a long-lived **device identity**: an ECDSA P-256
//!   key pair and a self-signed certificate. The identity of a device *is* the
//!   SHA-256 fingerprint of that certificate — there is no CA.
//! * Every connection is QUIC + TLS 1.3 with **mutual** certificate
//!   authentication. TLS proves possession of the private key; the
//!   application then authorises the fingerprint against the [`TrustStore`].
//! * Unknown devices must be **paired**: both screens show the same 6-digit
//!   code derived from both fingerprints ([`sas_code`]) and the host user
//!   approves. A man-in-the-middle necessarily changes at least one
//!   fingerprint, so the codes differ.
//! * The private key is stored encrypted by a [`SecretProtector`] (DPAPI on
//!   Windows, see `ruststream-platform-windows`).

mod fingerprint;
mod identity;
mod limiter;
mod tls;
mod trust;

pub use fingerprint::{Fingerprint, sas_code};
pub use identity::{DeviceIdentity, PlaintextProtector, SecretProtector};
pub use limiter::AttemptLimiter;
pub use tls::{
    SERVER_NAME, ServerPolicy, client_config, crypto_provider, peer_fingerprint, server_config,
};
pub use trust::{TrustStore, TrustedDevice};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("certificate generation failed: {0}")]
    CertGen(#[from] rcgen::Error),
    #[error("TLS configuration error: {0}")]
    Tls(#[from] rustls::Error),
    #[error("stored data is corrupted: {0}")]
    Corrupted(String),
    #[error("secret protection failed: {0}")]
    Protect(String),
}

pub type Result<T> = std::result::Result<T, SecurityError>;

/// Lowercase hex encoding (avoids an extra dependency for a few bytes).
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0xf)]));
    }
    s
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    s.as_bytes()
        .chunks(2)
        .map(|p| Some(val(p[0])? << 4 | val(p[1])?))
        .collect()
}

/// Writes a file atomically (temp file + rename) so a crash never leaves a
/// truncated identity or trust store behind.
pub(crate) fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let data = [0u8, 1, 0xab, 0xff, 0x10];
        let s = hex_encode(&data);
        assert_eq!(s, "0001abff10");
        assert_eq!(hex_decode(&s).as_deref(), Some(&data[..]));
        assert_eq!(hex_decode("ABFF").as_deref(), Some(&[0xab, 0xff][..]));
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }
}
