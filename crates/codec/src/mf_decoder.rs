//! D3D11/DXVA H.264 decoding with synchronous CPU RGBA readback.
//!
//! Hardware selection requires a hardware D3D device, NV12 DXVA support,
//! MF_SA_D3D11_AWARE, and decoded DXGI textures bound as D3D11_BIND_DECODER.
//! We never detach the device manager to allow an MFT's software fallback.
//! See https://learn.microsoft.com/windows/win32/medfound/supporting-direct3d-11-video-decoding-in-media-foundation.

use std::borrow::Cow;
use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_NOTIMPL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::mf_backend::{Activation, BufferLock, EventCallback, Runtime, nals};
use crate::{
    CodecError, EncoderPreference, EncoderSettings, OpenH264Encoder, VideoDecoder, VideoEncoder,
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn error(e: impl std::fmt::Display) -> CodecError {
    CodecError::Backend(format!("Media Foundation DXVA decoder: {e}"))
}

#[derive(Debug, Default)]
struct Job {
    data: Vec<u8>,
    rgba: Vec<u8>,
}
type Reply = (Job, Result<Option<(u32, u32)>, CodecError>);

/// Send facade. All MF/D3D interfaces and mapped resources stay on a private MTA.
#[derive(Debug)]
pub struct MfDecoder {
    name: String,
    input: Vec<u8>,
    jobs: Option<Sender<Job>>,
    replies: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
}

impl MfDecoder {
    pub fn new() -> Result<Self, CodecError> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (tx, replies) = mpsc::channel();
        let (ready, init) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("rs-mf-decoder".into())
            .spawn(move || {
                let runtime = match Runtime::new() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let initial = match State::enumerate() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let name = initial.name.clone();
                let activation = initial.activation.0.clone();
                let friendly = initial.friendly.clone();
                let mut state = Some(initial);
                if ready.send(Ok(name)).is_ok() {
                    while let Ok(mut job) = rx.recv() {
                        let result = (|| {
                            if state.is_none() {
                                if !nals(&job.data).any(|n| n[0] & 31 == 5) {
                                    return Err(error("waiting for an IDR after a decode error"));
                                }
                                state = Some(State::open(activation.clone(), &friendly)?);
                            }
                            state
                                .as_mut()
                                .ok_or_else(|| error("decoder stopped"))?
                                .decode(&job.data, &mut job.rgba)
                                .map(Some)
                        })();
                        if result.is_err() {
                            job.rgba.clear();
                            // A failed AU must never be returned as a future picture.
                            // Releasing the entire MFT also discards stale async events
                            // and reference surfaces. Reopen on the next IDR.
                            state.take();
                        }
                        if tx.send((job, result)).is_err() {
                            break;
                        }
                    }
                }
                drop(state);
                drop(activation);
                drop(runtime);
            })
            .map_err(error)?;
        let name = match init.recv().map_err(error)? {
            Ok(name) => name,
            Err(e) => {
                let _ = worker.join();
                return Err(e);
            }
        };
        Ok(Self {
            name,
            input: Vec::new(),
            jobs: Some(jobs),
            replies,
            worker: Some(worker),
        })
    }
}
impl Drop for MfDecoder {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl VideoDecoder for MfDecoder {
    fn name(&self) -> Cow<'static, str> {
        self.name.clone().into()
    }
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<Option<(u32, u32)>, CodecError> {
        out.clear();
        self.input.clear();
        self.input.extend_from_slice(data);
        let job = Job {
            data: std::mem::take(&mut self.input),
            rgba: std::mem::take(out),
        };
        self.jobs
            .as_ref()
            .ok_or_else(|| error("decoder stopped"))?
            .send(job)
            .map_err(error)?;
        let (job, result) = self.replies.recv().map_err(error)?;
        self.input = job.data;
        *out = job.rgba;
        result
    }
}

struct AsyncEvents {
    generator: IMFMediaEventGenerator,
    callback: IMFAsyncCallback,
    rx: Receiver<Result<u32, String>>,
    credit: usize,
}
impl AsyncEvents {
    fn new(transform: &IMFTransform) -> Result<Self, CodecError> {
        let generator: IMFMediaEventGenerator = transform.cast().map_err(error)?;
        let (sender, rx) = mpsc::channel();
        let callback: IMFAsyncCallback = EventCallback {
            generator: generator.clone(),
            sender,
        }
        .into();
        // SAFETY: exactly one outstanding request, completed by EventCallback.
        unsafe {
            generator.BeginGetEvent(&callback, None).map_err(error)?;
        }
        Ok(Self {
            generator,
            callback,
            rx,
            credit: 0,
        })
    }
    fn next(&mut self, deadline: Instant) -> Result<u32, CodecError> {
        let event = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(error)?
            .map_err(error)?;
        // SAFETY: previous callback completed; rearm once before consuming its event.
        unsafe {
            self.generator
                .BeginGetEvent(&self.callback, None)
                .map_err(error)?;
        }
        if event == METransformNeedInput.0 as u32 {
            self.credit += 1;
        }
        Ok(event)
    }
    fn need_input(&mut self, deadline: Instant) -> Result<(), CodecError> {
        while self.credit == 0 {
            if self.next(deadline)? == METransformHaveOutput.0 as u32 {
                return Err(error("unexpected queued output before this access unit"));
            }
        }
        self.credit -= 1;
        Ok(())
    }
    fn have_output(&mut self, deadline: Instant) -> Result<(), CodecError> {
        while self.next(deadline)? != METransformHaveOutput.0 as u32 {}
        Ok(())
    }
}

struct State {
    activation: Activation,
    transform: IMFTransform,
    gpu: Gpu,
    events: Option<AsyncEvents>,
    name: String,
    friendly: String,
    input_id: u32,
    output_id: u32,
    view: View,
    time: i64,
    first: bool,
}
impl State {
    fn enumerate() -> Result<Self, CodecError> {
        // The Microsoft DXVA-capable decoder is a synchronous MFT. Hardware
        // enumeration alone does not discover it; GPU output is verified below.
        // SAFETY: valid COM out parameters; take every reference before freeing
        // the MFTEnumEx task-allocated array (including rejected candidates).
        let activations = unsafe {
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut ptr = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT
                    | MFT_ENUM_FLAG_ASYNCMFT
                    | MFT_ENUM_FLAG_HARDWARE
                    | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut ptr,
                &mut count,
            )
            .map_err(error)?;
            let mut list = Vec::with_capacity(count as usize);
            if !ptr.is_null() {
                for a in std::slice::from_raw_parts_mut(ptr, count as usize) {
                    if let Some(a) = a.take() {
                        list.push(a);
                    }
                }
                CoTaskMemFree(Some(ptr.cast()));
            }
            list
        };
        let mut failures = Vec::new();
        for activation in activations {
            let mut buf = [0u16; 512];
            // SAFETY: writable stack UTF-16 buffer with its full length supplied.
            let name = unsafe {
                activation
                    .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None)
                    .map(|()| {
                        String::from_utf16_lossy(
                            &buf[..buf.iter().position(|c| *c == 0).unwrap_or(buf.len())],
                        )
                    })
                    .unwrap_or_else(|_| "unnamed H.264 decoder MFT".into())
            };
            let result = (|| {
                let mut candidate = Self::open(activation.clone(), &name)?;
                candidate.probe()?;
                drop(candidate);
                // A fresh instance prevents the probe's DPB, timestamps, output
                // format or events from affecting the caller's first access unit.
                Self::open(activation, &name)
            })();
            match result {
                Ok(state) => return Ok(state),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
        Err(error(if failures.is_empty() {
            "no DXVA-capable H.264 MFT registered".into()
        } else {
            failures.join("; ")
        }))
    }

    fn open(activation: IMFActivate, friendly: &str) -> Result<Self, CodecError> {
        let activation = Activation(activation);
        let gpu = Gpu::new()?;
        // SAFETY: activation/interfaces stay on this MTA; D3D's device manager
        // holds a reference for the MFT's lifetime and is never detached to
        // permit software fallback. Type attributes use the MF-required types.
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject().map_err(error)?;
            let attrs = transform.GetAttributes().map_err(error)?;
            if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(error("MFT is not D3D11-aware"));
            }
            let asynchronous = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if asynchronous {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(error)?;
            }
            attrs.SetUINT32(&MF_LOW_LATENCY, 1).map_err(error)?;
            let api: ICodecAPI = transform.cast().map_err(error)?;
            // Microsoft's H.264 decoder uses VT_UI4 for this property; other
            // implementations may require VT_BOOL. The output probe is decisive.
            api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(1u32))
                .or_else(|_| api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)))
                .map_err(error)?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                .map_err(error)?;
            let mut input_id = 0;
            let mut output_id = 0;
            if let Err(e) = transform.GetStreamIDs(
                std::slice::from_mut(&mut input_id),
                std::slice::from_mut(&mut output_id),
            ) && e.code() != E_NOTIMPL
            {
                return Err(error(e));
            }
            let input = MFCreateMediaType().map_err(error)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(error)?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(error)?;
            transform.SetInputType(input_id, &input, 0).map_err(error)?;
            let events = if asynchronous {
                Some(AsyncEvents::new(&transform)?)
            } else {
                None
            };
            let name = format!("{friendly} (D3D11/DXVA; {})", gpu.name);
            let mut state = Self {
                activation,
                transform,
                gpu,
                events,
                name,
                friendly: friendly.to_owned(),
                input_id,
                output_id,
                view: View::default(),
                time: 0,
                first: true,
            };
            state.negotiate()?;
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(error)?;
            state
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(error)?;
            Ok(state)
        }
    }

    fn probe(&mut self) -> Result<(), CodecError> {
        let (w, h) = (320, 180);
        let mut encoder = OpenH264Encoder::new(EncoderSettings {
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 5_000,
            preference: EncoderPreference::Software,
        })?;
        let mut frame = vec![0; (w * h * 4) as usize];
        for p in frame.as_chunks_mut::<4>().0 {
            p.copy_from_slice(&[24, 80, 160, 255]);
        }
        let mut bits = Vec::new();
        let mut rgba = Vec::new();
        for t in 0..4 {
            frame[(t * 4) as usize] = 32 + t as u8;
            encoder.encode(&frame, w, h, t == 0, &mut bits)?;
            if self.decode(&bits, &mut rgba)? != (w, h) {
                return Err(error("probe dimensions do not match"));
            }
            let mae = rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| u64::from(p[0].abs_diff(160)))
                .sum::<u64>() as f64
                / f64::from(w * h);
            if mae >= 12.0 {
                return Err(error(format!("probe RGBA mean red error {mae}")));
            }
        }
        Ok(())
    }

    fn negotiate(&mut self) -> Result<(), CodecError> {
        // SAFETY: enumerate types for this single output stream; choose only
        // NV12. Keeping the device manager attached makes unsupported DXVA
        // configurations fail, rather than renegotiating a CPU output path.
        unsafe {
            for i in 0..64 {
                let ty = self
                    .transform
                    .GetOutputAvailableType(self.output_id, i)
                    .map_err(error)?;
                if ty.GetGUID(&MF_MT_SUBTYPE).map_err(error)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.transform
                    .SetOutputType(self.output_id, &ty, 0)
                    .map_err(error)?;
                let info = self
                    .transform
                    .GetOutputStreamInfo(self.output_id)
                    .map_err(error)?;
                if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                    return Err(error("MFT does not provide DXGI output samples"));
                }
                self.view = View::from_type(&ty)?;
                return Ok(());
            }
        }
        Err(error("no NV12 output type"))
    }

    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(u32, u32), CodecError> {
        out.clear();
        if data.len() > u32::MAX as usize {
            return Err(error("access unit too large"));
        }
        let idr = nals(data).any(|n| n[0] & 31 == 5);
        if !nals(data).any(|n| matches!(n[0] & 31, 1 | 5))
            || nals(data).any(|n| n.len() < 2 || n[0] & 0x80 != 0)
        {
            return Err(error("malformed Annex-B picture access unit"));
        }
        if self.first && !idr {
            return Err(error("decoder requires an initial IDR"));
        }
        let started = Instant::now();
        let deadline = started + EVENT_TIMEOUT;
        if let Some(events) = &mut self.events {
            events.need_input(deadline)?;
        }
        // SAFETY: independent MF-owned input buffer; the MFT may retain it.
        // Output timestamps must match this AU, ruling out reordered pictures.
        unsafe {
            let sample = MFCreateSample().map_err(error)?;
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(error)?;
            {
                BufferLock::new(&buffer)?
                    .bytes_mut(data.len())
                    .copy_from_slice(data);
            }
            buffer.SetCurrentLength(data.len() as u32).map_err(error)?;
            sample.AddBuffer(&buffer).map_err(error)?;
            sample.SetSampleTime(self.time).map_err(error)?;
            sample.SetSampleDuration(166_667).map_err(error)?;
            if self.first {
                sample
                    .SetUINT32(&MFSampleExtension_Discontinuity, 1)
                    .map_err(error)?;
            }
            if idr {
                sample
                    .SetUINT32(&MFSampleExtension_CleanPoint, 1)
                    .map_err(error)?;
            }
            self.transform
                .ProcessInput(self.input_id, &sample, 0)
                .map_err(error)?;
        }
        for _ in 0..8 {
            if let Some(events) = &mut self.events {
                events.have_output(deadline)?;
            }
            let mut slot = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                ..Default::default()
            };
            // SAFETY: take both COM out fields on every HRESULT path; the
            // MFT allocates GPU samples, never caller-provided CPU samples.
            let (result, sample) = unsafe {
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut slot), &mut 0);
                let sample = ManuallyDrop::take(&mut slot.pSample);
                drop(ManuallyDrop::take(&mut slot.pEvents));
                (result, sample)
            };
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    self.negotiate()?;
                    continue;
                }
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Err(error(
                        "MFT retained this access unit instead of producing its picture immediately",
                    ));
                }
                return Err(error(e));
            }
            let sample = sample.ok_or_else(|| error("MFT returned no picture"))?;
            // SAFETY: live owned output sample; timestamp and buffer index are
            // read while the sample (and hence its reference surface) is retained.
            unsafe {
                if sample
                    .GetUINT32(&MFSampleExtension_FrameCorruption)
                    .unwrap_or(0)
                    != 0
                {
                    return Err(error("MFT detected corruption in the decoded picture"));
                }
                if sample.GetSampleTime().map_err(error)? != self.time {
                    return Err(error("MFT reordered the access unit"));
                }
                let buffer = sample.GetBufferByIndex(0).map_err(error)?;
                tracing::debug!(
                    processing_us = started.elapsed().as_micros() as u64,
                    "MF decoded access unit"
                );
                self.gpu.readback(&buffer, self.view, out)?;
            }
            self.time += 166_667;
            self.first = false;
            return Ok((self.view.width, self.view.height));
        }
        Err(error("too many consecutive output format changes"))
    }
}
impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: terminate streaming and cancel pending async callbacks before
        // any MF/D3D objects are dropped and before the worker calls MFShutdown.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.activation.0.ShutdownObject();
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct View {
    width: u32,
    height: u32,
    x: u32,
    y: u32,
}
impl View {
    fn from_type(ty: &IMFMediaType) -> Result<Self, CodecError> {
        // SAFETY: packed UINT64 frame size; MFVideoArea is an integer-only C
        // structure, read unaligned from an exactly-sized attribute blob.
        unsafe {
            let size = ty.GetUINT64(&MF_MT_FRAME_SIZE).map_err(error)?;
            let mut view = Self {
                width: (size >> 32) as u32,
                height: size as u32,
                x: 0,
                y: 0,
            };
            for key in [MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_GEOMETRIC_APERTURE] {
                if ty.GetBlobSize(&key).unwrap_or(0) as usize != std::mem::size_of::<MFVideoArea>()
                {
                    continue;
                }
                let mut blob = [0u8; std::mem::size_of::<MFVideoArea>()];
                ty.GetBlob(&key, &mut blob, None).map_err(error)?;
                let area = std::ptr::read_unaligned(blob.as_ptr().cast::<MFVideoArea>());
                if area.OffsetX.fract != 0
                    || area.OffsetY.fract != 0
                    || area.OffsetX.value < 0
                    || area.OffsetY.value < 0
                    || area.Area.cx <= 0
                    || area.Area.cy <= 0
                {
                    return Err(error("invalid/fractional display aperture"));
                }
                view = Self {
                    width: area.Area.cx as u32,
                    height: area.Area.cy as u32,
                    x: area.OffsetX.value as u32,
                    y: area.OffsetY.value as u32,
                };
                break;
            }
            if view.width == 0
                || view.height == 0
                || view.width > 16384
                || view.height > 16384
                || !view.width.is_multiple_of(2)
                || !view.height.is_multiple_of(2)
                || !view.x.is_multiple_of(2)
                || !view.y.is_multiple_of(2)
            {
                return Err(error("unsupported NV12 display dimensions"));
            }
            Ok(view)
        }
    }
}

struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
    staging: Option<ID3D11Texture2D>,
    staging_size: (u32, u32),
    nv12: Vec<u8>,
    name: String,
}
impl Gpu {
    fn new() -> Result<Self, CodecError> {
        // SAFETY: hardware-only device, valid out parameters, MTA lifetime.
        // The immediate context is protected because the MFT also uses it.
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(error)?;
            let device = device.ok_or_else(|| error("no D3D11 hardware device"))?;
            let context = context.ok_or_else(|| error("no D3D11 immediate context"))?;
            let multithread: ID3D10Multithread = device.cast().map_err(error)?;
            let _ = multithread.SetMultithreadProtected(true);
            let video: ID3D11VideoDevice = device.cast().map_err(error)?;
            if !video
                .CheckVideoDecoderFormat(&D3D11_DECODER_PROFILE_H264_VLD_NOFGT, DXGI_FORMAT_NV12)
                .map_err(error)?
                .as_bool()
            {
                return Err(error("hardware device has no H.264 VLD NV12 decoder"));
            }
            let dxgi: IDXGIDevice = device.cast().map_err(error)?;
            let desc = dxgi.GetAdapter().map_err(error)?.GetDesc().map_err(error)?;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            let mut manager = None;
            let mut token = 0;
            MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(error)?;
            let manager = manager.ok_or_else(|| error("no DXGI device manager"))?;
            manager.ResetDevice(&device, token).map_err(error)?;
            Ok(Self {
                device,
                context,
                manager,
                staging: None,
                staging_size: (0, 0),
                nv12: Vec::new(),
                name,
            })
        }
    }

    fn readback(
        &mut self,
        buffer: &IMFMediaBuffer,
        view: View,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        // SAFETY: the output sample retains the decoder texture throughout the
        // copy/map. Only its reported array subresource is copied. GPU mapping
        // is guarded by RAII, including on validation or conversion errors.
        unsafe {
            let dxgi: IMFDXGIBuffer = buffer
                .cast()
                .map_err(|e| error(format!("MFT returned CPU output, not a DXVA surface: {e}")))?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                .map_err(error)?;
            let texture = ID3D11Texture2D::from_raw(raw);
            let index = dxgi.GetSubresourceIndex().map_err(error)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_NV12 || desc.BindFlags & D3D11_BIND_DECODER.0 as u32 == 0
            {
                return Err(error("output is not an NV12 DXVA decoder texture"));
            }
            if view.x + view.width > desc.Width
                || view.y + view.height > desc.Height
                || desc.Height == 0
                || !desc.Height.is_multiple_of(2)
                || index >= desc.ArraySize * desc.MipLevels
            {
                return Err(error(
                    "display aperture or subresource exceeds the decoder texture",
                ));
            }
            if desc.MipLevels != 1
                || desc.SampleDesc.Count != 1
                || texture.GetDevice().map_err(error)? != self.device
            {
                return Err(error("unsupported decoder texture layout/device"));
            }
            if self.staging_size != (desc.Width, desc.Height) {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    MipLevels: 1,
                    ArraySize: 1,
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut staging = None;
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .map_err(error)?;
                self.staging = staging;
                self.staging_size = (desc.Width, desc.Height);
            }
            let staging = self
                .staging
                .as_ref()
                .ok_or_else(|| error("no staging texture"))?;
            let readback_start = Instant::now();
            self.context
                .CopySubresourceRegion(staging, 0, 0, 0, 0, &texture, index, None);
            let mapped = Mapped::new(&self.context, staging)?;
            let pitch = mapped.data.RowPitch as usize;
            if pitch < desc.Width as usize {
                return Err(error("NV12 row pitch is too small"));
            }
            let bytes = std::slice::from_raw_parts(
                mapped.data.pData.cast::<u8>(),
                pitch * desc.Height as usize * 3 / 2,
            );
            out.resize(view.width as usize * view.height as usize * 4, 0);
            // Read driver-mapped memory once in bulk; perform colour arithmetic
            // on a reusable ordinary CPU buffer, after releasing the mapping.
            self.nv12.resize(bytes.len(), 0);
            self.nv12.copy_from_slice(bytes);
            drop(mapped);
            let copy_map_us = readback_start.elapsed().as_micros() as u64;
            let conversion_start = Instant::now();
            nv12_to_rgba(&self.nv12, pitch, desc.Height as usize, view, out);
            tracing::debug!(
                copy_map_us,
                conversion_us = conversion_start.elapsed().as_micros() as u64,
                "D3D11 decoder readback"
            );
            Ok(())
        }
    }
}

struct Mapped<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
    data: D3D11_MAPPED_SUBRESOURCE,
}
impl<'a> Mapped<'a> {
    fn new(
        context: &'a ID3D11DeviceContext,
        texture: &'a ID3D11Texture2D,
    ) -> Result<Self, CodecError> {
        let mut data = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging texture was created CPU-readable; Drop unmaps once.
        unsafe {
            context
                .Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut data))
                .map_err(error)?;
        }
        Ok(Self {
            context,
            texture,
            data,
        })
    }
}
impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        // SAFETY: one successful Map, with no borrowed pixels remaining.
        unsafe {
            self.context.Unmap(self.texture, 0);
        }
    }
}

// BT.601 limited range. Share chroma terms for each horizontal pixel pair;
// padding and display-aperture offsets never enter the tightly packed RGBA.
fn nv12_to_rgba(src: &[u8], pitch: usize, coded_height: usize, view: View, dst: &mut [u8]) {
    let width = view.width as usize;
    for (y, row) in dst.chunks_exact_mut(width * 4).enumerate() {
        let sy = y + view.y as usize;
        let luma = &src[sy * pitch + view.x as usize..][..width];
        let chroma = &src[(coded_height + sy / 2) * pitch + view.x as usize..][..width];
        #[cfg(target_arch = "x86_64")]
        let head = {
            let head = width / 8 * 8;
            for ((ys, uv), pixels) in luma[..head]
                .as_chunks::<8>()
                .0
                .iter()
                .zip(chroma[..head].as_chunks::<8>().0)
                .zip(row[..head * 4].as_chunks_mut::<32>().0)
            {
                rgba8_sse2(ys, uv, pixels);
            }
            head
        };
        #[cfg(not(target_arch = "x86_64"))]
        let head = 0;
        for ((ys, uv), pixels) in luma[head..]
            .as_chunks::<2>()
            .0
            .iter()
            .zip(chroma[head..].as_chunks::<2>().0)
            .zip(row[head * 4..].as_chunks_mut::<8>().0)
        {
            let u = i32::from(uv[0]) - 128;
            let v = i32::from(uv[1]) - 128;
            let (r, g, b) = (409 * v, -100 * u - 208 * v, 516 * u);
            for (y, p) in ys.iter().zip(pixels.as_chunks_mut::<4>().0) {
                let c = 298 * (i32::from(*y) - 16);
                p.copy_from_slice(&[
                    ((c + r + 128) >> 8).clamp(0, 255) as u8,
                    ((c + g + 128) >> 8).clamp(0, 255) as u8,
                    ((c + b + 128) >> 8).clamp(0, 255) as u8,
                    255,
                ]);
            }
        }
    }
}

// Exact 32-bit BT.601 arithmetic, eight pixels at a time. SSE2 is part of the
// x86-64 baseline. Other architectures and the row tail use the scalar loop.
#[cfg(target_arch = "x86_64")]
fn rgba8_sse2(y: &[u8; 8], uv: &[u8; 8], dst: &mut [u8; 32]) {
    use std::arch::x86_64::*;
    // SAFETY: x86-64 guarantees SSE2. Fixed arrays provide eight readable bytes
    // for each unaligned 64-bit load and 32 writable bytes for the two unaligned
    // stores. All remaining operations manipulate registers, without pointers.
    unsafe {
        let zero = _mm_setzero_si128();
        let y = _mm_sub_epi16(
            _mm_unpacklo_epi8(_mm_loadl_epi64(y.as_ptr().cast()), zero),
            _mm_set1_epi16(16),
        );
        let uv = _mm_loadl_epi64(uv.as_ptr().cast());
        let u = _mm_sub_epi16(_mm_and_si128(uv, _mm_set1_epi16(255)), _mm_set1_epi16(128));
        let v = _mm_sub_epi16(_mm_srli_epi16::<8>(uv), _mm_set1_epi16(128));
        let u = _mm_unpacklo_epi16(u, u);
        let v = _mm_unpacklo_epi16(v, v);
        let convert = |y, uv| {
            let base = _mm_add_epi32(_mm_madd_epi16(y, _mm_set1_epi32(298)), _mm_set1_epi32(128));
            [
                _mm_srai_epi32::<8>(_mm_add_epi32(
                    base,
                    _mm_madd_epi16(uv, _mm_set1_epi32(409 << 16)),
                )),
                _mm_srai_epi32::<8>(_mm_add_epi32(
                    base,
                    _mm_madd_epi16(
                        uv,
                        _mm_setr_epi16(-100, -208, -100, -208, -100, -208, -100, -208),
                    ),
                )),
                _mm_srai_epi32::<8>(_mm_add_epi32(base, _mm_madd_epi16(uv, _mm_set1_epi32(516)))),
            ]
        };
        let low = convert(_mm_unpacklo_epi16(y, zero), _mm_unpacklo_epi16(u, v));
        let high = convert(_mm_unpackhi_epi16(y, zero), _mm_unpackhi_epi16(u, v));
        let r = _mm_packus_epi16(_mm_packs_epi32(low[0], high[0]), zero);
        let g = _mm_packus_epi16(_mm_packs_epi32(low[1], high[1]), zero);
        let b = _mm_packus_epi16(_mm_packs_epi32(low[2], high[2]), zero);
        let rg = _mm_unpacklo_epi8(r, g);
        let ba = _mm_unpacklo_epi8(b, _mm_set1_epi8(-1));
        _mm_storeu_si128(dst.as_mut_ptr().cast(), _mm_unpacklo_epi16(rg, ba));
        _mm_storeu_si128(dst.as_mut_ptr().add(16).cast(), _mm_unpackhi_epi16(rg, ba));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecoderPreference, EncodeOutcome};

    fn frame(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut src = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                src.extend_from_slice(&[
                    ((x + t * 13) % 256) as u8,
                    ((y * 2 + t * 13) % 256) as u8,
                    ((x + y + t * 13) % 256) as u8,
                    255,
                ]);
            }
        }
        src
    }

    #[test]
    fn hardware_decoder_roundtrip_resize_and_loss_recovery() {
        let mut decoder = match crate::create_decoder(DecoderPreference::Hardware) {
            Ok(decoder) => decoder,
            Err(e) => {
                eprintln!("SKIP hardware decoder round-trip: {e}");
                return;
            }
        };
        eprintln!("Hardware decoder round-trip running: {}", decoder.name());
        assert!(decoder.name().contains("D3D11/DXVA"));
        // Construct in one thread and use in another, exercising the Send facade.
        std::thread::spawn(move || {
            let mut encoder = OpenH264Encoder::new(EncoderSettings {
                width: 320,
                height: 180,
                fps: 60,
                bitrate_kbps: 5_000,
                preference: EncoderPreference::Software,
            })
            .expect("software encoder");
            let mut bits = Vec::new();
            let mut rgba = Vec::new();
            for t in 0..16 {
                let (w, h) = if t < 8 { (320, 180) } else { (640, 360) };
                let src = frame(w, h, t);
                let forced = matches!(t, 4 | 7 | 12);
                let outcome = encoder
                    .encode(&src, w, h, forced, &mut bits)
                    .expect("encode");
                if t == 2 {
                    // Lose a P frame; do not feed any replacement AU.
                    assert_eq!(outcome, EncodeOutcome::Encoded { keyframe: false });
                    continue;
                }
                if t == 3 {
                    // Decoder may reject or conceal the broken reference chain.
                    let _ = decoder.decode(&bits, &mut rgba);
                    continue;
                }
                if t == 5 {
                    // Truncated P AU must report an error and discard old state.
                    assert_eq!(outcome, EncodeOutcome::Encoded { keyframe: false });
                    assert!(
                        decoder
                            .decode(&[0, 0, 0, 1, 0x61, 0xff], &mut rgba)
                            .is_err()
                    );
                    assert!(rgba.is_empty());
                    continue;
                }
                if t == 6 {
                    assert!(
                        decoder.decode(&bits, &mut rgba).is_err(),
                        "must wait for IDR after an error"
                    );
                    continue;
                }
                // Each picture, including first/forced/resized IDRs, must be
                // returned in this call, without a future AU or a drain command.
                assert_eq!(
                    decoder.decode(&bits, &mut rgba).expect("DXVA decode"),
                    Some((w, h)),
                    "frame {t}"
                );
                assert_eq!(rgba.len(), (w * h * 4) as usize);
                let mut errors = [0u64; 3];
                for (s, d) in src.as_chunks::<4>().0.iter().zip(rgba.as_chunks::<4>().0) {
                    for c in 0..3 {
                        errors[c] += u64::from(s[2 - c].abs_diff(d[c]));
                    }
                    assert_eq!(d[3], 255);
                }
                for e in errors {
                    let mae = e as f64 / f64::from(w * h);
                    assert!(mae < 12.0, "frame {t} RGB mean absolute error {mae}");
                }
            }
        })
        .join()
        .expect("hardware decoder test thread");
    }

    #[test]
    fn nv12_rgba_stride_padding_and_crop() {
        let mut src = [0u8; 48]; // pitch=8, coded height=4; UV follows all four Y rows.
        for row in 2..4 {
            src[row * 8 + 2] = 16;
            src[row * 8 + 3] = 235;
        }
        src[42..44].copy_from_slice(&[128, 128]);
        let mut rgba = [0; 16];
        nv12_to_rgba(
            &src,
            8,
            4,
            View {
                width: 2,
                height: 2,
                x: 2,
                y: 2,
            },
            &mut rgba,
        );
        assert_eq!(
            rgba,
            [
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255
            ]
        );
        // Explicitly catch RGB/BGR swaps and chroma clipping.
        src[18..20].fill(82);
        src[26..28].fill(82);
        src[42..44].copy_from_slice(&[90, 240]);
        nv12_to_rgba(
            &src,
            8,
            4,
            View {
                width: 2,
                height: 2,
                x: 2,
                y: 2,
            },
            &mut rgba,
        );
        for p in rgba.as_chunks::<4>().0 {
            assert!(p[0] >= 254 && p[1] <= 1 && p[2] <= 1 && p[3] == 255);
        }
        // Compare packed SIMD lanes and scalar tails byte-for-byte with an
        // independent per-pixel reference, including unaligned cropped rows.
        for width in [2usize, 8, 10, 18, 320] {
            let pitch = width + 10;
            let coded_height = 8;
            let src: Vec<u8> = (0..pitch * coded_height * 3 / 2)
                .map(|i| ((i * 73 + 19) % 256) as u8)
                .collect();
            let mut dst = vec![0; width * 4 * 4];
            nv12_to_rgba(
                &src,
                pitch,
                coded_height,
                View {
                    width: width as u32,
                    height: 4,
                    x: 2,
                    y: 2,
                },
                &mut dst,
            );
            for y in 0..4 {
                for x in 0..width {
                    let sy = y + 2;
                    let sx = x + 2;
                    let c = i32::from(src[sy * pitch + sx]) - 16;
                    let uv = (coded_height + sy / 2) * pitch + sx / 2 * 2;
                    let u = i32::from(src[uv]) - 128;
                    let v = i32::from(src[uv + 1]) - 128;
                    let expected = [
                        ((298 * c + 409 * v + 128) >> 8).clamp(0, 255) as u8,
                        ((298 * c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8,
                        ((298 * c + 516 * u + 128) >> 8).clamp(0, 255) as u8,
                        255,
                    ];
                    let p = (y * width + x) * 4;
                    assert_eq!(dst[p..p + 4], expected, "width {width}, ({x},{y})");
                }
            }
        }
    }
}
