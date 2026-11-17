//! Windows-specific services used by RustStream.
//!
//! * [`DpapiProtector`]: encrypts secrets at rest with DPAPI, bound to the
//!   current Windows user account.
//! * [`list_gpus`]: enumerates graphics adapters (vendor detection for future
//!   hardware encoder selection).
//! * [`computer_name`]: default device name.
//! * [`HighResolutionTimer`]: 1 ms scheduler granularity while streaming.

use serde::{Deserialize, Serialize};

/// GPU vendors relevant for hardware video encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Microsoft,
    Other(u32),
}

impl GpuVendor {
    pub fn from_pci_id(id: u32) -> Self {
        match id {
            0x10DE => Self::Nvidia,
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            0x1414 => Self::Microsoft,
            other => Self::Other(other),
        }
    }

    /// Name of the vendor's hardware encoder family.
    pub fn encoder_family(&self) -> Option<&'static str> {
        match self {
            Self::Nvidia => Some("NVENC"),
            Self::Amd => Some("AMF"),
            Self::Intel => Some("Quick Sync"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vendor: GpuVendor,
    pub dedicated_vram_mb: u64,
    /// WARP / Basic Render Driver.
    pub software: bool,
}

#[cfg(windows)]
mod imp;
#[cfg(windows)]
pub use imp::{
    DpapiProtector, HighResolutionTimer, computer_name, enable_dpi_awareness, list_gpus,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_ids() {
        assert_eq!(GpuVendor::from_pci_id(0x10DE), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_pci_id(0x1002), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_pci_id(0x8086), GpuVendor::Intel);
        assert_eq!(GpuVendor::from_pci_id(0x1234), GpuVendor::Other(0x1234));
        assert_eq!(GpuVendor::Nvidia.encoder_family(), Some("NVENC"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        use ruststream_security::SecretProtector;
        let p = DpapiProtector;
        let secret = b"top secret key material";
        let enc = p.protect(secret).expect("protect");
        assert_ne!(&enc[..], &secret[..]);
        assert_eq!(p.unprotect(&enc).expect("unprotect"), secret);
        assert!(p.unprotect(b"not a dpapi blob").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn system_queries_do_not_fail() {
        assert!(!computer_name().is_empty());
        // CI runners may have no GPU besides the Basic Render Driver; only the
        // call itself must succeed.
        list_gpus().expect("enumerate adapters");
        let _t = HighResolutionTimer::new();
    }
}
