//! Call statistics for the Android engine.

/// State of the call.
/// Serializes as integer for easy parsing on the Kotlin side:
/// 0=Idle, 1=Connecting, 2=Active, 3=Reconnecting, 4=Closed
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CallState {
    #[default]
    Idle,
    Connecting,
    Active,
    Reconnecting,
    Closed,
}

impl serde::Serialize for CallState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let n: u8 = match self {
            CallState::Idle => 0,
            CallState::Connecting => 1,
            CallState::Active => 2,
            CallState::Reconnecting => 3,
            CallState::Closed => 4,
        };
        serializer.serialize_u8(n)
    }
}

/// Aggregated call statistics, serializable for JNI bridge.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct CallStats {
    /// Current call state.
    pub state: CallState,
    /// Call duration in seconds.
    pub duration_secs: f64,
    /// Current quality tier (0=GOOD, 1=DEGRADED, 2=CATASTROPHIC).
    pub quality_tier: u8,
    /// Observed packet loss percentage.
    pub loss_pct: f32,
    /// Smoothed round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// Jitter in milliseconds.
    pub jitter_ms: u32,
    /// Current jitter buffer depth in packets.
    pub jitter_buffer_depth: usize,
    /// Total frames encoded since call start.
    pub frames_encoded: u64,
    /// Total frames decoded since call start.
    pub frames_decoded: u64,
    /// Number of playout underruns (buffer empty when audio needed).
    pub underruns: u64,
    /// Frames recovered by FEC.
    pub fec_recovered: u64,
    /// Playout ring overflow count (reader was lapped by writer).
    pub playout_overflows: u64,
    /// Playout ring underrun count (reader found empty buffer).
    pub playout_underruns: u64,
    /// Capture ring overflow count.
    pub capture_overflows: u64,
    /// Current mic audio level (RMS of i16 samples, 0-32767).
    pub audio_level: u32,
    /// Number of participants in the room (from last RoomUpdate).
    pub room_participant_count: u32,
    /// Participant list (fingerprint + optional alias) serialized as JSON array.
    pub room_participants: Vec<RoomMember>,
}

/// A room member entry, serialized into the stats JSON.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RoomMember {
    pub fingerprint: String,
    pub alias: Option<String>,
}
