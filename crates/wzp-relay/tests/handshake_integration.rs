//! WZP-S-5 integration tests: crypto handshake wired into live QUIC path.
//!
//! Verifies that `perform_handshake` (client/caller) and `accept_handshake`
//! (relay/callee) complete successfully over a real in-process QUIC connection
//! and produce usable `CryptoSession` values.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use wzp_client::perform_handshake;
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};
use wzp_proto::{MediaTransport, SignalMessage};
use wzp_relay::handshake::accept_handshake;
use wzp_transport::{client_config, create_endpoint, server_config, QuinnTransport};

/// Establish a QUIC connection and wrap both sides in `QuinnTransport`.
///
/// Returns (client_transport, server_transport, _endpoints) where the endpoint
/// tuple must be kept alive for the duration of the test to avoid premature
/// connection teardown.
async fn connected_pair() -> (Arc<QuinnTransport>, Arc<QuinnTransport>, (quinn::Endpoint, quinn::Endpoint)) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (sc, _cert_der) = server_config();
    let server_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let server_ep = create_endpoint(server_addr, Some(sc)).expect("server endpoint");
    let server_listen = server_ep.local_addr().expect("server local addr");

    let client_addr: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let client_ep = create_endpoint(client_addr, None).expect("client endpoint");

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
// Test 1: handshake_succeeds
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_succeeds() {
    let (client_transport, server_transport, _endpoints) = connected_pair().await;

    let caller_seed: [u8; 32] = [0xAA; 32];
    let callee_seed: [u8; 32] = [0xBB; 32];

    // Clone Arc so the server transport stays alive in the main task too.
    let server_t = Arc::clone(&server_transport);
    let callee_handle = tokio::spawn(async move {
        accept_handshake(server_t.as_ref(), &callee_seed).await
    });

    let caller_session = perform_handshake(client_transport.as_ref(), &caller_seed)
        .await
        .expect("perform_handshake should succeed");

    let (callee_session, chosen_profile) = callee_handle
        .await
        .expect("join callee task")
        .expect("accept_handshake should succeed");

    // Both sides should have derived a working CryptoSession.
    // Verify by encrypting on one side and decrypting on the other.
    let header = b"test-header";
    let plaintext = b"hello warzone";

    let mut ciphertext = Vec::new();
    let mut caller_session = caller_session;
    let mut callee_session = callee_session;

    caller_session
        .encrypt(header, plaintext, &mut ciphertext)
        .expect("encrypt");

    let mut decrypted = Vec::new();
    callee_session
        .decrypt(header, &ciphertext, &mut decrypted)
        .expect("decrypt");

    assert_eq!(&decrypted, plaintext);
    assert_eq!(chosen_profile, wzp_proto::QualityProfile::GOOD);

    // Keep transports alive until test completes.
    drop(server_transport);
    drop(client_transport);
}

// -----------------------------------------------------------------------
// Test 2: handshake_verifies_identity
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_verifies_identity() {
    let (client_transport, server_transport, _endpoints) = connected_pair().await;

    // Two completely different seeds => different identity keys.
    let caller_seed: [u8; 32] = [0x11; 32];
    let callee_seed: [u8; 32] = [0x22; 32];

    // Confirm the seeds produce different identity public keys.
    let caller_kx = WarzoneKeyExchange::from_identity_seed(&caller_seed);
    let callee_kx = WarzoneKeyExchange::from_identity_seed(&callee_seed);
    assert_ne!(
        caller_kx.identity_public_key(),
        callee_kx.identity_public_key(),
        "different seeds must produce different identity keys"
    );

    let server_t = Arc::clone(&server_transport);
    let callee_handle = tokio::spawn(async move {
        accept_handshake(server_t.as_ref(), &callee_seed).await
    });

    let caller_session = perform_handshake(client_transport.as_ref(), &caller_seed)
        .await
        .expect("handshake must succeed even with different identities");

    let (callee_session, _profile) = callee_handle
        .await
        .expect("join")
        .expect("accept_handshake must succeed");

    // Cross-encrypt/decrypt to prove the shared session works.
    let header = b"id-test";
    let plaintext = b"identity verified";

    let mut ct = Vec::new();
    let mut caller_session = caller_session;
    let mut callee_session = callee_session;

    caller_session
        .encrypt(header, plaintext, &mut ct)
        .expect("encrypt");

    let mut pt = Vec::new();
    callee_session
        .decrypt(header, &ct, &mut pt)
        .expect("decrypt");

    assert_eq!(&pt, plaintext);

    drop(server_transport);
    drop(client_transport);
}

// -----------------------------------------------------------------------
// Test 3: auth_then_handshake
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_then_handshake() {
    let (client_transport, server_transport, _endpoints) = connected_pair().await;

    let caller_seed: [u8; 32] = [0xCC; 32];
    let callee_seed: [u8; 32] = [0xDD; 32];

    // The callee side: first consume the AuthToken, then run accept_handshake.
    let server_t = Arc::clone(&server_transport);
    let callee_handle = tokio::spawn(async move {
        // 1. Receive AuthToken
        let auth_msg = server_t
            .recv_signal()
            .await
            .expect("recv_signal should succeed")
            .expect("should receive a message");

        let token = match auth_msg {
            SignalMessage::AuthToken { token } => token,
            other => panic!("expected AuthToken, got {:?}", std::mem::discriminant(&other)),
        };

        // 2. Run the cryptographic handshake
        let (session, profile) = accept_handshake(server_t.as_ref(), &callee_seed)
            .await
            .expect("accept_handshake after auth");

        (token, session, profile)
    });

    // Caller side: send AuthToken first, then perform_handshake.
    let auth = SignalMessage::AuthToken {
        token: "bearer-test-token-12345".to_string(),
    };
    client_transport
        .send_signal(&auth)
        .await
        .expect("send AuthToken");

    let caller_session = perform_handshake(client_transport.as_ref(), &caller_seed)
        .await
        .expect("perform_handshake after auth");

    let (received_token, callee_session, _profile) = callee_handle
        .await
        .expect("join callee task");

    // Verify the auth token was received correctly.
    assert_eq!(received_token, "bearer-test-token-12345");

    // Verify the crypto session works after the auth preamble.
    let header = b"auth-hdr";
    let plaintext = b"post-auth payload";

    let mut ct = Vec::new();
    let mut caller_session = caller_session;
    let mut callee_session = callee_session;

    caller_session
        .encrypt(header, plaintext, &mut ct)
        .expect("encrypt");

    let mut pt = Vec::new();
    callee_session
        .decrypt(header, &ct, &mut pt)
        .expect("decrypt");

    assert_eq!(&pt, plaintext);

    drop(server_transport);
    drop(client_transport);
}

// -----------------------------------------------------------------------
// Test 4: handshake_rejects_bad_signature
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_rejects_bad_signature() {
    let (client_transport, server_transport, _endpoints) = connected_pair().await;

    let caller_seed: [u8; 32] = [0xEE; 32];
    let callee_seed: [u8; 32] = [0xFF; 32];

    // Spawn callee -- it should reject the tampered CallOffer.
    let server_t = Arc::clone(&server_transport);
    let callee_handle = tokio::spawn(async move {
        accept_handshake(server_t.as_ref(), &callee_seed).await
    });

    // Manually build a CallOffer with a corrupted signature.
    let mut kx = WarzoneKeyExchange::from_identity_seed(&caller_seed);
    let identity_pub = kx.identity_public_key();
    let ephemeral_pub = kx.generate_ephemeral();

    let mut sign_data = Vec::with_capacity(32 + 10);
    sign_data.extend_from_slice(&ephemeral_pub);
    sign_data.extend_from_slice(b"call-offer");
    let mut signature = kx.sign(&sign_data);

    // Tamper: flip bits in the signature.
    for byte in signature.iter_mut().take(8) {
        *byte ^= 0xFF;
    }

    let bad_offer = SignalMessage::CallOffer {
        identity_pub,
        ephemeral_pub,
        signature,
        supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
    };

    client_transport
        .send_signal(&bad_offer)
        .await
        .expect("send tampered CallOffer");

    // The callee should return an error about signature verification.
    let result = callee_handle.await.expect("join callee task");
    match result {
        Ok(_) => panic!("accept_handshake must reject a bad signature"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("signature verification failed"),
                "error should mention signature verification, got: {err_msg}"
            );
        }
    }

    drop(server_transport);
    drop(client_transport);
}
