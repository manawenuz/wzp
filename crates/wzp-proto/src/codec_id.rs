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
    /// Comfort noise descriptor (silence suppression)
    ComfortNoise = 5,
    /// Opus at 32kbps (studio low)
    Opus32k = 6,
    /// Opus at 48kbps (studio)
    Opus48k = 7,
    /// Opus at 64kbps (studio high)
    Opus64k = 8,
}

impl CodecId {
    /// Nominal bitrate in bits per second.
    pub const fn bitrate_bps(self) -> u32 {
        match self {
            Self::Opus24k => 24_000,
            Self::Opus16k => 16_000,
            Self::Opus6k => 6_000,
            Self::Opus32k => 32_000,
            Self::Opus48k => 48_000,
            Self::Opus64k => 64_000,
            Self::Codec2_3200 => 3_200,
            Self::Codec2_1200 => 1_200,
            Self::ComfortNoise => 0,
        }
    }

    /// Preferred frame duration in milliseconds.
    pub const fn frame_duration_ms(self) -> u8 {
        match self {
            Self::Opus24k | Self::Opus16k | Self::Opus32k | Self::Opus48k | Self::Opus64k => 20,
            Self::Opus6k => 40,
            Self::Codec2_3200 => 20,
            Self::Codec2_1200 => 40,
            Self::ComfortNoise => 20,
        }
    }

    /// Sample rate expected by this codec.
    pub const fn sample_rate_hz(self) -> u32 {
        match self {
            Self::Opus24k | Self::Opus16k | Self::Opus6k
            | Self::Opus32k | Self::Opus48k | Self::Opus64k => 48_000,
            Self::Codec2_3200 | Self::Codec2_1200 => 8_000,
            Self::ComfortNoise => 48_000,
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
            5 => Some(Self::ComfortNoise),
            6 => Some(Self::Opus32k),
            7 => Some(Self::Opus48k),
            8 => Some(Self::Opus64k),
            _ => None,
        }
    }

    /// Encode to the 4-bit wire representation.
    pub const fn to_wire(self) -> u8 {
        self as u8
    }

    /// Returns true if this is an Opus variant.
    pub const fn is_opus(self) -> bool {
        matches!(self, Self::Opus6k | Self::Opus16k | Self::Opus24k
            | Self::Opus32k | Self::Opus48k | Self::Opus64k)
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
    /// Good conditions: Opus 24kbps, FEC disabled for federation debugging.
    pub const GOOD: Self = Self {
        codec: CodecId::Opus24k,
        fec_ratio: 0.0,
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

    /// Studio low: Opus 32kbps, minimal FEC.
    pub const STUDIO_32K: Self = Self {
        codec: CodecId::Opus32k,
        fec_ratio: 0.1,
        frame_duration_ms: 20,
        frames_per_block: 5,
    };

    /// Studio: Opus 48kbps, minimal FEC.
    pub const STUDIO_48K: Self = Self {
        codec: CodecId::Opus48k,
        fec_ratio: 0.1,
        frame_duration_ms: 20,
        frames_per_block: 5,
    };

    /// Studio high: Opus 64kbps, minimal FEC.
    pub const STUDIO_64K: Self = Self {
        codec: CodecId::Opus64k,
        fec_ratio: 0.1,
        frame_duration_ms: 20,
        frames_per_block: 5,
    };

    /// Estimated total bandwidth in kbps including FEC overhead.
    pub fn total_bitrate_kbps(&self) -> f32 {
        let base = self.codec.bitrate_bps() as f32 / 1000.0;
        base * (1.0 + self.fec_ratio)
    }
}
