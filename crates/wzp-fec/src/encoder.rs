//! RaptorQ FEC encoder — accumulates source symbols into blocks and generates repair symbols.

use raptorq::{EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockEncoder};
use wzp_proto::FecEncoder;
use wzp_proto::error::FecError;

/// Maximum symbol size in bytes. Audio frames are typically < 200 bytes,
/// but we pad to a uniform size within a block.
/// Each symbol carries a 2-byte length prefix so recovered frames can be trimmed.
const DEFAULT_MAX_SYMBOL_SIZE: u16 = 256;

/// Length prefix size (u16 little-endian).
const LEN_PREFIX: usize = 2;

/// RaptorQ-based FEC encoder that groups audio frames into blocks
/// and generates fountain-code repair symbols.
pub struct RaptorQFecEncoder {
    /// Current block ID (wraps at u8).
    block_id: u8,
    /// Maximum source symbols per block.
    frames_per_block: usize,
    /// Accumulated source symbols for the current block.
    source_symbols: Vec<Vec<u8>>,
    /// Symbol size used for encoding (all symbols padded to this size).
    symbol_size: u16,
    /// True if at least one source symbol in the current block is a keyframe.
    has_keyframe: bool,
    /// Repair ratio to use when the block contains a keyframe.
    /// If zero, the nominal ratio passed to [`generate_repair`] is used.
    keyframe_ratio: f32,
}

impl RaptorQFecEncoder {
    /// Create a new encoder.
    ///
    /// * `frames_per_block` — number of source symbols per FEC block.
    /// * `symbol_size` — max byte length of any single source symbol (frames are zero-padded).
    pub fn new(frames_per_block: usize, symbol_size: u16) -> Self {
        Self {
            block_id: 0,
            frames_per_block,
            source_symbols: Vec::with_capacity(frames_per_block),
            symbol_size,
            has_keyframe: false,
            keyframe_ratio: 0.0,
        }
    }

    /// Set the repair ratio to use for blocks that contain at least one
    /// keyframe source symbol.
    ///
    /// When `keyframe_ratio > 0.0` and [`has_keyframe`](Self::has_keyframe)
    /// is true, [`generate_repair`](FecEncoder::generate_repair) uses this
    /// ratio instead of the nominal ratio passed by the caller.
    pub fn set_keyframe_ratio(&mut self, ratio: f32) {
        self.keyframe_ratio = ratio.max(0.0);
    }

    /// Returns true if the current block contains a keyframe source symbol.
    pub fn has_keyframe(&self) -> bool {
        self.has_keyframe
    }

    /// Create with default symbol size (256 bytes).
    pub fn with_defaults(frames_per_block: usize) -> Self {
        Self::new(frames_per_block, DEFAULT_MAX_SYMBOL_SIZE)
    }

    /// Build a contiguous data buffer from the accumulated source symbols,
    /// each prefixed with a 2-byte length and zero-padded to `symbol_size`.
    fn build_block_data(&self) -> Vec<u8> {
        let ss = self.symbol_size as usize;
        let mut data = vec![0u8; self.source_symbols.len() * ss];
        for (i, sym) in self.source_symbols.iter().enumerate() {
            let max_payload = ss - LEN_PREFIX;
            let payload_len = sym.len().min(max_payload);
            let offset = i * ss;
            // Write 2-byte little-endian length prefix.
            data[offset..offset + LEN_PREFIX].copy_from_slice(&(payload_len as u16).to_le_bytes());
            // Write payload after prefix.
            data[offset + LEN_PREFIX..offset + LEN_PREFIX + payload_len]
                .copy_from_slice(&sym[..payload_len]);
        }
        data
    }
}

impl FecEncoder for RaptorQFecEncoder {
    fn add_source_symbol(&mut self, data: &[u8]) -> Result<(), FecError> {
        if self.source_symbols.len() >= self.frames_per_block {
            return Err(FecError::BlockFull {
                max: self.frames_per_block,
            });
        }
        self.source_symbols.push(data.to_vec());
        Ok(())
    }

    fn add_source_symbol_with_keyframe(
        &mut self,
        data: &[u8],
        is_keyframe: bool,
    ) -> Result<(), FecError> {
        self.add_source_symbol(data)?;
        if is_keyframe {
            self.has_keyframe = true;
        }
        Ok(())
    }

    fn generate_repair(&mut self, ratio: f32) -> Result<Vec<(u16, Vec<u8>)>, FecError> {
        if self.source_symbols.is_empty() {
            return Ok(vec![]);
        }

        let effective_ratio = if self.has_keyframe && self.keyframe_ratio > 0.0 {
            self.keyframe_ratio
        } else {
            ratio
        };

        let block_data = self.build_block_data();
        let config =
            ObjectTransmissionInformation::with_defaults(block_data.len() as u64, self.symbol_size);
        let encoder = SourceBlockEncoder::new(self.block_id, &config, &block_data);

        let num_source = self.source_symbols.len() as u32;
        let num_repair = ((num_source as f32) * effective_ratio).ceil() as u32;
        if num_repair == 0 {
            return Ok(vec![]);
        }

        // Generate repair packets starting from offset 0 (ESIs begin at num_source).
        let repair_packets: Vec<EncodingPacket> = encoder.repair_packets(0, num_repair);

        let result: Vec<(u16, Vec<u8>)> = repair_packets
            .into_iter()
            .enumerate()
            .map(|(i, pkt): (usize, EncodingPacket)| {
                let idx = (num_source as u16).wrapping_add(i as u16);
                (idx, pkt.data().to_vec())
            })
            .collect();

        Ok(result)
    }

    fn finalize_block(&mut self) -> Result<u8, FecError> {
        let completed = self.block_id;
        self.block_id = self.block_id.wrapping_add(1);
        self.source_symbols.clear();
        self.has_keyframe = false;
        Ok(completed)
    }

    fn current_block_id(&self) -> u8 {
        self.block_id
    }

    fn current_block_size(&self) -> usize {
        self.source_symbols.len()
    }
}

/// Build a length-prefixed, padded block data buffer from raw symbols.
/// This matches what the encoder produces internally.
fn build_prefixed_block_data(symbols: &[Vec<u8>], symbol_size: u16) -> Vec<u8> {
    let ss = symbol_size as usize;
    let mut data = vec![0u8; symbols.len() * ss];
    for (i, sym) in symbols.iter().enumerate() {
        let max_payload = ss - LEN_PREFIX;
        let payload_len = sym.len().min(max_payload);
        let offset = i * ss;
        data[offset..offset + LEN_PREFIX].copy_from_slice(&(payload_len as u16).to_le_bytes());
        data[offset + LEN_PREFIX..offset + LEN_PREFIX + payload_len]
            .copy_from_slice(&sym[..payload_len]);
    }
    data
}

/// Helper: build source `EncodingPacket`s for a given block. Useful for
/// the decoder tests and interleaving.
pub fn source_packets_for_block(
    block_id: u8,
    symbols: &[Vec<u8>],
    symbol_size: u16,
) -> Vec<EncodingPacket> {
    let ss = symbol_size as usize;
    let data = build_prefixed_block_data(symbols, symbol_size);
    (0..symbols.len())
        .map(|i| {
            let offset = i * ss;
            let sym_data = data[offset..offset + ss].to_vec();
            EncodingPacket::new(PayloadId::new(block_id, i as u32), sym_data)
        })
        .collect()
}

/// Helper: generate repair packets for the given source symbols.
pub fn repair_packets_for_block(
    block_id: u8,
    symbols: &[Vec<u8>],
    symbol_size: u16,
    ratio: f32,
) -> Vec<EncodingPacket> {
    let data = build_prefixed_block_data(symbols, symbol_size);
    let config = ObjectTransmissionInformation::with_defaults(data.len() as u64, symbol_size);
    let encoder = SourceBlockEncoder::new(block_id, &config, &data);
    let num_source = symbols.len() as u32;
    let num_repair = ((num_source as f32) * ratio).ceil() as u32;
    encoder.repair_packets(0, num_repair)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_symbols_and_finalize() {
        let mut enc = RaptorQFecEncoder::with_defaults(5);
        assert_eq!(enc.current_block_id(), 0);
        assert_eq!(enc.current_block_size(), 0);

        for i in 0..5 {
            enc.add_source_symbol(&[i as u8; 100]).unwrap();
        }
        assert_eq!(enc.current_block_size(), 5);

        // Block full
        assert!(enc.add_source_symbol(&[0u8; 100]).is_err());

        let repair = enc.generate_repair(0.5).unwrap();
        assert!(!repair.is_empty());
        // 5 source * 0.5 = 3 repair (ceil)
        assert_eq!(repair.len(), 3);

        let id = enc.finalize_block().unwrap();
        assert_eq!(id, 0);
        assert_eq!(enc.current_block_id(), 1);
        assert_eq!(enc.current_block_size(), 0);
    }

    #[test]
    fn block_id_wraps() {
        let mut enc = RaptorQFecEncoder::with_defaults(1);
        for expected in 0..=255u8 {
            assert_eq!(enc.current_block_id(), expected);
            enc.add_source_symbol(&[expected; 10]).unwrap();
            enc.finalize_block().unwrap();
        }
        // After 256 blocks, wraps back to 0
        assert_eq!(enc.current_block_id(), 0);
    }

    #[test]
    fn keyframe_boost_uses_higher_ratio() {
        // Non-keyframe block with nominal ratio 0.2 → ceil(5 * 0.2) = 1 repair.
        let mut enc_normal = RaptorQFecEncoder::with_defaults(5);
        enc_normal.set_keyframe_ratio(0.8);
        for i in 0..5 {
            enc_normal
                .add_source_symbol_with_keyframe(&[i as u8; 100], false)
                .unwrap();
        }
        let normal_repair = enc_normal.generate_repair(0.2).unwrap();
        assert_eq!(normal_repair.len(), 1);

        // Keyframe block with same nominal ratio but boost to 0.8 → ceil(5 * 0.8) = 4 repairs.
        let mut enc_key = RaptorQFecEncoder::with_defaults(5);
        enc_key.set_keyframe_ratio(0.8);
        for i in 0..5 {
            enc_key
                .add_source_symbol_with_keyframe(&[i as u8; 100], i == 2)
                .unwrap();
        }
        let keyframe_repair = enc_key.generate_repair(0.2).unwrap();
        assert_eq!(keyframe_repair.len(), 4);
    }

    #[test]
    fn non_keyframe_block_uses_nominal_ratio() {
        let mut enc = RaptorQFecEncoder::with_defaults(5);
        enc.set_keyframe_ratio(0.8);

        for i in 0..5 {
            enc.add_source_symbol_with_keyframe(&[i as u8; 100], false)
                .unwrap();
        }

        let repair = enc.generate_repair(0.2).unwrap();
        assert_eq!(repair.len(), 1); // ceil(5 * 0.2) = 1
    }

    #[test]
    fn finalize_clears_keyframe_flag() {
        let mut enc = RaptorQFecEncoder::with_defaults(2);
        enc.add_source_symbol_with_keyframe(&[0u8; 10], true)
            .unwrap();
        assert!(enc.has_keyframe());

        enc.finalize_block().unwrap();
        assert!(!enc.has_keyframe());
    }
}
