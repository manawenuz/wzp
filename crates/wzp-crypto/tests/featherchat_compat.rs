//! Cross-project compatibility tests between WZP and featherChat.
//!
//! Verifies:
//! 1. Identity: same seed → same keys → same fingerprints (WZP-FC-8)
//! 2. CallSignal: WZP SignalMessage serializes into FC CallSignal.payload correctly
//! 3. Auth: WZP auth module request/response matches FC's /v1/auth/validate contract
//! 4. Mnemonic: BIP39 interop between both implementations

use wzp_proto::KeyExchange;

// ─── Identity Compatibility (WZP-FC-8) ──────────────────────────────────────

#[test]
fn same_seed_same_ed25519_key() {
    let seed = [42u8; 32];

    let wzp_kx = wzp_crypto::WarzoneKeyExchange::from_identity_seed(&seed);
    let wzp_pub = wzp_kx.identity_public_key();

    let fc_seed = warzone_protocol::identity::Seed::from_bytes(seed);
    let fc_id = fc_seed.derive_identity();
    let fc_pub = fc_id.signing.verifying_key();

    assert_eq!(&wzp_pub, fc_pub.as_bytes(), "Ed25519 keys must match");
}

#[test]
fn same_seed_same_fingerprint() {
    let seed = [99u8; 32];

    let wzp_kx = wzp_crypto::WarzoneKeyExchange::from_identity_seed(&seed);
    let wzp_fp = wzp_kx.fingerprint();

    let fc_seed = warzone_protocol::identity::Seed::from_bytes(seed);
    let fc_fp = fc_seed.derive_identity().public_identity().fingerprint.0;

    assert_eq!(wzp_fp, fc_fp, "Fingerprints must match");
}

#[test]
fn wzp_identity_module_matches_featherchat() {
    let seed = [0xAB; 32];

    let wzp_pub = wzp_crypto::Seed::from_bytes(seed)
        .derive_identity()
        .public_identity();

    let fc_pub = warzone_protocol::identity::Seed::from_bytes(seed)
        .derive_identity()
        .public_identity();

    assert_eq!(wzp_pub.signing.as_bytes(), fc_pub.signing.as_bytes());
    assert_eq!(wzp_pub.encryption.as_bytes(), fc_pub.encryption.as_bytes());
    assert_eq!(wzp_pub.fingerprint.0, fc_pub.fingerprint.0);
    assert_eq!(wzp_pub.fingerprint.to_string(), fc_pub.fingerprint.to_string());
}

#[test]
fn random_seed_identity_match() {
    let fc_seed = warzone_protocol::identity::Seed::generate();
    let raw = fc_seed.0;

    let fc_fp = fc_seed.derive_identity().public_identity().fingerprint.0;
    let wzp_fp = wzp_crypto::WarzoneKeyExchange::from_identity_seed(&raw).fingerprint();

    assert_eq!(wzp_fp, fc_fp);
}

#[test]
fn hkdf_derive_matches() {
    let seed = [0x55; 32];

    let fc_ed = warzone_protocol::crypto::hkdf_derive(&seed, b"", b"warzone-ed25519", 32);
    let fc_signing = ed25519_dalek::SigningKey::from_bytes(&fc_ed.try_into().unwrap());
    let fc_pub = fc_signing.verifying_key();

    let wzp_pub = wzp_crypto::WarzoneKeyExchange::from_identity_seed(&seed).identity_public_key();

    assert_eq!(&wzp_pub, fc_pub.as_bytes());
}

// ─── BIP39 Mnemonic Interop ─────────────────────────────────────────────────

#[test]
fn mnemonic_roundtrip_fc_to_wzp() {
    let seed = [0x77; 32];
    let fc_mnemonic = warzone_protocol::identity::Seed::from_bytes(seed).to_mnemonic();
    let wzp_recovered = wzp_crypto::Seed::from_mnemonic(&fc_mnemonic).unwrap();
    assert_eq!(wzp_recovered.0, seed);
}

#[test]
fn mnemonic_roundtrip_wzp_to_fc() {
    let seed = [0x33; 32];
    let wzp_mnemonic = wzp_crypto::Seed::from_bytes(seed).to_mnemonic();
    let fc_recovered = warzone_protocol::identity::Seed::from_mnemonic(&wzp_mnemonic).unwrap();
    assert_eq!(fc_recovered.0, seed);
}

#[test]
fn mnemonic_strings_identical() {
    let seed = [0xDE; 32];
    let fc_words = warzone_protocol::identity::Seed::from_bytes(seed).to_mnemonic();
    let wzp_words = wzp_crypto::Seed::from_bytes(seed).to_mnemonic();
    assert_eq!(fc_words, wzp_words);
}

// ─── CallSignal Payload Interop ─────────────────────────────────────────────

#[test]
fn wzp_signal_serializes_into_fc_callsignal_payload() {
    // WZP creates a CallOffer SignalMessage
    let offer = wzp_proto::SignalMessage::CallOffer {
        identity_pub: [1u8; 32],
        ephemeral_pub: [2u8; 32],
        signature: vec![3u8; 64],
        supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
    };

    // Encode as featherChat CallSignal payload
    let payload = wzp_client::featherchat::encode_call_payload(
        &offer,
        Some("relay.example.com:4433"),
        Some("myroom"),
    );

    // Verify it's valid JSON
    let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert!(parsed.get("signal").is_some());
    assert_eq!(parsed["relay_addr"], "relay.example.com:4433");
    assert_eq!(parsed["room"], "myroom");

    // featherChat would put this in WireMessage::CallSignal { payload, ... }
    // Verify the FC side can create a CallSignal with this payload
    let fc_msg = warzone_protocol::message::WireMessage::CallSignal {
        id: "call-123".to_string(),
        sender_fingerprint: "abcd1234".to_string(),
        signal_type: warzone_protocol::message::CallSignalType::Offer,
        payload: payload.clone(),
        target: "peer-fingerprint".to_string(),
    };

    // Verify it serializes with bincode (FC's wire format)
    let encoded = bincode::serialize(&fc_msg).unwrap();
    assert!(!encoded.is_empty());

    // And deserializes back
    let decoded: warzone_protocol::message::WireMessage = bincode::deserialize(&encoded).unwrap();
    if let warzone_protocol::message::WireMessage::CallSignal {
        id, payload: p, signal_type, ..
    } = decoded
    {
        assert_eq!(id, "call-123");
        assert!(matches!(signal_type, warzone_protocol::message::CallSignalType::Offer));

        // Decode the WZP payload back
        let wzp_payload = wzp_client::featherchat::decode_call_payload(&p).unwrap();
        assert_eq!(wzp_payload.relay_addr.unwrap(), "relay.example.com:4433");
        assert!(matches!(wzp_payload.signal, wzp_proto::SignalMessage::CallOffer { .. }));
    } else {
        panic!("expected CallSignal");
    }
}

#[test]
fn wzp_answer_round_trips_through_fc_callsignal() {
    let answer = wzp_proto::SignalMessage::CallAnswer {
        identity_pub: [10u8; 32],
        ephemeral_pub: [20u8; 32],
        signature: vec![30u8; 64],
        chosen_profile: wzp_proto::QualityProfile::DEGRADED,
    };

    let payload = wzp_client::featherchat::encode_call_payload(&answer, None, None);

    let fc_msg = warzone_protocol::message::WireMessage::CallSignal {
        id: "call-456".to_string(),
        sender_fingerprint: "efgh5678".to_string(),
        signal_type: warzone_protocol::message::CallSignalType::Answer,
        payload,
        target: "caller-fp".to_string(),
    };

    let bytes = bincode::serialize(&fc_msg).unwrap();
    let decoded: warzone_protocol::message::WireMessage = bincode::deserialize(&bytes).unwrap();

    if let warzone_protocol::message::WireMessage::CallSignal { payload, .. } = decoded {
        let wzp = wzp_client::featherchat::decode_call_payload(&payload).unwrap();
        if let wzp_proto::SignalMessage::CallAnswer { chosen_profile, .. } = wzp.signal {
            assert_eq!(chosen_profile.codec, wzp_proto::CodecId::Opus6k);
        } else {
            panic!("expected CallAnswer");
        }
    }
}

#[test]
fn wzp_hangup_round_trips_through_fc_callsignal() {
    let hangup = wzp_proto::SignalMessage::Hangup {
        reason: wzp_proto::HangupReason::Normal,
    };

    let payload = wzp_client::featherchat::encode_call_payload(&hangup, None, None);
    let signal_type = wzp_client::featherchat::signal_to_call_type(&hangup);
    assert!(matches!(signal_type, wzp_client::featherchat::CallSignalType::Hangup));

    let fc_msg = warzone_protocol::message::WireMessage::CallSignal {
        id: "call-789".to_string(),
        sender_fingerprint: "xyz".to_string(),
        signal_type: warzone_protocol::message::CallSignalType::Hangup,
        payload,
        target: "peer".to_string(),
    };

    let bytes = bincode::serialize(&fc_msg).unwrap();
    let decoded: warzone_protocol::message::WireMessage = bincode::deserialize(&bytes).unwrap();

    if let warzone_protocol::message::WireMessage::CallSignal { payload, .. } = decoded {
        let wzp = wzp_client::featherchat::decode_call_payload(&payload).unwrap();
        assert!(matches!(wzp.signal, wzp_proto::SignalMessage::Hangup { .. }));
    }
}

// ─── Auth Token Contract ────────────────────────────────────────────────────

#[test]
fn auth_validate_request_matches_fc_contract() {
    // WZP sends: { "token": "..." }
    // FC expects: ValidateRequest { token: String }
    let wzp_request = serde_json::json!({ "token": "test-token-123" });
    let json_str = wzp_request.to_string();

    // FC can deserialize this (same shape as their ValidateRequest)
    #[derive(serde::Deserialize)]
    struct FcValidateRequest {
        token: String,
    }
    let fc_req: FcValidateRequest = serde_json::from_str(&json_str).unwrap();
    assert_eq!(fc_req.token, "test-token-123");
}

#[test]
fn auth_validate_response_matches_wzp_expectations() {
    // FC returns: { "valid": true, "fingerprint": "...", "alias": "..." }
    // WZP expects: wzp_relay::auth::ValidateResponse
    let fc_response = serde_json::json!({
        "valid": true,
        "fingerprint": "a3f8:1b2c:3d4e:5f60:7182:93a4:b5c6:d7e8",
        "alias": "manwe",
        "eth_address": null
    });

    let wzp_resp: wzp_relay::auth::ValidateResponse =
        serde_json::from_value(fc_response).unwrap();
    assert!(wzp_resp.valid);
    assert_eq!(
        wzp_resp.fingerprint.unwrap(),
        "a3f8:1b2c:3d4e:5f60:7182:93a4:b5c6:d7e8"
    );
    assert_eq!(wzp_resp.alias.unwrap(), "manwe");
}

#[test]
fn auth_invalid_response_matches() {
    let fc_response = serde_json::json!({ "valid": false });
    let wzp_resp: wzp_relay::auth::ValidateResponse =
        serde_json::from_value(fc_response).unwrap();
    assert!(!wzp_resp.valid);
    assert!(wzp_resp.fingerprint.is_none());
}

// ─── Signal Type Mapping ────────────────────────────────────────────────────

#[test]
fn all_signal_types_map_correctly() {
    use wzp_client::featherchat::{signal_to_call_type, CallSignalType};

    let cases: Vec<(wzp_proto::SignalMessage, &str)> = vec![
        (
            wzp_proto::SignalMessage::CallOffer {
                identity_pub: [0; 32], ephemeral_pub: [0; 32],
                signature: vec![], supported_profiles: vec![],
            },
            "Offer",
        ),
        (
            wzp_proto::SignalMessage::CallAnswer {
                identity_pub: [0; 32], ephemeral_pub: [0; 32],
                signature: vec![],
                chosen_profile: wzp_proto::QualityProfile::GOOD,
            },
            "Answer",
        ),
        (
            wzp_proto::SignalMessage::IceCandidate {
                candidate: "candidate:1".to_string(),
            },
            "IceCandidate",
        ),
        (
            wzp_proto::SignalMessage::Hangup {
                reason: wzp_proto::HangupReason::Normal,
            },
            "Hangup",
        ),
    ];

    for (signal, expected_name) in cases {
        let ct = signal_to_call_type(&signal);
        let name = format!("{ct:?}");
        assert_eq!(name, expected_name, "signal type mapping for {expected_name}");
    }
}
