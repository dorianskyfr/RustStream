//! Software H.264 via Cisco OpenH264 (BSD-2-Clause).
//!
//! Licensing note: the `source` feature compiles OpenH264 from source. Cisco's
//! H.264 patent licence only covers Cisco-distributed binaries, so release
//! builds intended for redistribution should load Cisco's prebuilt DLL
//! (`libloading` feature). See docs/LICENSES.md.

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::{
    Codec, CodecError, EncodeOutcome, EncoderInfo, EncoderSettings, VideoDecoder, VideoEncoder,
};

fn backend(e: openh264::Error) -> CodecError {
    CodecError::Backend(e.to_string())
}

/// Low-latency software H.264 encoder.
pub struct OpenH264Encoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for OpenH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Encoder")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

fn check_dims(width: u32, height: u32) -> Result<(), CodecError> {
    if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CodecError::InvalidFrame("dimensions must be even and >= 2"));
    }
    Ok(())
}

impl OpenH264Encoder {
    pub fn new(s: EncoderSettings) -> Result<Self, CodecError> {
        check_dims(s.width, s.height)?;
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(s.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(s.fps as f32))
            // Keyframes only on demand (session start, loss recovery): periodic
            // IDR frames cause bitrate spikes, hence latency spikes.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            // Keep a steady cadence; dropping frames is the pipeline's decision.
            .skip_frames(false)
            .scene_change_detect(false)
            .background_detection(false)
            .complexity(Complexity::Low)
            .num_threads(0);
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), config).map_err(backend)?;
        Ok(Self {
            encoder,
            yuv: YUVBuffer::new(s.width as usize, s.height as usize),
            width: s.width,
            height: s.height,
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn info(&self) -> EncoderInfo {
        EncoderInfo {
            name: "OpenH264 (software)".into(),
            codec: Codec::H264,
            hardware: false,
        }
    }

    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodeOutcome, CodecError> {
        check_dims(width, height)?;
        if bgra.len() != width as usize * height as usize * 4 {
            return Err(CodecError::InvalidFrame(
                "buffer size does not match dimensions",
            ));
        }
        let mut force = force_keyframe;
        if (width, height) != (self.width, self.height) {
            self.yuv = YUVBuffer::new(width as usize, height as usize);
            self.width = width;
            self.height = height;
            force = true;
        }
        self.yuv
            .read_bgra8(BgraSliceU8::new(bgra, (width as usize, height as usize)));
        if force {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&self.yuv).map_err(backend)?;
        let keyframe = match bitstream.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
            FrameType::Skip | FrameType::Invalid => return Ok(EncodeOutcome::Skipped),
        };
        out.clear();
        bitstream.write_vec(out);
        if out.is_empty() {
            return Ok(EncodeOutcome::Skipped);
        }
        Ok(EncodeOutcome::Encoded { keyframe })
    }
}

/// Software H.264 decoder.
pub struct OpenH264Decoder {
    decoder: Decoder,
}

impl std::fmt::Debug for OpenH264Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenH264Decoder").finish_non_exhaustive()
    }
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self, CodecError> {
        // The default configuration flushes after every packet: one access
        // unit in, one picture out, no reordering delay.
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(backend)?;
        Ok(Self { decoder })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn name(&self) -> &'static str {
        "OpenH264 (software)"
    }

    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, CodecError> {
        let Some(yuv) = self.decoder.decode(data).map_err(backend)? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        out.resize(w * h * 4, 0);
        yuv.write_rgba8(out);
        Ok(Some((w as u32, h as u32)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&[
                    ((x + t) % 256) as u8,
                    ((y * 2) % 256) as u8,
                    ((x + y + t) % 256) as u8,
                    255,
                ]);
            }
        }
        v
    }

    fn settings(w: u32, h: u32) -> EncoderSettings {
        EncoderSettings {
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 5_000,
            preference: crate::EncoderPreference::Software,
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (320, 180);
        let mut enc = OpenH264Encoder::new(settings(w, h)).expect("encoder");
        let mut dec = OpenH264Decoder::new().expect("decoder");
        assert!(!enc.info().hardware);
        let mut bits = Vec::new();
        let mut rgba = Vec::new();
        for t in 0..10 {
            let src = frame(w, h, t);
            let outcome = enc.encode(&src, w, h, false, &mut bits).expect("encode");
            let EncodeOutcome::Encoded { keyframe } = outcome else {
                panic!("unexpected skip");
            };
            assert_eq!(keyframe, t == 0, "only the first frame is a keyframe");
            let dims = dec.decode(&bits, &mut rgba).expect("decode");
            assert_eq!(dims, Some((w, h)));
            // Lossy codec: compare the mean absolute error of the red channel
            // (BGRA index 2 → RGBA index 0).
            let err: u64 = src
                .as_chunks::<4>()
                .0
                .iter()
                .zip(rgba.as_chunks::<4>().0.iter())
                .map(|(s, d)| u64::from(s[2].abs_diff(d[0])))
                .sum();
            let mae = err as f64 / f64::from(w * h);
            assert!(mae < 12.0, "mean abs error too high: {mae}");
        }
    }

    #[test]
    fn forced_keyframe_and_resize() {
        let mut enc = OpenH264Encoder::new(settings(64, 64)).expect("encoder");
        let mut bits = Vec::new();
        enc.encode(&frame(64, 64, 0), 64, 64, false, &mut bits)
            .expect("encode");
        let o = enc
            .encode(&frame(64, 64, 1), 64, 64, true, &mut bits)
            .expect("encode");
        assert_eq!(o, EncodeOutcome::Encoded { keyframe: true });
        let o = enc
            .encode(&frame(128, 64, 2), 128, 64, false, &mut bits)
            .expect("encode");
        assert_eq!(o, EncodeOutcome::Encoded { keyframe: true });
    }

    #[test]
    fn rejects_invalid_frames() {
        let mut enc = OpenH264Encoder::new(settings(64, 64)).expect("encoder");
        let mut bits = Vec::new();
        assert!(enc.encode(&[0; 10], 64, 64, false, &mut bits).is_err());
        assert!(
            enc.encode(&frame(63, 64, 0), 63, 64, false, &mut bits)
                .is_err()
        );
        let mut dec = OpenH264Decoder::new().expect("decoder");
        let mut out = Vec::new();
        // Garbage must not crash the decoder.
        let _ = dec.decode(&[0, 0, 0, 1, 0xff, 0xff, 0xff], &mut out);
    }
}
