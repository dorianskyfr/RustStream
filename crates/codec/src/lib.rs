//! Video codecs.
//!
//! Backends implement [`VideoEncoder`] / [`VideoDecoder`]. Implemented today:
//!
//! | Backend | Codec | Kind | Status |
//! |---|---|---|---|
//! | OpenH264 | H.264 | **software fallback** | implemented |
//! | Media Foundation (Quick Sync / NVIDIA / AMD MFT) | H.264 | hardware | Windows, CPU BGRA → NV12 |
//! | Media Foundation decoder (D3D11/DXVA) | H.264 | hardware | Windows, NV12 GPU readback → RGBA |
//!
//! The software path is explicitly identified as such everywhere it is shown
//! (`EncoderInfo::hardware == false`).

#[cfg(windows)]
mod mf_backend;
#[cfg(windows)]
mod mf_decoder;
mod openh264_backend;

#[cfg(windows)]
pub use mf_backend::MfEncoder;
#[cfg(windows)]
pub use mf_decoder::MfDecoder;
pub use openh264_backend::{OpenH264Decoder, OpenH264Encoder};

/// Description of the selected encoder, including its runtime MFT name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub name: std::borrow::Cow<'static, str>,
    pub codec: Codec,
    pub hardware: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
}

/// Encoder settings decided by the session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub preference: EncoderPreference,
}

/// Backend selection. Hardware never silently falls back to software.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreference {
    #[default]
    Auto,
    Hardware,
    Software,
}

/// Decoder selection. Hardware requires a verified DXVA path and immediate output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DecoderPreference {
    #[default]
    Auto,
    Hardware,
    Software,
}

/// Result of encoding one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeOutcome {
    /// `out` contains an access unit (Annex-B NAL units).
    Encoded { keyframe: bool },
    /// The encoder decided not to emit anything for this frame.
    Skipped,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),
    #[error("codec backend error: {0}")]
    Backend(String),
}

/// Encodes BGRA frames into a compressed bitstream.
pub trait VideoEncoder: Send {
    fn info(&self) -> EncoderInfo;

    /// Encodes one tightly packed BGRA8 frame (`width * 4` bytes per row,
    /// even dimensions) into `out` (cleared first). A change of dimensions
    /// re-initialises the encoder and produces a keyframe.
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodeOutcome, CodecError>;
}

/// Decodes a compressed bitstream into RGBA8 pixels.
pub trait VideoDecoder: Send {
    fn name(&self) -> std::borrow::Cow<'static, str>;

    /// Decodes one access unit. On success with a picture, `out` holds
    /// `width * height * 4` RGBA bytes and the dimensions are returned.
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, CodecError>;
}

/// Tries hardware first in Auto mode, logging the reason for software fallback.
pub fn create_encoder(settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>, CodecError> {
    if settings.preference != EncoderPreference::Software {
        #[cfg(windows)]
        let hardware = MfEncoder::new(settings);
        #[cfg(not(windows))]
        let hardware: Result<OpenH264Encoder, CodecError> = Err(CodecError::Backend(
            "Media Foundation hardware encoding requires Windows".into(),
        ));
        match hardware {
            Ok(encoder) => return Ok(Box::new(encoder)),
            Err(error) if settings.preference == EncoderPreference::Hardware => return Err(error),
            Err(error) => tracing::warn!(%error, "hardware encoder unavailable; using OpenH264"),
        }
    }
    Ok(Box::new(OpenH264Encoder::new(settings)?))
}

/// Tries a probed D3D11/DXVA decoder first, logging Auto fallback reasons.
pub fn create_decoder(preference: DecoderPreference) -> Result<Box<dyn VideoDecoder>, CodecError> {
    if preference != DecoderPreference::Software {
        #[cfg(windows)]
        let hardware = MfDecoder::new();
        #[cfg(not(windows))]
        let hardware: Result<OpenH264Decoder, CodecError> = Err(CodecError::Backend(
            "Media Foundation hardware decoding requires Windows".into(),
        ));
        match hardware {
            Ok(decoder) => return Ok(Box::new(decoder)),
            Err(error) if preference == DecoderPreference::Hardware => return Err(error),
            Err(error) => tracing::warn!(%error, "hardware decoder unavailable; using OpenH264"),
        }
    }
    Ok(Box::new(OpenH264Decoder::new()?))
}
