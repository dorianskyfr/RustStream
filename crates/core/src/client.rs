//! Client side: connects to a host, decodes the stream, forwards input.
//!
//! * The async task owns the QUIC connection: datagram reassembly, control
//!   messages, statistics.
//! * `rs-client-decode` (OS thread) decodes frames and hands RGBA pictures to
//!   the frame callback. Its input queue is tiny (3 frames): if decoding falls
//!   behind, frames are dropped and a keyframe is requested rather than
//!   letting latency grow.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use ruststream_network::NetError;
use ruststream_network::stream::{recv_msg, send_msg};
use ruststream_network::transport::{self, close_code};
use ruststream_protocol::video::{FrameMeta, Reassembler};
use ruststream_protocol::{
    ControlMessage, DisplayInfo, Hello, InputEvent, RejectReason, SessionParams, StreamRequest,
};
use ruststream_security::{
    Commitment, DeviceIdentity, Fingerprint, Nonce, ServerPolicy, random_nonce, sas_code,
    verify_commitment,
};
use ruststream_telemetry::{RateMeter, SessionStats, Window};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::host::describe_close;
use crate::{CoreError, Result, unix_micros};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub address: SocketAddr,
    pub policy: ServerPolicy,
    pub device_name: String,
    pub request: StreamRequest,
}

#[derive(Debug, Clone)]
pub enum ClientEvent {
    Connecting,
    /// First connection: show this code; it must match the one on the host.
    PairingCode(String),
    Connected {
        host_name: String,
        host_fingerprint: Fingerprint,
        params: SessionParams,
        displays: Vec<DisplayInfo>,
        /// The host was paired during this connection (save it).
        newly_paired: bool,
    },
    SessionChanged(SessionParams),
    Stats(SessionStats),
    Disconnected {
        reason: String,
        error: bool,
        /// Set when the host refused the session.
        rejected: Option<RejectReason>,
    },
}

/// A decoded picture, valid only during the callback.
#[derive(Debug)]
pub struct DecodedFrame<'a> {
    pub width: u32,
    pub height: u32,
    /// RGBA8, `width * 4` bytes per row.
    pub rgba: &'a [u8],
    pub frame_id: u32,
    /// Host capture time (Unix µs, host clock).
    pub capture_timestamp_us: u64,
    pub decode_time: Duration,
}

pub type FrameCallback = Box<dyn FnMut(DecodedFrame<'_>) + Send>;

enum Command {
    Disconnect,
    /// The local user compared the pairing codes (true = identical).
    ConfirmPairing(bool),
}

/// Handle to a running client session. Dropping it disconnects.
#[derive(Debug)]
pub struct ClientHandle {
    input_tx: mpsc::UnboundedSender<InputEvent>,
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnect => f.write_str("Disconnect"),
            Self::ConfirmPairing(ok) => write!(f, "ConfirmPairing({ok})"),
        }
    }
}

impl ClientHandle {
    /// Queues an input event (never blocks).
    pub fn send_input(&self, ev: InputEvent) {
        let _ = self.input_tx.send(ev);
    }

    /// A cloneable, thread-safe input sender (for keyboard hooks).
    pub fn input_sender(&self) -> InputSender {
        InputSender(self.input_tx.clone())
    }

    /// Answer to [`ClientEvent::PairingCode`]: `true` if the code shown on
    /// the host is identical. `false` cancels the connection.
    pub fn confirm_pairing(&self, identical: bool) {
        let _ = self.cmd_tx.send(Command::ConfirmPairing(identical));
    }

    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(Command::Disconnect);
    }
}

/// Sends input events to the session from any thread.
#[derive(Debug, Clone)]
pub struct InputSender(mpsc::UnboundedSender<InputEvent>);

impl InputSender {
    pub fn send(&self, ev: InputEvent) {
        let _ = self.0.send(ev);
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Starts a client session on `rt`. Progress is reported through `events`.
pub fn start(
    rt: &tokio::runtime::Handle,
    identity: Arc<DeviceIdentity>,
    cfg: ClientConfig,
    events: mpsc::UnboundedSender<ClientEvent>,
    on_frame: FrameCallback,
) -> ClientHandle {
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    rt.spawn(async move {
        let ev = events.clone();
        let result = session(identity, cfg, events, on_frame, input_rx, cmd_rx).await;
        let (reason, error, rejected) = match result {
            Ok(r) => (r, false, None),
            Err(CoreError::Rejected(r)) => (r.to_string(), true, Some(r)),
            Err(e) => (e.to_string(), true, None),
        };
        tracing::info!(%reason, error, "client session ended");
        let _ = ev.send(ClientEvent::Disconnected {
            reason,
            error,
            rejected,
        });
    });
    ClientHandle { input_tx, cmd_tx }
}

#[derive(Debug)]
struct EncodedFrame {
    meta: FrameMeta,
    data: Vec<u8>,
}

#[derive(Debug)]
struct StatsState {
    width: u32,
    height: u32,
    decode: Window,
    end_to_end: Window,
    decoded: RateMeter,
    frames_decoded: u32,
}

#[allow(clippy::too_many_lines)]
async fn session(
    identity: Arc<DeviceIdentity>,
    cfg: ClientConfig,
    events: mpsc::UnboundedSender<ClientEvent>,
    on_frame: FrameCallback,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
) -> Result<String> {
    let _ = events.send(ClientEvent::Connecting);
    let endpoint = transport::client_endpoint()?;
    let conn = tokio::select! {
        r = timeout(Duration::from_secs(10), transport::connect(&endpoint, cfg.address, &identity, cfg.policy)) => {
            r.map_err(|_| CoreError::Timeout("connecting to host"))??
        }
        _ = cmd_rx.recv() => return Ok("cancelled".into()),
    };
    let host_fp = transport::peer_fingerprint(&conn)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(NetError::from)?;
    let pairing_mode = cfg.policy == ServerPolicy::Pairing;
    let hello = Hello::new(cfg.device_name.clone(), cfg.request.clone(), pairing_mode);
    send_msg(&mut send, &ControlMessage::Hello(hello)).await?;

    // Control reader: messages are consumed through a channel so that every
    // `select!` below is cancel-safe.
    let (in_tx, mut in_rx) = mpsc::channel::<ControlMessage>(32);
    let reader = tokio::spawn(async move {
        let mut scratch = Vec::new();
        while let Ok(Some(m)) = recv_msg::<ControlMessage>(&mut recv, &mut scratch).await {
            if m.validate().is_err() || in_tx.send(m).await.is_err() {
                break;
            }
        }
    });

    // Handshake. In pairing mode the host must run the pairing ceremony and
    // the local user must confirm that both screens show the same code;
    // nothing is displayed or sent before that.
    let mut code_shown = false;
    let mut confirmed = !pairing_mode;
    let mut pairing: Option<(Commitment, Nonce)> = None;
    let mut welcome: Option<(String, SessionParams, Vec<DisplayInfo>)> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    let handshake: Result<(String, SessionParams, Vec<DisplayInfo>)> = loop {
        if confirmed && let Some(w) = welcome.take() {
            break Ok(w);
        }
        tokio::select! {
            m = in_rx.recv() => match m {
                Some(ControlMessage::PairingRequired { commitment }) => {
                    if pairing.is_some() {
                        break Err(CoreError::Unexpected("repeated pairing request"));
                    }
                    let nonce = random_nonce()?;
                    pairing = Some((commitment, nonce));
                    if let Err(e) = send_msg(&mut send, &ControlMessage::PairingNonce { nonce }).await {
                        break Err(e.into());
                    }
                }
                Some(ControlMessage::PairingReveal { nonce: host_nonce }) => {
                    let Some((commitment, client_nonce)) = pairing else {
                        break Err(CoreError::Unexpected("pairing reveal without commitment"));
                    };
                    if code_shown || !verify_commitment(&commitment, &host_fp, &host_nonce) {
                        break Err(CoreError::Unexpected(
                            "the host's pairing data is inconsistent (possible attack)",
                        ));
                    }
                    code_shown = true;
                    let code = sas_code(&identity.fingerprint(), &host_fp, &client_nonce, &host_nonce);
                    let _ = events.send(ClientEvent::PairingCode(code));
                }
                Some(ControlMessage::Welcome { host_name, session, displays }) => {
                    if pairing_mode && !code_shown {
                        break Err(CoreError::Unexpected(
                            "the host skipped the pairing verification",
                        ));
                    }
                    welcome = Some((host_name, session, displays));
                }
                Some(ControlMessage::Rejected { reason }) => break Err(CoreError::Rejected(reason)),
                Some(_) => break Err(CoreError::Unexpected("message during handshake")),
                None => break Err(CoreError::Unexpected("host closed the control stream")),
            },
            c = cmd_rx.recv() => match c {
                Some(Command::ConfirmPairing(true)) if code_shown => confirmed = true,
                Some(Command::ConfirmPairing(true)) => {}
                Some(Command::ConfirmPairing(false) | Command::Disconnect) | None => {
                    reader.abort();
                    conn.close(close_code::NORMAL.into(), b"cancelled");
                    return Ok("cancelled".into());
                }
            },
            _ = tokio::time::sleep_until(deadline) => break Err(CoreError::Timeout("waiting for the host")),
        }
    };
    let (host_name, params, displays) = match handshake {
        Ok(w) => w,
        Err(e) => {
            reader.abort();
            conn.close(close_code::PROTOCOL_ERROR.into(), b"handshake failed");
            return Err(e);
        }
    };
    let newly_paired = code_shown;
    let _ = events.send(ClientEvent::Connected {
        host_name,
        host_fingerprint: host_fp,
        params,
        displays,
        newly_paired,
    });

    // Input forwarder.
    let mut uni = conn.open_uni().await.map_err(NetError::from)?;
    let input_task = tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            if send_msg(&mut uni, &ev).await.is_err() {
                break;
            }
        }
        let _ = uni.finish();
    });
    // Control writer / reader.
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ControlMessage>();
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if send_msg(&mut send, &m).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });
    // Decoder thread.
    let stats = Arc::new(Mutex::new(StatsState {
        width: params.width,
        height: params.height,
        decode: Window::new(600),
        end_to_end: Window::new(600),
        decoded: RateMeter::new(Duration::from_secs(1)),
        frames_decoded: 0,
    }));
    let decode_failed = Arc::new(AtomicBool::new(false));
    let (dec_tx, dec_rx) = std_mpsc::sync_channel::<EncodedFrame>(3);
    let decoder = {
        let stats = stats.clone();
        let failed = decode_failed.clone();
        std::thread::Builder::new()
            .name("rs-client-decode".into())
            .spawn(move || decoder_loop(dec_rx, on_frame, stats, failed))?
    };

    let mut reasm = Reassembler::new(4);
    let mut awaiting_key = true;
    let mut last_key_request: Option<Instant> = None;
    let mut frames_dropped: u64 = 0;
    let mut bytes = RateMeter::new(Duration::from_secs(1));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    /// Requests an IDR frame, at most every 250 ms (the request itself is
    /// reliable; repeating covers a lost keyframe).
    fn request_key(last: &mut Option<Instant>, tx: &mpsc::UnboundedSender<ControlMessage>) {
        if last.is_none_or(|t| t.elapsed() > Duration::from_millis(250)) {
            *last = Some(Instant::now());
            let _ = tx.send(ControlMessage::RequestKeyframe);
        }
    }

    let reason = loop {
        tokio::select! {
            d = conn.read_datagram() => {
                let d = match d {
                    Ok(d) => d,
                    Err(e) => break describe_close(&e),
                };
                bytes.record(Instant::now(), d.len() as u64);
                if decode_failed.swap(false, Ordering::AcqRel) {
                    awaiting_key = true;
                    request_key(&mut last_key_request, &ctl_tx);
                }
                match reasm.push(d) {
                    Ok(Some(f)) => {
                        if f.lost_before > 0 && !f.meta.keyframe {
                            awaiting_key = true;
                        }
                        if awaiting_key && !f.meta.keyframe {
                            frames_dropped += 1;
                            request_key(&mut last_key_request, &ctl_tx);
                            continue;
                        }
                        awaiting_key = false;
                        let frame = EncodedFrame { meta: f.meta, data: f.data.to_vec() };
                        match dec_tx.try_send(frame) {
                            Ok(()) => {}
                            Err(std_mpsc::TrySendError::Full(_)) => {
                                frames_dropped += 1;
                                awaiting_key = true;
                                request_key(&mut last_key_request, &ctl_tx);
                            }
                            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                                break "decoder stopped".to_string();
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!(error = %e, "malformed datagram"),
                }
            }
            m = in_rx.recv() => match m {
                Some(ControlMessage::SessionChanged(p)) => {
                    if let Ok(mut s) = stats.lock() {
                        s.width = p.width;
                        s.height = p.height;
                    }
                    let _ = events.send(ClientEvent::SessionChanged(p));
                }
                Some(ControlMessage::Ping { nonce }) => {
                    let _ = ctl_tx.send(ControlMessage::Pong { nonce });
                }
                Some(ControlMessage::Goodbye { reason }) => break format!("host: {reason}"),
                Some(_) => {}
                None => break "control stream closed".to_string(),
            },
            c = cmd_rx.recv() => match c {
                Some(Command::ConfirmPairing(_)) => {}
                Some(Command::Disconnect) | None => {
                    let _ = ctl_tx.send(ControlMessage::Goodbye { reason: "disconnected".into() });
                    break "disconnected".to_string();
                }
            },
            _ = tick.tick() => {
                // Recovery must not depend on incoming video: if the keyframe
                // itself was lost and the host screen is static, nothing else
                // would ever arrive.
                if awaiting_key {
                    request_key(&mut last_key_request, &ctl_tx);
                }
                let now = Instant::now();
                let link = transport::link_stats(&conn);
                let reasm_stats = reasm.stats();
                let snapshot = stats.lock().ok().map(|mut s| {
                    let decode = s.decode.summary();
                    let e2e = s.end_to_end.summary();
                    let fps = s.decoded.rate(now);
                    let decoded = s.frames_decoded;
                    (s.width, s.height, decode, e2e, fps, decoded)
                });
                if let Some((width, height, decode_us, end_to_end_us, fps, decoded)) = snapshot {
                    let _ = events.send(ClientEvent::Stats(SessionStats {
                        width,
                        height,
                        fps,
                        bitrate_mbps: bytes.bitrate(now) / 1e6,
                        rtt_ms: link.rtt.as_secs_f64() * 1000.0,
                        lost_packets: link.lost_packets,
                        frames_dropped: frames_dropped + reasm_stats.frames_incomplete,
                        decode_us,
                        encode_us: Default::default(),
                        end_to_end_us,
                    }));
                    let _ = ctl_tx.send(ControlMessage::ClientReport {
                        frames_decoded: decoded,
                        frames_dropped: u32::try_from(frames_dropped).unwrap_or(u32::MAX),
                        decode_us_p50: u32::try_from(decode_us.p50).unwrap_or(u32::MAX),
                    });
                }
            }
        }
    };

    drop(dec_tx);
    drop(ctl_tx);
    let _ = timeout(Duration::from_millis(200), writer).await;
    input_task.abort();
    reader.abort();
    conn.close(close_code::NORMAL.into(), reason.as_bytes());
    let _ = timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    Ok(reason)
}

fn decoder_loop(
    rx: std_mpsc::Receiver<EncodedFrame>,
    mut on_frame: FrameCallback,
    stats: Arc<Mutex<StatsState>>,
    failed: Arc<AtomicBool>,
) {
    let mut decoder = match ruststream_codec::create_decoder() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cannot create decoder");
            return;
        }
    };
    tracing::info!(decoder = decoder.name(), "decoder ready");
    let mut rgba = Vec::new();
    while let Ok(f) = rx.recv() {
        let t0 = Instant::now();
        match decoder.decode(&f.data, &mut rgba) {
            Ok(Some((width, height))) => {
                let decode_time = t0.elapsed();
                on_frame(DecodedFrame {
                    width,
                    height,
                    rgba: &rgba,
                    frame_id: f.meta.frame_id,
                    capture_timestamp_us: f.meta.timestamp_us,
                    decode_time,
                });
                if let Ok(mut s) = stats.lock() {
                    s.decode.record_duration(decode_time);
                    s.end_to_end
                        .record(unix_micros().saturating_sub(f.meta.timestamp_us));
                    s.decoded.record(Instant::now(), 0);
                    s.frames_decoded = s.frames_decoded.wrapping_add(1);
                    s.width = width;
                    s.height = height;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "decode error, requesting keyframe");
                failed.store(true, Ordering::Release);
            }
        }
    }
}
