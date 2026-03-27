//! Block manager — tracks the lifecycle of FEC blocks on both encoder and decoder sides.

use std::collections::{HashMap, HashSet};

/// Block lifecycle state on the encoder side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBlockState {
    /// Block is currently being built (accumulating source symbols).
    Building,
    /// Block has been finalized and repair generated; awaiting transmission.
    Pending,
    /// All symbols for this block have been sent.
    Sent,
    /// Peer acknowledged receipt / successful decode.
    Acknowledged,
}

/// Block lifecycle state on the decoder side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderBlockState {
    /// Receiving symbols for this block.
    Assembling,
    /// Block successfully decoded.
    Complete,
    /// Block expired (too old, dropped).
    Expired,
}

/// Manages encoder-side block tracking.
pub struct EncoderBlockManager {
    /// Current block ID being built.
    current_id: u8,
    /// State of known blocks.
    blocks: HashMap<u8, EncoderBlockState>,
}

impl EncoderBlockManager {
    pub fn new() -> Self {
        let mut blocks = HashMap::new();
        blocks.insert(0, EncoderBlockState::Building);
        Self {
            current_id: 0,
            blocks,
        }
    }

    /// Get the next block ID (advances the current building block).
    pub fn next_block_id(&mut self) -> u8 {
        let old = self.current_id;
        // Mark old block as pending.
        self.blocks.insert(old, EncoderBlockState::Pending);

        self.current_id = self.current_id.wrapping_add(1);
        self.blocks
            .insert(self.current_id, EncoderBlockState::Building);
        self.current_id
    }

    /// Current block ID being built.
    pub fn current_id(&self) -> u8 {
        self.current_id
    }

    /// Mark a block as fully sent.
    pub fn mark_sent(&mut self, block_id: u8) {
        self.blocks.insert(block_id, EncoderBlockState::Sent);
    }

    /// Mark a block as acknowledged by the peer.
    pub fn mark_acknowledged(&mut self, block_id: u8) {
        self.blocks
            .insert(block_id, EncoderBlockState::Acknowledged);
    }

    /// Get the state of a block.
    pub fn state(&self, block_id: u8) -> Option<EncoderBlockState> {
        self.blocks.get(&block_id).copied()
    }

    /// Remove old acknowledged blocks to limit memory.
    pub fn prune_acknowledged(&mut self) {
        self.blocks
            .retain(|_, state| *state != EncoderBlockState::Acknowledged);
    }
}

impl Default for EncoderBlockManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages decoder-side block tracking.
pub struct DecoderBlockManager {
    /// State of known blocks.
    blocks: HashMap<u8, DecoderBlockState>,
    /// Set of completed block IDs.
    completed: HashSet<u8>,
}

impl DecoderBlockManager {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            completed: HashSet::new(),
        }
    }

    /// Register that we are receiving symbols for a block.
    pub fn touch(&mut self, block_id: u8) {
        self.blocks
            .entry(block_id)
            .or_insert(DecoderBlockState::Assembling);
    }

    /// Mark a block as successfully decoded.
    pub fn mark_complete(&mut self, block_id: u8) {
        self.blocks.insert(block_id, DecoderBlockState::Complete);
        self.completed.insert(block_id);
    }

    /// Mark a block as expired.
    pub fn mark_expired(&mut self, block_id: u8) {
        self.blocks.insert(block_id, DecoderBlockState::Expired);
        self.completed.remove(&block_id);
    }

    /// Check if a block has been fully decoded.
    pub fn is_block_complete(&self, block_id: u8) -> bool {
        self.completed.contains(&block_id)
    }

    /// Get the state of a block.
    pub fn state(&self, block_id: u8) -> Option<DecoderBlockState> {
        self.blocks.get(&block_id).copied()
    }

    /// Expire all blocks older than the given block_id (using wrapping distance).
    pub fn expire_before(&mut self, block_id: u8) {
        let to_expire: Vec<u8> = self
            .blocks
            .keys()
            .copied()
            .filter(|&id| {
                let distance = block_id.wrapping_sub(id);
                distance > 0 && distance <= 128
            })
            .collect();

        for id in to_expire {
            self.blocks.insert(id, DecoderBlockState::Expired);
            self.completed.remove(&id);
        }
    }

    /// Remove expired blocks entirely to free memory.
    pub fn prune_expired(&mut self) {
        self.blocks
            .retain(|_, state| *state != DecoderBlockState::Expired);
    }
}

impl Default for DecoderBlockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_block_lifecycle() {
        let mut mgr = EncoderBlockManager::new();
        assert_eq!(mgr.current_id(), 0);
        assert_eq!(mgr.state(0), Some(EncoderBlockState::Building));

        let next = mgr.next_block_id();
        assert_eq!(next, 1);
        assert_eq!(mgr.state(0), Some(EncoderBlockState::Pending));
        assert_eq!(mgr.state(1), Some(EncoderBlockState::Building));

        mgr.mark_sent(0);
        assert_eq!(mgr.state(0), Some(EncoderBlockState::Sent));

        mgr.mark_acknowledged(0);
        assert_eq!(mgr.state(0), Some(EncoderBlockState::Acknowledged));

        mgr.prune_acknowledged();
        assert_eq!(mgr.state(0), None);
    }

    #[test]
    fn decoder_block_lifecycle() {
        let mut mgr = DecoderBlockManager::new();

        mgr.touch(0);
        assert_eq!(mgr.state(0), Some(DecoderBlockState::Assembling));
        assert!(!mgr.is_block_complete(0));

        mgr.mark_complete(0);
        assert!(mgr.is_block_complete(0));
        assert_eq!(mgr.state(0), Some(DecoderBlockState::Complete));
    }

    #[test]
    fn decoder_expire_before() {
        let mut mgr = DecoderBlockManager::new();
        for i in 0..5u8 {
            mgr.touch(i);
        }
        mgr.mark_complete(1);

        mgr.expire_before(3);

        // Blocks 0, 1, 2 should be expired
        assert_eq!(mgr.state(0), Some(DecoderBlockState::Expired));
        assert_eq!(mgr.state(1), Some(DecoderBlockState::Expired));
        assert_eq!(mgr.state(2), Some(DecoderBlockState::Expired));
        // Block 3 and 4 untouched
        assert_eq!(mgr.state(3), Some(DecoderBlockState::Assembling));
        assert_eq!(mgr.state(4), Some(DecoderBlockState::Assembling));

        assert!(!mgr.is_block_complete(1)); // was complete but now expired

        mgr.prune_expired();
        assert_eq!(mgr.state(0), None);
    }

    #[test]
    fn next_block_id_wraps() {
        let mut mgr = EncoderBlockManager::new();
        // Start at 0, advance to 255 then wrap
        for _ in 0..255 {
            mgr.next_block_id();
        }
        assert_eq!(mgr.current_id(), 255);
        let next = mgr.next_block_id();
        assert_eq!(next, 0);
    }
}
