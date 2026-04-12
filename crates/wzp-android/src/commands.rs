//! Engine commands sent from the JNI/UI thread to the engine.

use wzp_proto::QualityProfile;

/// Commands that can be sent to the running engine.
pub enum EngineCommand {
    /// Mute or unmute the microphone.
    SetMute(bool),
    /// Enable or disable speaker (loudspeaker) mode.
    SetSpeaker(bool),
    /// Force a specific quality profile (overrides adaptive logic).
    ForceProfile(QualityProfile),
    /// Stop the call and shut down the engine.
    Stop,
    /// Place a direct call to a fingerprint (requires signal connection).
    PlaceCall { target_fingerprint: String },
    /// Answer an incoming direct call.
    AnswerCall {
        call_id: String,
        accept_mode: wzp_proto::CallAcceptMode,
    },
    /// Reject an incoming direct call.
    RejectCall { call_id: String },
}
