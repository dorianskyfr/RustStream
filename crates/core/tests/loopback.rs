//! End-to-end session tests over 127.0.0.1: real QUIC + TLS, pairing,
//! H.264 encoding/decoding and datagram transport. The host streams a
//! synthetic test pattern and ignores input, so these tests never touch the
//! machine's real desktop, mouse or keyboard.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ruststream_core::client::{self, ClientConfig, ClientEvent, ClientHandle};
use ruststream_core::host::{self, HostConfig, HostEvent, HostHandle, InputMode, SourceKind};
use ruststream_core::settings::Settings;
use ruststream_core::{AppContext, CoreError, Paths};
use ruststream_protocol::RejectReason;
use ruststream_security::{PlaintextProtector, ServerPolicy};
use tokio::sync::mpsc;
use tokio::time::timeout;

const W: u32 = 320;
const H: u32 = 180;

fn context(dir: &tempfile::TempDir, name: &str) -> AppContext {
    AppContext::load(Paths::in_dir(dir.path().join(name)), &PlaintextProtector).expect("context")
}

async fn start_host(
    ctx: &AppContext,
    allow_pairing: bool,
) -> (HostHandle, mpsc::UnboundedReceiver<HostEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let cfg = HostConfig {
        device_name: "Test host".into(),
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        allow_pairing,
        source: SourceKind::TestPattern {
            width: W,
            height: H,
        },
        input: InputMode::Ignore,
        pairing_timeout: Duration::from_secs(10),
    };
    let handle = host::start(ctx, cfg, tx).await.expect("host start");
    (handle, rx)
}

struct TestClient {
    handle: ClientHandle,
    events: mpsc::UnboundedReceiver<ClientEvent>,
    frames: Arc<AtomicU32>,
    last_size: Arc<Mutex<(u32, u32)>>,
}

fn start_client(ctx: &AppContext, addr: SocketAddr, policy: ServerPolicy) -> TestClient {
    let (tx, rx) = mpsc::unbounded_channel();
    let frames = Arc::new(AtomicU32::new(0));
    let last_size = Arc::new(Mutex::new((0, 0)));
    let (f, s) = (frames.clone(), last_size.clone());
    let cfg = ClientConfig {
        address: addr,
        policy,
        device_name: "Test client".into(),
        request: Settings::default().stream_request(0),
    };
    let handle = client::start(
        &tokio::runtime::Handle::current(),
        ctx.identity.clone(),
        cfg,
        tx,
        Box::new(move |frame| {
            assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
            *s.lock().expect("lock") = (frame.width, frame.height);
            f.fetch_add(1, Ordering::Relaxed);
        }),
    );
    TestClient {
        handle,
        events: rx,
        frames,
        last_size,
    }
}

async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("event timeout")
        .expect("channel closed")
}

async fn wait_connected(c: &mut TestClient) -> (bool, Option<String>) {
    let mut code = None;
    loop {
        match next(&mut c.events).await {
            ClientEvent::Connecting => {}
            ClientEvent::PairingCode(s) => {
                // The local user confirms the codes match.
                c.handle.confirm_pairing(true);
                code = Some(s);
            }
            ClientEvent::Connected { newly_paired, .. } => return (newly_paired, code),
            ClientEvent::Disconnected { reason, .. } => panic!("disconnected: {reason}"),
            _ => {}
        }
    }
}

async fn wait_disconnected(c: &mut TestClient) -> String {
    loop {
        if let ClientEvent::Disconnected { reason, .. } = next(&mut c.events).await {
            return reason;
        }
    }
}

async fn wait_frames(c: &TestClient, n: u32) {
    timeout(Duration::from_secs(20), async {
        while c.frames.load(Ordering::Relaxed) < n {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("frames not received in time");
}

/// Answers the next pairing request and checks the code matches the client's.
async fn answer_pairing(host_rx: &mut mpsc::UnboundedReceiver<HostEvent>, accept: bool) -> String {
    loop {
        match next(host_rx).await {
            HostEvent::PairingRequest(req) => {
                assert_eq!(req.peer_name, "Test client");
                let code = req.code.clone();
                req.respond(accept);
                return code;
            }
            HostEvent::Listening(_) | HostEvent::Warning(_) => {}
            other => panic!("unexpected host event: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_streaming_reconnect_and_revocation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host_ctx = context(&dir, "host");
    let client_ctx = context(&dir, "client");
    let (host, mut host_rx) = start_host(&host_ctx, true).await;
    let addr = host.local_addr();

    // 1. First connection: pairing with matching codes on both sides.
    let mut c = start_client(&client_ctx, addr, ServerPolicy::Pairing);
    let host_code = answer_pairing(&mut host_rx, true).await;
    let (newly_paired, client_code) = wait_connected(&mut c).await;
    assert!(newly_paired);
    assert_eq!(client_code.as_deref(), Some(host_code.as_str()));
    wait_frames(&c, 20).await;
    assert_eq!(*c.last_size.lock().expect("lock"), (W, H));
    c.handle.disconnect();
    assert_eq!(wait_disconnected(&mut c).await, "disconnected");
    assert!(
        host_ctx
            .trusted_clients
            .lock()
            .expect("lock")
            .is_trusted(&client_ctx.identity.fingerprint())
    );

    // 2. Reconnection with the pinned host fingerprint: no pairing needed.
    let mut c = start_client(
        &client_ctx,
        addr,
        ServerPolicy::Pinned(host_ctx.identity.fingerprint()),
    );
    let (newly_paired, code) = wait_connected(&mut c).await;
    assert!(!newly_paired);
    assert!(code.is_none());
    wait_frames(&c, 10).await;
    // Dropping the handle must disconnect as well.
    let TestClient {
        handle, mut events, ..
    } = c;
    drop(handle);
    loop {
        if let ClientEvent::Disconnected { .. } = next(&mut events).await {
            break;
        }
    }

    // 3. Revocation: the same client must pair again, and denial is enforced.
    host_ctx
        .trusted_clients
        .lock()
        .expect("lock")
        .revoke(&client_ctx.identity.fingerprint())
        .expect("revoke");
    // Wait until the host has ended both previous sessions.
    let mut ended = 0;
    while ended < 2 {
        if let HostEvent::SessionEnded { .. } = next(&mut host_rx).await {
            ended += 1;
        }
    }
    let mut c = start_client(
        &client_ctx,
        addr,
        ServerPolicy::Pinned(host_ctx.identity.fingerprint()),
    );
    loop {
        match next(&mut host_rx).await {
            HostEvent::PairingRequest(req) => {
                req.respond(false);
                break;
            }
            HostEvent::SessionEnded { .. } | HostEvent::Stats(_) => {}
            other => panic!("unexpected host event: {other:?}"),
        }
    }
    let reason = wait_disconnected(&mut c).await;
    assert!(
        reason.contains(&RejectReason::PairingDenied.to_string()),
        "unexpected reason: {reason}"
    );
    assert_eq!(c.frames.load(Ordering::Relaxed), 0);

    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_fingerprint_mismatch_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host_ctx = context(&dir, "host");
    let client_ctx = context(&dir, "client");
    let impostor_ctx = context(&dir, "impostor");
    let (host, _host_rx) = start_host(&host_ctx, true).await;

    // The client expects another host: the TLS handshake must fail.
    let mut c = start_client(
        &client_ctx,
        host.local_addr(),
        ServerPolicy::Pinned(impostor_ctx.identity.fingerprint()),
    );
    loop {
        match next(&mut c.events).await {
            ClientEvent::Connecting => {}
            ClientEvent::Disconnected { error, .. } => {
                assert!(error);
                break;
            }
            other => panic!("must not connect: {other:?}"),
        }
    }
    assert!(
        !host_ctx
            .trusted_clients
            .lock()
            .expect("lock")
            .is_trusted(&client_ctx.identity.fingerprint())
    );
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_disabled_and_busy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host_ctx = context(&dir, "host");
    let a = context(&dir, "a");
    let b = context(&dir, "b");
    let stranger = context(&dir, "stranger");
    for ctx in [&a, &b] {
        host_ctx
            .trusted_clients
            .lock()
            .expect("lock")
            .trust("Test client", ctx.identity.fingerprint())
            .expect("trust");
    }
    let (host, _rx) = start_host(&host_ctx, false).await;
    let pin = ServerPolicy::Pinned(host_ctx.identity.fingerprint());

    // Unknown device while pairing is disabled.
    let mut s = start_client(&stranger, host.local_addr(), pin);
    let reason = wait_disconnected(&mut s).await;
    assert!(
        reason.contains(&RejectReason::PairingDisabled.to_string()),
        "{reason}"
    );

    // One session at a time.
    let mut first = start_client(&a, host.local_addr(), pin);
    wait_connected(&mut first).await;
    wait_frames(&first, 5).await;
    let mut second = start_client(&b, host.local_addr(), pin);
    let reason = wait_disconnected(&mut second).await;
    assert!(reason.contains(&RejectReason::Busy.to_string()), "{reason}");

    first.handle.disconnect();
    wait_disconnected(&mut first).await;
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_in_pairing_mode_always_verifies_the_host() {
    let dir = tempfile::tempdir().expect("tempdir");
    let host_ctx = context(&dir, "host");
    let client_ctx = context(&dir, "client");
    // The host already trusts the client…
    host_ctx
        .trusted_clients
        .lock()
        .expect("lock")
        .trust("Test client", client_ctx.identity.fingerprint())
        .expect("trust");
    let (host, mut host_rx) = start_host(&host_ctx, true).await;

    // …but the client does not know the host: the ceremony still happens.
    let mut c = start_client(&client_ctx, host.local_addr(), ServerPolicy::Pairing);
    let _ = answer_pairing(&mut host_rx, true).await;
    // The local user says the codes differ: no session, no frames.
    loop {
        match next(&mut c.events).await {
            ClientEvent::PairingCode(_) => c.handle.confirm_pairing(false),
            ClientEvent::Connected { .. } => panic!("must not connect without confirmation"),
            ClientEvent::Disconnected { reason, .. } => {
                assert_eq!(reason, "cancelled");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(c.frames.load(Ordering::Relaxed), 0);
    host.shutdown().await;
}

#[test]
fn rejected_error_message_is_readable() {
    let e = CoreError::Rejected(RejectReason::Busy);
    assert!(e.to_string().contains("already in a session"));
}
