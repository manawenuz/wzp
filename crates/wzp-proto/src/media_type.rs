use serde::{Deserialize, Serialize};

/// Media stream type carried in a v2 [`MediaHeader`](crate::MediaHeaderV2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum MediaType {
    Audio = 0,
    Video = 1,
    Data = 2,
    Control = 3,
}

impl MediaType {
    pub const fn to_wire(self) -> u8 {
        self as u8
    }

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
