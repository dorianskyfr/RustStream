use ruststream_security::{SecretProtector, SecurityError};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIFactory1,
};
use windows::Win32::Media::{TIMERR_NOERROR, timeBeginPeriod, timeEndPeriod};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW,
};
use windows::core::{PWSTR, w};

use crate::{GpuInfo, GpuVendor};

/// Application-specific entropy so that other applications of the same user
/// calling DPAPI without it cannot decrypt our blobs by accident.
const ENTROPY: &[u8] = b"RustStream device identity v1";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr().cast_mut(),
    }
}

/// Takes ownership of a DPAPI output blob and frees it with `LocalFree`.
fn take_blob(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    if out.pbData.is_null() {
        return Vec::new();
    }
    // SAFETY: DPAPI returned `cbData` valid bytes at `pbData`, allocated with
    // LocalAlloc; we copy them before freeing exactly once.
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: pointer comes from DPAPI's LocalAlloc and is freed once.
    unsafe { LocalFree(Some(HLOCAL(out.pbData.cast()))) };
    v
}

/// DPAPI (`CryptProtectData`) bound to the current user account.
#[derive(Debug, Default, Clone, Copy)]
pub struct DpapiProtector;

impl SecretProtector for DpapiProtector {
    fn name(&self) -> &'static str {
        "dpapi-user"
    }

    fn protect(&self, plaintext: &[u8]) -> ruststream_security::Result<Vec<u8>> {
        let input = blob(plaintext);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input/entropy blobs point to live slices for the duration of
        // the call (DPAPI does not write through them); `out` is a live local.
        unsafe {
            CryptProtectData(
                &input,
                w!("RustStream"),
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(|e| SecurityError::Protect(e.message()))?;
        Ok(take_blob(&out))
    }

    fn unprotect(&self, protected: &[u8]) -> ruststream_security::Result<Vec<u8>> {
        let input = blob(protected);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        }
        .map_err(|e| SecurityError::Protect(e.message()))?;
        Ok(take_blob(&out))
    }
}

/// Enumerates graphics adapters.
pub fn list_gpus() -> Result<Vec<GpuInfo>, String> {
    // SAFETY: factory creation; the interface is owned and released on drop.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| e.message())?;
    let mut gpus = Vec::new();
    for i in 0.. {
        // SAFETY: index-based enumeration terminated by DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(i) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.message()),
        };
        // SAFETY: valid adapter.
        let d = unsafe { adapter.GetDesc1() }.map_err(|e| e.message())?;
        let end = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
        gpus.push(GpuInfo {
            name: String::from_utf16_lossy(&d.Description[..end]),
            vendor: GpuVendor::from_pci_id(d.VendorId),
            dedicated_vram_mb: (d.DedicatedVideoMemory / (1024 * 1024)) as u64,
            software: d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        });
    }
    Ok(gpus)
}

/// Makes the process per-monitor DPI aware (v2) so display sizes, capture
/// and absolute mouse coordinates are all in physical pixels. Must be called
/// before any window is created; harmless if already set (e.g. by the UI
/// toolkit or a manifest).
pub fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: no pointers; fails harmlessly if the awareness was already set.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        tracing::debug!(error = %e, "DPI awareness not changed");
    }
}

/// DNS host name of this computer, used as the default device name.
pub fn computer_name() -> String {
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has `len` UTF-16 units of capacity; `len` is updated with
    // the number of characters written (excluding the terminator).
    let ok = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut len,
        )
    };
    match ok {
        Ok(()) => String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]),
        Err(_) => "Windows PC".to_string(),
    }
}

/// Requests 1 ms timer resolution for the lifetime of the value, so frame
/// pacing sleeps are accurate. Released on drop.
#[derive(Debug)]
pub struct HighResolutionTimer {
    active: bool,
}

impl HighResolutionTimer {
    pub fn new() -> Self {
        // SAFETY: no pointers involved; paired with timeEndPeriod in Drop.
        let active = unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR;
        Self { active }
    }
}

impl Default for HighResolutionTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for HighResolutionTimer {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: matches the successful timeBeginPeriod(1).
            unsafe { timeEndPeriod(1) };
        }
    }
}

/// Matches the native title bar to the app theme: dark mode flag (Windows 10
/// 20H1+ and 11) and, on Windows 11, the caption colour (`0x00BBGGRR`, `None`
/// restores the system default). Purely cosmetic: failures are ignored.
pub fn style_title_bar(hwnd: isize, dark: bool, caption_bgr: Option<u32>) {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CAPTION_COLOR, DWMWA_COLOR_DEFAULT, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmSetWindowAttribute,
    };
    let hwnd = HWND(hwnd as *mut c_void);
    let dark_flag = i32::from(dark);
    let caption = caption_bgr.unwrap_or(DWMWA_COLOR_DEFAULT);
    // SAFETY: both attributes take a 4-byte value that outlives the call; an
    // invalid window handle only makes the call fail.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark_flag).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            std::ptr::from_ref(&caption).cast(),
            4,
        );
    }
}
