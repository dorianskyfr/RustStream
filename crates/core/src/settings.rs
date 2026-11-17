//! Persisted user preferences and saved computers.

use std::path::Path;

use ruststream_protocol::{DEFAULT_PORT, StreamRequest, VideoCodec};
use ruststream_security::Fingerprint;
use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Streaming profiles shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Profile {
    /// Balanced defaults. Dynamic adaptation is not implemented yet: today
    /// this is a fixed balanced configuration (see PROGRESS.md).
    #[default]
    Auto,
    /// Latency first: lower bitrate, frame rate up to the display refresh.
    Performance,
    /// Fidelity first: higher bitrate.
    Quality,
}

/// Fine-grained overrides ("Advanced" mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advanced {
    pub fps: u16,
    pub bitrate_kbps: u32,
}

/// A host this client paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedComputer {
    pub name: String,
    /// `host:port` or `ip:port`.
    pub address: String,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub device_name: String,
    pub theme: Theme,
    pub animations: bool,
    pub profile: Profile,
    pub advanced: Option<Advanced>,
    /// Accept incoming connections from trusted devices.
    pub hosting_enabled: bool,
    /// Accept pairing requests from new devices (each still needs approval).
    pub allow_pairing: bool,
    pub host_port: u16,
    pub computers: Vec<SavedComputer>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            device_name: crate::default_device_name(),
            theme: Theme::Dark,
            animations: true,
            profile: Profile::Auto,
            advanced: None,
            // Nothing is exposed on the network until the user opts in.
            hosting_enabled: false,
            allow_pairing: true,
            host_port: DEFAULT_PORT,
            computers: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|e| CoreError::Other(format!("settings file is corrupted: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| CoreError::Other(e.to_string()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Adds or updates a saved computer (matched by fingerprint).
    pub fn upsert_computer(&mut self, c: SavedComputer) {
        match self
            .computers
            .iter_mut()
            .find(|x| x.fingerprint == c.fingerprint)
        {
            Some(x) => *x = c,
            None => self.computers.push(c),
        }
    }

    pub fn remove_computer(&mut self, fp: &Fingerprint) {
        self.computers.retain(|c| &c.fingerprint != fp);
    }

    /// What to ask the host for, given the profile.
    pub fn stream_request(&self, display: u32) -> StreamRequest {
        let (fps, bitrate_kbps) = match (self.advanced, self.profile) {
            (Some(a), _) => (a.fps, a.bitrate_kbps),
            (None, Profile::Auto) => (60, 20_000),
            (None, Profile::Performance) => (120, 15_000),
            (None, Profile::Quality) => (60, 40_000),
        };
        StreamRequest {
            display,
            max_width: 0,
            max_height: 0,
            fps,
            bitrate_kbps,
            codecs: vec![VideoCodec::H264],
        }
    }
}

/// Parses `host[:port]` with the default port.
pub fn parse_address(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    if s.parse::<std::net::SocketAddr>().is_ok() {
        return Some(s.to_string());
    }
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Some(std::net::SocketAddr::new(ip, DEFAULT_PORT).to_string());
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0) => {
            Some(s.to_string())
        }
        Some(_) => None,
        None => Some(format!("{s}:{DEFAULT_PORT}")),
    }
}

/// Resolves `host:port` to a socket address (blocking DNS lookup).
pub fn resolve(address: &str) -> Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    address
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .or_else(|| address.to_socket_addrs().ok()?.next())
        .ok_or_else(|| CoreError::Other(format!("cannot resolve '{address}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_private() {
        let s = Settings::default();
        assert!(!s.hosting_enabled, "hosting must be opt-in");
        assert_eq!(s.theme, Theme::Dark);
    }

    #[test]
    fn save_load_and_forward_compat() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let mut s = Settings::load(&path).expect("default");
        s.profile = Profile::Quality;
        s.upsert_computer(SavedComputer {
            name: "Desk".into(),
            address: "192.168.1.2:47800".into(),
            fingerprint: Fingerprint::of_certificate(b"a"),
        });
        s.save(&path).expect("save");
        assert_eq!(Settings::load(&path).expect("load"), s);
        // Missing fields fall back to defaults.
        std::fs::write(&path, br#"{"profile":"Performance"}"#).expect("write");
        let s = Settings::load(&path).expect("partial");
        assert_eq!(s.profile, Profile::Performance);
        assert_eq!(s.host_port, DEFAULT_PORT);
    }

    #[test]
    fn profiles_map_to_valid_requests() {
        for p in [Profile::Auto, Profile::Performance, Profile::Quality] {
            let s = Settings {
                profile: p,
                ..Settings::default()
            };
            s.stream_request(0).validate().expect("valid request");
        }
    }

    #[test]
    fn address_parsing() {
        assert_eq!(
            parse_address("192.168.1.5").as_deref(),
            Some("192.168.1.5:47800")
        );
        assert_eq!(
            parse_address("192.168.1.5:9000").as_deref(),
            Some("192.168.1.5:9000")
        );
        assert_eq!(
            parse_address("desk.local").as_deref(),
            Some("desk.local:47800")
        );
        assert_eq!(
            parse_address("desk.local:1").as_deref(),
            Some("desk.local:1")
        );
        assert_eq!(parse_address("[::1]:47800").as_deref(), Some("[::1]:47800"));
        assert!(parse_address("").is_none());
        assert!(parse_address("a b").is_none());
        assert!(parse_address("host:0").is_none());
        assert!(parse_address("host:abc").is_none());
    }
}
