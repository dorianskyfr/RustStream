use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Fingerprint, Result, SecurityError, hex_decode, hex_encode, write_atomic};

/// Encrypts secrets at rest. The Windows implementation uses DPAPI (bound to
/// the current user account).
pub trait SecretProtector: Send + Sync {
    /// Short identifier stored next to the protected data.
    fn name(&self) -> &'static str;
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>>;
    fn unprotect(&self, protected: &[u8]) -> Result<Vec<u8>>;
}

/// No protection at all. Only for tests and platforms without a secure
/// store; the identity file then relies on file-system permissions alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlaintextProtector;

impl SecretProtector for PlaintextProtector {
    fn name(&self) -> &'static str {
        "plaintext"
    }
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        Ok(plaintext.to_vec())
    }
    fn unprotect(&self, protected: &[u8]) -> Result<Vec<u8>> {
        Ok(protected.to_vec())
    }
}

/// Long-lived identity of this installation.
pub struct DeviceIdentity {
    cert_der: Vec<u8>,
    key_pkcs8: Vec<u8>,
    fingerprint: Fingerprint,
}

impl std::fmt::Debug for DeviceIdentity {
    // Never print the private key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    version: u32,
    protector: String,
    cert_der: String,
    key: String,
}

impl DeviceIdentity {
    /// Generates a fresh ECDSA P-256 identity.
    pub fn generate() -> Result<Self> {
        let key = rcgen::KeyPair::generate()?;
        let params = rcgen::CertificateParams::new(vec![crate::SERVER_NAME.to_string()])?;
        let cert = params.self_signed(&key)?;
        let cert_der = cert.der().to_vec();
        Ok(Self {
            fingerprint: Fingerprint::of_certificate(&cert_der),
            cert_der,
            key_pkcs8: key.serialize_der(),
        })
    }

    /// Loads the identity from `path`, or generates and saves a new one.
    pub fn load_or_create(path: &Path, protector: &dyn SecretProtector) -> Result<Self> {
        match std::fs::read(path) {
            Ok(data) => Self::decode(&data, protector),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                id.save(path, protector)?;
                tracing::info!(fingerprint = %id.fingerprint, "generated new device identity");
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path, protector: &dyn SecretProtector) -> Result<()> {
        let stored = StoredIdentity {
            version: 1,
            protector: protector.name().to_string(),
            cert_der: hex_encode(&self.cert_der),
            key: hex_encode(&protector.protect(&self.key_pkcs8)?),
        };
        let json = serde_json::to_vec_pretty(&stored)
            .map_err(|e| SecurityError::Corrupted(e.to_string()))?;
        write_atomic(path, &json)?;
        Ok(())
    }

    fn decode(data: &[u8], protector: &dyn SecretProtector) -> Result<Self> {
        let stored: StoredIdentity =
            serde_json::from_slice(data).map_err(|e| SecurityError::Corrupted(e.to_string()))?;
        if stored.version != 1 {
            return Err(SecurityError::Corrupted(format!(
                "unsupported identity version {}",
                stored.version
            )));
        }
        if stored.protector != protector.name() {
            return Err(SecurityError::Corrupted(format!(
                "identity protected with '{}' but '{}' is in use",
                stored.protector,
                protector.name()
            )));
        }
        let cert_der = hex_decode(&stored.cert_der)
            .ok_or_else(|| SecurityError::Corrupted("certificate".into()))?;
        let protected =
            hex_decode(&stored.key).ok_or_else(|| SecurityError::Corrupted("key".into()))?;
        let key_pkcs8 = protector.unprotect(&protected)?;
        // Validate that the key parses. Key/certificate consistency is checked
        // by rustls when the TLS configuration is built.
        rcgen::KeyPair::try_from(key_pkcs8.as_slice())?;
        Ok(Self {
            fingerprint: Fingerprint::of_certificate(&cert_der),
            cert_der,
            key_pkcs8,
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub(crate) fn key_pkcs8(&self) -> &[u8] {
        &self.key_pkcs8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct XorProtector;
    impl SecretProtector for XorProtector {
        fn name(&self) -> &'static str {
            "xor-test"
        }
        fn protect(&self, p: &[u8]) -> Result<Vec<u8>> {
            Ok(p.iter().map(|b| b ^ 0x5a).collect())
        }
        fn unprotect(&self, p: &[u8]) -> Result<Vec<u8>> {
            self.protect(p)
        }
    }

    #[test]
    fn persist_and_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.json");
        let a = DeviceIdentity::load_or_create(&path, &XorProtector).expect("create");
        let b = DeviceIdentity::load_or_create(&path, &XorProtector).expect("reload");
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.key_pkcs8(), b.key_pkcs8());
        // The private key is not stored in clear.
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(!raw.contains(&hex_encode(a.key_pkcs8())));
        // Wrong protector is refused instead of silently producing garbage.
        assert!(DeviceIdentity::load_or_create(&path, &PlaintextProtector).is_err());
    }

    #[test]
    fn identities_are_unique() {
        let a = DeviceIdentity::generate().expect("gen");
        let b = DeviceIdentity::generate().expect("gen");
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert!(!format!("{a:?}").contains("key"));
    }
}
