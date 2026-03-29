//! featherChat signaling bridge.
//!
//! Sends WZP call signaling (Offer/Answer/Hangup) through featherChat's
//! E2E encrypted WebSocket channel as `WireMessage::CallSignal`.
//!
//! Flow:
//! 1. Client connects to featherChat WS with bearer token
//! 2. Sends CallOffer as CallSignal(signal_type=Offer, payload=JSON SignalMessage)
//! 3. Receives CallAnswer as CallSignal(signal_type=Answer, payload=JSON SignalMessage)
//! 4. Extracts relay address from the answer
//! 5. Connects QUIC to relay for media

use serde::{Deserialize, Serialize};
use wzp_proto::packet::SignalMessage;

/// featherChat CallSignal types (mirrors warzone-protocol::message::CallSignalType).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CallSignalType {
    Offer,
    Answer,
    IceCandidate,
    Hangup,
    Reject,
    Ringing,
    Busy,
    Hold,
    Unhold,
    Mute,
    Unmute,
    Transfer,
}

/// A CallSignal as sent through featherChat's WireMessage.
/// This is what goes in the `payload` field of `WireMessage::CallSignal`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WzpCallPayload {
    /// The WZP SignalMessage (CallOffer, CallAnswer, etc.) serialized as JSON.
    pub signal: SignalMessage,
    /// The relay address to connect to for media (host:port).
    pub relay_addr: Option<String>,
    /// Room name on the relay.
    pub room: Option<String>,
}

/// Parameters for initiating a call through featherChat.
pub struct CallInitParams {
    /// featherChat server URL (e.g., "wss://chat.example.com/ws").
    pub server_url: String,
    /// Bearer token for authentication.
    pub token: String,
    /// Target peer fingerprint (who to call).
    pub target_fingerprint: String,
    /// Relay address for media transport.
    pub relay_addr: String,
    /// Room name on the relay.
    pub room: String,
    /// Our identity seed for crypto.
    pub seed: [u8; 32],
}

/// Result of a successful call setup.
pub struct CallSetupResult {
    /// Relay address to connect to.
    pub relay_addr: String,
    /// Room name.
    pub room: String,
    /// The peer's CallAnswer signal (contains ephemeral key, etc.)
    pub answer: SignalMessage,
}

/// Serialize a WZP SignalMessage into a featherChat CallSignal payload string.
pub fn encode_call_payload(
    signal: &SignalMessage,
    relay_addr: Option<&str>,
    room: Option<&str>,
) -> String {
    let payload = WzpCallPayload {
        signal: signal.clone(),
        relay_addr: relay_addr.map(|s| s.to_string()),
        room: room.map(|s| s.to_string()),
    };
    serde_json::to_string(&payload).unwrap_or_default()
}

/// Deserialize a featherChat CallSignal payload back to WZP types.
pub fn decode_call_payload(payload: &str) -> Result<WzpCallPayload, String> {
    serde_json::from_str(payload).map_err(|e| format!("invalid call payload: {e}"))
}

/// Map WZP SignalMessage type to featherChat CallSignalType.
pub fn signal_to_call_type(signal: &SignalMessage) -> CallSignalType {
    match signal {
        SignalMessage::CallOffer { .. } => CallSignalType::Offer,
        SignalMessage::CallAnswer { .. } => CallSignalType::Answer,
        SignalMessage::IceCandidate { .. } => CallSignalType::IceCandidate,
        SignalMessage::Hangup { .. } => CallSignalType::Hangup,
        SignalMessage::Rekey { .. } => CallSignalType::Offer, // reuse
        SignalMessage::QualityUpdate { .. } => CallSignalType::Offer, // reuse
        SignalMessage::Ping { .. } | SignalMessage::Pong { .. } => CallSignalType::Offer,
        SignalMessage::AuthToken { .. } => CallSignalType::Offer,
        SignalMessage::Hold => CallSignalType::Hold,
        SignalMessage::Unhold => CallSignalType::Unhold,
        SignalMessage::Mute => CallSignalType::Mute,
        SignalMessage::Unmute => CallSignalType::Unmute,
        SignalMessage::Transfer { .. } => CallSignalType::Transfer,
        SignalMessage::TransferAck => CallSignalType::Offer, // reuse
        SignalMessage::PresenceUpdate { .. } => CallSignalType::Offer, // reuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let signal = SignalMessage::CallOffer {
            identity_pub: [1u8; 32],
            ephemeral_pub: [2u8; 32],
            signature: vec![3u8; 64],
            supported_profiles: vec![QualityProfile::GOOD],
        };

        let encoded = encode_call_payload(&signal, Some("relay.example.com:4433"), Some("myroom"));
        let decoded = decode_call_payload(&encoded).unwrap();

        assert_eq!(decoded.relay_addr.unwrap(), "relay.example.com:4433");
        assert_eq!(decoded.room.unwrap(), "myroom");
        assert!(matches!(decoded.signal, SignalMessage::CallOffer { .. }));
    }

    #[test]
    fn signal_type_mapping() {
        let offer = SignalMessage::CallOffer {
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            signature: vec![],
            supported_profiles: vec![],
        };
        assert!(matches!(signal_to_call_type(&offer), CallSignalType::Offer));

        let hangup = SignalMessage::Hangup {
            reason: wzp_proto::HangupReason::Normal,
        };
        assert!(matches!(signal_to_call_type(&hangup), CallSignalType::Hangup));

        assert!(matches!(signal_to_call_type(&SignalMessage::Hold), CallSignalType::Hold));
        assert!(matches!(signal_to_call_type(&SignalMessage::Unhold), CallSignalType::Unhold));
        assert!(matches!(signal_to_call_type(&SignalMessage::Mute), CallSignalType::Mute));
        assert!(matches!(signal_to_call_type(&SignalMessage::Unmute), CallSignalType::Unmute));

        let transfer = SignalMessage::Transfer {
            target_fingerprint: "abc".to_string(),
            relay_addr: None,
        };
        assert!(matches!(signal_to_call_type(&transfer), CallSignalType::Transfer));
    }
}
