//! Call statistics for the Android engine.

/// State of the call.
#[derive(Clone, Debug, Default, serde::Serialize, PartialEq, Eq)]
pub enum CallState {
    /// Engine is idle, no active call.
    #[default]
    Idle,
    /// Establishing connection to the relay.
    Connecting,
    /// Call is active with audio flowing.
    Active,
    /// Temporarily lost connection, attempting to recover.
    Reconnecting,
    /// Call has ended.
    Closed,
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
}
