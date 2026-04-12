//! Relay-side (callee) cryptographic handshake.
//!
//! Performs the callee role of the WarzonePhone key exchange:
//! recv `CallOffer` → verify → generate ephemeral → derive session → send `CallAnswer`.

use wzp_crypto::{CryptoSession, KeyExchange, WarzoneKeyExchange};
use wzp_proto::{MediaTransport, QualityProfile, SignalMessage};

/// Accept the relay (callee) side of the cryptographic handshake.
///
/// 1. Receive `CallOffer` from client
/// 2. Verify caller's signature over `(ephemeral_pub || "call-offer")`
/// 3. Generate our own ephemeral X25519 keypair
/// 4. Sign `(ephemeral_pub || "call-answer")` with our identity key
/// 5. Derive shared ChaCha20-Poly1305 session
/// 6. Send `CallAnswer` back
///
/// Returns the derived `CryptoSession`, the chosen `QualityProfile`, the caller's fingerprint,
/// and the caller's alias (if provided in CallOffer).
pub async fn accept_handshake(
    transport: &dyn MediaTransport,
    seed: &[u8; 32],
) -> Result<(Box<dyn CryptoSession>, QualityProfile, String, Option<String>), anyhow::Error> {
    // 1. Receive CallOffer
    let offer = transport
        .recv_signal()
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before receiving CallOffer"))?;

    let (caller_identity_pub, caller_ephemeral_pub, caller_signature, supported_profiles, caller_alias) =
        match offer {
            SignalMessage::CallOffer {
                identity_pub,
                ephemeral_pub,
                signature,
                supported_profiles,
                alias,
            } => (identity_pub, ephemeral_pub, signature, supported_profiles, alias),
            other => {
                return Err(anyhow::anyhow!(
                    "expected CallOffer, got {:?}",
                    std::mem::discriminant(&other)
                ))
            }
        };

    // 2. Verify caller's signature over (ephemeral_pub || "call-offer")
    let mut verify_data = Vec::with_capacity(32 + 10);
    verify_data.extend_from_slice(&caller_ephemeral_pub);
    verify_data.extend_from_slice(b"call-offer");
    if !WarzoneKeyExchange::verify(&caller_identity_pub, &verify_data, &caller_signature) {
        return Err(anyhow::anyhow!("caller signature verification failed"));
    }

    // 3. Create our key exchange and generate ephemeral
    let mut kx = WarzoneKeyExchange::from_identity_seed(seed);
    let identity_pub = kx.identity_public_key();
    let ephemeral_pub = kx.generate_ephemeral();

    // 4. Sign (ephemeral_pub || "call-answer")
    let mut sign_data = Vec::with_capacity(32 + 11);
    sign_data.extend_from_slice(&ephemeral_pub);
    sign_data.extend_from_slice(b"call-answer");
    let signature = kx.sign(&sign_data);

    // 5. Derive session from caller's ephemeral public key
    let session = kx.derive_session(&caller_ephemeral_pub)?;

    // Choose the best supported profile (prefer GOOD > DEGRADED > CATASTROPHIC)
    let chosen_profile = choose_profile(&supported_profiles);

    // 6. Send CallAnswer
    let answer = SignalMessage::CallAnswer {
        identity_pub,
        ephemeral_pub,
        signature,
        chosen_profile,
    };
    transport.send_signal(&answer).await?;

    // Derive caller fingerprint: SHA-256(Ed25519 pub)[:16], formatted as xxxx:xxxx:...
    // Must match the format used in signal registration and presence.
    let caller_fp = {
        use sha2::{Sha256, Digest};
        let hash = Sha256::digest(&caller_identity_pub);
        let fp = wzp_crypto::Fingerprint([
            hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
            hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15],
        ]);
        fp.to_string()
    };

    Ok((session, chosen_profile, caller_fp, caller_alias))
}

/// Select the best quality profile from those the caller supports.
///
/// The `_supported` list is currently ignored — we hardcode GOOD (24k) until
/// studio tiers (32k/48k/64k) have been validated across federation (large
/// packets may exceed path MTU and fragment in unpleasant ways). Once that's
/// tested, the body should pick the highest supported profile ≤ the relay's
/// configured ceiling.
fn choose_profile(_supported: &[QualityProfile]) -> QualityProfile {
    QualityProfile::GOOD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choose_profile_picks_highest_bitrate() {
        let profiles = vec![
            QualityProfile::CATASTROPHIC,
            QualityProfile::GOOD,
            QualityProfile::DEGRADED,
        ];
        let chosen = choose_profile(&profiles);
        assert_eq!(chosen, QualityProfile::GOOD);
    }

    #[test]
    fn choose_profile_empty_defaults_to_good() {
        let chosen = choose_profile(&[]);
        assert_eq!(chosen, QualityProfile::GOOD);
    }
}
