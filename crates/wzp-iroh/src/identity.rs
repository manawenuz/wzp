//! Identity: WZP BIP39 seed -> iroh [`SecretKey`].
//!
//! The iroh secret key **is** the WZP Ed25519 identity signing key
//! (`HKDF-SHA256(seed, info="warzone-ed25519")`, see
//! `wzp_crypto::identity::Seed::derive_identity`). Consequently the iroh
//! [`EndpointId`] equals the WZP identity public key and the WZP fingerprint
//! (`SHA-256(pubkey)[..16]`) can be computed from any `EndpointId`.
//!
//! Rationale and threat analysis are in `docs/IROH.md` ("Identity").

use iroh::{EndpointId, SecretKey};
use sha2::{Digest, Sha256};

/// Derive the iroh secret key from a 32-byte WZP seed.
pub fn secret_key_from_seed(seed: &[u8; 32]) -> SecretKey {
    let id = wzp_crypto::Seed::from_bytes(*seed).derive_identity();
    let mut bytes = id.signing.to_bytes();
    let sk = SecretKey::from_bytes(&bytes);
    bytes.fill(0);
    sk
}

/// The 16-byte WZP fingerprint of an endpoint id (`SHA-256(pubkey)[..16]`).
///
/// Identical to `wzp_crypto::Fingerprint::from_verifying_key`.
pub fn fingerprint_bytes(id: &EndpointId) -> [u8; 16] {
    let hash = Sha256::digest(id.as_bytes());
    let mut fp = [0u8; 16];
    fp.copy_from_slice(&hash[..16]);
    fp
}

/// The WZP fingerprint, formatted `xxxx:xxxx:...` (8 groups) exactly as
/// `wzp_crypto::Fingerprint`'s `Display`.
pub fn fingerprint(id: &EndpointId) -> String {
    wzp_crypto::Fingerprint(fingerprint_bytes(id)).to_string()
}

/// Lower-case hex of the endpoint id (the WZP `identity_pub`).
pub fn id_hex(id: &EndpointId) -> String {
    hex::encode(id.as_bytes())
}

/// Parse an endpoint id given as 64 hex chars or iroh's z-base-32 form.
pub fn parse_endpoint_id(s: &str) -> Option<EndpointId> {
    let s = s.trim();
    if s.len() == 64
        && let Ok(b) = hex::decode(s)
    {
        let arr: [u8; 32] = b.try_into().ok()?;
        return EndpointId::from_bytes(&arr).ok();
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_equals_wzp_identity_key() {
        let seed = [7u8; 32];
        let sk = secret_key_from_seed(&seed);
        let wzp = wzp_crypto::Seed::from_bytes(seed).derive_identity();
        let pubid = wzp.public_identity();
        assert_eq!(sk.public().as_bytes(), pubid.signing.as_bytes());
        assert_eq!(fingerprint(&sk.public()), pubid.fingerprint.to_string());
        assert_eq!(
            &fingerprint_bytes(&sk.public()),
            pubid.fingerprint.as_bytes()
        );
    }

    #[test]
    fn parse_ids() {
        let id = secret_key_from_seed(&[1u8; 32]).public();
        assert_eq!(parse_endpoint_id(&id_hex(&id)), Some(id));
        assert_eq!(parse_endpoint_id(&id.to_string()), Some(id));
        assert_eq!(parse_endpoint_id("nope"), None);
    }
}
