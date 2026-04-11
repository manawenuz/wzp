//! Integration test: full client-relay handshake with mock transport.
//!
//! Verifies that both sides derive the same session key by encrypting
//! a message on one side and decrypting it on the other.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

use wzp_proto::packet::MediaPacket;
use wzp_proto::traits::{MediaTransport, PathQuality};
use wzp_proto::{SignalMessage, TransportError};

/// A mock transport backed by two mpsc channels (one per direction).
///
/// `signal_tx` sends signals *to* the peer.
/// `signal_rx` receives signals *from* the peer.
struct MockTransport {
    signal_tx: mpsc::Sender<SignalMessage>,
    signal_rx: Mutex<mpsc::Receiver<SignalMessage>>,
}

impl MockTransport {
    fn pair() -> (Arc<Self>, Arc<Self>) {
        let (tx_a, rx_a) = mpsc::channel(16);
        let (tx_b, rx_b) = mpsc::channel(16);

        let a = Arc::new(Self {
            signal_tx: tx_b, // A sends to B's rx
            signal_rx: Mutex::new(rx_a),
        });
        let b = Arc::new(Self {
            signal_tx: tx_a, // B sends to A's rx
            signal_rx: Mutex::new(rx_b),
        });
        (a, b)
    }
}

#[async_trait]
impl MediaTransport for MockTransport {
    async fn send_media(&self, _packet: &MediaPacket) -> Result<(), TransportError> {
        Ok(())
    }

    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError> {
        Ok(None)
    }

    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError> {
        self.signal_tx
            .send(msg.clone())
            .await
            .map_err(|e| TransportError::Internal(format!("send failed: {e}")))?;
        Ok(())
    }

    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError> {
        let mut rx = self.signal_rx.lock().await;
        Ok(rx.recv().await)
    }

    fn path_quality(&self) -> PathQuality {
        PathQuality::default()
    }

    async fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}

#[tokio::test]
async fn full_handshake_both_sides_derive_same_session() {
    let (client_transport, relay_transport) = MockTransport::pair();

    let client_seed = [0xAA_u8; 32];
    let relay_seed = [0xBB_u8; 32];

    let client_transport_clone = Arc::clone(&client_transport);
    let relay_transport_clone = Arc::clone(&relay_transport);

    // Run client and relay handshakes concurrently.
    let (client_result, relay_result) = tokio::join!(
        wzp_client::handshake::perform_handshake(client_transport_clone.as_ref(), &client_seed, None),
        wzp_relay::handshake::accept_handshake(relay_transport_clone.as_ref(), &relay_seed),
    );

    let mut client_session = client_result.expect("client handshake should succeed");
    let (mut relay_session, chosen_profile, _caller_fp, _caller_alias) =
        relay_result.expect("relay handshake should succeed");

    // Verify a profile was chosen.
    assert_eq!(chosen_profile, wzp_proto::QualityProfile::GOOD);

    // Verify both sides can communicate: client encrypts, relay decrypts.
    let header = b"test-header";
    let plaintext = b"hello from client to relay";

    let mut ciphertext = Vec::new();
    client_session
        .encrypt(header, plaintext, &mut ciphertext)
        .expect("client encrypt should succeed");

    let mut decrypted = Vec::new();
    relay_session
        .decrypt(header, &ciphertext, &mut decrypted)
        .expect("relay decrypt should succeed");

    assert_eq!(&decrypted[..], plaintext);

    // Verify reverse direction: relay encrypts, client decrypts.
    let plaintext2 = b"hello from relay to client";
    let mut ciphertext2 = Vec::new();
    relay_session
        .encrypt(header, plaintext2, &mut ciphertext2)
        .expect("relay encrypt should succeed");

    let mut decrypted2 = Vec::new();
    client_session
        .decrypt(header, &ciphertext2, &mut decrypted2)
        .expect("client decrypt should succeed");

    assert_eq!(&decrypted2[..], plaintext2);
}

#[tokio::test]
async fn handshake_rejects_tampered_signature() {
    let (client_transport, relay_transport) = MockTransport::pair();

    let _client_seed = [0xCC_u8; 32];
    let relay_seed = [0xDD_u8; 32];

    // We'll manually tamper: run the relay side with a modified caller signature.
    // Create a custom client that sends a bad signature.
    let client_transport_clone = Arc::clone(&client_transport);

    let bad_client = tokio::spawn(async move {
        use wzp_crypto::{KeyExchange, WarzoneKeyExchange};

        let mut kx = WarzoneKeyExchange::from_identity_seed(&[0xCC_u8; 32]);
        let identity_pub = kx.identity_public_key();
        let ephemeral_pub = kx.generate_ephemeral();

        // Create a BAD signature (sign wrong data)
        let bad_signature = kx.sign(b"wrong-data-intentionally");

        let offer = SignalMessage::CallOffer {
            identity_pub,
            ephemeral_pub,
            signature: bad_signature,
            supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
            alias: None,
        };
        client_transport_clone
            .send_signal(&offer)
            .await
            .expect("send should work");
    });

    let relay_result =
        wzp_relay::handshake::accept_handshake(relay_transport.as_ref(), &relay_seed).await;

    bad_client.await.unwrap();

    match relay_result {
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("signature verification failed"),
                "error should mention signature: {err_msg}"
            );
        }
        Ok(_) => panic!("relay should reject tampered signature"),
    }
}
