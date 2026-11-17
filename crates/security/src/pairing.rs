//! Pairing short authentication string (SAS) with a commit–reveal exchange.
//!
//! A SAS derived only from the two (public, static) certificate fingerprints
//! can be brute-forced offline by a man-in-the-middle: it generates
//! certificates until both intercepted connections show the same 6 digits.
//! Instead, each pairing mixes in fresh random nonces, exchanged so that
//! neither side can choose its nonce after seeing the other's:
//!
//! ```text
//! host   → client : commitment = H(host_fp ‖ Nh)
//! client → host   : Nc
//! host   → client : Nh            (client checks the commitment)
//! both            : SAS = H(client_fp ‖ host_fp ‖ Nc ‖ Nh) mod 10^6
//! ```
//!
//! An attacker relaying two connections must commit to its nonce before
//! learning the peer's nonce on each side, so the two codes match with
//! probability 10⁻⁶ per attempt — and attempts are rate-limited.

use sha2::{Digest, Sha256};

use crate::{Fingerprint, Result, SecurityError};

pub const NONCE_LEN: usize = 32;
pub type Nonce = [u8; NONCE_LEN];
pub type Commitment = [u8; 32];

/// Fresh random nonce from the TLS provider's CSPRNG.
pub fn random_nonce() -> Result<Nonce> {
    let mut n = [0u8; NONCE_LEN];
    crate::crypto_provider()
        .secure_random
        .fill(&mut n)
        .map_err(|_| SecurityError::Protect("system random generator unavailable".into()))?;
    Ok(n)
}

/// Host commitment to its nonce.
pub fn commitment(host_fp: &Fingerprint, host_nonce: &Nonce) -> Commitment {
    let mut h = Sha256::new();
    h.update(b"RustStream pairing commit v2");
    h.update(host_fp.0);
    h.update(host_nonce);
    h.finalize().into()
}

/// Checks a revealed host nonce against its commitment (constant time).
pub fn verify_commitment(c: &Commitment, host_fp: &Fingerprint, host_nonce: &Nonce) -> bool {
    let expected = commitment(host_fp, host_nonce);
    expected
        .iter()
        .zip(c.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// The 6-digit code shown on both screens, formatted `123 456`.
pub fn sas_code(
    client_fp: &Fingerprint,
    host_fp: &Fingerprint,
    client_nonce: &Nonce,
    host_nonce: &Nonce,
) -> String {
    let mut h = Sha256::new();
    h.update(b"RustStream pairing SAS v2");
    h.update(client_fp.0);
    h.update(host_fp.0);
    h.update(client_nonce);
    h.update(host_nonce);
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(s: &[u8]) -> Fingerprint {
        Fingerprint::of_certificate(s)
    }

    #[test]
    fn commit_reveal_roundtrip() {
        let host = fp(b"host");
        let nh = random_nonce().expect("nonce");
        let c = commitment(&host, &nh);
        assert!(verify_commitment(&c, &host, &nh));
        let mut other = nh;
        other[0] ^= 1;
        assert!(!verify_commitment(&c, &host, &other));
        assert!(!verify_commitment(&c, &fp(b"impostor"), &nh));
    }

    #[test]
    fn nonces_are_random() {
        assert_ne!(random_nonce().expect("a"), random_nonce().expect("b"));
    }

    #[test]
    fn sas_depends_on_every_input() {
        let (c, h) = (fp(b"client"), fp(b"host"));
        let (nc, nh) = ([1u8; 32], [2u8; 32]);
        let code = sas_code(&c, &h, &nc, &nh);
        assert_eq!(code.len(), 7);
        assert!(
            code.chars()
                .filter(|ch| *ch != ' ')
                .all(|ch| ch.is_ascii_digit())
        );
        assert_eq!(code, sas_code(&c, &h, &nc, &nh));
        assert_ne!(code, sas_code(&h, &c, &nc, &nh), "roles are bound");
        assert_ne!(code, sas_code(&c, &fp(b"x"), &nc, &nh));
        assert_ne!(code, sas_code(&c, &h, &[3u8; 32], &nh));
        assert_ne!(code, sas_code(&c, &h, &nc, &[3u8; 32]));
    }
}
