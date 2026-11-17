//! DXGI Desktop Duplication capture (Windows 8+).
//!
//! All `unsafe` blocks in this module are COM/Win32 calls through the
//! `windows` crate. Invariants upheld throughout:
//! * every COM interface is reference counted by its Rust wrapper (RAII);
//! * out-pointers passed to Win32 point to live, correctly typed locals;
//! * `Map`/`Unmap` and `AcquireNextFrame`/`ReleaseFrame` are always paired;
//! * the device context is only used from the thread that owns `DxgiCapture`
//!   (`&mut self` methods), as required by D3D11 immediate contexts.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::core::{Interface, PCWSTR};

use crate::{CaptureError, DisplayDesc, Frame, FrameSource, even};

fn sys(e: windows::core::Error) -> CaptureError {
    CaptureError::System(e.message())
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

struct OutputRef {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    desc: DisplayDesc,
}

/// Enumerates outputs attached to the desktop, across all adapters, in a
/// stable order (adapter, then output).
fn enumerate() -> Result<Vec<OutputRef>, CaptureError> {
    // SAFETY: plain factory creation; the returned interface is owned.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(sys)?;
    let mut out = Vec::new();
    for ai in 0.. {
        // SAFETY: index-based enumeration; NOT_FOUND terminates.
        let adapter = match unsafe { factory.EnumAdapters1(ai) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(sys(e)),
        };
        // SAFETY: adapter is a valid interface.
        let adesc = unsafe { adapter.GetDesc1() }.map_err(sys)?;
        let adapter_name = wide_to_string(&adesc.Description);
        for oi in 0.. {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(oi) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(sys(e)),
            };
            // SAFETY: output is a valid interface.
            let odesc = unsafe { output.GetDesc() }.map_err(sys)?;
            if !odesc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = odesc.DesktopCoordinates;
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: DeviceName is a NUL-terminated UTF-16 buffer owned by
            // `odesc`, alive for the duration of the call; `mode` is a valid
            // DEVMODEW with dmSize set.
            let refresh_hz = if unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(odesc.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                )
            }
            .as_bool()
            {
                mode.dmDisplayFrequency
            } else {
                0
            };
            let output1: IDXGIOutput1 = output.cast().map_err(sys)?;
            out.push(OutputRef {
                adapter: adapter.clone(),
                output: output1,
                desc: DisplayDesc {
                    index: 0,
                    name: wide_to_string(&odesc.DeviceName)
                        .trim_start_matches(r"\\.\")
                        .to_string(),
                    adapter: adapter_name.clone(),
                    left: r.left,
                    top: r.top,
                    width: (r.right - r.left).unsigned_abs(),
                    height: (r.bottom - r.top).unsigned_abs(),
                    refresh_hz,
                    primary: r.left == 0 && r.top == 0,
                },
            });
        }
    }
    for (i, o) in out.iter_mut().enumerate() {
        o.desc.index = i as u32;
    }
    Ok(out)
}

pub fn list_displays() -> Result<Vec<DisplayDesc>, CaptureError> {
    Ok(enumerate()?.into_iter().map(|o| o.desc).collect())
}

/// Desktop Duplication capture of one display.
pub struct DxgiCapture {
    index: u32,
    desc: DisplayDesc,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    output: IDXGIOutput1,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    width: u32,
    height: u32,
    buf: Vec<u8>,
    last_at: Option<Instant>,
}

// SAFETY: `DxgiCapture` is moved into the capture thread and only used there
// through `&mut self`. The D3D11 device is free-threaded; the immediate
// context is never accessed concurrently because all access is exclusive.
unsafe impl Send for DxgiCapture {}

impl std::fmt::Debug for DxgiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapture")
            .field("display", &self.desc)
            .finish_non_exhaustive()
    }
}

impl DxgiCapture {
    pub fn new(index: u32) -> Result<Self, CaptureError> {
        let o = enumerate()?
            .into_iter()
            .find(|o| o.desc.index == index)
            .ok_or(CaptureError::DisplayNotFound(index))?;
        let mut device = None;
        let mut context = None;
        // SAFETY: out-pointers reference live locals; the adapter is valid.
        unsafe {
            D3D11CreateDevice(
                &o.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(sys)?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(CaptureError::System(
                "D3D11 device creation returned null".into(),
            ));
        };
        let mut cap = Self {
            index,
            width: even(o.desc.width),
            height: even(o.desc.height),
            desc: o.desc,
            device,
            context,
            duplication: None,
            output: o.output,
            staging: None,
            buf: Vec::new(),
            last_at: None,
        };
        cap.duplicate()?;
        tracing::info!(display = ?cap.desc, "DXGI desktop duplication started");
        Ok(cap)
    }

    fn duplicate(&mut self) -> Result<(), CaptureError> {
        self.duplication = None;
        // SAFETY: valid output and device interfaces.
        let dup = unsafe { self.output.DuplicateOutput(&self.device) }.map_err(|e| {
            // E_ACCESSDENIED on the secure desktop (UAC, lock screen).
            tracing::warn!(error = %e, "DuplicateOutput failed");
            CaptureError::AccessLost
        })?;
        self.duplication = Some(dup);
        Ok(())
    }

    fn ensure_staging(
        &mut self,
        src: &D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        if let Some((tex, w, h)) = &self.staging
            && *w == src.Width
            && *h == src.Height
        {
            return Ok(tex.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.Width,
            Height: src.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: `desc` is a valid staging description; out-pointer is a live local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(sys)?;
        let tex = tex.ok_or_else(|| CaptureError::System("null staging texture".into()))?;
        self.staging = Some((tex.clone(), src.Width, src.Height));
        Ok(tex)
    }

    /// Copies the acquired desktop texture into `self.buf`.
    fn read_back(&mut self, resource: &IDXGIResource) -> Result<(), CaptureError> {
        let tex: ID3D11Texture2D = resource.cast().map_err(sys)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture; `desc` is a live out-parameter.
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::UnsupportedFormat(format!(
                "{:?} (HDR desktops are not supported yet)",
                desc.Format
            )));
        }
        let staging = self.ensure_staging(&desc)?;
        // SAFETY: both textures belong to `self.device`, same size and format.
        unsafe { self.context.CopyResource(&staging, &tex) };

        let (w, h) = (even(desc.Width), even(desc.Height));
        let row = w as usize * 4;
        self.buf.resize(row * h as usize, 0);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging texture has CPU read access; paired with Unmap below.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(sys)?;
        let pitch = mapped.RowPitch as usize;
        if mapped.pData.is_null() || pitch < row {
            // SAFETY: paired with the successful Map above.
            unsafe { self.context.Unmap(&staging, 0) };
            return Err(CaptureError::System("invalid mapped texture".into()));
        }
        // SAFETY: the mapping is valid for `RowPitch * Height` bytes until
        // Unmap; we read `h <= Height` rows of `row <= RowPitch` bytes.
        let src = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize)
        };
        for (dst, src_row) in self.buf.chunks_exact_mut(row).zip(src.chunks(pitch)) {
            dst.copy_from_slice(&src_row[..row]);
        }
        // SAFETY: paired with the successful Map above; `src` is not used after.
        unsafe { self.context.Unmap(&staging, 0) };
        self.width = w;
        self.height = h;
        Ok(())
    }
}

impl FrameSource for DxgiCapture {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, CaptureError> {
        if self.duplication.is_none() {
            self.duplicate()?;
        }
        let Some(dup) = self.duplication.clone() else {
            return Err(CaptureError::AccessLost);
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: out-pointers reference live locals.
        match unsafe { dup.AcquireNextFrame(ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.duplication = None;
                return Err(CaptureError::AccessLost);
            }
            Err(e) => {
                self.duplication = None;
                return Err(sys(e));
            }
        }
        // A frame is held from here: always release it.
        let result = if info.LastPresentTime == 0 || info.AccumulatedFrames == 0 {
            // Only the pointer moved; the desktop image did not change.
            Ok(false)
        } else {
            match resource.as_ref() {
                Some(r) => self.read_back(r).map(|()| true),
                None => Ok(false),
            }
        };
        // SAFETY: paired with the successful AcquireNextFrame above.
        if let Err(e) = unsafe { dup.ReleaseFrame() } {
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                self.duplication = None;
            }
            tracing::debug!(error = %e, "ReleaseFrame failed");
        }
        if !result? {
            return Ok(None);
        }
        let at = Instant::now();
        self.last_at = Some(at);
        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            data: &self.buf,
            captured_at: at,
        }))
    }

    fn last_frame(&self) -> Option<Frame<'_>> {
        self.last_at.map(|at| Frame {
            width: self.width,
            height: self.height,
            data: &self.buf,
            captured_at: at,
        })
    }

    fn describe(&self) -> String {
        format!(
            "DXGI display {} '{}' {}x{} @ {} Hz on {}",
            self.index,
            self.desc.name,
            self.desc.width,
            self.desc.height,
            self.desc.refresh_hz,
            self.desc.adapter
        )
    }
}
