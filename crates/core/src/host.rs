//! Host side: accepts authenticated clients and streams the desktop.
//!
//! Threads/tasks per session:
//! * `rs-host-video` (OS thread): capture → encode → `send_datagram`. Paced
//!   at the negotiated frame rate; the newest desktop image is always the one
//!   encoded at each deadline, and a changed image is never left unsent.
//! * control reader / writer tasks (reliable bidirectional stream);
//! * input task: applies remote input; dropping it releases every pressed
//!   key and button.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ruststream_capture::{CaptureError, DisplayDesc, FrameSource, TestPattern};
pub use ruststream_codec::EncoderPreference;
use ruststream_codec::{EncodeOutcome, EncoderSettings, VideoEncoder};
use ruststream_input::{InjectBackend, Injector, NullBackend};
use ruststream_network::stream::{recv_msg, send_msg};
use ruststream_network::transport::{self, close_code};
use ruststream_network::{Connection, Endpoint, NetError, RecvStream, SendStream};
use ruststream_protocol::video::FrameMeta;
use ruststream_protocol::{
    ControlMessage, DisplayInfo, Hello, InputEvent, PROTOCOL_VERSION, RejectReason, SessionParams,
    VideoCodec,
};
use ruststream_security::{
    AttemptLimiter, DeviceIdentity, Fingerprint, TrustStore, commitment, random_nonce, sas_code,
};
use ruststream_telemetry::{RateMeter, Summary, Window};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use crate::{AppContext, CoreError, Result, unix_micros};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the host streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// The real desktop (display chosen by the client request).
    Desktop,
    /// Synthetic frames (tests, benchmarks). Never used by the desktop app.
    TestPattern { width: u32, height: u32 },
}

/// Whether remote input is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Inject,
    /// View-only (and tests).
    Ignore,
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub encoder_preference: EncoderPreference,
    pub device_name: String,
    pub bind: SocketAddr,
    pub allow_pairing: bool,
    pub source: SourceKind,
    pub input: InputMode,
    /// How long the host user has to answer a pairing request.
    pub pairing_timeout: Duration,
}

/// A device asks to pair. Show `code` and ask the user; answer with
/// [`PairingRequest::respond`]. Dropping it denies the request.
#[derive(Debug)]
pub struct PairingRequest {
    /// Unique per request; matches [`HostEvent::PairingCancelled::id`].
    pub id: u64,
    pub peer_name: String,
    pub fingerprint: Fingerprint,
    pub code: String,
    pub address: SocketAddr,
    responder: oneshot::Sender<bool>,
}

impl PairingRequest {
    pub fn respond(self, accept: bool) {
        let _ = self.responder.send(accept);
    }
}

/// Host-side statistics, emitted about once per second during a session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostStats {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bitrate_mbps: f64,
    pub encode_us: Summary,
    pub rtt_ms: f64,
}

#[derive(Debug)]
pub enum HostEvent {
    Listening(SocketAddr),
    PairingRequest(PairingRequest),
    /// The pairing request for this device expired or the client left.
    PairingCancelled {
        id: u64,
        fingerprint: Fingerprint,
    },
    Paired {
        name: String,
        fingerprint: Fingerprint,
    },
    SessionStarted {
        peer_name: String,
        fingerprint: Fingerprint,
        params: SessionParams,
        encoder: String,
    },
    Stats(HostStats),
    SessionEnded {
        peer_name: String,
        reason: String,
    },
    Warning(String),
}

/// Running host. Dropping the handle does not stop it; call [`HostHandle::shutdown`].
pub struct HostHandle {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for HostHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostHandle")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl HostHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether new (unknown) devices may request pairing.
    pub fn set_allow_pairing(&self, allow: bool) {
        self.shared.allow_pairing.store(allow, Ordering::Release);
    }

    /// Ends the active session, if any (the host user's "end session" button).
    pub fn end_session(&self) {
        if let Ok(active) = self.shared.active.lock()
            && let Some(conn) = active.as_ref()
        {
            conn.close(close_code::NORMAL.into(), b"ended by the host user");
        }
    }

    /// Stops accepting connections, ends the active session and waits.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

struct Shared {
    cfg: HostConfig,
    identity: Arc<DeviceIdentity>,
    trust: Arc<Mutex<TrustStore>>,
    events: mpsc::UnboundedSender<HostEvent>,
    limiter: Mutex<AttemptLimiter>,
    busy: AtomicBool,
    allow_pairing: AtomicBool,
    next_pairing_id: AtomicU64,
    /// Connection of the active session (for "end session").
    active: Mutex<Option<Connection>>,
}

impl Shared {
    fn emit(&self, e: HostEvent) {
        let _ = self.events.send(e);
    }

    fn record_failure(&self, ip: std::net::IpAddr) {
        if let Ok(mut l) = self.limiter.lock() {
            l.record_failure(ip, Instant::now());
        }
    }
}

/// Starts the host endpoint. Must be called from within a Tokio runtime.
pub async fn start(
    ctx: &AppContext,
    cfg: HostConfig,
    events: mpsc::UnboundedSender<HostEvent>,
) -> Result<HostHandle> {
    let endpoint = transport::server_endpoint(cfg.bind, &ctx.identity)?;
    let local_addr = endpoint.local_addr()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        allow_pairing: AtomicBool::new(cfg.allow_pairing),
        next_pairing_id: AtomicU64::new(1),
        cfg,
        identity: ctx.identity.clone(),
        trust: ctx.trusted_clients.clone(),
        events,
        limiter: Mutex::new(AttemptLimiter::new(5, Duration::from_secs(60))),
        busy: AtomicBool::new(false),
        active: Mutex::new(None),
    });
    shared.emit(HostEvent::Listening(local_addr));
    tracing::info!(%local_addr, fingerprint = %ctx.identity.fingerprint(), "host listening");
    let task = tokio::spawn(accept_loop(shared.clone(), endpoint, shutdown_rx));
    Ok(HostHandle {
        local_addr,
        shutdown: shutdown_tx,
        task,
        shared,
    })
}

async fn accept_loop(shared: Arc<Shared>, endpoint: Endpoint, mut shutdown: watch::Receiver<bool>) {
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            // Reap finished connection tasks so a long-running host does not
            // accumulate completed entries.
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let ip = incoming.remote_address().ip();
                let allowed = shared
                    .limiter
                    .lock()
                    .map(|mut l| l.allowed(ip, Instant::now()))
                    .unwrap_or(false);
                if !allowed {
                    tracing::warn!(%ip, "connection refused: rate limited");
                    incoming.refuse();
                    continue;
                }
                let s = shared.clone();
                let sd = shutdown.clone();
                sessions.spawn(async move {
                    let remote = incoming.remote_address();
                    if let Err(e) = serve(s, incoming, sd).await {
                        tracing::info!(%remote, error = %e, "connection ended");
                    }
                });
            }
        }
    }
    // Active sessions observe the shutdown signal and close themselves.
    let _ = timeout(Duration::from_secs(3), async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    endpoint.close(close_code::HOST_SHUTDOWN.into(), b"host stopped");
    let _ = timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
}

async fn reject(conn: &Connection, send: &mut SendStream, reason: RejectReason) -> CoreError {
    let _ = send_msg(send, &ControlMessage::Rejected { reason }).await;
    let _ = send.finish();
    // Give the peer a moment to read the reason before closing.
    let _ = timeout(Duration::from_millis(300), conn.closed()).await;
    conn.close(close_code::REJECTED.into(), reason.to_string().as_bytes());
    CoreError::Rejected(reason)
}

/// Resets the "session active" state when the session ends, whatever happens.
struct BusyGuard(Arc<Shared>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut a) = self.0.active.lock() {
            *a = None;
        }
        self.0.busy.store(false, Ordering::Release);
    }
}

async fn serve(
    shared: Arc<Shared>,
    incoming: quinn::Incoming,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let remote = incoming.remote_address();
    let connecting = incoming.accept().map_err(NetError::from)?;
    let conn = timeout(HANDSHAKE_TIMEOUT, connecting)
        .await
        .map_err(|_| CoreError::Timeout("TLS handshake"))?
        .map_err(NetError::from)?;
    let fp = transport::peer_fingerprint(&conn)?;
    let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| CoreError::Timeout("control stream"))?
        .map_err(NetError::from)?;
    let mut scratch = Vec::new();
    let hello = match timeout(HANDSHAKE_TIMEOUT, recv_msg(&mut recv, &mut scratch))
        .await
        .map_err(|_| CoreError::Timeout("hello"))??
    {
        Some(ControlMessage::Hello(h)) => h,
        _ => {
            shared.record_failure(remote.ip());
            return Err(reject(&conn, &mut send, RejectReason::InvalidRequest).await);
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(reject(&conn, &mut send, RejectReason::VersionMismatch).await);
    }
    if hello.validate().is_err() {
        shared.record_failure(remote.ip());
        return Err(reject(&conn, &mut send, RejectReason::InvalidRequest).await);
    }

    let trusted = shared
        .trust
        .lock()
        .map(|t| t.is_trusted(&fp))
        .unwrap_or(false);
    // A client that does not know this host yet always gets the pairing
    // ceremony, so that *it* can verify the host too.
    if !trusted || hello.request_pairing {
        if !shared.allow_pairing.load(Ordering::Acquire) {
            shared.record_failure(remote.ip());
            return Err(reject(&conn, &mut send, RejectReason::PairingDisabled).await);
        }
        // Commit–reveal: commit to our nonce before seeing the client's.
        let host_fp = shared.identity.fingerprint();
        let host_nonce = random_nonce()?;
        send_msg(
            &mut send,
            &ControlMessage::PairingRequired {
                commitment: commitment(&host_fp, &host_nonce),
            },
        )
        .await?;
        let client_nonce = match timeout(HANDSHAKE_TIMEOUT * 3, recv_msg(&mut recv, &mut scratch))
            .await
            .map_err(|_| CoreError::Timeout("pairing nonce"))??
        {
            Some(ControlMessage::PairingNonce { nonce }) => nonce,
            _ => {
                shared.record_failure(remote.ip());
                return Err(reject(&conn, &mut send, RejectReason::InvalidRequest).await);
            }
        };
        send_msg(
            &mut send,
            &ControlMessage::PairingReveal { nonce: host_nonce },
        )
        .await?;
        let (tx, rx) = oneshot::channel();
        let id = shared.next_pairing_id.fetch_add(1, Ordering::Relaxed);
        shared.emit(HostEvent::PairingRequest(PairingRequest {
            id,
            peer_name: hello.device_name.clone(),
            fingerprint: fp,
            code: sas_code(&fp, &host_fp, &client_nonce, &host_nonce),
            address: remote,
            responder: tx,
        }));
        let accepted = tokio::select! {
            r = timeout(shared.cfg.pairing_timeout, rx) => matches!(r, Ok(Ok(true))),
            _ = conn.closed() => false,
        };
        if !accepted {
            shared.emit(HostEvent::PairingCancelled {
                id,
                fingerprint: fp,
            });
            shared.record_failure(remote.ip());
            return Err(reject(&conn, &mut send, RejectReason::PairingDenied).await);
        }
        if let Ok(mut t) = shared.trust.lock() {
            t.trust(&hello.device_name, fp)?;
        }
        shared.emit(HostEvent::Paired {
            name: hello.device_name.clone(),
            fingerprint: fp,
        });
    }
    if let Ok(mut l) = shared.limiter.lock() {
        l.record_success(remote.ip());
    }
    if shared.busy.swap(true, Ordering::AcqRel) {
        return Err(reject(&conn, &mut send, RejectReason::Busy).await);
    }
    let _busy = BusyGuard(shared.clone());
    if let Ok(mut a) = shared.active.lock() {
        *a = Some(conn.clone());
    }
    if let Ok(mut t) = shared.trust.lock() {
        let _ = t.touch(&fp);
    }
    let peer_name = hello.device_name.clone();
    let reason = run_session(&shared, conn, send, recv, hello, fp, shutdown).await;
    let reason = match reason {
        Ok(r) => r,
        Err(e) => e.to_string(),
    };
    tracing::info!(peer = %peer_name, %reason, "session ended");
    shared.emit(HostEvent::SessionEnded { peer_name, reason });
    Ok(())
}

struct OpenedSource {
    source: Box<dyn FrameSource>,
    displays: Vec<DisplayInfo>,
    selected: Option<DisplayDesc>,
}

fn open_source(kind: &SourceKind, display: u32, fps: u16) -> Result<OpenedSource> {
    match kind {
        SourceKind::TestPattern { width, height } => {
            let source = TestPattern::new(*width, *height, u32::from(fps));
            let (w, h) = source.size();
            Ok(OpenedSource {
                source: Box::new(source),
                displays: vec![DisplayInfo {
                    index: 0,
                    name: "Test pattern".into(),
                    width: w,
                    height: h,
                    refresh_hz: u32::from(fps),
                    primary: true,
                }],
                selected: None,
            })
        }
        SourceKind::Desktop => {
            let descs = ruststream_capture::list_displays()?;
            let selected = descs
                .iter()
                .find(|d| d.index == display)
                .or_else(|| descs.iter().find(|d| d.primary))
                .or_else(|| descs.first())
                .cloned()
                .ok_or(CaptureError::DisplayNotFound(display))?;
            let source = ruststream_capture::open_display(selected.index)?;
            let displays = descs
                .iter()
                .map(|d| DisplayInfo {
                    index: d.index,
                    name: d.name.chars().take(64).collect(),
                    width: d.width,
                    height: d.height,
                    refresh_hz: d.refresh_hz,
                    primary: d.primary,
                })
                .take(16)
                .collect();
            Ok(OpenedSource {
                source,
                displays,
                selected: Some(selected),
            })
        }
    }
}

/// Geometry of the streamed display used by input injection, updated when
/// the host display mode changes during a session.
#[cfg(windows)]
type Geometry = Option<ruststream_input::windows::SharedDisplayRect>;
#[cfg(not(windows))]
type Geometry = Option<()>;

#[cfg(windows)]
fn rect_of(d: &DisplayDesc) -> ruststream_input::windows::DisplayRect {
    ruststream_input::windows::DisplayRect {
        left: d.left,
        top: d.top,
        width: d.width,
        height: d.height,
    }
}

fn make_injector(mode: InputMode, display: Option<&DisplayDesc>) -> (Injector, Geometry) {
    match (mode, display) {
        #[cfg(windows)]
        (InputMode::Inject, Some(d)) => {
            let geometry = Arc::new(Mutex::new(rect_of(d)));
            let backend = ruststream_input::windows::SendInputBackend::shared(geometry.clone());
            (Injector::new(Box::new(backend)), Some(geometry))
        }
        _ => {
            let _ = display;
            let backend: Box<dyn InjectBackend> = Box::new(NullBackend);
            (Injector::new(backend), None)
        }
    }
}

/// Re-reads the display layout after a mode change so absolute mouse
/// coordinates keep mapping onto the right pixels.
fn refresh_geometry(geometry: &Geometry, display_index: u32) {
    #[cfg(windows)]
    if let Some(g) = geometry.clone() {
        tokio::task::spawn_blocking(move || match ruststream_capture::list_displays() {
            Ok(list) => {
                if let Some(d) = list.iter().find(|d| d.index == display_index)
                    && let Ok(mut r) = g.lock()
                {
                    *r = rect_of(d);
                    tracing::info!(?d, "input geometry updated after mode change");
                }
            }
            Err(e) => tracing::warn!(error = %e, "cannot refresh display geometry"),
        });
    }
    #[cfg(not(windows))]
    let _ = (geometry, display_index);
}

enum VideoEvent {
    Resized(SessionParams),
    Stats {
        width: u32,
        height: u32,
        fps: f64,
        bitrate_mbps: f64,
        encode_us: Summary,
    },
    Fatal(String),
}

async fn run_session(
    shared: &Arc<Shared>,
    conn: Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    hello: Hello,
    fp: Fingerprint,
    mut shutdown: watch::Receiver<bool>,
) -> Result<String> {
    let req = hello.request.clone();
    if !req.codecs.contains(&VideoCodec::H264) {
        return Err(reject(&conn, &mut send, RejectReason::NoCommonCodec).await);
    }
    let kind = shared.cfg.source.clone();
    let opened = tokio::task::spawn_blocking(move || open_source(&kind, req.display, req.fps))
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?;
    let opened = match opened {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "cannot open capture");
            shared.emit(HostEvent::Warning(format!("Capture unavailable: {e}")));
            return Err(reject(&conn, &mut send, RejectReason::HostError).await);
        }
    };
    let (width, height) = opened.source.size();
    let refresh = opened
        .selected
        .as_ref()
        .map(|d| d.refresh_hz)
        .filter(|&r| r > 0)
        .unwrap_or(240);
    let fps = hello
        .request
        .fps
        .min(u16::try_from(refresh).unwrap_or(u16::MAX))
        .max(1);
    let params = SessionParams {
        codec: VideoCodec::H264,
        display: opened.selected.as_ref().map_or(0, |d| d.index),
        width,
        height,
        fps,
        bitrate_kbps: hello.request.bitrate_kbps.clamp(500, 500_000),
    };
    let encoder = match ruststream_codec::create_encoder(EncoderSettings {
        width,
        height,
        fps: u32::from(fps),
        bitrate_kbps: params.bitrate_kbps,
        preference: shared.cfg.encoder_preference,
    }) {
        Ok(e) => e,
        Err(e) => {
            shared.emit(HostEvent::Warning(format!("Encoder unavailable: {e}")));
            return Err(reject(&conn, &mut send, RejectReason::HostError).await);
        }
    };
    let encoder_name = encoder.info().name.to_string();
    send_msg(
        &mut send,
        &ControlMessage::Welcome {
            host_name: shared.cfg.device_name.chars().take(64).collect(),
            session: params,
            displays: opened.displays.clone(),
        },
    )
    .await?;
    shared.emit(HostEvent::SessionStarted {
        peer_name: hello.device_name.clone(),
        fingerprint: fp,
        params,
        encoder: encoder_name.clone(),
    });
    tracing::info!(peer = %hello.device_name, ?params, encoder = %encoder_name, "session started");

    // Control writer.
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ControlMessage>();
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if send_msg(&mut send, &m).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });
    // Control reader (cancel-safe consumption through a channel).
    let (in_tx, mut in_rx) = mpsc::channel::<ControlMessage>(32);
    let reader = tokio::spawn(async move {
        let mut scratch = Vec::new();
        while let Ok(Some(m)) = recv_msg::<ControlMessage>(&mut recv, &mut scratch).await {
            if m.validate().is_err() || in_tx.send(m).await.is_err() {
                break;
            }
        }
    });
    // Input.
    let (injector, geometry) = make_injector(shared.cfg.input, opened.selected.as_ref());
    let input = tokio::spawn(input_loop(conn.clone(), injector));
    // Video.
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(false));
    let (video_tx, mut video_rx) = mpsc::unbounded_channel();
    let video = {
        let ctx = VideoCtx {
            conn: conn.clone(),
            source: opened.source,
            encoder,
            params,
            stop: stop.clone(),
            keyframe: keyframe.clone(),
            events: video_tx,
        };
        std::thread::Builder::new()
            .name("rs-host-video".into())
            .spawn(move || video_loop(ctx))?
    };

    let reason = loop {
        tokio::select! {
            m = in_rx.recv() => match m {
                Some(ControlMessage::RequestKeyframe) => keyframe.store(true, Ordering::Release),
                Some(ControlMessage::Ping { nonce }) => {
                    let _ = ctl_tx.send(ControlMessage::Pong { nonce });
                }
                Some(ControlMessage::ClientReport { frames_dropped, .. }) => {
                    tracing::trace!(frames_dropped, "client report");
                }
                Some(ControlMessage::Goodbye { .. }) => break "client disconnected".to_string(),
                Some(other) => tracing::debug!(?other, "ignored control message"),
                None => break "control stream closed".to_string(),
            },
            v = video_rx.recv() => match v {
                Some(VideoEvent::Resized(p)) => {
                    refresh_geometry(&geometry, p.display);
                    let _ = ctl_tx.send(ControlMessage::SessionChanged(p));
                }
                Some(VideoEvent::Stats { width, height, fps, bitrate_mbps, encode_us }) => {
                    shared.emit(HostEvent::Stats(HostStats {
                        width,
                        height,
                        fps,
                        bitrate_mbps,
                        encode_us,
                        rtt_ms: conn.rtt().as_secs_f64() * 1000.0,
                    }));
                }
                Some(VideoEvent::Fatal(e)) => break format!("video pipeline error: {e}"),
                None => break "video pipeline stopped".to_string(),
            },
            e = conn.closed() => break describe_close(&e),
            _ = shutdown.changed() => {
                let _ = ctl_tx.send(ControlMessage::Goodbye { reason: "host stopped".into() });
                break "host stopped".to_string();
            }
        }
    };

    stop.store(true, Ordering::Release);
    drop(ctl_tx);
    let _ = timeout(Duration::from_millis(200), writer).await;
    conn.close(close_code::NORMAL.into(), b"session ended");
    reader.abort();
    // Aborting the input task drops the injector, which releases every key.
    input.abort();
    let _ = input.await;
    let _ = tokio::task::spawn_blocking(move || video.join()).await;
    Ok(reason)
}

pub(crate) fn describe_close(e: &quinn::ConnectionError) -> String {
    match e {
        quinn::ConnectionError::ApplicationClosed(c) => {
            let reason = String::from_utf8_lossy(&c.reason);
            if reason.is_empty() {
                "closed by peer".to_string()
            } else {
                format!("closed by peer: {reason}")
            }
        }
        quinn::ConnectionError::TimedOut => "connection timed out".to_string(),
        quinn::ConnectionError::LocallyClosed => "closed".to_string(),
        other => other.to_string(),
    }
}

async fn input_loop(conn: Connection, mut injector: Injector) {
    let Ok(mut recv) = conn.accept_uni().await else {
        return;
    };
    let mut scratch = Vec::new();
    while let Ok(Some(ev)) = recv_msg::<InputEvent>(&mut recv, &mut scratch).await {
        if let Err(e) = injector.apply(ev) {
            tracing::debug!(error = %e, "input not applied");
        }
    }
    // `injector` is dropped here → release_all.
}

struct VideoCtx {
    conn: Connection,
    source: Box<dyn FrameSource>,
    encoder: Box<dyn VideoEncoder>,
    params: SessionParams,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<VideoEvent>,
}

fn video_loop(mut c: VideoCtx) {
    #[cfg(windows)]
    let _timer = ruststream_platform_windows::HighResolutionTimer::new();
    let interval = Duration::from_secs_f64(1.0 / f64::from(c.params.fps.max(1)));
    let mut next_due = Instant::now();
    let mut pending = false;
    let mut force_key = true;
    let mut frame_id: u32 = 0;
    let mut bits = Vec::with_capacity(1 << 20);
    let mut encode_w = Window::new(600);
    let mut rate = RateMeter::new(Duration::from_secs(1));
    let mut last_stats = Instant::now();
    let mut size = (c.params.width, c.params.height);
    let mut capture_errors = 0u32;

    while !c.stop.load(Ordering::Acquire) {
        if c.keyframe.swap(false, Ordering::AcqRel) {
            force_key = true;
            pending = true;
        }
        let now = Instant::now();
        let wait = if pending {
            next_due.saturating_duration_since(now)
        } else {
            Duration::from_millis(50)
        };
        match c.source.next_frame(wait) {
            Ok(Some(_)) => {
                pending = true;
                capture_errors = 0;
            }
            Ok(None) => {}
            Err(CaptureError::AccessLost) => {
                // Secure desktop or mode change: retry quietly.
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                capture_errors += 1;
                if capture_errors > 50 {
                    let _ = c.events.send(VideoEvent::Fatal(e.to_string()));
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        }
        let now = Instant::now();
        if !pending || now < next_due {
            continue;
        }
        let Some(frame) = c.source.last_frame() else {
            continue;
        };
        if (frame.width, frame.height) != size {
            size = (frame.width, frame.height);
            c.params.width = frame.width;
            c.params.height = frame.height;
            let _ = c.events.send(VideoEvent::Resized(c.params));
            force_key = true;
        }
        let timestamp_us =
            unix_micros().saturating_sub(frame.captured_at.elapsed().as_micros() as u64);
        let t0 = Instant::now();
        let outcome = c
            .encoder
            .encode(frame.data, frame.width, frame.height, force_key, &mut bits);
        match outcome {
            Ok(EncodeOutcome::Encoded { keyframe }) => {
                encode_w.record_duration(t0.elapsed());
                if keyframe {
                    force_key = false;
                }
                let meta = FrameMeta {
                    frame_id,
                    keyframe,
                    timestamp_us,
                };
                frame_id = frame_id.wrapping_add(1);
                match ruststream_network::video::send_frame(&c.conn, meta, &bits) {
                    Ok(_) => rate.record(now, bits.len() as u64),
                    Err(NetError::Datagram(quinn::SendDatagramError::ConnectionLost(_))) => break,
                    Err(e) => {
                        let _ = c.events.send(VideoEvent::Fatal(e.to_string()));
                        break;
                    }
                }
            }
            Ok(EncodeOutcome::Skipped) => {}
            Err(e) => {
                let _ = c.events.send(VideoEvent::Fatal(e.to_string()));
                break;
            }
        }
        pending = false;
        next_due = if now > next_due + interval {
            now + interval
        } else {
            next_due + interval
        };
        if last_stats.elapsed() >= Duration::from_secs(1) {
            last_stats = Instant::now();
            let _ = c.events.send(VideoEvent::Stats {
                width: size.0,
                height: size.1,
                fps: rate.rate(last_stats),
                bitrate_mbps: rate.bitrate(last_stats) / 1e6,
                encode_us: encode_w.summary(),
            });
        }
    }
}
