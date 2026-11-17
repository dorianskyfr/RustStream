//! RustStream session engine.
//!
//! * [`host`]: accepts authenticated connections, runs pairing, and streams
//!   the desktop (capture → encode → datagrams) while injecting remote input.
//! * [`client`]: connects to a host, reassembles and decodes video, forwards
//!   local input.
//! * [`settings`]: persisted preferences, saved computers and streaming
//!   profiles.
//! * [`AppContext`]: identity, trust store and paths shared by the UI and CLI.

pub mod client;
pub mod host;
pub mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub use ruststream_network as network;
pub use ruststream_protocol as protocol;
use ruststream_protocol::RejectReason;
pub use ruststream_security as security;
use ruststream_security::{DeviceIdentity, SecretProtector, TrustStore};
pub use ruststream_telemetry as telemetry;

/// Errors surfaced by the engine.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("network: {0}")]
    Net(#[from] ruststream_network::NetError),
    #[error("security: {0}")]
    Security(#[from] ruststream_security::SecurityError),
    #[error("capture: {0}")]
    Capture(#[from] ruststream_capture::CaptureError),
    #[error("codec: {0}")]
    Codec(#[from] ruststream_codec::CodecError),
    #[error("protocol: {0}")]
    Protocol(#[from] ruststream_protocol::ProtocolError),
    #[error("connection rejected: {0}")]
    Rejected(RejectReason),
    #[error("timed out: {0}")]
    Timeout(&'static str),
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// Wall-clock microseconds since the Unix epoch. Used for frame timestamps so
/// that end-to-end latency can be computed when clocks are shared (loopback
/// benchmarks) or synchronised.
pub fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

/// On-disk locations.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
}

impl Paths {
    /// Per-user configuration directory (`%APPDATA%\RustStream\config` on Windows).
    pub fn user_default() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "RustStream")
            .ok_or_else(|| CoreError::Other("no home directory".into()))?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
        })
    }

    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: dir.into(),
        }
    }

    pub fn identity(&self) -> PathBuf {
        self.config_dir.join("identity.json")
    }

    /// Devices allowed to connect to this computer.
    pub fn trusted_clients(&self) -> PathBuf {
        self.config_dir.join("trusted-clients.json")
    }

    pub fn settings(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }
}

/// The platform's secret protector (DPAPI on Windows).
pub fn default_protector() -> Box<dyn SecretProtector> {
    #[cfg(windows)]
    {
        Box::new(ruststream_platform_windows::DpapiProtector)
    }
    #[cfg(not(windows))]
    {
        tracing::warn!("no OS secret store available: identity key stored unencrypted");
        Box::new(ruststream_security::PlaintextProtector)
    }
}

#[cfg(windows)]
pub use ruststream_platform_windows::{GpuInfo, GpuVendor};

/// Process-wide platform setup; call first thing in `main`.
pub fn init_platform() {
    #[cfg(windows)]
    ruststream_platform_windows::enable_dpi_awareness();
}

/// Graphics adapters of this computer.
#[cfg(windows)]
pub fn gpus() -> std::result::Result<Vec<GpuInfo>, String> {
    ruststream_platform_windows::list_gpus()
}

/// Default device name (computer name).
pub fn default_device_name() -> String {
    #[cfg(windows)]
    {
        ruststream_platform_windows::computer_name()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "RustStream device".into())
    }
}

/// State shared by the UI/CLI and the engine.
#[derive(Debug, Clone)]
pub struct AppContext {
    pub paths: Paths,
    pub identity: Arc<DeviceIdentity>,
    /// Clients allowed to connect to this computer when it hosts.
    pub trusted_clients: Arc<Mutex<TrustStore>>,
}

impl AppContext {
    pub fn load(paths: Paths, protector: &dyn SecretProtector) -> Result<Self> {
        let identity = DeviceIdentity::load_or_create(&paths.identity(), protector)?;
        let trusted = TrustStore::load(&paths.trusted_clients())?;
        Ok(Self {
            paths,
            identity: Arc::new(identity),
            trusted_clients: Arc::new(Mutex::new(trusted)),
        })
    }
}
