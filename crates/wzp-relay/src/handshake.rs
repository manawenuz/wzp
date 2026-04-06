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

    // Derive caller fingerprint from their identity public key (first 8 bytes as hex)
    let caller_fp = caller_identity_pub[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    Ok((session, chosen_profile, caller_fp, caller_alias))
}

/// Select the best quality profile from those the caller supports.
fn choose_profile(supported: &[QualityProfile]) -> QualityProfile {
    // Prefer higher-quality profiles. Use GOOD as default if supported list is empty.
    if supported.is_empty() {
        return QualityProfile::GOOD;
    }
    // Pick the profile with the highest bitrate.
    supported
        .iter()
        .max_by(|a, b| {
            a.total_bitrate_kbps()
                .partial_cmp(&b.total_bitrate_kbps())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or(QualityProfile::GOOD)
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
