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
    fn serde_as_hex_string() {
        let fp = Fingerprint::of_certificate(b"x");
        let json = serde_json::to_string(&fp).expect("ser");
        assert_eq!(json, format!("\"{}\"", fp.to_hex()));
        let back: Fingerprint = serde_json::from_str(&json).expect("de");
        assert_eq!(back, fp);
    }
}
