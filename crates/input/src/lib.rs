//! Remote input.
//!
//! * Host: [`Injector`] applies [`InputEvent`]s through an [`InjectBackend`]
//!   (`SendInput` on Windows) and tracks what is pressed, so that
//!   [`Injector::release_all`] can always restore a clean state — on
//!   disconnect, focus loss or error. No key can stay stuck on the host.
//! * Client (Windows): [`windows::KeyboardCapture`] captures hardware scan
//!   codes with a low-level keyboard hook, only while a RustStream window is
//!   in the foreground and a session is active.
//!
//! Security boundaries are respected, never bypassed: `SendInput` cannot
//! reach the secure desktop (UAC prompts, Ctrl+Alt+Del, lock screen) nor
//! windows of a higher integrity level than the host process (UIPI).

use std::collections::HashSet;

pub use ruststream_protocol::{InputEvent, MouseButton};

#[cfg(windows)]
pub mod windows;

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const ABS_MAX_F: f64 = ruststream_protocol::ABS_MAX as f64;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// The system refused the injection (e.g. secure desktop active).
    #[error("input injection blocked by the system")]
    Blocked,
    #[error("input error: {0}")]
    Other(String),
}

/// Where the host injects events.
pub trait InjectBackend: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError>;
}

/// Applies remote input and remembers pressed keys and buttons.
pub struct Injector {
    backend: Box<dyn InjectBackend>,
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<MouseButton>,
}

impl std::fmt::Debug for Injector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injector")
            .field("keys_down", &self.keys.len())
            .field("buttons_down", &self.buttons.len())
            .finish()
    }
}

impl Injector {
    pub fn new(backend: Box<dyn InjectBackend>) -> Self {
        Self {
            backend,
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }

    pub fn apply(&mut self, event: InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::ReleaseAll => return self.release_all(),
            InputEvent::Key {
                scancode,
                extended,
                down,
            } => {
                if scancode == 0 || scancode > 0x7f {
                    // Set-1 make codes are 7-bit; anything else is malformed.
                    return Err(InputError::Other(format!("invalid scancode {scancode:#x}")));
                }
                if down {
                    self.keys.insert((scancode, extended));
                } else {
                    self.keys.remove(&(scancode, extended));
                }
            }
            InputEvent::MouseButton { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            _ => {}
        }
        self.backend.inject(&event)
    }

    /// Releases everything currently pressed. Errors are logged, not
    /// propagated: releasing as much as possible is what matters.
    pub fn release_all(&mut self) -> Result<(), InputError> {
        let keys: Vec<_> = self.keys.drain().collect();
        let buttons: Vec<_> = self.buttons.drain().collect();
        for (scancode, extended) in keys {
            let ev = InputEvent::Key {
                scancode,
                extended,
                down: false,
            };
            if let Err(e) = self.backend.inject(&ev) {
                tracing::warn!(error = %e, "failed to release key");
            }
        }
        for button in buttons {
            let ev = InputEvent::MouseButton {
                button,
                down: false,
            };
            if let Err(e) = self.backend.inject(&ev) {
                tracing::warn!(error = %e, "failed to release mouse button");
            }
        }
        Ok(())
    }

    pub fn pressed_count(&self) -> usize {
        self.keys.len() + self.buttons.len()
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

/// Backend that discards events (input disabled / view-only sessions).
#[derive(Debug, Default)]
pub struct NullBackend;

impl InjectBackend for NullBackend {
    fn inject(&mut self, _event: &InputEvent) -> Result<(), InputError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<InputEvent>>>);

    impl InjectBackend for Recorder {
        fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
            self.0.lock().expect("lock").push(*event);
            Ok(())
        }
    }

    fn key(sc: u16, down: bool) -> InputEvent {
        InputEvent::Key {
            scancode: sc,
            extended: false,
            down,
        }
    }

    #[test]
    fn release_all_releases_only_pressed() {
        let rec = Recorder::default();
        let mut inj = Injector::new(Box::new(rec.clone()));
        inj.apply(key(0x1E, true)).expect("a down");
        inj.apply(key(0x1F, true)).expect("s down");
        inj.apply(key(0x1F, false)).expect("s up");
        inj.apply(InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        })
        .expect("click");
        assert_eq!(inj.pressed_count(), 2);
        rec.0.lock().expect("lock").clear();
        inj.apply(InputEvent::ReleaseAll).expect("release");
        let got = rec.0.lock().expect("lock").clone();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&key(0x1E, false)));
        assert!(got.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false
        }));
        assert_eq!(inj.pressed_count(), 0);
    }

    #[test]
    fn drop_releases_keys() {
        let rec = Recorder::default();
        {
            let mut inj = Injector::new(Box::new(rec.clone()));
            inj.apply(key(0x2A, true)).expect("shift down");
        }
        assert_eq!(rec.0.lock().expect("lock").last(), Some(&key(0x2A, false)));
    }

    #[test]
    fn rejects_invalid_scancodes() {
        let mut inj = Injector::new(Box::new(NullBackend));
        assert!(inj.apply(key(0, true)).is_err());
        assert!(inj.apply(key(0x1ff, true)).is_err());
        assert_eq!(inj.pressed_count(), 0);
    }
}
