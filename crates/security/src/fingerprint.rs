use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::{hex_decode, hex_encode};

/// SHA-256 of a device certificate (DER). This is the device identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of_certificate(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let bytes = hex_decode(s)?;
        Some(Self(bytes.try_into().ok()?))
    }

    /// Short human-readable form, e.g. `3F2A-91C0-77DE-0B14`. Used in the UI
    /// to recognise a device; never used for authorisation.
    pub fn short(&self) -> String {
        let hex = hex_encode(&self.0[..8]).to_uppercase();
        hex.as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap_or("????"))
            .collect::<Vec<_>>()
            .join("-")
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.short())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid fingerprint"))
    }
}

/// Short authentication string shown on both devices during pairing.
///
/// It is symmetric (both sides compute the same value regardless of role)
/// and changes if either certificate changes. Six decimal digits give a
/// 1-in-a-million chance for an active attacker to match by luck on a single
/// attempt; attempts are rate-limited by [`crate::AttemptLimiter`].
pub fn sas_code(a: &Fingerprint, b: &Fingerprint) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(b"RustStream pairing SAS v1");
    h.update(lo.0);
    h.update(hi.0);
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_short_forms() {
        let fp = Fingerprint::of_certificate(b"cert");
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(fp.short().len(), 19);
        assert!(Fingerprint::from_hex("00").is_none());
    }

    #[test]
    fn sas_is_symmetric_and_sensitive() {
        let a = Fingerprint::of_certificate(b"a");
        let b = Fingerprint::of_certificate(b"b");
        let c = Fingerprint::of_certificate(b"c");
        assert_eq!(sas_code(&a, &b), sas_code(&b, &a));
        assert_ne!(sas_code(&a, &b), sas_code(&a, &c));
        let code = sas_code(&a, &b);
        assert_eq!(code.len(), 7);
        assert!(
            code.chars()
                .filter(|c| *c != ' ')
                .all(|c| c.is_ascii_digit())
        );
    }

    #[test]
    fn serde_as_hex_string() {
        let fp = Fingerprint::of_certificate(b"x");
        let json = serde_json::to_string(&fp).expect("ser");
        assert_eq!(json, format!("\"{}\"", fp.to_hex()));
        let back: Fingerprint = serde_json::from_str(&json).expect("de");
        assert_eq!(back, fp);
    }
}
