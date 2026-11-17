//! `ruststream-cli`: headless host/client, diagnostics and benchmarks.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ruststream_core::client::{self, ClientConfig, ClientEvent};
use ruststream_core::host::{self, HostConfig, HostEvent, InputMode, SourceKind};
use ruststream_core::security::{Fingerprint, PlaintextProtector, ServerPolicy, TrustStore};
use ruststream_core::settings::{Settings, parse_address, resolve};
use ruststream_core::telemetry::{Summary, Window};
use ruststream_core::{AppContext, Paths};
use tokio::sync::mpsc;

const USAGE: &str = "\
RustStream command line

USAGE:
    ruststream-cli <COMMAND> [OPTIONS]

COMMANDS:
    info                          Device fingerprint, displays and GPUs
    host [--port N] [--view-only] [--pattern WxH] [--no-pairing]
                                  Share this computer (pairing approved in the terminal)
    connect <ADDRESS> [--fingerprint HEX] [--seconds N]
                                  Headless client printing live statistics
    bench-loopback [--size WxH] [--fps N] [--bitrate KBPS] [--seconds N]
                                  In-process host+client over 127.0.0.1 (synthetic frames)
    bench-capture [--display N] [--seconds N]
                                  Desktop capture + software encode timings
    trusted [--revoke HEX]        List (or revoke) devices allowed to connect

Logs: set RUST_LOG=debug for details.
";

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        match self.0.iter().position(|a| a == name) {
            Some(i) => {
                self.0.remove(i);
                true
            }
            None => false,
        }
    }

    fn value(&mut self, name: &str) -> Result<Option<String>> {
        match self.0.iter().position(|a| a == name) {
            Some(i) if i + 1 < self.0.len() => {
                let v = self.0.remove(i + 1);
                self.0.remove(i);
                Ok(Some(v))
            }
            Some(_) => bail!("missing value for {name}"),
            None => Ok(None),
        }
    }

    fn parsed<T: std::str::FromStr>(&mut self, name: &str, default: T) -> Result<T> {
        match self.value(name)? {
            Some(v) => v
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid value for {name}: {v}")),
            None => Ok(default),
        }
    }

    fn finish(self) -> Result<()> {
        if let Some(extra) = self.0.first() {
            bail!("unexpected argument: {extra}");
        }
        Ok(())
    }
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').context("size must be WIDTHxHEIGHT")?;
    Ok((w.parse()?, h.parse()?))
}

fn load_context() -> Result<AppContext> {
    let paths = Paths::user_default()?;
    let protector = ruststream_core::default_protector();
    Ok(AppContext::load(paths, protector.as_ref())?)
}

fn main() -> Result<()> {
    ruststream_core::init_platform();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || raw[0] == "-h" || raw[0] == "--help" {
        print!("{USAGE}");
        return Ok(());
    }
    let cmd = raw.remove(0);
    let args = Args(raw);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rs-cli-rt")
        .build()?;
    match cmd.as_str() {
        "info" => info(args),
        "host" => rt.block_on(host_cmd(args)),
        "connect" => rt.block_on(connect_cmd(args)),
        "bench-loopback" => rt.block_on(bench_loopback(args)),
        "bench-capture" => bench_capture(args),
        "trusted" => trusted(args),
        other => {
            eprint!("unknown command '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn info(args: Args) -> Result<()> {
    args.finish()?;
    let ctx = load_context()?;
    println!("Device name:  {}", ruststream_core::default_device_name());
    println!("Fingerprint:  {}", ctx.identity.fingerprint().to_hex());
    println!("Config dir:   {}", ctx.paths.config_dir.display());
    match ruststream_capture::list_displays() {
        Ok(ds) => {
            println!("Displays:");
            for d in ds {
                println!(
                    "  [{}] {} {}x{} @ {} Hz at ({}, {}){} — {}",
                    d.index,
                    d.name,
                    d.width,
                    d.height,
                    d.refresh_hz,
                    d.left,
                    d.top,
                    if d.primary { " primary" } else { "" },
                    d.adapter
                );
            }
        }
        Err(e) => println!("Displays: unavailable ({e})"),
    }
    #[cfg(windows)]
    match ruststream_platform_windows_info() {
        Ok(lines) => {
            println!("GPUs:");
            for l in lines {
                println!("  {l}");
            }
        }
        Err(e) => println!("GPUs: unavailable ({e})"),
    }
    println!("Video encoder: OpenH264 (software fallback; hardware encoders not implemented yet)");
    Ok(())
}

#[cfg(windows)]
fn ruststream_platform_windows_info() -> Result<Vec<String>, String> {
    Ok(ruststream_core::gpus()?
        .into_iter()
        .map(|g| {
            format!(
                "{} — {:?}, {} MB VRAM{}",
                g.name,
                g.vendor,
                g.dedicated_vram_mb,
                if g.software {
                    " (software adapter)"
                } else {
                    ""
                }
            )
        })
        .collect())
}

fn trusted(mut args: Args) -> Result<()> {
    let revoke = args.value("--revoke")?;
    args.finish()?;
    let paths = Paths::user_default()?;
    let mut store = TrustStore::load(&paths.trusted_clients())?;
    if let Some(hex) = revoke {
        let fp = Fingerprint::from_hex(&hex).context("invalid fingerprint")?;
        if store.revoke(&fp)? {
            println!("Revoked {fp}");
        } else {
            println!("Not found");
        }
        return Ok(());
    }
    if store.devices().is_empty() {
        println!("No trusted devices.");
    }
    for d in store.devices() {
        println!("{}  {}  {}", d.fingerprint.to_hex(), d.fingerprint, d.name);
    }
    Ok(())
}

async fn read_line() -> String {
    tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s.trim().to_lowercase()
    })
    .await
    .unwrap_or_default()
}

async fn host_cmd(mut args: Args) -> Result<()> {
    let port: u16 = args.parsed("--port", ruststream_core::protocol::DEFAULT_PORT)?;
    let view_only = args.flag("--view-only");
    let no_pairing = args.flag("--no-pairing");
    let pattern = args
        .value("--pattern")?
        .map(|s| parse_size(&s))
        .transpose()?;
    args.finish()?;
    let ctx = load_context()?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = HostConfig {
        device_name: ruststream_core::default_device_name(),
        bind: SocketAddr::from(([0, 0, 0, 0], port)),
        allow_pairing: !no_pairing,
        source: match pattern {
            Some((width, height)) => SourceKind::TestPattern { width, height },
            None => SourceKind::Desktop,
        },
        input: if view_only {
            InputMode::Ignore
        } else {
            InputMode::Inject
        },
        pairing_timeout: Duration::from_secs(60),
    };
    let handle = host::start(&ctx, cfg, tx).await?;
    println!(
        "Sharing this computer on UDP port {} — fingerprint {}",
        handle.local_addr().port(),
        ctx.identity.fingerprint()
    );
    println!("Press Ctrl+C to stop.");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            ev = rx.recv() => match ev {
                Some(HostEvent::PairingRequest(req)) => {
                    println!(
                        "\nPairing request from '{}' ({}) — code {}\nDoes the other device show the same code? Accept? [y/N]",
                        req.peer_name, req.address, req.code
                    );
                    let accept = read_line().await == "y";
                    req.respond(accept);
                }
                Some(HostEvent::SessionStarted { peer_name, params, encoder, .. }) => {
                    println!("Session started with '{peer_name}': {}x{} @ {} fps, {} kbps, {encoder}",
                        params.width, params.height, params.fps, params.bitrate_kbps);
                }
                Some(HostEvent::Stats(s)) => {
                    println!("  {:.0} fps  {:.1} Mbit/s  encode p50 {:.1} ms p95 {:.1} ms  rtt {:.1} ms",
                        s.fps, s.bitrate_mbps, ms(s.encode_us.p50), ms(s.encode_us.p95), s.rtt_ms);
                }
                Some(HostEvent::SessionEnded { peer_name, reason }) => println!("Session with '{peer_name}' ended: {reason}"),
                Some(HostEvent::Warning(w)) => println!("Warning: {w}"),
                Some(other) => tracing::debug!(?other, "host event"),
                None => break,
            }
        }
    }
    handle.shutdown().await;
    Ok(())
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

async fn connect_cmd(mut args: Args) -> Result<()> {
    let fp = args
        .value("--fingerprint")?
        .map(|h| Fingerprint::from_hex(&h).context("invalid fingerprint"))
        .transpose()?;
    let seconds: u64 = args.parsed("--seconds", 0)?;
    let address = args.0.first().cloned().context("missing ADDRESS")?;
    args.0.remove(0);
    args.finish()?;
    let address = parse_address(&address).context("invalid address")?;
    let addr = tokio::task::spawn_blocking(move || resolve(&address)).await??;
    let ctx = load_context()?;
    let settings = Settings::load(&ctx.paths.settings())?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let frames = Arc::new(AtomicU32::new(0));
    let f = frames.clone();
    let handle = client::start(
        &tokio::runtime::Handle::current(),
        ctx.identity.clone(),
        ClientConfig {
            address: addr,
            policy: fp.map_or(ServerPolicy::Pairing, ServerPolicy::Pinned),
            device_name: settings.device_name.clone(),
            request: settings.stream_request(0),
        },
        tx,
        Box::new(move |_| {
            f.fetch_add(1, Ordering::Relaxed);
        }),
    );
    let deadline =
        (seconds > 0).then(|| tokio::time::Instant::now() + Duration::from_secs(seconds));
    loop {
        let ev = match deadline {
            Some(d) => tokio::select! {
                e = rx.recv() => e,
                _ = tokio::time::sleep_until(d) => { handle.disconnect(); rx.recv().await }
            },
            None => tokio::select! {
                e = rx.recv() => e,
                _ = tokio::signal::ctrl_c() => { handle.disconnect(); rx.recv().await }
            },
        };
        match ev {
            Some(ClientEvent::PairingCode(code)) => {
                println!(
                    "Pairing code: {code}\nDoes the host show exactly the same code? [y/N] (then approve on the host)"
                );
                let same = read_line().await == "y";
                handle.confirm_pairing(same);
            }
            Some(ClientEvent::Connected {
                host_name,
                host_fingerprint,
                params,
                ..
            }) => {
                println!(
                    "Connected to '{host_name}' ({host_fingerprint}): {}x{} @ {} fps",
                    params.width, params.height, params.fps
                );
                println!(
                    "Reconnect later with --fingerprint {}",
                    host_fingerprint.to_hex()
                );
            }
            Some(ClientEvent::Stats(s)) => println!(
                "  {:.0} fps  {:.1} Mbit/s  rtt {:.1} ms  decode p50 {:.1} ms  dropped {}  lost pkts {}",
                s.fps,
                s.bitrate_mbps,
                s.rtt_ms,
                ms(s.decode_us.p50),
                s.frames_dropped,
                s.lost_packets
            ),
            Some(ClientEvent::Disconnected { reason, error, .. }) => {
                println!("Disconnected: {reason}");
                if error {
                    std::process::exit(1);
                }
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    println!("Frames decoded: {}", frames.load(Ordering::Relaxed));
    Ok(())
}

fn print_summary(name: &str, s: &Summary) {
    println!(
        "| {name:<28} | {:>8.2} | {:>8.2} | {:>8.2} | {:>8.2} | {:>6} |",
        ms(s.p50),
        ms(s.p95),
        ms(s.p99),
        ms(s.max),
        s.count
    );
}

async fn bench_loopback(mut args: Args) -> Result<()> {
    let (w, h) = parse_size(&args.value("--size")?.unwrap_or_else(|| "1920x1080".into()))?;
    let fps: u16 = args.parsed("--fps", 60)?;
    let bitrate: u32 = args.parsed("--bitrate", 20_000)?;
    let seconds: u64 = args.parsed("--seconds", 10)?;
    args.finish()?;

    let dir = std::env::temp_dir().join(format!("ruststream-bench-{}", std::process::id()));
    let host_ctx = AppContext::load(Paths::in_dir(dir.join("host")), &PlaintextProtector)?;
    let client_ctx = AppContext::load(Paths::in_dir(dir.join("client")), &PlaintextProtector)?;
    host_ctx
        .trusted_clients
        .lock()
        .map_err(|_| anyhow::anyhow!("lock"))?
        .trust("bench client", client_ctx.identity.fingerprint())?;

    let (htx, mut hrx) = mpsc::unbounded_channel();
    let host = host::start(
        &host_ctx,
        HostConfig {
            device_name: "bench host".into(),
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            allow_pairing: false,
            source: SourceKind::TestPattern {
                width: w,
                height: h,
            },
            input: InputMode::Ignore,
            pairing_timeout: Duration::from_secs(1),
        },
        htx,
    )
    .await?;

    let mut request = Settings::default().stream_request(0);
    request.fps = fps;
    request.bitrate_kbps = bitrate;
    // Per-frame measurements, recorded on the decoder thread after warm-up.
    let measuring = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let per_frame = Arc::new(std::sync::Mutex::new((
        Window::new(100_000),
        Window::new(100_000),
    )));
    let (m, pf) = (measuring.clone(), per_frame.clone());
    let (ctx_tx, mut crx) = mpsc::unbounded_channel();
    let handle = client::start(
        &tokio::runtime::Handle::current(),
        client_ctx.identity.clone(),
        ClientConfig {
            address: host.local_addr(),
            policy: ServerPolicy::Pinned(host_ctx.identity.fingerprint()),
            device_name: "bench client".into(),
            request,
        },
        ctx_tx,
        Box::new(move |f| {
            if m.load(Ordering::Relaxed)
                && let Ok(mut g) = pf.lock()
            {
                g.0.record_duration(f.decode_time);
                g.1.record(ruststream_core::unix_micros().saturating_sub(f.capture_timestamp_us));
            }
        }),
    );

    println!(
        "Loopback benchmark: {w}x{h} @ {fps} fps, {bitrate} kbps, {seconds} s (synthetic frames, software H.264)"
    );
    let warmup = Duration::from_secs(2);
    let start = Instant::now();
    let mut encode = Window::new(100_000);
    let mut fps_samples = Vec::new();
    let mut last_client = None;
    let end = start + warmup + Duration::from_secs(seconds);
    while Instant::now() < end {
        if start.elapsed() > warmup {
            measuring.store(true, Ordering::Relaxed);
        }
        tokio::select! {
            Some(ev) = hrx.recv() => {
                if let HostEvent::Stats(s) = ev
                    && start.elapsed() > warmup
                {
                    // Summaries are per-second windows: keep their medians.
                    encode.record(s.encode_us.p50);
                }
            }
            Some(ev) = crx.recv() => match ev {
                ClientEvent::Stats(s) if start.elapsed() > warmup => {
                    fps_samples.push(s.fps);
                    last_client = Some(s);
                }
                ClientEvent::Disconnected { reason, .. } => bail!("disconnected: {reason}"),
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
    handle.disconnect();
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);

    measuring.store(false, Ordering::Relaxed);
    let (mut decode, mut e2e) = per_frame
        .lock()
        .map(|g| (g.0.clone(), g.1.clone()))
        .map_err(|_| anyhow::anyhow!("lock"))?;
    println!();
    println!(
        "Latency (ms). Decode and capture→decoded are per frame; encode is the per-second median."
    );
    println!(
        "| Stage                        |      p50 |      p95 |      p99 |      max |      n |"
    );
    println!(
        "|------------------------------|----------|----------|----------|----------|--------|"
    );
    print_summary("Encode (host, 1 s medians)", &encode.summary());
    print_summary("Decode (client)", &decode.summary());
    print_summary("Capture → decoded", &e2e.summary());
    let mean_fps = fps_samples.iter().sum::<f64>() / fps_samples.len().max(1) as f64;
    println!();
    println!("Mean decoded fps: {mean_fps:.1}");
    if let Some(s) = last_client {
        println!(
            "Last second: {:.1} Mbit/s, dropped frames {}, lost packets {}",
            s.bitrate_mbps, s.frames_dropped, s.lost_packets
        );
    }
    Ok(())
}

fn bench_capture(mut args: Args) -> Result<()> {
    let display: u32 = args.parsed("--display", 0)?;
    let seconds: u64 = args.parsed("--seconds", 5)?;
    args.finish()?;
    let mut src = ruststream_capture::open_display(display)?;
    let (w, h) = src.size();
    println!("Capturing {} for {seconds} s", src.describe());
    let mut encoder = ruststream_codec::create_encoder(ruststream_codec::EncoderSettings {
        width: w,
        height: h,
        fps: 60,
        bitrate_kbps: 20_000,
    })?;
    let mut acquire = Window::new(100_000);
    let mut enc = Window::new(100_000);
    let mut bits = Vec::new();
    let mut frames = 0u32;
    let mut idle = 0u32;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let t0 = Instant::now();
        let got = src.next_frame(Duration::from_millis(100))?.is_some();
        if !got {
            idle += 1;
            continue;
        }
        acquire.record_duration(t0.elapsed());
        if let Some(f) = src.last_frame() {
            let t1 = Instant::now();
            encoder.encode(f.data, f.width, f.height, frames == 0, &mut bits)?;
            enc.record_duration(t1.elapsed());
        }
        frames += 1;
    }
    println!("Frames with desktop changes: {frames}, timeouts without change: {idle}");
    println!(
        "| Stage                        |      p50 |      p95 |      p99 |      max |      n |"
    );
    println!(
        "|------------------------------|----------|----------|----------|----------|--------|"
    );
    print_summary("Acquire + GPU→CPU copy", &acquire.summary());
    print_summary("Encode (OpenH264, software)", &enc.summary());
    println!("Note: acquire time includes waiting for the next desktop update (vsync).");
    Ok(())
}
