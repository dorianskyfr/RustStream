use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Fingerprint, Result, SecurityError, write_atomic};

/// A device allowed to connect to this host (or a host this client paired with).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub name: String,
    pub fingerprint: Fingerprint,
    /// Unix seconds.
    pub paired_at: u64,
    pub last_seen: Option<u64>,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredTrust {
    version: u32,
    devices: Vec<TrustedDevice>,
}

/// Persistent list of trusted device fingerprints. Revocation removes the
/// entry; the next connection attempt from that device requires pairing again.
#[derive(Debug)]
pub struct TrustStore {
    path: Option<PathBuf>,
    devices: Vec<TrustedDevice>,
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl TrustStore {
    /// In-memory store (tests, ephemeral sessions).
    pub fn in_memory() -> Self {
        Self {
            path: None,
            devices: Vec::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let devices = match std::fs::read(path) {
            Ok(data) => {
                let stored: StoredTrust = serde_json::from_slice(&data)
                    .map_err(|e| SecurityError::Corrupted(e.to_string()))?;
                stored.devices
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            devices,
        })
    }

    fn persist(&self) -> Result<()> {
        if let Some(path) = &self.path {
            let stored = StoredTrust {
                version: 1,
                devices: self.devices.clone(),
            };
            let json = serde_json::to_vec_pretty(&stored)
                .map_err(|e| SecurityError::Corrupted(e.to_string()))?;
            write_atomic(path, &json)?;
        }
        Ok(())
    }

    pub fn devices(&self) -> &[TrustedDevice] {
        &self.devices
    }

    pub fn get(&self, fp: &Fingerprint) -> Option<&TrustedDevice> {
        self.devices.iter().find(|d| &d.fingerprint == fp)
    }

    pub fn is_trusted(&self, fp: &Fingerprint) -> bool {
        self.get(fp).is_some()
    }

    /// Adds or renames a trusted device and saves the store.
    pub fn trust(&mut self, name: &str, fp: Fingerprint) -> Result<()> {
        let now = unix_now();
        match self.devices.iter_mut().find(|d| d.fingerprint == fp) {
            Some(d) => d.name = name.to_string(),
            None => self.devices.push(TrustedDevice {
                name: name.to_string(),
                fingerprint: fp,
                paired_at: now,
                last_seen: Some(now),
            }),
        }
        self.persist()
    }

    pub fn touch(&mut self, fp: &Fingerprint) -> Result<()> {
        if let Some(d) = self.devices.iter_mut().find(|d| &d.fingerprint == fp) {
            d.last_seen = Some(unix_now());
            self.persist()?;
        }
        Ok(())
    }

    /// Removes a device. Returns whether it was present.
    pub fn revoke(&mut self, fp: &Fingerprint) -> Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|d| &d.fingerprint != fp);
        let removed = self.devices.len() != before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_persist_revoke() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trusted.json");
        let fp = Fingerprint::of_certificate(b"device");
        {
            let mut s = TrustStore::load(&path).expect("load");
            assert!(!s.is_trusted(&fp));
            s.trust("Laptop", fp).expect("trust");
            s.trust("Laptop renamed", fp).expect("rename");
            assert_eq!(s.devices().len(), 1);
        }
        let mut s = TrustStore::load(&path).expect("reload");
        assert_eq!(s.get(&fp).map(|d| d.name.as_str()), Some("Laptop renamed"));
        assert!(s.revoke(&fp).expect("revoke"));
        assert!(!s.revoke(&fp).expect("revoke again"));
        let s = TrustStore::load(&path).expect("reload");
        assert!(!s.is_trusted(&fp));
    }

    #[test]
    fn corrupted_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trusted.json");
        std::fs::write(&path, b"{not json").expect("write");
        assert!(TrustStore::load(&path).is_err());
    }
}
