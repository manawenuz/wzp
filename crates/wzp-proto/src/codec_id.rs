use serde::{Deserialize, Serialize};

/// Identifies the audio codec and bitrate configuration.
///
/// Encoded as 4 bits in the media packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CodecId {
    /// Opus at 24kbps (good conditions)
    Opus24k = 0,
    /// Opus at 16kbps (moderate conditions)
    Opus16k = 1,
    /// Opus at 6kbps (degraded conditions)
    Opus6k = 2,
    /// Codec2 at 3200bps (poor conditions)
    Codec2_3200 = 3,
    /// Codec2 at 1200bps (catastrophic conditions)
    Codec2_1200 = 4,
}

impl CodecId {
    /// Nominal bitrate in bits per second.
    pub const fn bitrate_bps(self) -> u32 {
        match self {
            Self::Opus24k => 24_000,
            Self::Opus16k => 16_000,
            Self::Opus6k => 6_000,
            Self::Codec2_3200 => 3_200,
            Self::Codec2_1200 => 1_200,
        }
    }

    /// Preferred frame duration in milliseconds.
    pub const fn frame_duration_ms(self) -> u8 {
        match self {
            Self::Opus24k => 20,
            Self::Opus16k => 20,
            Self::Opus6k => 40,
            Self::Codec2_3200 => 20,
            Self::Codec2_1200 => 40,
        }
    }

    /// Sample rate expected by this codec.
    pub const fn sample_rate_hz(self) -> u32 {
        match self {
            Self::Opus24k | Self::Opus16k | Self::Opus6k => 48_000,
            Self::Codec2_3200 | Self::Codec2_1200 => 8_000,
        }
    }

    /// Try to decode from the 4-bit wire representation.
    pub const fn from_wire(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Opus24k),
            1 => Some(Self::Opus16k),
            2 => Some(Self::Opus6k),
            3 => Some(Self::Codec2_3200),
            4 => Some(Self::Codec2_1200),
            _ => None,
        }
    }

    /// Encode to the 4-bit wire representation.
    pub const fn to_wire(self) -> u8 {
        self as u8
    }
}

/// Describes the complete quality configuration for a call session.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualityProfile {
    /// Active codec.
    pub codec: CodecId,
    /// FEC repair ratio (0.0 = no FEC, 1.0 = 100% overhead, 2.0 = 200% overhead).
    pub fec_ratio: f32,
    /// Audio frame duration in ms (20 or 40).
    pub frame_duration_ms: u8,
    /// Number of source frames per FEC block.
    pub frames_per_block: u8,
}

impl QualityProfile {
    /// Good conditions: Opus 24kbps, light FEC.
    pub const GOOD: Self = Self {
        codec: CodecId::Opus24k,
        fec_ratio: 0.2,
        frame_duration_ms: 20,
        frames_per_block: 5,
    };

    /// Degraded conditions: Opus 6kbps, moderate FEC.
    pub const DEGRADED: Self = Self {
        codec: CodecId::Opus6k,
        fec_ratio: 0.5,
        frame_duration_ms: 40,
        frames_per_block: 10,
    };

    /// Catastrophic conditions: Codec2 1.2kbps, heavy FEC.
    pub const CATASTROPHIC: Self = Self {
        codec: CodecId::Codec2_1200,
        fec_ratio: 1.0,
        frame_duration_ms: 40,
        frames_per_block: 8,
    };

    /// Estimated total bandwidth in kbps including FEC overhead.
    pub fn total_bitrate_kbps(&self) -> f32 {
        let base = self.codec.bitrate_bps() as f32 / 1000.0;
        base * (1.0 + self.fec_ratio)
    }
}
