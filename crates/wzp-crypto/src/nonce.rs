//! Nonce construction for ChaCha20-Poly1305.
//!
//! 12-byte nonce layout:
//!   session_id[0..4] || sequence_number (u32 BE) || direction (1 byte)
//!   || media_type (1 byte) || stream_id (1 byte) || 0x00
//!
//! Both peers share one key, so the direction byte MUST differ between the
//! two sending sides, and media_type/stream_id keep independently numbered
//! streams from colliding. Use [`build_media_nonce`] for media packets.

/// Direction of packet flow, used in nonce construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send = 0,
    Recv = 1,
}

/// Build a 12-byte nonce from session_id, sequence number, and direction.
///
/// This deterministic construction allows both sides to derive the same nonce
/// without transmitting it, saving 12 bytes per packet.
pub fn build_nonce(session_id: &[u8; 4], seq: u32, direction: Direction) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..4].copy_from_slice(session_id);
    nonce[4..8].copy_from_slice(&seq.to_be_bytes());
    nonce[8] = direction as u8;
    // nonce[9..12] remain zero (padding)
    nonce
}

/// Build a 12-byte media nonce that also binds the media type and stream id,
/// so streams with independent sequence counters never share a nonce.
pub fn build_media_nonce(
    session_id: &[u8; 4],
    seq: u32,
    direction: Direction,
    media_type: u8,
    stream_id: u8,
) -> [u8; 12] {
    let mut nonce = build_nonce(session_id, seq, direction);
    nonce[9] = media_type;
    nonce[10] = stream_id;
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_is_deterministic() {
        let sid = [0xDE, 0xAD, 0xBE, 0xEF];
        let n1 = build_nonce(&sid, 42, Direction::Send);
        let n2 = build_nonce(&sid, 42, Direction::Send);
        assert_eq!(n1, n2);
    }

    #[test]
    fn nonce_differs_by_direction() {
        let sid = [0x01, 0x02, 0x03, 0x04];
        let send = build_nonce(&sid, 0, Direction::Send);
        let recv = build_nonce(&sid, 0, Direction::Recv);
        assert_ne!(send, recv);
    }

    #[test]
    fn nonce_differs_by_seq() {
        let sid = [0x01, 0x02, 0x03, 0x04];
        let n1 = build_nonce(&sid, 0, Direction::Send);
        let n2 = build_nonce(&sid, 1, Direction::Send);
        assert_ne!(n1, n2);
    }

    #[test]
    fn nonce_layout_correct() {
        let sid = [0xAA, 0xBB, 0xCC, 0xDD];
        let seq: u32 = 0x00000100;
        let nonce = build_nonce(&sid, seq, Direction::Recv);
        assert_eq!(&nonce[0..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&nonce[4..8], &[0x00, 0x00, 0x01, 0x00]);
        assert_eq!(nonce[8], 1); // Recv
        assert_eq!(&nonce[9..12], &[0, 0, 0]);
    }
}
