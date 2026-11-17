//! Windows input: `SendInput` injection (host) and low-level keyboard
//! capture (client).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ::windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Threading::GetCurrentProcessId;
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, HC_ACTION,
    HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::{ABS_MAX_F, InjectBackend, InputError, InputEvent, MouseButton};

/// Rectangle of the streamed display in virtual-desktop pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// Injects input with `SendInput`. Absolute coordinates are mapped onto the
/// streamed display inside the virtual desktop.
#[derive(Debug)]
pub struct SendInputBackend {
    display: DisplayRect,
}

impl SendInputBackend {
    pub fn new(display: DisplayRect) -> Self {
        Self { display }
    }

    pub fn set_display(&mut self, display: DisplayRect) {
        self.display = display;
    }

    /// Converts a normalised position on the display into the 0..=65535
    /// virtual-desktop coordinates expected by `MOUSEEVENTF_VIRTUALDESK`.
    fn to_virtual_desk(&self, x: u16, y: u16) -> (i32, i32) {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        map_abs(self.display, (vx, vy, vw, vh), x, y)
    }
}

/// Pure mapping used by [`SendInputBackend`]; separated for unit testing.
pub(crate) fn map_abs(d: DisplayRect, virt: (i32, i32, i32, i32), x: u16, y: u16) -> (i32, i32) {
    let (vx, vy, vw, vh) = virt;
    let px = f64::from(d.left) + f64::from(x) / ABS_MAX_F * f64::from(d.width.max(1) - 1);
    let py = f64::from(d.top) + f64::from(y) / ABS_MAX_F * f64::from(d.height.max(1) - 1);
    let nx = (px - f64::from(vx)) * ABS_MAX_F / f64::from(vw - 1);
    let ny = (py - f64::from(vy)) * ABS_MAX_F / f64::from(vh - 1);
    (
        nx.round().clamp(0.0, ABS_MAX_F) as i32,
        ny.round().clamp(0.0, ABS_MAX_F) as i32,
    )
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    // SAFETY: `inputs` is a valid slice of initialised INPUT structures and
    // the size argument is the size of one element, as required.
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        // Typically UIPI or the secure desktop.
        Err(InputError::Blocked)
    }
}

impl InjectBackend for SendInputBackend {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        let input = match *event {
            InputEvent::Key {
                scancode,
                extended,
                down,
            } => {
                let mut flags = KEYEVENTF_SCANCODE;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: scancode,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                }
            }
            InputEvent::MouseMoveAbs { x, y } => {
                let (nx, ny) = self.to_virtual_desk(x, y);
                mouse(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )
            }
            InputEvent::MouseMoveRel { dx, dy } => mouse(dx, dy, 0, MOUSEEVENTF_MOVE),
            InputEvent::MouseButton { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1),
                    (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
                    (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2),
                    (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
                };
                mouse(0, 0, data, flags)
            }
            InputEvent::Wheel { dx, dy } => {
                let mut v = Vec::with_capacity(2);
                if dy != 0 {
                    v.push(mouse(0, 0, i32::from(dy) as u32, MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    v.push(mouse(0, 0, i32::from(dx) as u32, MOUSEEVENTF_HWHEEL));
                }
                return if v.is_empty() { Ok(()) } else { send(&v) };
            }
            InputEvent::ReleaseAll => return Ok(()),
        };
        send(&[input])
    }
}

/// What the keyboard hook reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Input(InputEvent),
    /// Ctrl+Alt+Shift+Q: local escape combination (never forwarded). Lets
    /// keyboard-only users leave the captured session.
    Escape,
}

type Sink = Box<dyn Fn(HookEvent) + Send>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Bit 0: Ctrl, bit 1: Alt, bit 2: Shift.
static MODS: AtomicU8 = AtomicU8::new(0);

const SC_CTRL: u32 = 0x1D;
const SC_ALT: u32 = 0x38;
const SC_LSHIFT: u32 = 0x2A;
const SC_RSHIFT: u32 = 0x36;
const SC_Q: u32 = 0x10;

/// Whether the foreground window belongs to this process.
pub fn foreground_is_ours() -> bool {
    let mut pid = 0u32;
    // SAFETY: no preconditions; `pid` is a live out-parameter.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ACTIVE.load(Ordering::Acquire) && foreground_is_ours() {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a valid
        // KBDLLHOOKSTRUCT for the duration of the call.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // Never re-capture injected events (avoids loops with local hosts).
        if info.flags.0 & LLKHF_INJECTED.0 == 0 {
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
            let bit = match info.scanCode {
                SC_CTRL => 1,
                SC_ALT => 2,
                SC_LSHIFT | SC_RSHIFT => 4,
                _ => 0,
            };
            if bit != 0 {
                if down {
                    MODS.fetch_or(bit, Ordering::AcqRel);
                } else {
                    MODS.fetch_and(!bit, Ordering::AcqRel);
                }
            }
            let ev = if down && info.scanCode == SC_Q && MODS.load(Ordering::Acquire) == 7 {
                HookEvent::Escape
            } else {
                HookEvent::Input(InputEvent::Key {
                    scancode: (info.scanCode & 0x7f) as u16,
                    extended,
                    down,
                })
            };
            if let Ok(sink) = SINK.lock()
                && let Some(f) = sink.as_ref()
            {
                f(ev);
            }
            // Swallow: the key goes to the remote computer only.
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding to the next hook with the original arguments.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Low-level keyboard capture. Only one instance may exist at a time; it
/// must be created on a thread that runs a message loop (the UI thread) and
/// is removed on drop.
#[derive(Debug)]
pub struct KeyboardCapture {
    hook: HHOOK,
}

impl KeyboardCapture {
    pub fn install(sink: impl Fn(HookEvent) + Send + 'static) -> Result<Self, InputError> {
        if INSTALLED.swap(true, Ordering::AcqRel) {
            return Err(InputError::Other(
                "keyboard capture already installed".into(),
            ));
        }
        if let Ok(mut s) = SINK.lock() {
            *s = Some(Box::new(sink));
        }
        // SAFETY: a null module name returns the handle of the current executable.
        let module =
            unsafe { GetModuleHandleW(None) }.map_err(|e| InputError::Other(e.message()))?;
        // SAFETY: `hook_proc` has the HOOKPROC signature and lives for the
        // whole program; the hook is removed in Drop.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(hook_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        };
        match hook {
            Ok(hook) => Ok(Self { hook }),
            Err(e) => {
                INSTALLED.store(false, Ordering::Release);
                Err(InputError::Other(e.message()))
            }
        }
    }

    /// Enables or disables forwarding (e.g. session view focused or not).
    /// Disabled, every key reaches the local system normally.
    pub fn set_active(&self, active: bool) {
        ACTIVE.store(active, Ordering::Release);
        if !active {
            MODS.store(0, Ordering::Release);
        }
    }
}

impl Drop for KeyboardCapture {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
        // SAFETY: `self.hook` was returned by SetWindowsHookExW and is
        // unhooked exactly once.
        if let Err(e) = unsafe { UnhookWindowsHookEx(self.hook) } {
            tracing::warn!(error = %e, "failed to remove keyboard hook");
        }
        if let Ok(mut s) = SINK.lock() {
            *s = None;
        }
        MODS.store(0, Ordering::Release);
        INSTALLED.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_mapping_single_display() {
        let d = DisplayRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        let virt = (0, 0, 1920, 1080);
        assert_eq!(map_abs(d, virt, 0, 0), (0, 0));
        assert_eq!(map_abs(d, virt, u16::MAX, u16::MAX), (65535, 65535));
    }

    #[test]
    fn abs_mapping_second_display() {
        // Second 1920x1080 display to the right of the primary.
        let d = DisplayRect {
            left: 1920,
            top: 0,
            width: 1920,
            height: 1080,
        };
        let virt = (0, 0, 3840, 1080);
        let (x, _) = map_abs(d, virt, 0, 0);
        // Pixel 1920 of 3840 → about half of the virtual desktop.
        assert!((32700..=32800).contains(&x), "x = {x}");
        assert_eq!(map_abs(d, virt, u16::MAX, 0).0, 65535);
    }
}
