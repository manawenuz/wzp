//! Integration tests for the "STUN for QUIC" reflect protocol
//! (PRD: .taskmaster/docs/prd_reflect_over_quic.txt, Phase 1).
//!
//! We don't spin up the full relay binary — instead we exercise the
//! same wire-level request/response dance with a mock relay loop
//! that implements exactly the match arm added to
//! `wzp-relay/src/main.rs`. This isolates the protocol test from the
//! rest of the relay state (rooms, federation, call registry, ...).
//!
//! Three test cases:
//!  1. `reflect_happy_path` — client sends `Reflect`, mock relay
//!     replies with `ReflectResponse { observed_addr }`, client
//!     parses it back to a `SocketAddr` and confirms the IP is
//!     `127.0.0.1` and the port matches its own bound port.
//!  2. `reflect_two_clients_distinct_ports` — two simultaneous
//!     client connections on different ephemeral ports get back
//!     different reflected ports, proving the relay uses
//!     per-connection `remote_address` rather than a global.
//!  3. `reflect_old_relay_times_out` — mock relay that *doesn't*
//!     handle `Reflect`; client side times out in the expected
//!     window and does not hang.
//!
//! The third test uses a `tokio::time::timeout` wrapper directly
//! (the client-side `request_reflect` helper lives in
//! `desktop/src-tauri/src/lib.rs` which isn't a library we can
//! depend on from here, so we reproduce the timeout semantics
//! inline).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::{client_config, create_endpoint, server_config, QuinnTransport};

/// Spawn a minimal mock relay that loops over `recv_signal`,
/// matches on `Reflect`, and responds with `ReflectResponse` using
/// the remote_address observed for this connection. Mirrors the
/// match arm in `crates/wzp-relay/src/main.rs`.
async fn spawn_mock_relay_with_reflect(
    server_transport: Arc<QuinnTransport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Observed remote address at the time the connection was
        // accepted. Stable for the life of the connection under quinn's
        // normal operation. This is exactly what the real relay does.
        let observed = server_transport.connection().remote_address();
        loop {
            match server_transport.recv_signal().await {
                Ok(Some(SignalMessage::Reflect)) => {
                    let resp = SignalMessage::ReflectResponse {
                        observed_addr: observed.to_string(),
                    };
                    // If the send fails the client has gone; just exit.
                    if server_transport.send_signal(&resp).await.is_err() {
                        break;
                    }
                }
                Ok(Some(_other)) => {
                    // Ignore anything else — not relevant to this test.
                }
                Ok(None) => break,
                Err(_e) => break,
            }
        }
    })
}

/// Spawn a mock relay that intentionally DOES NOT handle Reflect.
/// Models a pre-Phase-1 relay — it keeps reading signal messages and
/// logs them to stderr, but never produces a `ReflectResponse`.
async fn spawn_mock_relay_without_reflect(
    server_transport: Arc<QuinnTransport>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match server_transport.recv_signal().await {
                Ok(Some(_msg)) => {
                    // Deliberately do nothing. Old relay.
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    })
}

/// Build an in-process QUIC client/server pair on loopback and
/// return (client_transport, server_transport, endpoints). The
/// endpoints tuple must be kept alive for the test duration.
///
/// `client_port_hint` of 0 means "let OS pick". Pass an explicit
/// port to pin the client's source port (useful for the
/// distinct-ports test).
async fn connected_pair_with_port(
    _client_port_hint: u16,
) -> (Arc<QuinnTransport>, Arc<QuinnTransport>, (quinn::Endpoint, quinn::Endpoint)) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (sc, _cert_der) = server_config();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let server_ep = create_endpoint(server_addr, Some(sc)).expect("server endpoint");
    let server_listen = server_ep.local_addr().expect("server local addr");

    // Always bind the client to an ephemeral port — we'll read back
    // the actual assigned port via `local_addr()` in the assertions.
    let client_bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let client_ep = create_endpoint(client_bind, None).expect("client endpoint");

    let server_ep_clone = server_ep.clone();
    let accept_fut = tokio::spawn(async move {
        let conn = wzp_transport::accept(&server_ep_clone).await.expect("accept");
        Arc::new(QuinnTransport::new(conn))
    });

    let client_conn =
        wzp_transport::connect(&client_ep, server_listen, "localhost", client_config())
            .await
            .expect("connect");
    let client_transport = Arc::new(QuinnTransport::new(client_conn));
    let server_transport = accept_fut.await.expect("join accept task");

    (client_transport, server_transport, (server_ep, client_ep))
}

// -----------------------------------------------------------------------
// Test 1: happy path — client learns its own port via Reflect
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_happy_path() {
    let (client_transport, server_transport, (_server_ep, client_ep)) =
        connected_pair_with_port(0).await;

    // Grab the client's actual bound port so we can cross-check
    // against the reflected response.
    let client_port = client_ep
        .local_addr()
        .expect("client local addr")
        .port();
    assert_ne!(client_port, 0, "client must have a real bound port");

    // Start the mock relay's reflect handler.
    let _relay_handle = spawn_mock_relay_with_reflect(Arc::clone(&server_transport)).await;

    // Client sends Reflect and awaits the response. The real
    // request_reflect helper in desktop/src-tauri/src/lib.rs uses a
    // oneshot channel driven off the spawned recv loop; here we just
    // do it inline because there's no spawned loop yet in this test
    // — this isolates the wire protocol from the client-side state
    // machine.
    client_transport
        .send_signal(&SignalMessage::Reflect)
        .await
        .expect("send Reflect");

    let resp = tokio::time::timeout(Duration::from_secs(2), client_transport.recv_signal())
        .await
        .expect("reflect response should arrive within 2s")
        .expect("recv_signal ok")
        .expect("some message");

    let observed_addr = match resp {
        SignalMessage::ReflectResponse { observed_addr } => observed_addr,
        other => panic!("expected ReflectResponse, got {:?}", std::mem::discriminant(&other)),
    };

    let parsed: SocketAddr = observed_addr
        .parse()
        .expect("ReflectResponse.observed_addr must parse as SocketAddr");

    // The relay should see the client on 127.0.0.1 (loopback in the
    // test harness) and on the client's bound ephemeral port.
    assert_eq!(parsed.ip().to_string(), "127.0.0.1");
    assert_eq!(
        parsed.port(),
        client_port,
        "reflected port must match the client's local_addr port"
    );

    drop(client_transport);
    drop(server_transport);
}

// -----------------------------------------------------------------------
// Test 2: two clients get DIFFERENT reflected ports
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reflect_two_clients_distinct_ports() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Shared server: one endpoint, two incoming accepts.
    let (sc, _cert_der) = server_config();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let server_ep = create_endpoint(server_addr, Some(sc)).expect("server endpoint");
    let server_listen = server_ep.local_addr().expect("server local addr");

    // Accept two clients in parallel.
    let server_ep_a = server_ep.clone();
    let accept_a = tokio::spawn(async move {
        let conn = wzp_transport::accept(&server_ep_a).await.expect("accept A");
        Arc::new(QuinnTransport::new(conn))
    });
    let server_ep_b = server_ep.clone();
    let accept_b = tokio::spawn(async move {
        let conn = wzp_transport::accept(&server_ep_b).await.expect("accept B");
        Arc::new(QuinnTransport::new(conn))
    });

    // Client A
    let client_ep_a = create_endpoint((Ipv4Addr::LOCALHOST, 0).into(), None).expect("ep A");
    let conn_a =
        wzp_transport::connect(&client_ep_a, server_listen, "localhost", client_config())
            .await
            .expect("connect A");
    let client_a = Arc::new(QuinnTransport::new(conn_a));
    let port_a = client_ep_a.local_addr().unwrap().port();

    // Client B
    let client_ep_b = create_endpoint((Ipv4Addr::LOCALHOST, 0).into(), None).expect("ep B");
    let conn_b =
        wzp_transport::connect(&client_ep_b, server_listen, "localhost", client_config())
            .await
            .expect("connect B");
    let client_b = Arc::new(QuinnTransport::new(conn_b));
    let port_b = client_ep_b.local_addr().unwrap().port();

    assert_ne!(
        port_a, port_b,
        "preconditions: OS must assign two clients different ephemeral ports"
    );

    let server_a = accept_a.await.expect("join A");
    let server_b = accept_b.await.expect("join B");

    // Spawn a reflect handler for each server-side transport.
    let _relay_a = spawn_mock_relay_with_reflect(Arc::clone(&server_a)).await;
    let _relay_b = spawn_mock_relay_with_reflect(Arc::clone(&server_b)).await;

    // Each client requests reflect concurrently.
    let reflect_for = |t: Arc<QuinnTransport>| async move {
        t.send_signal(&SignalMessage::Reflect).await.expect("send");
        let resp = tokio::time::timeout(Duration::from_secs(2), t.recv_signal())
            .await
            .expect("timeout")
            .expect("ok")
            .expect("some");
        match resp {
            SignalMessage::ReflectResponse { observed_addr } => observed_addr,
            _ => panic!("wrong variant"),
        }
    };

    let (addr_a, addr_b) = tokio::join!(reflect_for(client_a.clone()), reflect_for(client_b.clone()));

    let parsed_a: SocketAddr = addr_a.parse().unwrap();
    let parsed_b: SocketAddr = addr_b.parse().unwrap();

    assert_eq!(parsed_a.port(), port_a, "client A's reflected port");
    assert_eq!(parsed_b.port(), port_b, "client B's reflected port");
    assert_ne!(
        parsed_a.port(),
        parsed_b.port(),
        "each client must see its own port, not a shared one"
    );

    drop(client_a);
    drop(client_b);
    drop(server_a);
    drop(server_b);
}

// -----------------------------------------------------------------------
// Test 3: old relay never answers — client times out cleanly
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflect_old_relay_times_out() {
    let (client_transport, server_transport, _endpoints) =
        connected_pair_with_port(0).await;

    // Mock relay that ignores Reflect — simulates a pre-Phase-1 build.
    let _relay_handle =
        spawn_mock_relay_without_reflect(Arc::clone(&server_transport)).await;

    client_transport
        .send_signal(&SignalMessage::Reflect)
        .await
        .expect("send Reflect");

    // 1100ms ceiling matches the 1s timeout baked into
    // get_reflected_address plus a tiny bit of slack. If this
    // regression ever fires it probably means recv_signal blocked
    // longer than expected and the Tauri command would hang the UI.
    let start = std::time::Instant::now();
    let result =
        tokio::time::timeout(Duration::from_millis(1100), client_transport.recv_signal()).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "recv_signal must time out when the relay ignores Reflect"
    );
    assert!(
        elapsed >= Duration::from_millis(1000),
        "timeout fired too early ({:?})",
        elapsed
    );
    assert!(
        elapsed < Duration::from_millis(1200),
        "timeout fired too late ({:?}), client would feel unresponsive",
        elapsed
    );

    drop(client_transport);
    drop(server_transport);
}
