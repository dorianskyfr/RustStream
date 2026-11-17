//! Remote input events (client → host).
//!
//! Keyboard events carry **hardware scan codes** (PS/2 set 1, as used by
//! Windows), not characters, so the host applies them with its own keyboard
//! layout — exactly like a physical keyboard plugged into the host.

use serde::{Deserialize, Serialize};

/// Absolute coordinates are normalised to `0..=ABS_MAX` over the streamed
/// display, independently of the client window size.
pub const ABS_MAX: u16 = u16::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// A key changed state. `extended` is the E0 prefix (arrows, right Ctrl…).
    Key {
        scancode: u16,
        extended: bool,
        down: bool,
    },
    /// Absolute pointer position, normalised over the streamed display.
    MouseMoveAbs {
        x: u16,
        y: u16,
    },
    /// Relative pointer motion in device units (used when the cursor is locked).
    MouseMoveRel {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel motion in Windows `WHEEL_DELTA` units (120 = one notch).
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// Release every key and button the client believes is pressed. Sent when
    /// the client window loses focus; the host also does it on disconnect.
    ReleaseAll,
}

impl InputEvent {
    /// Builds an absolute move from a position inside a `w`×`h` view.
    /// Out-of-range positions are clamped to the edges.
    pub fn abs_from_view(x: f32, y: f32, w: f32, h: f32) -> Self {
        let norm = |v: f32, max: f32| -> u16 {
            if max <= 1.0 || !v.is_finite() {
                return 0;
            }
            let t = (v / (max - 1.0)).clamp(0.0, 1.0);
            (t * f32::from(ABS_MAX)).round() as u16
        };
        Self::MouseMoveAbs {
            x: norm(x, w),
            y: norm(y, h),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_mapping_clamps() {
        assert_eq!(
            InputEvent::abs_from_view(0.0, 0.0, 100.0, 100.0),
            InputEvent::MouseMoveAbs { x: 0, y: 0 }
        );
        assert_eq!(
            InputEvent::abs_from_view(99.0, 99.0, 100.0, 100.0),
            InputEvent::MouseMoveAbs {
                x: ABS_MAX,
                y: ABS_MAX
            }
        );
        assert_eq!(
            InputEvent::abs_from_view(-5.0, 500.0, 100.0, 100.0),
            InputEvent::MouseMoveAbs { x: 0, y: ABS_MAX }
        );
        assert_eq!(
            InputEvent::abs_from_view(f32::NAN, 1.0, 0.0, 100.0),
            InputEvent::MouseMoveAbs { x: 0, y: 662 }
        );
    }
}
