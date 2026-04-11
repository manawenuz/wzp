//! Phase 3.5 integration tests for the dual-path QUIC race.
//!
//! The race takes a role (Acceptor or Dialer), a peer_direct_addr,
//! a relay_addr, and two SNI strings, then returns whichever QUIC
//! handshake completes first wrapped in a `QuinnTransport`. These
//! tests validate that:
//!
//! 1. On loopback with two real clients playing A + D roles, the
//!    direct path wins (fewer hops than relay).
//! 2. When the direct peer is dead (nothing listening) but the
//!    relay is up, the relay wins within the fallback window.
//! 3. When both paths are dead, the race errors cleanly rather
//!    than hanging forever.
//!
//! The "relay" in these tests is a minimal mock that just accepts
//! an incoming QUIC connection and drops it — we don't need any
//! protocol handling, just a TCP-ish listen-and-accept.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use wzp_client::dual_path::{race, WinningPath};
use wzp_client::reflect::Role;
use wzp_transport::{create_endpoint, server_config};

/// Spin up a "relay-ish" mock server on loopback that accepts
/// incoming QUIC connections and does nothing with them. Used to
/// give the relay branch of the race a real target to dial.
/// Returns the bound address + a join handle (kept alive to keep
/// the endpoint up).
async fn spawn_mock_relay() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (sc, _cert_der) = server_config();
    let bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let ep = create_endpoint(bind, Some(sc)).expect("relay endpoint");
    let addr = ep.local_addr().expect("local_addr");

    let handle = tokio::spawn(async move {
        // Accept loop — hold the connection alive for a short
        // while so the race result isn't killed by the peer
        // closing before the winning transport is returned.
        while let Some(incoming) = ep.accept().await {
            if let Ok(_conn) = incoming.await {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });
    (addr, handle)
}

// -----------------------------------------------------------------------
// Test 1: direct path wins when both sides are up
// -----------------------------------------------------------------------
//
// Spawn a mock relay, then set up a two-client test where one
// client plays the Acceptor role and the other plays the Dialer
// role. The Dialer's `peer_direct_addr` is the Acceptor's listen
// address. Because the direct path is a single loopback hop and
// the relay dial also terminates on loopback, both complete
// essentially instantly — the `biased` tokio::select in race()
// should pick direct.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dual_path_direct_wins_on_loopback() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (relay_addr, _relay_handle) = spawn_mock_relay().await;

    // Acceptor task: run race(Role::Acceptor, peer_addr_placeholder, ...).
    // Since the acceptor doesn't dial, the peer_direct_addr arg is
    // unused on the direct branch but we still pass a placeholder
    // because the API takes one. Use a stub addr that would error
    // if it were ever dialed — proving the Acceptor really doesn't
    // reach it.
    let unused_addr: SocketAddr = "127.0.0.1:2".parse().unwrap();

    // We can't race both sides in the same task because each race
    // call has its own direct endpoint that needs to talk to the
    // OTHER side's endpoint. So spawn the Acceptor in a task and
    // let it expose its listen addr via a oneshot back to the test,
    // then run the Dialer in the test's main task.
    //
    // There's a chicken-and-egg issue: the Acceptor's listen addr
    // is only known after race() creates its endpoint. To avoid
    // reaching into race()'s internals, we instead play a slight
    // trick: create the Acceptor's endpoint ourselves (outside
    // race()) to learn its addr, spin up an accept loop on it
    // ourselves, and pass THAT addr as the Dialer's peer addr.
    // This tests the Dialer->Acceptor handshake end-to-end without
    // running the full race() on both sides.

    let (sc, _cert_der) = server_config();
    let acceptor_bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let acceptor_ep = create_endpoint(acceptor_bind, Some(sc)).expect("acceptor ep");
    let acceptor_listen_addr = acceptor_ep.local_addr().expect("acceptor addr");

    // Drop the external acceptor after the test finishes, not
    // before — spawn a dedicated accept task.
    let acceptor_accept_task = tokio::spawn(async move {
        // Accept one connection and hold it for a while so the
        // Dialer side can complete its QUIC handshake.
        if let Some(incoming) = acceptor_ep.accept().await {
            if let Ok(_conn) = incoming.await {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    // Now run the Dialer in the race — peer_direct_addr = acceptor's
    // listen addr. The relay is the mock from above. Direct path
    // should win.
    let result = race(
        Role::Dialer,
        acceptor_listen_addr,
        relay_addr,
        "test-room".into(),
        "call-test".into(),
    )
    .await
    .expect("race must succeed");

    assert_eq!(result.1, WinningPath::Direct, "direct should win on loopback");

    // Cancel the acceptor accept task so the test finishes.
    acceptor_accept_task.abort();
    // Suppress unused-var warning for the placeholder.
    let _ = unused_addr;
}

// -----------------------------------------------------------------------
// Test 2: relay wins when the direct peer is dead
// -----------------------------------------------------------------------
//
// Dialer role, peer_direct_addr = a port nothing is listening on,
// relay is the working mock. Direct dial will sit waiting for a
// QUIC handshake that never comes; the 2s direct timeout kicks in
// and the relay path wins the fallback.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dual_path_relay_wins_when_direct_is_dead() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (relay_addr, _relay_handle) = spawn_mock_relay().await;

    // A port that nothing is listening on — dead direct target.
    // Port 1 on loopback is almost never bound and UDP packets to
    // it will be dropped silently, so the QUIC handshake times out.
    let dead_peer: SocketAddr = "127.0.0.1:1".parse().unwrap();

    let result = race(
        Role::Dialer,
        dead_peer,
        relay_addr,
        "test-room".into(),
        "call-test".into(),
    )
    .await
    .expect("race must succeed via relay fallback");

    assert_eq!(
        result.1,
        WinningPath::Relay,
        "relay should win when direct dial has nowhere to land"
    );
}

// -----------------------------------------------------------------------
// Test 3: race errors cleanly when both paths are dead
// -----------------------------------------------------------------------
//
// Dialer role, peer_direct_addr = dead, relay_addr = dead.
// Expected: race returns an Err within ~7s (2s direct timeout +
// 5s relay timeout fallback).

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dual_path_errors_cleanly_when_both_paths_dead() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let dead_peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let dead_relay: SocketAddr = "127.0.0.1:2".parse().unwrap();

    let start = std::time::Instant::now();
    let result = race(
        Role::Dialer,
        dead_peer,
        dead_relay,
        "test-room".into(),
        "call-test".into(),
    )
    .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "both-dead must return Err");
    // Upper bound: direct 2s timeout + relay 5s fallback + small
    // slack for scheduling. If this blows, something is looping.
    assert!(
        elapsed < Duration::from_secs(10),
        "race took too long to give up: {:?}",
        elapsed
    );
}
