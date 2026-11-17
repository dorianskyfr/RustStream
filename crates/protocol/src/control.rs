//! Messages exchanged on the reliable control stream.
//!
//! Session establishment:
//!
//! ```text
//! client                                   host
//!   | --- QUIC + TLS 1.3, mutual certs ---> |
//!   | -- Hello ----------------------------> |  host checks the client fingerprint
//!   | <------- PairingRequired{commitment} - |  (unknown client, or client asks)
//!   | -- PairingNonce{Nc} -----------------> |
//!   | <-------------- PairingReveal{Nh} ---- |  client checks the commitment
//!   |   both show the same 6-digit code; both users confirm
//!   | <---------------------------- Welcome |  or Rejected
//!   | <======== video datagrams ============ |
//!   | ========= input stream =============> |
//! ```

use serde::{Deserialize, Serialize};

use crate::{MAX_NAME_LEN, PROTOCOL_VERSION, ProtocolError};

/// Largest frame dimension accepted on the wire (covers 8K).
pub const MAX_DIMENSION: u32 = 8192;
/// Highest frame rate accepted on the wire.
pub const MAX_FPS: u16 = 360;
/// Highest bitrate accepted on the wire, in kbit/s (2 Gbit/s).
pub const MAX_BITRATE_KBPS: u32 = 2_000_000;

/// Video codecs that may be negotiated. A peer only advertises codecs it can
/// actually encode/decode on the current hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

/// What the client asks for. The host clamps it to what it can deliver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRequest {
    /// Preferred display index on the host (0 = primary).
    pub display: u32,
    /// Upper bound for the streamed resolution (0 = native).
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
    /// Codecs the client can decode, in order of preference.
    pub codecs: Vec<VideoCodec>,
}

/// First message sent by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub device_name: String,
    pub request: StreamRequest,
    /// The client does not know this host yet and requires a pairing
    /// ceremony (code shown on both screens) even if the host already trusts
    /// the client. Without it, the client could not verify the host.
    pub request_pairing: bool,
}

/// A display available for streaming on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
}

/// Parameters of the running video stream, decided by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionParams {
    pub codec: VideoCodec,
    pub display: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

/// Why a host refused a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The peers do not speak a compatible protocol version.
    VersionMismatch,
    /// The host user refused (or did not answer) the pairing request.
    PairingDenied,
    /// The host does not accept new pairings right now.
    PairingDisabled,
    /// Another session is already active.
    Busy,
    /// Too many failed attempts from this address.
    RateLimited,
    /// No codec in common between host and client.
    NoCommonCodec,
    /// The request was malformed.
    InvalidRequest,
    /// Unexpected host-side failure (capture or encoder unavailableâ€¦).
    HostError,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::VersionMismatch => "incompatible protocol version",
            Self::PairingDenied => "pairing denied by the host",
            Self::PairingDisabled => "the host does not accept new devices",
            Self::Busy => "the host is already in a session",
            Self::RateLimited => "too many attempts, try again later",
            Self::NoCommonCodec => "no video codec in common",
            Self::InvalidRequest => "invalid request",
            Self::HostError => "host error",
        };
        f.write_str(s)
    }
}

/// Every message that can travel on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    Hello(Hello),
    /// Host → client: pairing ceremony starts. Carries the host's commitment
    /// to its random nonce (commit–reveal; see `ruststream-security`).
    PairingRequired {
        commitment: [u8; 32],
    },
    /// Client → host: the client's random pairing nonce.
    PairingNonce {
        nonce: [u8; 32],
    },
    /// Host → client: the host nonce, which must match the commitment. Both
    /// sides then display the same 6-digit code; both users must confirm.
    PairingReveal {
        nonce: [u8; 32],
    },
    Welcome {
        host_name: String,
        session: SessionParams,
        displays: Vec<DisplayInfo>,
    },
    Rejected {
        reason: RejectReason,
    },
    /// The stream parameters changed (e.g. the host display resolution
    /// changed). The next video frame is a keyframe with the new parameters.
    SessionChanged(SessionParams),
    /// Client â†’ host: the decoder lost its reference, send an IDR frame.
    RequestKeyframe,
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Periodic client statistics, used by the host for adaptation.
    ClientReport {
        frames_decoded: u32,
        frames_dropped: u32,
        decode_us_p50: u32,
    },
    /// Orderly shutdown. The sender closes the connection afterwards.
    Goodbye {
        reason: String,
    },
}

fn check_name(name: &str) -> Result<(), ProtocolError> {
    if name.len() > MAX_NAME_LEN {
        return Err(ProtocolError::Malformed("name too long"));
    }
    if name.chars().any(char::is_control) {
        return Err(ProtocolError::Malformed("control character in name"));
    }
    Ok(())
}

impl StreamRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.max_width > MAX_DIMENSION || self.max_height > MAX_DIMENSION {
            return Err(ProtocolError::Malformed("resolution out of range"));
        }
        if self.fps == 0 || self.fps > MAX_FPS {
            return Err(ProtocolError::Malformed("fps out of range"));
        }
        if self.bitrate_kbps == 0 || self.bitrate_kbps > MAX_BITRATE_KBPS {
            return Err(ProtocolError::Malformed("bitrate out of range"));
        }
        if self.codecs.is_empty() || self.codecs.len() > 8 {
            return Err(ProtocolError::Malformed("invalid codec list"));
        }
        Ok(())
    }
}

impl Hello {
    pub fn new(
        device_name: impl Into<String>,
        request: StreamRequest,
        request_pairing: bool,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            device_name: device_name.into(),
            request,
            request_pairing,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        check_name(&self.device_name)?;
        self.request.validate()
    }
}

impl SessionParams {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
        {
            return Err(ProtocolError::Malformed("resolution out of range"));
        }
        if self.fps == 0 || self.fps > MAX_FPS {
            return Err(ProtocolError::Malformed("fps out of range"));
        }
        Ok(())
    }
}

impl ControlMessage {
    /// Semantic validation applied to every decoded message, on top of the
    /// structural validation done by the decoder.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Hello(h) => h.validate(),
            Self::Welcome {
                host_name,
                session,
                displays,
            } => {
                check_name(host_name)?;
                session.validate()?;
                if displays.len() > 16 {
                    return Err(ProtocolError::Malformed("too many displays"));
                }
                displays.iter().try_for_each(|d| check_name(&d.name))
            }
            Self::SessionChanged(s) => s.validate(),
            Self::Goodbye { reason } => {
                if reason.len() > 256 {
                    Err(ProtocolError::Malformed("reason too long"))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> StreamRequest {
        StreamRequest {
            display: 0,
            max_width: 1920,
            max_height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            codecs: vec![VideoCodec::H264],
        }
    }

    #[test]
    fn hello_validation() {
        assert!(Hello::new("Desk", request(), false).validate().is_ok());
        assert!(
            Hello::new("x".repeat(65), request(), false)
                .validate()
                .is_err()
        );
        assert!(
            Hello::new("bad\u{0007}", request(), false)
                .validate()
                .is_err()
        );
        let mut r = request();
        r.fps = 0;
        assert!(Hello::new("Desk", r, false).validate().is_err());
        let mut r = request();
        r.codecs.clear();
        assert!(Hello::new("Desk", r, false).validate().is_err());
    }

    #[test]
    fn roundtrip_postcard() {
        let msg = ControlMessage::Hello(Hello::new("Desk", request(), false));
        let bytes = postcard::to_allocvec(&msg).expect("encode");
        let back: ControlMessage = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(msg, back);
    }
}
