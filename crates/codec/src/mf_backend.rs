//! Hardware-only Media Foundation MFTs. All COM objects live on one MTA thread.
//!
//! The public wrapper is Send without moving apartment-bound COM pointers. CPU
//! conversion and channel buffers are reused; MF owns each submitted sample
//! until it releases it (never overwrite an input sample retained by a driver).

use std::mem::ManuallyDrop;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, Ref, implement};

use crate::{Codec, CodecError, EncodeOutcome, EncoderInfo, EncoderSettings, VideoEncoder};

const TIMEOUT: Duration = Duration::from_secs(5);

fn error(e: impl std::fmt::Display) -> CodecError {
    CodecError::Backend(format!("Media Foundation: {e}"))
}

fn validate(s: EncoderSettings) -> Result<usize, CodecError> {
    if s.width < 2 || s.height < 2 || !s.width.is_multiple_of(2) || !s.height.is_multiple_of(2) {
        return Err(CodecError::InvalidFrame("dimensions must be even and >= 2"));
    }
    if s.fps == 0 || s.bitrate_kbps == 0 || s.bitrate_kbps.checked_mul(1000).is_none() {
        return Err(CodecError::InvalidFrame(
            "fps and bitrate must be positive and fit in u32 bps",
        ));
    }
    let pixels = (s.width as usize)
        .checked_mul(s.height as usize)
        .filter(|n| *n <= u32::MAX as usize / 4)
        .ok_or(CodecError::InvalidFrame("frame is too large"))?;
    Ok(pixels)
}

#[derive(Debug)]
struct Job {
    nv12: Vec<u8>,
    bits: Vec<u8>,
    settings: EncoderSettings,
    force: bool,
    name: Option<String>,
}

type Reply = (Job, Result<EncodeOutcome, CodecError>);

/// Synchronous, movable facade over an asynchronous hardware encoder MFT.
#[derive(Debug)]
pub struct MfEncoder {
    name: String,
    settings: EncoderSettings,
    nv12: Vec<u8>,
    jobs: Option<Sender<Job>>,
    replies: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
}

impl MfEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        let pixels = validate(settings)?;
        let (jobs, rx) = mpsc::channel::<Job>();
        let (tx, replies) = mpsc::channel();
        let (ready, init) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("rs-mf-encoder".into())
            .spawn(move || {
                let runtime = match Runtime::new() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let mut state = match State::enumerate(settings) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                if ready.send(Ok(state.name.clone())).is_ok() {
                    while let Ok(mut job) = rx.recv() {
                        let result = (|| {
                            if (state.settings.width, state.settings.height)
                                != (job.settings.width, job.settings.height)
                            {
                                // Release the old hardware session before opening the new one.
                                state.stop();
                                state = State::enumerate(job.settings)?;
                                job.name = Some(state.name.clone());
                                job.force = true;
                            }
                            state.encode(&job.nv12, job.force, &mut job.bits)
                        })();
                        let failed = result.is_err();
                        if tx.send((job, result)).is_err() || failed {
                            break;
                        }
                    }
                }
                drop(state);
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
            settings,
            nv12: vec![0; pixels * 3 / 2],
            jobs: Some(jobs),
            replies,
            worker: Some(worker),
        })
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn info(&self) -> EncoderInfo {
        EncoderInfo {
            name: self.name.clone().into(),
            codec: Codec::H264,
            hardware: true,
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
        out.clear();
        let settings = EncoderSettings {
            width,
            height,
            ..self.settings
        };
        let pixels = validate(settings)?;
        if bgra.len() != pixels * 4 {
            return Err(CodecError::InvalidFrame(
                "buffer size does not match dimensions",
            ));
        }
        self.nv12.resize(pixels * 3 / 2, 0);
        bgra_to_nv12(bgra, width as usize, height as usize, &mut self.nv12);
        let job = Job {
            nv12: std::mem::take(&mut self.nv12),
            bits: std::mem::take(out),
            settings,
            force: force_keyframe,
            name: None,
        };
        self.jobs
            .as_ref()
            .ok_or_else(|| error("encoder stopped"))?
            .send(job)
            .map_err(error)?;
        let (job, result) = self.replies.recv().map_err(error)?;
        self.nv12 = job.nv12;
        *out = job.bits;
        if let Some(name) = job.name {
            self.name = name;
        }
        result
    }
}

// BT.601 limited-range 4:2:0. Average each 2x2 RGB block before chroma
// conversion, matching NV12's interleaved UV plane. No temporary pixel arrays.
fn bgra_to_nv12(src: &[u8], w: usize, h: usize, dst: &mut [u8]) {
    let (y_plane, uv) = dst.split_at_mut(w * h);
    for (p, y) in src.as_chunks::<4>().0.iter().zip(y_plane) {
        *y = (((66 * i32::from(p[2]) + 129 * i32::from(p[1]) + 25 * i32::from(p[0]) + 128) >> 8)
            + 16) as u8;
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let a = (y * w + x) * 4;
            let b = a + w * 4;
            let avg = |c| {
                (i32::from(src[a + c])
                    + i32::from(src[a + 4 + c])
                    + i32::from(src[b + c])
                    + i32::from(src[b + 4 + c])
                    + 2)
                    / 4
            };
            let (r, g, b) = (avg(2), avg(1), avg(0));
            let i = y / 2 * w + x;
            uv[i] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv[i + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

pub(super) struct Runtime;
impl Runtime {
    pub(super) fn new() -> Result<Self, CodecError> {
        // SAFETY: called only on the new private worker; balanced on this thread.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(error)?;
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(error(e));
            }
        }
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: all MF objects were released first, on this same MTA thread.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

#[implement(IMFAsyncCallback)]
pub(super) struct EventCallback {
    pub(super) generator: IMFMediaEventGenerator,
    pub(super) sender: Sender<Result<u32, String>>,
}
impl IMFAsyncCallback_Impl for EventCallback_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        // SAFETY: MF supplies a live async result for this free-threaded MFT's
        // event generator. Only EndGetEvent runs here; worker rearms the callback.
        let event = unsafe {
            self.generator
                .EndGetEvent(
                    result
                        .as_ref()
                        .ok_or(windows::Win32::Foundation::E_POINTER)?,
                )
                .and_then(|e| {
                    e.GetStatus()?.ok()?;
                    e.GetType()
                })
        };
        let _ = self.sender.send(event.map_err(|e| e.to_string()));
        Ok(())
    }
}

// ShutdownObject must run even when type negotiation fails halfway through.
pub(super) struct Activation(pub(super) IMFActivate);
impl Drop for Activation {
    fn drop(&mut self) {
        // SAFETY: owned activation, called on its MTA before MFShutdown.
        unsafe {
            let _ = self.0.ShutdownObject();
        }
    }
}

struct State {
    activation: Activation,
    transform: IMFTransform,
    api: ICodecAPI,
    events: IMFMediaEventGenerator,
    callback: IMFAsyncCallback,
    event_rx: Receiver<Result<u32, String>>,
    settings: EncoderSettings,
    name: String,
    input_id: u32,
    output_id: u32,
    input_credit: usize,
    time: i64,
    headers: Vec<u8>,
    scratch: Vec<u8>,
    nal_length: usize,
    stopped: bool,
}

impl State {
    fn enumerate(settings: EncoderSettings) -> Result<Self, CodecError> {
        // SAFETY: out parameters are valid; MFTEnumEx owns the COM array until
        // we take each reference and free the array with CoTaskMemFree.
        let activations = unsafe {
            let output = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut ptr = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output),
                &mut ptr,
                &mut count,
            )
            .map_err(error)?;
            let mut list = Vec::with_capacity(count as usize);
            if !ptr.is_null() {
                for item in std::slice::from_raw_parts_mut(ptr, count as usize) {
                    if let Some(a) = item.take() {
                        list.push(a);
                    }
                }
                CoTaskMemFree(Some(ptr.cast()));
            }
            list
        };
        let mut failures = Vec::new();
        for activation in activations {
            let mut name = [0u16; 512];
            // SAFETY: the stack UTF-16 buffer is writable for its full length.
            let name = unsafe {
                activation
                    .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, None)
                    .map(|()| {
                        String::from_utf16_lossy(
                            &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
                        )
                    })
                    .unwrap_or_else(|_| "unnamed hardware H.264 MFT".into())
            };
            // Probe one real sample before selecting a candidate. This catches
            // drivers that accept settings but buffer input or ignore profile
            // constraints, so Auto can fall back before the session starts.
            let candidate =
                Self::open(Activation(activation), name.clone(), settings).and_then(|mut state| {
                    let pixels = validate(settings)?;
                    let mut black = vec![128; pixels * 3 / 2];
                    black[..pixels].fill(16);
                    state.encode(&black, true, &mut Vec::new())?;
                    state.time = 0; // First caller frame must also be an IDR.
                    Ok(state)
                });
            match candidate {
                Ok(state) => return Ok(state),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
        Err(error(if failures.is_empty() {
            "no hardware H.264 encoder MFT registered".into()
        } else {
            failures.join("; ")
        }))
    }

    fn open(
        activation: Activation,
        name: String,
        settings: EncoderSettings,
    ) -> Result<Self, CodecError> {
        // SAFETY: activation and all interfaces remain on this MTA. Attribute
        // GUIDs and media types have the required MF value types.
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject().map_err(error)?;
            let attrs = transform.GetAttributes().map_err(error)?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(error("hardware MFT does not expose the async event model"));
            }
            attrs
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(error)?;
            let api: ICodecAPI = transform.cast().map_err(error)?;
            set(&api, &CODECAPI_AVLowLatencyMode, true.into())?;
            set(
                &api,
                &CODECAPI_AVEncCommonRateControlMode,
                (eAVEncCommonRateControlMode_CBR.0 as u32).into(),
            )?;
            set(
                &api,
                &CODECAPI_AVEncCommonMeanBitRate,
                (settings.bitrate_kbps * 1000).into(),
            )?;
            // Constrained Baseline already excludes B frames and CABAC. These
            // optional controls also help drivers choose a zero-reorder path.
            let _ = set(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, 0u32.into());
            let _ = set(&api, &CODECAPI_AVEncH264CABACEnable, false.into());
            let mut input_id = 0;
            let mut output_id = 0;
            if let Err(e) = transform.GetStreamIDs(
                std::slice::from_mut(&mut input_id),
                std::slice::from_mut(&mut output_id),
            ) && e.code() != windows::Win32::Foundation::E_NOTIMPL
            {
                return Err(error(e));
            }
            let output = media_type(settings, MFVideoFormat_H264)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_kbps * 1000)
                .map_err(error)?;
            output
                .SetUINT32(
                    &MF_MT_MPEG2_PROFILE,
                    eAVEncH264VProfile_ConstrainedBase.0 as u32,
                )
                .map_err(error)?;
            transform
                .SetOutputType(output_id, &output, 0)
                .map_err(error)?;
            let input = media_type(settings, MFVideoFormat_NV12)?;
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, settings.width)
                .map_err(error)?;
            input
                .SetUINT32(&MF_MT_SAMPLE_SIZE, settings.width * settings.height * 3 / 2)
                .map_err(error)?;
            input
                .SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)
                .map_err(error)?;
            transform.SetInputType(input_id, &input, 0).map_err(error)?;
            let events: IMFMediaEventGenerator = transform.cast().map_err(error)?;
            let (sender, event_rx) = mpsc::channel();
            let callback: IMFAsyncCallback = EventCallback {
                generator: events.clone(),
                sender,
            }
            .into();
            let mut state = Self {
                activation,
                transform,
                api,
                events,
                callback,
                event_rx,
                settings,
                name,
                input_id,
                output_id,
                input_credit: 0,
                time: 0,
                headers: Vec::new(),
                scratch: Vec::new(),
                nal_length: 0,
                stopped: false,
            };
            state.read_headers()?;
            state
                .events
                .BeginGetEvent(&state.callback, None)
                .map_err(error)?;
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

    fn stop(&mut self) {
        if !self.stopped {
            // SAFETY: terminate pending callbacks before releasing the event
            // generator; ShutdownObject breaks its reference to the callback.
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
            self.stopped = true;
        }
    }

    fn next_event(&mut self, deadline: Instant) -> Result<u32, CodecError> {
        let event = self
            .event_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| error(format!("waiting for hardware MFT event: {e}")))?
            .map_err(error)?;
        // SAFETY: previous BeginGetEvent completed; exactly one request pending.
        unsafe {
            self.events
                .BeginGetEvent(&self.callback, None)
                .map_err(error)?;
        }
        Ok(event)
    }

    fn read_headers(&mut self) -> Result<(), CodecError> {
        // SAFETY: current output type and a correctly sized writable blob buffer.
        unsafe {
            let ty = self
                .transform
                .GetOutputCurrentType(self.output_id)
                .map_err(error)?;
            if let Ok(size) = ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                self.scratch.resize(size as usize, 0);
                ty.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut self.scratch, None)
                    .map_err(error)?;
                self.headers.clear();
                sequence_header(&self.scratch, &mut self.headers, &mut self.nal_length)?;
            }
        }
        Ok(())
    }

    fn encode(
        &mut self,
        nv12: &[u8],
        force: bool,
        out: &mut Vec<u8>,
    ) -> Result<EncodeOutcome, CodecError> {
        out.clear();
        let deadline = Instant::now() + TIMEOUT;
        while self.input_credit == 0 {
            let event = self.next_event(deadline)?;
            if event == METransformNeedInput.0 as u32 {
                self.input_credit += 1;
            } else if event == METransformHaveOutput.0 as u32 {
                return Err(error("unexpected output before input"));
            }
        }
        // SAFETY: input bytes are copied into a separately owned MF buffer; the
        // driver may retain it. Sample timestamps use MF's 100 ns units.
        unsafe {
            if force || self.time == 0 {
                set(&self.api, &CODECAPI_AVEncVideoForceKeyFrame, 1u32.into())?;
            }
            let sample = MFCreateSample().map_err(error)?;
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32).map_err(error)?;
            {
                let mut lock = BufferLock::new(&buffer)?;
                lock.bytes_mut(nv12.len()).copy_from_slice(nv12);
            }
            buffer.SetCurrentLength(nv12.len() as u32).map_err(error)?;
            sample.AddBuffer(&buffer).map_err(error)?;
            let duration = 10_000_000 / i64::from(self.settings.fps);
            sample.SetSampleTime(self.time).map_err(error)?;
            sample.SetSampleDuration(duration).map_err(error)?;
            self.transform
                .ProcessInput(self.input_id, &sample, 0)
                .map_err(error)?;
            self.time += duration;
            self.input_credit -= 1;
        }
        loop {
            let event = self.next_event(deadline)?;
            if event == METransformNeedInput.0 as u32 {
                self.input_credit += 1;
            }
            if event != METransformHaveOutput.0 as u32 {
                continue;
            }
            if self.output(out)? {
                break;
            }
        }
        let keyframe = nals(out).any(|n| n[0] & 31 == 5);
        if force && !keyframe {
            return Err(error("MFT did not honour force-keyframe"));
        }
        if nals(out).any(|n| n[0] & 31 == 7) {
            self.headers.clear();
            for nal in nals(out).filter(|n| matches!(n[0] & 31, 7 | 8)) {
                self.headers.extend_from_slice(&[0, 0, 0, 1]);
                self.headers.extend_from_slice(nal);
            }
        } else if keyframe {
            self.scratch.clear();
            self.scratch.extend_from_slice(&self.headers);
            self.scratch.extend_from_slice(out);
            std::mem::swap(out, &mut self.scratch);
        }
        if keyframe && (!nals(out).any(|n| n[0] & 31 == 7) || !nals(out).any(|n| n[0] & 31 == 8)) {
            return Err(error("IDR is missing SPS/PPS required by a fresh decoder"));
        }
        // Reject a driver that silently ignores the requested profile. An SPS
        // advertises Baseline (66) plus constraint_set1 for Constrained Baseline.
        for sps in nals(out).filter(|n| n[0] & 31 == 7) {
            if sps.len() < 4 || sps[1] != 66 || sps[2] & 0x40 == 0 {
                return Err(error("MFT did not emit Constrained Baseline H.264"));
            }
        }
        Ok(EncodeOutcome::Encoded { keyframe })
    }

    fn output(&mut self, out: &mut Vec<u8>) -> Result<bool, CodecError> {
        // SAFETY: output slot owns any supplied sample. ManuallyDrop fields
        // are taken on every HRESULT path, including stream changes/errors.
        unsafe {
            let info = self
                .transform
                .GetOutputStreamInfo(self.output_id)
                .map_err(error)?;
            let sample = if info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                == 0
            {
                let sample = MFCreateSample().map_err(error)?;
                sample
                    .AddBuffer(
                        &MFCreateAlignedMemoryBuffer(
                            info.cbSize,
                            info.cbAlignment.saturating_sub(1),
                        )
                        .map_err(error)?,
                    )
                    .map_err(error)?;
                Some(sample)
            } else {
                None
            };
            let mut slot = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_id,
                pSample: ManuallyDrop::new(sample),
                ..Default::default()
            };
            let result = self
                .transform
                .ProcessOutput(0, std::slice::from_mut(&mut slot), &mut 0);
            let sample = ManuallyDrop::take(&mut slot.pSample);
            drop(ManuallyDrop::take(&mut slot.pEvents));
            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    let ty = self
                        .transform
                        .GetOutputAvailableType(self.output_id, 0)
                        .map_err(error)?;
                    self.transform
                        .SetOutputType(self.output_id, &ty, 0)
                        .map_err(error)?;
                    self.read_headers()?;
                    // An async stream change consumes the event; the MFT sends
                    // another HaveOutput for the pending sample.
                    return Ok(false);
                }
                return Err(error(e));
            }
            let sample = sample.ok_or_else(|| error("MFT returned no output sample"))?;
            let buffer = sample.ConvertToContiguousBuffer().map_err(error)?;
            let length = buffer.GetCurrentLength().map_err(error)? as usize;
            let mut lock = BufferLock::new(&buffer)?;
            append_annex_b(lock.bytes_mut(length), self.nal_length, out)?;
            Ok(!out.is_empty())
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.stop();
    }
}

fn set(api: &ICodecAPI, key: &GUID, value: VARIANT) -> Result<(), CodecError> {
    // SAFETY: GUID refers to a CODECAPI property with the supplied scalar type.
    unsafe {
        api.SetValue(key, &value)
            .map_err(|e| error(format!("CODECAPI {key:?}: {e}")))
    }
}

fn media_type(s: EncoderSettings, subtype: GUID) -> Result<IMFMediaType, CodecError> {
    // SAFETY: all attributes use MF's specified GUID/u32/packed-u64 types.
    unsafe {
        let ty = MFCreateMediaType().map_err(error)?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(error)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &subtype).map_err(error)?;
        ty.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(s.width) << 32) | u64::from(s.height),
        )
        .map_err(error)?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(s.fps) << 32) | 1)
            .map_err(error)?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
            .map_err(error)?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(error)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)
            .map_err(error)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .map_err(error)?;
        Ok(ty)
    }
}

pub(super) struct BufferLock<'a> {
    buffer: &'a IMFMediaBuffer,
    ptr: *mut u8,
    capacity: usize,
}
impl<'a> BufferLock<'a> {
    pub(super) fn new(buffer: &'a IMFMediaBuffer) -> Result<Self, CodecError> {
        let mut ptr = std::ptr::null_mut();
        let mut capacity = 0;
        // SAFETY: out parameters live through Lock; Drop unlocks the buffer.
        unsafe {
            buffer
                .Lock(&mut ptr, Some(&mut capacity), None)
                .map_err(error)?;
        }
        Ok(Self {
            buffer,
            ptr,
            capacity: capacity as usize,
        })
    }
    pub(super) fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= self.capacity);
        if len == 0 {
            return &mut [];
        }
        // SAFETY: Lock supplies capacity bytes valid until Unlock; unique borrow.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, len) }
    }
}
impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        // SAFETY: exactly one successful Lock, and no slice outlives this guard.
        unsafe {
            let _ = self.buffer.Unlock();
        }
    }
}

fn append_annex_b(data: &[u8], length_size: usize, out: &mut Vec<u8>) -> Result<(), CodecError> {
    if length_size == 0 && (data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])) {
        out.extend_from_slice(data);
        return Ok(());
    }
    let length_size = if length_size == 0 { 4 } else { length_size };
    let mut rest = data;
    while !rest.is_empty() {
        if !(1..=4).contains(&length_size) || rest.len() < length_size {
            return Err(error("truncated H.264 NAL length"));
        }
        let length = rest[..length_size]
            .iter()
            .fold(0usize, |n, b| (n << 8) | usize::from(*b));
        rest = &rest[length_size..];
        if length == 0 || length > rest.len() {
            return Err(error("invalid H.264 NAL length"));
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rest[..length]);
        rest = &rest[length..];
    }
    Ok(())
}

fn sequence_header(
    data: &[u8],
    out: &mut Vec<u8>,
    length_size: &mut usize,
) -> Result<(), CodecError> {
    if data.first() != Some(&1) {
        *length_size = 0;
        return append_annex_b(data, 0, out);
    }
    if data.len() < 7 {
        return Err(error("truncated AVC configuration"));
    }
    *length_size = usize::from((data[4] & 3) + 1);
    let mut pos = 6;
    let mut count = data[5] & 31;
    for group in 0..2 {
        for _ in 0..count {
            if pos + 2 > data.len() {
                return Err(error("truncated AVC parameter length"));
            }
            let len = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
            pos += 2;
            if len == 0 || pos + len > data.len() {
                return Err(error("truncated AVC parameter set"));
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }
        if group == 0 {
            count = *data
                .get(pos)
                .ok_or_else(|| error("missing AVC PPS count"))?;
            pos += 1;
        }
    }
    Ok(())
}

pub(super) fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    fn start(data: &[u8], from: usize) -> Option<(usize, usize)> {
        (from..data.len().saturating_sub(2)).find_map(|i| {
            if data[i..].starts_with(&[0, 0, 0, 1]) {
                Some((i, i + 4))
            } else if data[i..].starts_with(&[0, 0, 1]) {
                Some((i, i + 3))
            } else {
                None
            }
        })
    }
    let mut current = start(data, 0);
    std::iter::from_fn(move || {
        loop {
            let (_, begin) = current?;
            current = start(data, begin);
            let end = current.map_or(data.len(), |(i, _)| i);
            if begin < end {
                return Some(&data[begin..end]);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EncoderPreference, OpenH264Decoder, VideoDecoder};

    #[test]
    fn hardware_roundtrip_keyframe_and_resize() {
        let settings = EncoderSettings {
            width: 320,
            height: 180,
            fps: 60,
            bitrate_kbps: 5_000,
            preference: EncoderPreference::Hardware,
        };
        let mut encoder = match crate::create_encoder(settings) {
            Ok(encoder) => encoder,
            Err(e) => {
                eprintln!("SKIP hardware round-trip: {e}");
                return;
            }
        };
        assert!(encoder.info().hardware);
        eprintln!("Hardware round-trip running: {}", encoder.info().name);
        // Creation and use on different threads exercises the Send facade.
        std::thread::spawn(move || {
            let mut decoder = OpenH264Decoder::new().expect("decoder");
            let mut bits = Vec::new();
            let mut rgba = Vec::new();
            for t in 0..16 {
                let (w, h) = if t < 8 { (320, 180) } else { (640, 360) };
                let mut src = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h {
                    for x in 0..w {
                        src.extend_from_slice(&[
                            ((x + t) % 256) as u8,
                            ((y * 2) % 256) as u8,
                            ((x + y + t) % 256) as u8,
                            255,
                        ]);
                    }
                }
                let forced = t == 4 || t == 12;
                let outcome = encoder
                    .encode(&src, w, h, forced, &mut bits)
                    .expect("hardware encode");
                let EncodeOutcome::Encoded { keyframe } = outcome else {
                    panic!("hardware skipped frame {t}")
                };
                if forced || t == 0 || t == 8 {
                    assert!(keyframe, "expected IDR at frame {t}");
                }
                assert_eq!(
                    decoder.decode(&bits, &mut rgba).expect("OpenH264 decode"),
                    Some((w, h))
                );
                let error: u64 = src
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(rgba.as_chunks::<4>().0.iter())
                    .map(|(s, d)| u64::from(s[2].abs_diff(d[0])))
                    .sum();
                let mae = error as f64 / f64::from(w * h);
                assert!(mae < 12.0, "frame {t} mean absolute red error {mae}");
                if forced {
                    let mut fresh = OpenH264Decoder::new().expect("fresh decoder");
                    assert_eq!(
                        fresh.decode(&bits, &mut rgba).expect("standalone IDR"),
                        Some((w, h))
                    );
                }
            }
            assert!(encoder.encode(&[0; 10], 64, 64, false, &mut bits).is_err());
        })
        .join()
        .expect("hardware test thread");
    }

    #[test]
    fn nv12_colour_and_chroma_average() {
        let mut dst = [0; 6];
        bgra_to_nv12(&[0; 16], 2, 2, &mut dst);
        assert_eq!(dst, [16, 16, 16, 16, 128, 128]);
        bgra_to_nv12(&[255; 16], 2, 2, &mut dst);
        assert_eq!(dst, [235, 235, 235, 235, 128, 128]);
        bgra_to_nv12(
            &[
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
            2,
            2,
            &mut dst,
        );
        assert_eq!(dst, [82, 144, 41, 235, 128, 128]);
    }

    #[test]
    fn annex_b_and_avc_configuration() {
        let mut out = Vec::new();
        let mut length_size = 0;
        sequence_header(
            &[
                1, 66, 0x40, 31, 0xff, 0xe1, 0, 4, 0x67, 66, 0x40, 31, 1, 0, 2, 0x68, 0,
            ],
            &mut out,
            &mut length_size,
        )
        .expect("avcC");
        assert_eq!(length_size, 4);
        assert_eq!(nals(&out).map(|n| n[0] & 31).collect::<Vec<_>>(), [7, 8]);
        out.clear();
        append_annex_b(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 2, 0x61, 2], 4, &mut out)
            .expect("length prefixed");
        assert_eq!(nals(&out).map(|n| n[0] & 31).collect::<Vec<_>>(), [5, 1]);
        // A 256-byte AVC NAL length starts with 00 00 01: do not mistake
        // this for an Annex-B start code when avcC specified length prefixes.
        let mut avc = vec![0x61; 260];
        avc[..4].copy_from_slice(&[0, 0, 1, 0]);
        out.clear();
        append_annex_b(&avc, 4, &mut out).expect("ambiguous prefix");
        assert_eq!(nals(&out).next().expect("NAL").len(), 256);
        assert!(append_annex_b(&[0, 0, 0, 9, 0x65], 4, &mut out).is_err());
        assert!(sequence_header(&[1, 66], &mut out, &mut length_size).is_err());
    }
}
