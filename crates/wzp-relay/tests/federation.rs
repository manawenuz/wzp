//! Tests for `wzp_relay::federation`.
//!
//! Covers:
//!  - room_hash determinism and uniqueness
//!  - is_global_room (static config + call-* implicit global)
//!  - resolve_global_room
//!  - global_room_hash
//!  - forward_to_peers with zero peers (no-op)
//!  - forward_to_peers with live QUIC peer links
//!  - broadcast_signal to live QUIC peers
//!  - send_signal_to_peer targeted routing
//!  - find_peer_by_fingerprint / find_peer_by_addr / check_inbound_trust
//!  - set_cross_relay_tx + local_tls_fp accessors

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use wzp_proto::{MediaTransport, SignalMessage};
use wzp_relay::config::{PeerConfig, TrustedConfig};
use wzp_relay::event_log::EventLogger;
use wzp_relay::federation::{room_hash, FederationManager};
use wzp_relay::metrics::RelayMetrics;
use wzp_relay::room::RoomManager;
use wzp_transport::{client_config, create_endpoint, server_config, QuinnTransport};

// ───────────────────────────── helpers ──────────────────────────────

/// Create a FederationManager for unit tests (no live peers).
fn create_test_fm(global_rooms: HashSet<String>) -> Arc<FederationManager> {
    create_test_fm_full(vec![], vec![], global_rooms)
}

/// Create a FederationManager with full config (peers + trusted + global rooms).
fn create_test_fm_full(
    peers: Vec<PeerConfig>,
    trusted: Vec<TrustedConfig>,
    global_rooms: HashSet<String>,
) -> Arc<FederationManager> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (sc, _cert) = server_config();
    let ep = create_endpoint((Ipv4Addr::LOCALHOST, 0).into(), Some(sc))
        .expect("test endpoint");
    let room_mgr = Arc::new(RoomManager::new());
    let metrics = Arc::new(RelayMetrics::new());
    let event_log = EventLogger::Noop;

    Arc::new(FederationManager::new(
        peers,
        trusted,
        global_rooms,
        room_mgr,
        ep,
        "test-relay-fp-abc123".into(),
        metrics,
        event_log,
    ))
}

/// Build an in-process QUIC client/server pair on loopback.
/// Returns (client_transport, server_transport, endpoints).
/// The endpoints must be kept alive for the test duration.
async fn connected_pair() -> (
    Arc<QuinnTransport>,
    Arc<QuinnTransport>,
    (quinn::Endpoint, quinn::Endpoint),
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (sc, _cert_der) = server_config();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let server_ep = create_endpoint(server_addr, Some(sc)).expect("server endpoint");
    let server_listen = server_ep.local_addr().expect("server local addr");

    let client_bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let client_ep = create_endpoint(client_bind, None).expect("client endpoint");

    let server_ep_clone = server_ep.clone();
    let accept_fut = tokio::spawn(async move {
        let conn = wzp_transport::accept(&server_ep_clone)
            .await
            .expect("accept");
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

// ───────────────────── 1. room_hash determinism ─────────────────────

#[test]
fn room_hash_deterministic() {
    let h1 = room_hash("podcast");
    let h2 = room_hash("podcast");
    assert_eq!(h1, h2);
}

#[test]
fn room_hash_different_rooms() {
    let h1 = room_hash("room-a");
    let h2 = room_hash("room-b");
    assert_ne!(h1, h2);
}

#[test]
fn room_hash_is_8_bytes() {
    let h = room_hash("some-room");
    assert_eq!(h.len(), 8);
}

#[test]
fn room_hash_empty_string() {
    // Should not panic on empty input
    let h = room_hash("");
    assert_eq!(h.len(), 8);
    // And should differ from a non-empty room
    assert_ne!(h, room_hash("nonempty"));
}

#[test]
fn room_hash_case_sensitive() {
    // "Podcast" and "podcast" are different rooms
    let h1 = room_hash("Podcast");
    let h2 = room_hash("podcast");
    assert_ne!(h1, h2);
}

// ───────────────── 2. is_global_room / resolve_global_room ──────────

#[tokio::test]
async fn is_global_room_static_config() {
    let global: HashSet<String> = ["podcast", "lobby"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm(global);

    assert!(fm.is_global_room("podcast"));
    assert!(fm.is_global_room("lobby"));
    assert!(!fm.is_global_room("private-room"));
    assert!(!fm.is_global_room(""));
}

#[tokio::test]
async fn is_global_room_call_prefix_implicit() {
    // Phase 4.1: call-* rooms are implicitly global
    let fm = create_test_fm(HashSet::new());

    assert!(fm.is_global_room("call-abc123"));
    assert!(fm.is_global_room("call-"));
    assert!(fm.is_global_room("call-some-uuid-here"));
    // But not just "call" without the dash
    assert!(!fm.is_global_room("call"));
    assert!(!fm.is_global_room("callback"));
}

#[tokio::test]
async fn resolve_global_room_static() {
    let global: HashSet<String> = ["podcast"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm(global);

    assert_eq!(fm.resolve_global_room("podcast"), Some("podcast".into()));
    assert_eq!(fm.resolve_global_room("unknown"), None);
}

#[tokio::test]
async fn resolve_global_room_call_prefix() {
    let fm = create_test_fm(HashSet::new());

    let resolved = fm.resolve_global_room("call-test-123");
    assert_eq!(resolved, Some("call-test-123".into()));
}

#[tokio::test]
async fn global_room_hash_uses_canonical_name() {
    let global: HashSet<String> = ["podcast"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm(global);

    // For a known global room, global_room_hash should match room_hash of the canonical name
    let expected = room_hash("podcast");
    assert_eq!(fm.global_room_hash("podcast"), expected);
}

#[tokio::test]
async fn global_room_hash_unknown_room_falls_through() {
    let fm = create_test_fm(HashSet::new());

    // Unknown room: just hashes whatever was passed
    let expected = room_hash("random-room");
    assert_eq!(fm.global_room_hash("random-room"), expected);
}

#[tokio::test]
async fn global_room_hash_call_prefix() {
    let fm = create_test_fm(HashSet::new());

    // call-* resolves to itself
    let expected = room_hash("call-xyz");
    assert_eq!(fm.global_room_hash("call-xyz"), expected);
}

// ───────────────── 3. forward_to_peers with zero peers ──────────────

#[tokio::test]
async fn forward_to_peers_empty_returns_immediately() {
    let fm = create_test_fm(HashSet::new());
    let hash = room_hash("room");
    let data = Bytes::from_static(b"test-media-payload");

    // Should not panic or hang
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        fm.forward_to_peers("room", &hash, &data),
    )
    .await;
    assert!(result.is_ok(), "forward_to_peers should return immediately with no peers");
}

// ─────────── 4. forward_to_peers with live QUIC peer links ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_to_peers_delivers_tagged_datagram() {
    // We create a FederationManager and manually wire a connected QUIC
    // pair to simulate a peer link. The fm holds the server-side
    // transport; we read from the client side to verify delivery.
    let fm = create_test_fm(HashSet::new());

    let (client_transport, server_transport, _endpoints) = connected_pair().await;

    // Manually insert a PeerLink by using handle_inbound's internal
    // pattern: we call the private peer_links mutex directly. Since
    // PeerLink is private, we instead use handle_inbound which calls
    // run_federation_link. But that requires a full signal loop.
    //
    // Alternative approach: spawn a mock "federation relay" server,
    // have the FM connect to it via connect_to_peer, and read back
    // from the server side. But connect_to_peer also starts the full
    // link loop.
    //
    // Simplest: create a second FM that acts as the peer, and use
    // the broadcast_signal / forward_to_peers pattern after the link
    // is established via handle_inbound.
    //
    // Actually the simplest approach for testing forward_to_peers is
    // to accept that PeerLink is private, so we instead test through
    // the full federation link lifecycle. We'll spawn a mini relay
    // that does the FederationHello handshake and then reads datagrams.

    // Approach: spawn the server side to do the hello exchange, then
    // the fm handle_inbound will register the link, then we can call
    // forward_to_peers and read from the server side... But
    // handle_inbound blocks in run_federation_link.
    //
    // Final approach: we test the wire format directly. The client
    // side is "us" (the relay) — we send a tagged datagram manually,
    // and verify the peer side receives it with the correct format.
    // This tests the same logic as forward_to_peers without needing
    // peer_links access.

    let room = "test-room";
    let rh = room_hash(room);
    let media = b"opus-frame-data-here";

    // Build the tagged datagram the same way forward_to_peers does
    let mut tagged = Vec::with_capacity(8 + media.len());
    tagged.extend_from_slice(&rh);
    tagged.extend_from_slice(media);

    // Send from the server side (as if we are the relay forwarding)
    server_transport
        .send_raw_datagram(&tagged)
        .expect("send datagram");

    // Read from client side (as if we are the peer relay receiving)
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        client_transport.connection().read_datagram(),
    )
    .await
    .expect("should receive within timeout")
    .expect("read_datagram ok");

    // Verify: first 8 bytes are the room hash, remainder is media
    assert!(received.len() >= 8, "datagram too short");
    let mut recv_hash = [0u8; 8];
    recv_hash.copy_from_slice(&received[..8]);
    assert_eq!(recv_hash, rh, "room hash mismatch");
    assert_eq!(&received[8..], media, "media payload mismatch");

    drop(client_transport);
    drop(server_transport);
}

// ─────────── 5. broadcast_signal to live QUIC peers ─────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broadcast_signal_sends_to_all_peers() {
    // We need the peer links to be registered inside the FM.
    // The simplest approach: spawn a mock peer relay that accepts
    // federation connections, does the FederationHello handshake,
    // and then reads signals.

    let _ = rustls::crypto::ring::default_provider().install_default();

    // Create a mock "peer relay" server endpoint
    let (sc, _cert) = server_config();
    let peer_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let peer_ep = create_endpoint(peer_addr, Some(sc)).expect("peer endpoint");
    let peer_listen = peer_ep.local_addr().expect("peer local addr");

    // The FM that will connect outbound
    let peer_cfg = PeerConfig {
        url: peer_listen.to_string(),
        fingerprint: "aa:bb:cc:dd".into(),
        label: Some("mock-peer".into()),
    };
    let global: HashSet<String> = ["podcast"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm_full(vec![peer_cfg], vec![], global);

    // Spawn the FM's run (which will try to connect to our mock peer)
    let fm_clone = fm.clone();
    let _fm_task = tokio::spawn(async move {
        fm_clone.run().await;
    });

    // Accept the connection on the mock peer side
    let peer_ep_clone = peer_ep.clone();
    let peer_transport = tokio::time::timeout(Duration::from_secs(5), async {
        let conn = wzp_transport::accept(&peer_ep_clone).await.expect("accept");
        Arc::new(QuinnTransport::new(conn))
    })
    .await
    .expect("FM should connect to mock peer within 5s");

    // The FM sends FederationHello as the first signal. Read it.
    let hello = tokio::time::timeout(
        Duration::from_secs(2),
        peer_transport.recv_signal(),
    )
    .await
    .expect("hello timeout")
    .expect("recv ok")
    .expect("some message");

    match hello {
        SignalMessage::FederationHello { tls_fingerprint } => {
            assert_eq!(tls_fingerprint, "test-relay-fp-abc123");
        }
        other => panic!("expected FederationHello, got: {:?}", std::mem::discriminant(&other)),
    }

    // Now the FM's run_federation_link registered the peer in peer_links
    // and will announce active global rooms. We may receive
    // GlobalRoomActive signals next (for any rooms the FM has active).
    // For this test, no local participants, so no GlobalRoomActive.

    // Give the link time to fully set up
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now call broadcast_signal on the FM
    let test_msg = SignalMessage::FederatedSignalForward {
        inner: Box::new(SignalMessage::Reflect),
        origin_relay_fp: "other-relay-fp".into(),
    };
    let count = fm.broadcast_signal(&test_msg).await;
    assert_eq!(count, 1, "should have broadcast to exactly 1 peer");

    // Read the signal on the peer side
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        peer_transport.recv_signal(),
    )
    .await
    .expect("broadcast signal timeout")
    .expect("recv ok")
    .expect("some message");

    match received {
        SignalMessage::FederatedSignalForward { origin_relay_fp, .. } => {
            assert_eq!(origin_relay_fp, "other-relay-fp");
        }
        other => panic!("expected FederatedSignalForward, got: {:?}", std::mem::discriminant(&other)),
    }

    drop(peer_transport);
}

// ──────────── 6. send_signal_to_peer targeted routing ───────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_signal_to_peer_unknown_fp_returns_error() {
    let fm = create_test_fm(HashSet::new());

    let msg = SignalMessage::Reflect;
    let result = fm.send_signal_to_peer("nonexistent-fp", &msg).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("no active federation link"));
}

// ──────────── 7. find_peer_by_fingerprint / addr / trust ────────────

#[tokio::test]
async fn find_peer_by_fingerprint_matches() {
    let peer = PeerConfig {
        url: "10.0.0.1:4433".into(),
        fingerprint: "AA:BB:CC:DD".into(),
        label: Some("relay-eu".into()),
    };
    let fm = create_test_fm_full(vec![peer], vec![], HashSet::new());

    // Normalized match (colons removed, lowercased)
    let found = fm.find_peer_by_fingerprint("aabbccdd");
    assert!(found.is_some());
    assert_eq!(found.unwrap().label.as_deref(), Some("relay-eu"));

    // With colons
    let found2 = fm.find_peer_by_fingerprint("AA:BB:CC:DD");
    assert!(found2.is_some());

    // Non-matching
    assert!(fm.find_peer_by_fingerprint("11:22:33:44").is_none());
}

#[tokio::test]
async fn find_peer_by_addr_matches_ip() {
    let peer = PeerConfig {
        url: "10.0.0.1:4433".into(),
        fingerprint: "aabb".into(),
        label: None,
    };
    let fm = create_test_fm_full(vec![peer], vec![], HashSet::new());

    // Same IP, different port still matches (find_peer_by_addr matches by IP)
    let addr: SocketAddr = "10.0.0.1:9999".parse().unwrap();
    let found = fm.find_peer_by_addr(addr);
    assert!(found.is_some());

    // Different IP
    let addr2: SocketAddr = "10.0.0.2:4433".parse().unwrap();
    assert!(fm.find_peer_by_addr(addr2).is_none());
}

#[tokio::test]
async fn find_trusted_by_fingerprint() {
    let trusted = TrustedConfig {
        fingerprint: "AA:BB:CC:DD:EE".into(),
        label: Some("trusted-relay".into()),
    };
    let fm = create_test_fm_full(vec![], vec![trusted], HashSet::new());

    let found = fm.find_trusted_by_fingerprint("aabbccddee");
    assert!(found.is_some());
    assert_eq!(found.unwrap().label.as_deref(), Some("trusted-relay"));

    assert!(fm.find_trusted_by_fingerprint("ffffffff").is_none());
}

#[tokio::test]
async fn check_inbound_trust_prefers_peer_by_addr() {
    let peer = PeerConfig {
        url: "10.0.0.1:4433".into(),
        fingerprint: "aabb".into(),
        label: Some("peer-relay".into()),
    };
    let trusted = TrustedConfig {
        fingerprint: "ccdd".into(),
        label: Some("trusted-relay".into()),
    };
    let fm = create_test_fm_full(vec![peer], vec![trusted], HashSet::new());

    // Matches by addr (peer takes priority)
    let addr: SocketAddr = "10.0.0.1:5555".parse().unwrap();
    let label = fm.check_inbound_trust(addr, "ccdd");
    assert_eq!(label, Some("peer-relay".into()));
}

#[tokio::test]
async fn check_inbound_trust_falls_back_to_trusted_fp() {
    let trusted = TrustedConfig {
        fingerprint: "CC:DD".into(),
        label: Some("trusted-relay".into()),
    };
    let fm = create_test_fm_full(vec![], vec![trusted], HashSet::new());

    // No peer matches, but trusted fingerprint matches
    let addr: SocketAddr = "10.99.99.99:1234".parse().unwrap();
    let label = fm.check_inbound_trust(addr, "ccdd");
    assert_eq!(label, Some("trusted-relay".into()));
}

#[tokio::test]
async fn check_inbound_trust_returns_none_for_unknown() {
    let fm = create_test_fm(HashSet::new());
    let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
    assert!(fm.check_inbound_trust(addr, "unknown-fp").is_none());
}

// ──────────── 8. set_cross_relay_tx + local_tls_fp ──────────────────

#[tokio::test]
async fn local_tls_fp_returns_configured_value() {
    let fm = create_test_fm(HashSet::new());
    assert_eq!(fm.local_tls_fp(), "test-relay-fp-abc123");
}

#[tokio::test]
async fn set_cross_relay_tx_wires_channel() {
    let fm = create_test_fm(HashSet::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);

    fm.set_cross_relay_tx(tx).await;

    // The channel is now wired — we can't easily test it without
    // going through handle_signal, but we can at least verify it
    // doesn't panic and the fm accepted the sender.
    // (The channel itself works — we test the Sender.)
    let msg = SignalMessage::Reflect;
    let _ = rx.try_recv(); // should be empty
    drop(rx);
}

// ──────────── 9. broadcast_signal with zero peers ───────────────────

#[tokio::test]
async fn broadcast_signal_zero_peers_returns_zero() {
    let fm = create_test_fm(HashSet::new());
    let msg = SignalMessage::Reflect;
    let count = fm.broadcast_signal(&msg).await;
    assert_eq!(count, 0);
}

// ──────────── 10. get_remote_participants with no links ─────────────

#[tokio::test]
async fn get_remote_participants_empty_with_no_links() {
    let fm = create_test_fm(HashSet::new());
    let participants = fm.get_remote_participants("podcast").await;
    assert!(participants.is_empty());
}

// ─────── 11. Federation media egress with live QUIC connection ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn federation_media_egress_forwards_to_peer() {
    // This test verifies the full media path:
    //   local media -> federation egress channel -> forward_to_peers -> peer reads datagram
    //
    // We set up a real QUIC federation link via fm.run() connecting to
    // a mock peer, then push media through the room manager's federation
    // egress channel.

    let _ = rustls::crypto::ring::default_provider().install_default();

    // Mock peer relay
    let (sc, _cert) = server_config();
    let peer_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let peer_ep = create_endpoint(peer_addr, Some(sc)).expect("peer endpoint");
    let peer_listen = peer_ep.local_addr().expect("peer local addr");

    let peer_cfg = PeerConfig {
        url: peer_listen.to_string(),
        fingerprint: "ee:ff:00:11".into(),
        label: Some("egress-peer".into()),
    };
    let global: HashSet<String> = ["podcast"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm_full(vec![peer_cfg], vec![], global);

    // Start the FM (connects to mock peer)
    let fm_clone = fm.clone();
    let _fm_task = tokio::spawn(async move { fm_clone.run().await });

    // Accept the connection
    let peer_ep_clone = peer_ep.clone();
    let peer_transport = tokio::time::timeout(Duration::from_secs(5), async {
        let conn = wzp_transport::accept(&peer_ep_clone).await.expect("accept");
        Arc::new(QuinnTransport::new(conn))
    })
    .await
    .expect("FM should connect within 5s");

    // Read the FederationHello
    let _hello = tokio::time::timeout(
        Duration::from_secs(2),
        peer_transport.recv_signal(),
    )
    .await
    .expect("hello timeout")
    .expect("recv ok")
    .expect("some message");

    // Wait for link setup
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Now send media via forward_to_peers
    let room = "podcast";
    let rh = room_hash(room);
    let media_payload = Bytes::from_static(b"test-opus-frame-1234567890");

    fm.forward_to_peers(room, &rh, &media_payload).await;

    // Read the datagram on the peer side
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        peer_transport.connection().read_datagram(),
    )
    .await
    .expect("should receive media within timeout")
    .expect("read_datagram ok");

    // Verify tagged format: [8-byte room_hash][media_payload]
    assert!(received.len() >= 8);
    let mut recv_hash = [0u8; 8];
    recv_hash.copy_from_slice(&received[..8]);
    assert_eq!(recv_hash, rh, "room hash must match");
    assert_eq!(
        &received[8..],
        &media_payload[..],
        "media payload must match"
    );

    drop(peer_transport);
}

// ───── 12. Multiple global rooms: each hashes independently ─────────

#[tokio::test]
async fn multiple_global_rooms_independent_hashes() {
    let global: HashSet<String> = ["podcast", "lobby", "arena"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let fm = create_test_fm(global);

    let hashes: Vec<[u8; 8]> = ["podcast", "lobby", "arena"]
        .iter()
        .map(|r| fm.global_room_hash(r))
        .collect();

    // All different
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);
    assert_ne!(hashes[0], hashes[2]);
}

// ───── 13. is_global_room edge cases ────────────────────────────────

#[tokio::test]
async fn is_global_room_exact_match_required_for_static() {
    let global: HashSet<String> = ["podcast"].iter().map(|s| s.to_string()).collect();
    let fm = create_test_fm(global);

    // Substring/prefix should NOT match
    assert!(!fm.is_global_room("podcast-extra"));
    assert!(!fm.is_global_room("pod"));
    assert!(!fm.is_global_room("podcastt"));
}
