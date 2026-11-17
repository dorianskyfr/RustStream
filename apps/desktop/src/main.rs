//! RustStream desktop application: one window, both roles (host and client).
//!
//! Threading: Slint runs on the main thread; the engine runs on a Tokio
//! runtime. Background tasks talk to the UI exclusively through
//! [`post`] (`slint::invoke_from_event_loop`), and all UI state lives in a
//! thread-local [`App`] owned by the main thread.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ruststream_core::client::{self, ClientConfig, ClientEvent, ClientHandle};
use ruststream_core::host::{
    self, HostConfig, HostEvent, HostHandle, HostStats, InputMode, PairingRequest, SourceKind,
};
use ruststream_core::network::discovery::{
    self, Beacon, DiscoveredHost, tokio_util_lite::CancelFlag,
};
use ruststream_core::protocol::{ABS_MAX, DisplayInfo, InputEvent, MouseButton, RejectReason};
use ruststream_core::security::{Fingerprint, ServerPolicy};
use ruststream_core::settings::{
    Advanced, Profile, SavedComputer, Settings, Theme as ThemePref, parse_address, resolve,
};
use ruststream_core::telemetry::SessionStats;
use ruststream_core::{AppContext, Paths};
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use tokio::sync::mpsc;

mod ui {
    // Generated code: Slint components do not implement Debug.
    #![allow(missing_debug_implementations, clippy::all, clippy::unwrap_used)]
    slint::include_modules!();
}
use ui::*;

/// How long a LAN beacon keeps a computer marked as "on the network".
const DISCOVERY_TTL: Duration = Duration::from_secs(6);

enum UiMsg {
    HostStarted(Result<HostHandle, String>),
    Host(HostEvent),
    Discovered(DiscoveredHost),
    Resolved {
        session: u64,
        result: Result<SocketAddr, String>,
    },
    Client {
        session: u64,
        event: ClientEvent,
    },
    KeyboardEscape,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            tracing::error!("re-entrant UI state access");
            None
        }
    })
}

/// Delivers a message to the UI thread.
fn post(msg: UiMsg) {
    let _ = slint::invoke_from_event_loop(move || {
        with_app(|app| app.on_msg(msg));
    });
}

/// Latest decoded picture, handed from the decoder thread to the UI thread.
/// Only one UI update is ever queued: the UI always shows the newest frame.
struct FrameSlot {
    frame: Mutex<Option<SharedPixelBuffer<Rgba8Pixel>>>,
    pending: AtomicBool,
}

struct Target {
    name: String,
    address: String,
    pinned: Option<Fingerprint>,
    display: u32,
}

struct ClientSession {
    id: u64,
    handle: Option<ClientHandle>,
    target: Target,
    connected: bool,
    displays: Vec<DisplayInfo>,
    /// Set when the user switches the remote display: reconnect afterwards.
    reconnect_display: Option<u32>,
    foreground: bool,
    #[cfg(windows)]
    keyboard: Option<ruststream_input::windows::KeyboardCapture>,
}

struct App {
    ui: AppWindow,
    rt: tokio::runtime::Runtime,
    ctx: AppContext,
    settings: Settings,
    host: Option<HostHandle>,
    host_starting: bool,
    announce_stop: Option<CancelFlag>,
    pairing: Option<PairingRequest>,
    host_peer: Option<String>,
    discovered: HashMap<Fingerprint, (DiscoveredHost, Instant)>,
    discovery_stop: CancelFlag,
    client: Option<ClientSession>,
    next_session: u64,
    frames: Arc<FrameSlot>,
}

fn s(v: impl AsRef<str>) -> SharedString {
    SharedString::from(v.as_ref())
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn local_ip() -> Option<std::net::IpAddr> {
    // Connecting a UDP socket only selects a route: no packet is sent.
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip())
}

fn relative_time(unix: Option<u64>) -> String {
    let Some(t) = unix else {
        return "jamais connecté".into();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let d = now.saturating_sub(t);
    match d {
        0..60 => "vu à l'instant".into(),
        60..3600 => format!("vu il y a {} min", d / 60),
        3600..86_400 => format!("vu il y a {} h", d / 3600),
        _ => format!("vu il y a {} j", d / 86_400),
    }
}

fn reject_text(r: RejectReason) -> &'static str {
    match r {
        RejectReason::VersionMismatch => {
            "Les deux ordinateurs n'ont pas des versions compatibles de RustStream."
        }
        RejectReason::PairingDenied => "La connexion a été refusée sur l'autre ordinateur.",
        RejectReason::PairingDisabled => {
            "L'autre ordinateur n'accepte pas de nouveaux appareils (Paramètres › Accepter de nouveaux appareils)."
        }
        RejectReason::Busy => "L'autre ordinateur est déjà en session avec un autre appareil.",
        RejectReason::RateLimited => "Trop de tentatives. Réessayez dans une minute.",
        RejectReason::NoCommonCodec => "Aucun format vidéo commun entre les deux ordinateurs.",
        RejectReason::InvalidRequest => "L'autre ordinateur a jugé la demande invalide.",
        RejectReason::HostError => {
            "L'ordinateur distant n'a pas pu démarrer la capture de l'écran ou l'encodage."
        }
    }
}

fn error_text(reason: &str) -> String {
    let r = reason.to_lowercase();
    if r.contains("certificate") {
        "L'identité de l'ordinateur distant ne correspond pas à celle enregistrée. Connexion bloquée par sécurité. S'il a été réinstallé, retirez-le de la liste puis ajoutez-le à nouveau.".into()
    } else if r.contains("timed out") || r.contains("timeout") {
        "Ordinateur injoignable. Vérifiez qu'il est allumé, que « Partager » y est activé et que le pare-feu Windows autorise RustStream.".into()
    } else if r.contains("resolve") {
        "Adresse introuvable. Vérifiez le nom ou l'adresse IP.".into()
    } else if r.contains("skipped the pairing") {
        "L'ordinateur distant n'a pas présenté de code de vérification : connexion bloquée par sécurité.".into()
    } else {
        format!("La connexion s'est interrompue ({reason}).")
    }
}

fn session_stat_lines(st: &SessionStats) -> Vec<StatLine> {
    let ms = |us: u64| us as f64 / 1000.0;
    vec![
        StatLine {
            label: s("Résolution"),
            value: s(format!("{} × {}", st.width, st.height)),
        },
        StatLine {
            label: s("Images par seconde"),
            value: s(format!("{:.0}", st.fps)),
        },
        StatLine {
            label: s("Débit"),
            value: s(format!("{:.1} Mbit/s", st.bitrate_mbps)),
        },
        StatLine {
            label: s("Latence réseau (aller-retour)"),
            value: s(format!("{:.1} ms", st.rtt_ms)),
        },
        StatLine {
            label: s("Décodage p50 / p95"),
            value: s(format!(
                "{:.1} / {:.1} ms",
                ms(st.decode_us.p50),
                ms(st.decode_us.p95)
            )),
        },
        StatLine {
            label: s("Images abandonnées"),
            value: s(st.frames_dropped.to_string()),
        },
        StatLine {
            label: s("Paquets perdus"),
            value: s(st.lost_packets.to_string()),
        },
        StatLine {
            label: s("Décodeur"),
            value: s("OpenH264 (logiciel)"),
        },
    ]
}

fn host_stat_lines(st: &HostStats) -> Vec<StatLine> {
    let ms = |us: u64| us as f64 / 1000.0;
    vec![
        StatLine {
            label: s("Résolution"),
            value: s(format!("{} × {}", st.width, st.height)),
        },
        StatLine {
            label: s("Images par seconde"),
            value: s(format!("{:.0}", st.fps)),
        },
        StatLine {
            label: s("Débit"),
            value: s(format!("{:.1} Mbit/s", st.bitrate_mbps)),
        },
        StatLine {
            label: s("Encodage p50 / p95"),
            value: s(format!(
                "{:.1} / {:.1} ms",
                ms(st.encode_us.p50),
                ms(st.encode_us.p95)
            )),
        },
        StatLine {
            label: s("Latence réseau (aller-retour)"),
            value: s(format!("{:.1} ms", st.rtt_ms)),
        },
        StatLine {
            label: s("Encodeur"),
            value: s("OpenH264 (logiciel)"),
        },
    ]
}

fn profile_index(p: Profile) -> i32 {
    match p {
        Profile::Auto => 0,
        Profile::Performance => 1,
        Profile::Quality => 2,
    }
}

impl App {
    fn save_settings(&self) {
        if let Err(e) = self.settings.save(&self.ctx.paths.settings()) {
            self.show_error(format!("Impossible d'enregistrer les paramètres : {e}"));
        }
    }

    fn show_error(&self, msg: impl AsRef<str>) {
        self.ui.set_error_text(s(msg));
    }

    // ---------- UI refresh ----------

    fn refresh_all(&self) {
        let ui = &self.ui;
        ui.set_device_name(s(&self.settings.device_name));
        let fp = self.ctx.identity.fingerprint();
        ui.set_fingerprint(s(fp.short()));
        ui.set_fingerprint_full(s(fp.to_hex()));
        ui.set_port(i32::from(self.settings.host_port));
        ui.set_dark(self.settings.theme == ThemePref::Dark);
        ui.set_animations(self.settings.animations);
        ui.set_profile(profile_index(self.settings.profile));
        ui.set_advanced(self.settings.advanced.is_some());
        if let Some(a) = self.settings.advanced {
            ui.set_adv_fps(i32::from(a.fps));
            ui.set_adv_bitrate(i32::try_from(a.bitrate_kbps / 1000).unwrap_or(20));
        }
        ui.set_allow_pairing(self.settings.allow_pairing);
        ui.invoke_apply_theme();
        self.refresh_hosting();
        self.refresh_computers();
        self.refresh_trusted();
    }

    fn refresh_hosting(&self) {
        let ui = &self.ui;
        ui.set_hosting(self.settings.hosting_enabled);
        let addr = local_ip().map_or_else(
            || format!("port {}", self.settings.host_port),
            |ip| format!("{ip}:{}", self.settings.host_port),
        );
        ui.set_local_address(s(addr));
        ui.set_host_session_peer(s(self.host_peer.clone().unwrap_or_default()));
    }

    fn refresh_computers(&self) {
        let own = self.ctx.identity.fingerprint();
        let now = Instant::now();
        let live = |fp: &Fingerprint| {
            self.discovered
                .get(fp)
                .filter(|(_, seen)| now.duration_since(*seen) < DISCOVERY_TTL)
                .map(|(h, _)| h)
        };
        let mut items = Vec::new();
        for c in &self.settings.computers {
            let online = live(&c.fingerprint);
            items.push(ComputerItem {
                id: s(format!("saved:{}", c.fingerprint.to_hex())),
                name: s(&c.name),
                address: s(online.map_or_else(|| c.address.clone(), |h| h.addr.to_string())),
                status: s(if online.is_some() {
                    "Sur le réseau"
                } else {
                    "Enregistré"
                }),
                online: online.is_some(),
                saved: true,
                fingerprint: s(c.fingerprint.short()),
            });
        }
        let mut lan: Vec<_> = self
            .discovered
            .values()
            .filter(|(h, seen)| {
                now.duration_since(*seen) < DISCOVERY_TTL
                    && h.fingerprint != own
                    && !self
                        .settings
                        .computers
                        .iter()
                        .any(|c| c.fingerprint == h.fingerprint)
            })
            .map(|(h, _)| h)
            .collect();
        lan.sort_by(|a, b| a.name.cmp(&b.name));
        for h in lan {
            items.push(ComputerItem {
                id: s(format!("lan:{}", h.fingerprint.to_hex())),
                name: s(&h.name),
                address: s(h.addr.to_string()),
                status: s("Nouveau · réseau local"),
                online: true,
                saved: false,
                fingerprint: s(h.fingerprint.short()),
            });
        }
        self.ui.set_computers(model(items));
    }

    fn refresh_trusted(&self) {
        let items = self
            .ctx
            .trusted_clients
            .lock()
            .map(|t| {
                t.devices()
                    .iter()
                    .map(|d| TrustedItem {
                        id: s(d.fingerprint.to_hex()),
                        name: s(&d.name),
                        fingerprint: s(d.fingerprint.short()),
                        last_seen: s(relative_time(d.last_seen)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.ui.set_trusted(model(items));
    }

    // ---------- Hosting ----------

    fn set_hosting(&mut self, on: bool) {
        self.settings.hosting_enabled = on;
        self.save_settings();
        if on {
            self.start_host();
        } else {
            self.stop_host();
        }
        self.refresh_hosting();
    }

    fn start_host(&mut self) {
        if self.host.is_some() || self.host_starting {
            return;
        }
        self.host_starting = true;
        let ctx = self.ctx.clone();
        let cfg = HostConfig {
            device_name: self.settings.device_name.clone(),
            bind: SocketAddr::from(([0, 0, 0, 0], self.settings.host_port)),
            allow_pairing: self.settings.allow_pairing,
            source: SourceKind::Desktop,
            input: InputMode::Inject,
            pairing_timeout: Duration::from_secs(60),
        };
        self.rt.spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let result = host::start(&ctx, cfg, tx).await.map_err(|e| e.to_string());
            let ok = result.is_ok();
            post(UiMsg::HostStarted(result));
            if ok {
                while let Some(ev) = rx.recv().await {
                    post(UiMsg::Host(ev));
                }
            }
        });
    }

    fn start_announcer(&mut self) {
        if let Some(stop) = self.announce_stop.take() {
            stop.set();
        }
        let stop = CancelFlag::new();
        let beacon = Beacon::new(
            &self.settings.device_name,
            self.ctx.identity.fingerprint(),
            self.settings.host_port,
        );
        let flag = stop.clone();
        self.rt.spawn(async move {
            if let Err(e) = discovery::announce(beacon, Duration::from_secs(1), flag).await {
                tracing::warn!(error = %e, "LAN announcement stopped");
            }
        });
        self.announce_stop = Some(stop);
    }

    fn stop_host(&mut self) {
        if let Some(stop) = self.announce_stop.take() {
            stop.set();
        }
        if let Some(req) = self.pairing.take() {
            req.respond(false);
        }
        self.ui.set_host_request_code(s(""));
        self.host_peer = None;
        if let Some(h) = self.host.take() {
            self.rt.spawn(h.shutdown());
        }
        if self.client.as_ref().is_none_or(|c| !c.connected) {
            self.ui.set_stats(model(Vec::new()));
        }
    }

    fn on_host_event(&mut self, ev: HostEvent) {
        match ev {
            HostEvent::Listening(_) | HostEvent::Paired { .. } => self.refresh_trusted(),
            HostEvent::PairingRequest(req) => {
                if self.pairing.is_some() {
                    // One request at a time; the other device can retry.
                    req.respond(false);
                    return;
                }
                self.ui.set_host_request_name(s(&req.peer_name));
                self.ui.set_host_request_code(s(&req.code));
                self.ui.window().request_redraw();
                self.pairing = Some(req);
            }
            HostEvent::PairingCancelled { fingerprint } => {
                if self
                    .pairing
                    .as_ref()
                    .is_some_and(|p| p.fingerprint == fingerprint)
                {
                    self.pairing = None;
                    self.ui.set_host_request_code(s(""));
                }
            }
            HostEvent::SessionStarted { peer_name, .. } => {
                self.host_peer = Some(peer_name);
                self.refresh_hosting();
                self.refresh_trusted();
            }
            HostEvent::Stats(st) => {
                if self.client.as_ref().is_none_or(|c| !c.connected) {
                    self.ui
                        .set_stats_title(s("SESSION EN COURS (CET ORDINATEUR EST PARTAGÉ)"));
                    self.ui.set_stats(model(host_stat_lines(&st)));
                }
            }
            HostEvent::SessionEnded { peer_name, reason } => {
                tracing::info!(%peer_name, %reason, "hosted session ended");
                self.host_peer = None;
                self.refresh_hosting();
                if self.client.as_ref().is_none_or(|c| !c.connected) {
                    self.ui.set_stats(model(Vec::new()));
                }
            }
            HostEvent::Warning(w) => self.show_error(w),
        }
    }

    // ---------- Client ----------

    fn find_target(&self, id: &str) -> Option<Target> {
        let display = 0;
        if let Some(hex) = id.strip_prefix("saved:") {
            let fp = Fingerprint::from_hex(hex)?;
            let c = self
                .settings
                .computers
                .iter()
                .find(|c| c.fingerprint == fp)?;
            // A fresher LAN address for the same (pinned) identity wins.
            let address = self
                .discovered
                .get(&fp)
                .filter(|(_, seen)| seen.elapsed() < DISCOVERY_TTL)
                .map_or_else(|| c.address.clone(), |(h, _)| h.addr.to_string());
            return Some(Target {
                name: c.name.clone(),
                address,
                pinned: Some(fp),
                display,
            });
        }
        if let Some(hex) = id.strip_prefix("lan:") {
            let fp = Fingerprint::from_hex(hex)?;
            let (h, _) = self.discovered.get(&fp)?;
            // Beacons are unauthenticated: never pin from a beacon, always pair.
            return Some(Target {
                name: h.name.clone(),
                address: h.addr.to_string(),
                pinned: None,
                display,
            });
        }
        None
    }

    fn connect_to(&mut self, target: Target) {
        if self.client.is_some() {
            return;
        }
        self.next_session += 1;
        let session = self.next_session;
        self.ui.set_error_text(s(""));
        self.ui.set_client_code(s(""));
        self.ui
            .set_connecting_text(s(format!("Connexion à « {} »", target.name)));
        self.ui.set_connecting(true);
        let address = target.address.clone();
        self.rt.spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || resolve(&address).map_err(|e| e.to_string()))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
            post(UiMsg::Resolved { session, result });
        });
        self.client = Some(ClientSession {
            id: session,
            handle: None,
            target,
            connected: false,
            displays: Vec::new(),
            reconnect_display: None,
            foreground: true,
            #[cfg(windows)]
            keyboard: None,
        });
    }

    fn on_resolved(&mut self, session: u64, result: Result<SocketAddr, String>) {
        let Some(c) = self.client.as_mut().filter(|c| c.id == session) else {
            return;
        };
        let addr = match result {
            Ok(a) => a,
            Err(e) => {
                self.client = None;
                self.ui.set_connecting(false);
                self.show_error(error_text(&format!("cannot resolve: {e}")));
                return;
            }
        };
        let cfg = ClientConfig {
            address: addr,
            policy: c
                .target
                .pinned
                .map_or(ServerPolicy::Pairing, ServerPolicy::Pinned),
            device_name: self.settings.device_name.clone(),
            request: self.settings.stream_request(c.target.display),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            while let Some(event) = rx.recv().await {
                post(UiMsg::Client { session, event });
            }
        });
        let slot = self.frames.clone();
        let weak = self.ui.as_weak();
        let on_frame = Box::new(move |f: client::DecodedFrame<'_>| {
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(f.width, f.height);
            buf.make_mut_bytes().copy_from_slice(f.rgba);
            if let Ok(mut g) = slot.frame.lock() {
                *g = Some(buf);
            }
            if !slot.pending.swap(true, Ordering::AcqRel) {
                let slot = slot.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    slot.pending.store(false, Ordering::Release);
                    let Some(buf) = slot.frame.lock().ok().and_then(|mut g| g.take()) else {
                        return;
                    };
                    if !ui.get_in_session() {
                        return;
                    }
                    let (w, h) = (buf.width() as i32, buf.height() as i32);
                    if ui.get_stream_width() != w || ui.get_stream_height() != h {
                        ui.set_stream_width(w);
                        ui.set_stream_height(h);
                    }
                    ui.set_frame(Image::from_rgba8(buf));
                    ui.set_waiting_frame(false);
                });
            }
        });
        c.handle = Some(client::start(
            self.rt.handle(),
            self.ctx.identity.clone(),
            cfg,
            tx,
            on_frame,
        ));
    }

    fn on_client_event(&mut self, session: u64, ev: ClientEvent) {
        if self.client.as_ref().is_none_or(|c| c.id != session) {
            return;
        }
        match ev {
            ClientEvent::Connecting => {}
            ClientEvent::PairingCode(code) => self.ui.set_client_code(s(code)),
            ClientEvent::Connected {
                host_name,
                host_fingerprint,
                params,
                displays,
                newly_paired,
            } => {
                let Some(c) = self.client.as_mut() else {
                    return;
                };
                c.connected = true;
                c.displays = displays.clone();
                c.target.pinned = Some(host_fingerprint);
                c.target.name = host_name.clone();
                let saved = SavedComputer {
                    name: host_name.clone(),
                    address: c.target.address.clone(),
                    fingerprint: host_fingerprint,
                };
                let display = c.target.display;
                #[cfg(windows)]
                {
                    if let Some(handle) = c.handle.as_ref() {
                        let sender = handle.input_sender();
                        match ruststream_input::windows::KeyboardCapture::install(
                            move |ev| match ev {
                                ruststream_input::windows::HookEvent::Input(e) => sender.send(e),
                                ruststream_input::windows::HookEvent::Escape => {
                                    post(UiMsg::KeyboardEscape)
                                }
                            },
                        ) {
                            Ok(k) => {
                                k.set_active(true);
                                c.keyboard = Some(k);
                            }
                            Err(e) => tracing::warn!(error = %e, "keyboard capture unavailable"),
                        }
                    }
                }
                let _ = newly_paired;
                self.settings.upsert_computer(saved);
                self.save_settings();
                let ui = &self.ui;
                ui.set_connecting(false);
                ui.set_client_code(s(""));
                ui.set_session_title(s(&host_name));
                ui.set_stream_width(params.width as i32);
                ui.set_stream_height(params.height as i32);
                ui.set_waiting_frame(true);
                ui.set_capture_paused(false);
                let names: Vec<SharedString> = if displays.len() > 1 {
                    (1..=displays.len())
                        .map(|i| s(format!("Écran {i}")))
                        .collect()
                } else {
                    Vec::new()
                };
                ui.set_remote_displays(model(names));
                ui.set_remote_display(
                    displays
                        .iter()
                        .position(|d| d.index == params.display)
                        .unwrap_or(display as usize) as i32,
                );
                ui.set_stats_title(s("SESSION EN COURS"));
                ui.set_in_session(true);
                self.refresh_computers();
            }
            ClientEvent::SessionChanged(p) => {
                self.ui.set_stream_width(p.width as i32);
                self.ui.set_stream_height(p.height as i32);
            }
            ClientEvent::Stats(st) => self.ui.set_stats(model(session_stat_lines(&st))),
            ClientEvent::Disconnected {
                reason,
                error,
                rejected,
            } => {
                let Some(c) = self.client.take() else { return };
                self.end_session_ui();
                if let Some(display) = c.reconnect_display {
                    let mut target = c.target;
                    target.display = display;
                    self.connect_to(target);
                    return;
                }
                if let Some(r) = rejected {
                    self.show_error(reject_text(r));
                } else if error {
                    self.show_error(error_text(&reason));
                } else if c.connected
                    && (reason.starts_with("host") || reason.contains("closed by peer"))
                {
                    self.show_error(format!(
                        "Session terminée par l'autre ordinateur ({reason})."
                    ));
                }
            }
        }
    }

    fn end_session_ui(&self) {
        let ui = &self.ui;
        if ui.get_fullscreen() {
            ui.window().set_fullscreen(false);
            ui.set_fullscreen(false);
        }
        ui.set_in_session(false);
        ui.set_connecting(false);
        ui.set_client_code(s(""));
        ui.set_capture_paused(false);
        ui.set_frame(Image::default());
        ui.set_stats(model(Vec::new()));
        ui.set_remote_displays(model(Vec::new()));
        self.refresh_computers();
    }

    fn send_input(&self, ev: InputEvent) {
        if let Some(c) = self.client.as_ref().filter(|c| c.connected)
            && let Some(h) = c.handle.as_ref()
        {
            h.send_input(ev);
        }
    }

    fn keyboard_escape(&mut self) {
        #[cfg(windows)]
        if let Some(k) = self.client.as_ref().and_then(|c| c.keyboard.as_ref()) {
            k.set_active(false);
        }
        self.send_input(InputEvent::ReleaseAll);
        self.ui.set_capture_paused(true);
        if self.ui.get_fullscreen() {
            self.ui.window().set_fullscreen(false);
            self.ui.set_fullscreen(false);
        }
    }

    fn resume_capture(&mut self) {
        #[cfg(windows)]
        if let Some(k) = self.client.as_ref().and_then(|c| c.keyboard.as_ref()) {
            k.set_active(true);
        }
        self.ui.set_capture_paused(false);
    }

    /// Periodic housekeeping (250 ms): focus loss, LAN list expiry.
    fn tick(&mut self, n: u64) {
        #[cfg(windows)]
        {
            let fg = ruststream_input::windows::foreground_is_ours();
            let mut lost_focus = false;
            if let Some(c) = self.client.as_mut().filter(|c| c.connected) {
                lost_focus = c.foreground && !fg;
                c.foreground = fg;
            }
            if lost_focus {
                // Keys held while switching away must not stay pressed remotely.
                self.send_input(InputEvent::ReleaseAll);
            }
        }
        if n.is_multiple_of(8) {
            let before = self.discovered.len();
            self.discovered
                .retain(|_, (_, seen)| seen.elapsed() < DISCOVERY_TTL * 3);
            if before != self.discovered.len() || n.is_multiple_of(16) {
                self.refresh_computers();
            }
        }
    }

    fn on_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::HostStarted(Ok(handle)) => {
                self.host_starting = false;
                if self.settings.hosting_enabled {
                    handle.set_allow_pairing(self.settings.allow_pairing);
                    self.host = Some(handle);
                    self.start_announcer();
                } else {
                    self.rt.spawn(handle.shutdown());
                }
                self.refresh_hosting();
            }
            UiMsg::HostStarted(Err(e)) => {
                self.host_starting = false;
                self.settings.hosting_enabled = false;
                self.save_settings();
                self.refresh_hosting();
                self.show_error(format!(
                    "Impossible de partager cet ordinateur : {e}. Le port UDP {} est peut-être déjà utilisé.",
                    self.settings.host_port
                ));
            }
            UiMsg::Host(ev) => self.on_host_event(ev),
            UiMsg::Discovered(h) => {
                let is_new = !self.discovered.contains_key(&h.fingerprint);
                self.discovered.insert(h.fingerprint, (h, Instant::now()));
                if is_new {
                    self.refresh_computers();
                }
            }
            UiMsg::Resolved { session, result } => self.on_resolved(session, result),
            UiMsg::Client { session, event } => self.on_client_event(session, event),
            UiMsg::KeyboardEscape => self.keyboard_escape(),
        }
    }
}

fn wire_callbacks(ui: &AppWindow) {
    ui.on_set_hosting(|on| {
        with_app(|a| a.set_hosting(on));
    });
    ui.on_set_allow_pairing(|v| {
        with_app(|a| {
            a.settings.allow_pairing = v;
            a.save_settings();
            if let Some(h) = &a.host {
                h.set_allow_pairing(v);
            }
            a.ui.set_allow_pairing(v);
        });
    });
    ui.on_stop_host_session(|| {
        with_app(|a| {
            if let Some(h) = &a.host {
                h.end_session();
            }
        });
    });
    ui.on_answer_pairing(|accept| {
        with_app(|a| {
            if let Some(req) = a.pairing.take() {
                req.respond(accept);
            }
            a.ui.set_host_request_code(s(""));
        });
    });
    ui.on_revoke(|id| {
        with_app(|a| {
            if let Some(fp) = Fingerprint::from_hex(&id) {
                let r = a.ctx.trusted_clients.lock().map(|mut t| t.revoke(&fp));
                if let Ok(Err(e)) = r {
                    a.show_error(format!("Révocation impossible : {e}"));
                }
            }
            a.refresh_trusted();
        });
    });
    ui.on_rename_device(|name| {
        with_app(|a| {
            let name = name.trim();
            if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                a.show_error("Nom invalide (1 à 64 caractères).");
                a.ui.set_device_name(s(&a.settings.device_name));
                return;
            }
            a.settings.device_name = name.to_string();
            a.save_settings();
            a.ui.set_device_name(s(name));
            if a.host.is_some() {
                a.start_announcer();
            }
        });
    });
    ui.on_set_dark(|v| {
        with_app(|a| {
            a.settings.theme = if v { ThemePref::Dark } else { ThemePref::Light };
            a.save_settings();
            a.ui.set_dark(v);
            a.ui.invoke_apply_theme();
        });
    });
    ui.on_set_animations(|v| {
        with_app(|a| {
            a.settings.animations = v;
            a.save_settings();
            a.ui.set_animations(v);
        });
    });
    ui.on_set_profile(|p| {
        with_app(|a| {
            a.settings.profile = match p {
                1 => Profile::Performance,
                2 => Profile::Quality,
                _ => Profile::Auto,
            };
            a.settings.advanced = None;
            a.save_settings();
            a.ui.set_profile(p);
            a.ui.set_advanced(false);
        });
    });
    ui.on_set_advanced(|on, fps, mbps| {
        with_app(|a| {
            a.settings.advanced = on.then(|| Advanced {
                fps: u16::try_from(fps.clamp(1, 360)).unwrap_or(60),
                bitrate_kbps: u32::try_from(mbps.clamp(1, 2000)).unwrap_or(20) * 1000,
            });
            a.save_settings();
            a.ui.set_advanced(on);
        });
    });
    ui.on_connect(|id| {
        with_app(|a| match a.find_target(&id) {
            Some(t) => a.connect_to(t),
            None => a.show_error("Cet ordinateur n'est plus disponible."),
        });
    });
    ui.on_add_computer(|text| {
        with_app(|a| match parse_address(&text) {
            Some(address) => a.connect_to(Target {
                name: address.clone(),
                address,
                pinned: None,
                display: 0,
            }),
            None => a.show_error("Adresse invalide. Exemple : 192.168.1.20 ou mon-pc.local"),
        });
    });
    ui.on_remove_computer(|id| {
        with_app(|a| {
            if let Some(fp) = id.strip_prefix("saved:").and_then(Fingerprint::from_hex) {
                a.settings.remove_computer(&fp);
                a.save_settings();
                a.refresh_computers();
            }
        });
    });
    ui.on_cancel_connect(|| {
        with_app(|a| {
            match a.client.as_ref().and_then(|c| c.handle.as_ref()) {
                Some(h) => h.disconnect(),
                None => {
                    // Still resolving the address: just forget the attempt.
                    a.client = None;
                    a.end_session_ui();
                }
            }
        });
    });
    ui.on_confirm_pairing(|same| {
        with_app(|a| {
            if let Some(c) = a.client.as_ref()
                && let Some(h) = c.handle.as_ref()
            {
                h.confirm_pairing(same);
                if same {
                    a.ui.set_connecting_text(s(format!(
                        "Autorisez la connexion sur « {} »",
                        c.target.name
                    )));
                }
            }
            a.ui.set_client_code(s(""));
        });
    });
    ui.on_disconnect(|| {
        with_app(|a| {
            if let Some(h) = a.client.as_ref().and_then(|c| c.handle.as_ref()) {
                h.disconnect();
            }
        });
    });
    ui.on_toggle_fullscreen(|| {
        with_app(|a| {
            let v = !a.ui.get_fullscreen();
            a.ui.window().set_fullscreen(v);
            a.ui.set_fullscreen(v);
        });
    });
    ui.on_select_display(|i| {
        with_app(|a| {
            let Some(c) = a.client.as_mut().filter(|c| c.connected) else {
                return;
            };
            let Some(d) = usize::try_from(i).ok().and_then(|i| c.displays.get(i)) else {
                return;
            };
            if d.index == c.target.display {
                return;
            }
            c.reconnect_display = Some(d.index);
            if let Some(h) = c.handle.as_ref() {
                h.disconnect();
            }
        });
    });
    ui.on_dismiss_error(|| {
        with_app(|a| a.ui.set_error_text(s("")));
    });
    ui.on_stream_pointer(|x, y| {
        with_app(|a| {
            let n = |v: f32| (v.clamp(0.0, 1.0) * f32::from(ABS_MAX)).round() as u16;
            a.send_input(InputEvent::MouseMoveAbs { x: n(x), y: n(y) });
        });
    });
    ui.on_stream_button(|b, down| {
        let button = match b {
            0 => MouseButton::Left,
            1 => MouseButton::Right,
            2 => MouseButton::Middle,
            3 => MouseButton::X1,
            4 => MouseButton::X2,
            _ => return,
        };
        with_app(|a| a.send_input(InputEvent::MouseButton { button, down }));
    });
    ui.on_stream_wheel(|dx, dy| {
        // Slint reports 60 logical px per wheel line; Windows uses 120 units.
        let conv = |v: f32| {
            (v * 2.0)
                .round()
                .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
        };
        let (dx, dy) = (conv(dx), conv(dy));
        if dx != 0 || dy != 0 {
            with_app(|a| a.send_input(InputEvent::Wheel { dx, dy }));
        }
    });
    ui.on_resume_capture(|| {
        with_app(|a| a.resume_capture());
    });
}

fn main() -> anyhow::Result<()> {
    ruststream_core::init_platform();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("rs-engine")
        .enable_all()
        .build()?;
    let paths = Paths::user_default()?;
    let protector = ruststream_core::default_protector();
    let ctx = AppContext::load(paths, protector.as_ref())?;
    let settings = Settings::load(&ctx.paths.settings()).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "settings unreadable, using defaults");
        Settings::default()
    });

    let ui = AppWindow::new()?;
    wire_callbacks(&ui);

    #[cfg(windows)]
    let gpus: Vec<SharedString> = {
        // DXGI can list the same adapter once per output: deduplicate.
        let mut list = ruststream_core::gpus().unwrap_or_default();
        list.retain(|g| !g.software);
        list.dedup_by(|a, b| a.name == b.name && a.vendor == b.vendor);
        list
    }
    .into_iter()
    .map(|g| {
        let family = g
            .vendor
            .encoder_family()
            .map(|f| format!(" — {f} détecté (encodage matériel pas encore pris en charge)"))
            .unwrap_or_default();
        s(format!("{}{family}", g.name))
    })
    .collect();
    #[cfg(not(windows))]
    let gpus: Vec<SharedString> = Vec::new();
    ui.set_gpus(model(gpus));
    ui.set_encoder_text(s(
        "Encodeur vidéo actuel : OpenH264 (logiciel, H.264). Les encodeurs matériels NVENC, AMF et Quick Sync ne sont pas encore implémentés : le flux est plus lent et sollicite davantage le processeur.",
    ));

    // LAN discovery listener.
    let discovery_stop = CancelFlag::new();
    {
        let stop = discovery_stop.clone();
        rt.spawn(async move {
            let (tx, mut rx) = mpsc::channel(32);
            let listener = tokio::spawn(discovery::listen(tx, stop));
            while let Some(h) = rx.recv().await {
                post(UiMsg::Discovered(h));
            }
            if let Ok(Err(e)) = listener.await {
                tracing::warn!(error = %e, "LAN discovery unavailable");
            }
        });
    }

    let hosting = settings.hosting_enabled;
    APP.with(|cell| {
        *cell.borrow_mut() = Some(App {
            ui: ui.clone_strong(),
            rt,
            ctx,
            settings,
            host: None,
            host_starting: false,
            announce_stop: None,
            pairing: None,
            host_peer: None,
            discovered: HashMap::new(),
            discovery_stop,
            client: None,
            next_session: 0,
            frames: Arc::new(FrameSlot {
                frame: Mutex::new(None),
                pending: AtomicBool::new(false),
            }),
        });
    });
    with_app(|a| {
        a.refresh_all();
        if hosting {
            a.start_host();
        }
    });

    let timer = slint::Timer::default();
    let mut n = 0u64;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            n += 1;
            with_app(|a| a.tick(n));
        },
    );

    ui.run()?;
    drop(timer);

    // Orderly shutdown: end the session, stop hosting, release the runtime.
    if let Some(mut app) = APP.with(|cell| cell.borrow_mut().take()) {
        if let Some(c) = app.client.take()
            && let Some(h) = c.handle
        {
            h.disconnect();
        }
        app.discovery_stop.set();
        if let Some(stop) = app.announce_stop.take() {
            stop.set();
        }
        if let Some(h) = app.host.take() {
            app.rt.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(3), h.shutdown()).await;
            });
        }
        app.rt.shutdown_timeout(Duration::from_secs(2));
    }
    Ok(())
}
