//! Client-side cryptographic handshake.
//!
//! Performs the caller role of the WarzonePhone key exchange:
//! send `CallOffer` → recv `CallAnswer` → derive shared `CryptoSession`.

use wzp_crypto::{CryptoSession, KeyExchange, WarzoneKeyExchange};
use wzp_proto::{MediaTransport, QualityProfile, SignalMessage};

/// Perform the client (caller) side of the cryptographic handshake.
///
/// 1. Derive identity from `seed`
/// 2. Generate ephemeral X25519 keypair
/// 3. Sign `(ephemeral_pub || "call-offer")` with identity key
/// 4. Send `CallOffer` with identity_pub, ephemeral_pub, signature
/// 5. Receive `CallAnswer`, verify callee signature
/// 6. Derive shared ChaCha20-Poly1305 session
pub async fn perform_handshake(
    transport: &dyn MediaTransport,
    seed: &[u8; 32],
    alias: Option<&str>,
) -> Result<Box<dyn CryptoSession>, anyhow::Error> {
    // 1. Create key exchange from identity seed
    let mut kx = WarzoneKeyExchange::from_identity_seed(seed);
    let identity_pub = kx.identity_public_key();

    // 2. Generate ephemeral key
    let ephemeral_pub = kx.generate_ephemeral();

    // 3. Sign (ephemeral_pub || "call-offer")
    let mut sign_data = Vec::with_capacity(32 + 10);
    sign_data.extend_from_slice(&ephemeral_pub);
    sign_data.extend_from_slice(b"call-offer");
    let signature = kx.sign(&sign_data);

    // 4. Send CallOffer
    let offer = SignalMessage::CallOffer {
        identity_pub,
        ephemeral_pub,
        signature,
        supported_profiles: vec![
            QualityProfile::GOOD,
            QualityProfile::DEGRADED,
            QualityProfile::CATASTROPHIC,
        ],
        alias: alias.map(|s| s.to_string()),
    };
    transport.send_signal(&offer).await?;

    // 5. Wait for CallAnswer
    let answer = transport
        .recv_signal()
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before receiving CallAnswer"))?;

    let (callee_identity_pub, callee_ephemeral_pub, callee_signature, _chosen_profile) = match answer
    {
        SignalMessage::CallAnswer {
            identity_pub,
            ephemeral_pub,
            signature,
            chosen_profile,
        } => (identity_pub, ephemeral_pub, signature, chosen_profile),
        other => {
            return Err(anyhow::anyhow!(
                "expected CallAnswer, got {:?}",
                std::mem::discriminant(&other)
            ))
        }
    };

    // 6. Verify callee's signature over (ephemeral_pub || "call-answer")
    let mut verify_data = Vec::with_capacity(32 + 11);
    verify_data.extend_from_slice(&callee_ephemeral_pub);
    verify_data.extend_from_slice(b"call-answer");
    if !WarzoneKeyExchange::verify(&callee_identity_pub, &verify_data, &callee_signature) {
        return Err(anyhow::anyhow!("callee signature verification failed"));
    }

    // 7. Derive session
    let session = kx.derive_session(&callee_ephemeral_pub)?;

    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration test lives in tests/ — unit-level coverage relies on wzp-crypto tests.
    #[test]
    fn sign_data_format() {
        let kx = WarzoneKeyExchange::from_identity_seed(&[0xAA; 32]);
        let eph = [0x11u8; 32];
        let mut data = Vec::new();
        data.extend_from_slice(&eph);
        data.extend_from_slice(b"call-offer");
        let sig = kx.sign(&data);
        assert!(WarzoneKeyExchange::verify(
            &kx.identity_public_key(),
            &data,
            &sig,
        ));
    }
}
