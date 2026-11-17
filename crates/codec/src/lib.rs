//! Video codecs.
//!
//! Backends implement [`VideoEncoder`] / [`VideoDecoder`]. Implemented today:
//!
//! | Backend | Codec | Kind | Status |
//! |---|---|---|---|
//! | OpenH264 | H.264 | **software fallback** | implemented |
//! | NVENC / AMF / Quick Sync | H.264/HEVC/AV1 | hardware | not implemented yet |
//!
//! The software path is explicitly identified as such everywhere it is shown
//! (`EncoderInfo::hardware == false`).

mod openh264_backend;

pub use openh264_backend::{OpenH264Decoder, OpenH264Encoder};

/// Static description of an encoder backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderInfo {
    pub name: &'static str,
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
    fn name(&self) -> &'static str;

    /// Decodes one access unit. On success with a picture, `out` holds
    /// `width * height * 4` RGBA bytes and the dimensions are returned.
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, CodecError>;
}

/// Creates the best available encoder. Today this is always the software
/// OpenH264 backend.
pub fn create_encoder(settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>, CodecError> {
    Ok(Box::new(OpenH264Encoder::new(settings)?))
}

/// Creates the best available decoder for H.264.
pub fn create_decoder() -> Result<Box<dyn VideoDecoder>, CodecError> {
    Ok(Box::new(OpenH264Decoder::new()?))
}
