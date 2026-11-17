//! Screen capture.
//!
//! [`FrameSource`] produces tightly packed BGRA frames. Implementations:
//!
//! * [`dxgi::DxgiCapture`] (Windows): DXGI Desktop Duplication. The current
//!   implementation copies the desktop texture to a staging texture and reads
//!   it back to system memory because the only encoder implemented so far is
//!   the software OpenH264 fallback. The hardware-encoder backends will consume
//!   the GPU texture directly instead (see ARCHITECTURE.md).
//! * [`TestPattern`]: deterministic synthetic frames for tests and benchmarks
//!   on machines without a desktop (CI). It is never used for real sessions.

use std::time::{Duration, Instant};

#[cfg(windows)]
pub mod dxgi;
mod pattern;

pub use pattern::TestPattern;

/// One captured frame. `data` is BGRA8, `width * 4` bytes per row, and both
/// dimensions are even (odd desktop sizes are cropped by one pixel because
/// 4:2:0 encoders require even sizes).
#[derive(Debug)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
    pub captured_at: Instant,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("display {0} not found")]
    DisplayNotFound(u32),
    /// The desktop switched (UAC prompt, lock screen, mode change, full-screen
    /// exclusive app…). The source re-initialises itself on the next call.
    #[error("capture access lost, re-initialising")]
    AccessLost,
    #[error("unsupported desktop format: {0}")]
    UnsupportedFormat(String),
    #[error("capture is not supported on this platform")]
    Unsupported,
    #[error("system error: {0}")]
    System(String),
}

/// A source of desktop frames. Not shared between threads: a capture
/// pipeline owns it on a dedicated thread.
pub trait FrameSource: Send {
    /// Current frame size (even dimensions).
    fn size(&self) -> (u32, u32);

    /// Waits up to `timeout` for a new frame. Returns `Ok(None)` when the
    /// screen did not change. Callers that need a picture anyway (keyframe
    /// request) use [`FrameSource::last_frame`].
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, CaptureError>;

    /// The most recent frame returned by `next_frame`, if any.
    fn last_frame(&self) -> Option<Frame<'_>>;

    /// Human-readable description for logs and the UI.
    fn describe(&self) -> String;
}

/// A display that can be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayDesc {
    /// Index used to select the display (stable while the configuration
    /// does not change).
    pub index: u32,
    pub name: String,
    pub adapter: String,
    /// Position in the virtual desktop (pixels).
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub primary: bool,
}

/// Lists displays attached to the desktop.
pub fn list_displays() -> Result<Vec<DisplayDesc>, CaptureError> {
    #[cfg(windows)]
    {
        dxgi::list_displays()
    }
    #[cfg(not(windows))]
    {
        Err(CaptureError::Unsupported)
    }
}

/// Opens the capture of display `index` with the best backend available.
pub fn open_display(index: u32) -> Result<Box<dyn FrameSource>, CaptureError> {
    #[cfg(windows)]
    {
        Ok(Box::new(dxgi::DxgiCapture::new(index)?))
    }
    #[cfg(not(windows))]
    {
        let _ = index;
        Err(CaptureError::Unsupported)
    }
}

/// Rounds a dimension down to an even value (minimum 2).
pub(crate) fn even(v: u32) -> u32 {
    (v & !1).max(2)
}
