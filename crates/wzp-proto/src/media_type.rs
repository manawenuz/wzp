use serde::{Deserialize, Serialize};

/// Media stream type carried in a v2 [`MediaHeaderV2`](crate::MediaHeaderV2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MediaType {
    /// Encoded speech / music (Opus, Codec2, ComfortNoise).
    Audio = 0,
    /// Encoded video access unit (H.264, H.265, AV1; PRD-video-multicodec).
    Video = 1,
    /// Opaque payload not interpreted by the relay (reserved).
    Data = 2,
    /// In-band control message carried on the media plane (reserved).
    Control = 3,
}

impl MediaType {
    /// Encode to the wire byte representation (`self as u8`).
    pub const fn to_wire(self) -> u8 {
        self as u8
    }

    /// Decode from a wire byte. Returns `None` for values outside 0..=3.
    pub const fn from_wire(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Audio),
            1 => Some(Self::Video),
            2 => Some(Self::Data),
            3 => Some(Self::Control),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_roundtrip() {
        for mt in [
            MediaType::Audio,
            MediaType::Video,
            MediaType::Data,
            MediaType::Control,
        ] {
            assert_eq!(MediaType::from_wire(mt.to_wire()), Some(mt));
        }
    }

    #[test]
    fn media_type_unknown_rejected() {
        for v in 4u8..=255 {
            assert!(MediaType::from_wire(v).is_none(), "v={v}");
        }
    }
}
