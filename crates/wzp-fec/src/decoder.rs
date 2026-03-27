//! RaptorQ FEC decoder — reassembles source blocks from received source and repair symbols.

use std::collections::HashMap;

use raptorq::{EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockDecoder};
use wzp_proto::error::FecError;
use wzp_proto::FecDecoder;

/// Length prefix size (u16 little-endian), must match encoder.
const LEN_PREFIX: usize = 2;

/// State for one in-flight block being decoded.
struct BlockState {
    /// Number of source symbols expected.
    num_source_symbols: Option<usize>,
    /// Collected encoding packets (source + repair).
    packets: Vec<EncodingPacket>,
    /// Symbol size in bytes.
    symbol_size: u16,
    /// Whether decoding has already succeeded for this block.
    decoded: bool,
    /// Cached decoded result.
    result: Option<Vec<Vec<u8>>>,
}

/// RaptorQ-based FEC decoder that handles multiple concurrent blocks.
pub struct RaptorQFecDecoder {
    /// Per-block decoder state, keyed by block_id.
    blocks: HashMap<u8, BlockState>,
    /// Symbol size (must match encoder).
    symbol_size: u16,
    /// Number of source symbols per block (from encoder config).
    frames_per_block: usize,
}

impl RaptorQFecDecoder {
    /// Create a new decoder.
    ///
    /// * `frames_per_block` — expected number of source symbols per block.
    /// * `symbol_size` — must match the encoder's symbol size.
    pub fn new(frames_per_block: usize, symbol_size: u16) -> Self {
        Self {
            blocks: HashMap::new(),
            symbol_size,
            frames_per_block,
        }
    }

    /// Create with default symbol size (256).
    pub fn with_defaults(frames_per_block: usize) -> Self {
        Self::new(frames_per_block, 256)
    }

    fn get_or_create_block(&mut self, block_id: u8) -> &mut BlockState {
        self.blocks.entry(block_id).or_insert_with(|| BlockState {
            num_source_symbols: Some(self.frames_per_block),
            packets: Vec::new(),
            symbol_size: self.symbol_size,
            decoded: false,
            result: None,
        })
    }
}

impl FecDecoder for RaptorQFecDecoder {
    fn add_symbol(
        &mut self,
        block_id: u8,
        symbol_index: u8,
        _is_repair: bool,
        data: &[u8],
    ) -> Result<(), FecError> {
        let ss = self.symbol_size as usize;
        let block = self.get_or_create_block(block_id);

        if block.decoded {
            // Already decoded, ignore additional symbols.
            return Ok(());
        }

        // Data should already be at symbol_size (length-prefixed and padded by the encoder).
        // But if caller sends raw data, pad it.
        let mut padded = vec![0u8; ss];
        let len = data.len().min(ss);
        padded[..len].copy_from_slice(&data[..len]);

        let esi = symbol_index as u32;
        let packet = EncodingPacket::new(PayloadId::new(block_id, esi), padded);
        block.packets.push(packet);

        Ok(())
    }

    fn try_decode(&mut self, block_id: u8) -> Result<Option<Vec<Vec<u8>>>, FecError> {
        let frames_per_block = self.frames_per_block;
        let block = match self.blocks.get_mut(&block_id) {
            Some(b) => b,
            None => return Ok(None),
        };

        if let Some(ref result) = block.result {
            return Ok(Some(result.clone()));
        }

        let num_source = block.num_source_symbols.unwrap_or(frames_per_block);
        let block_length = (num_source as u64) * (block.symbol_size as u64);

        let config = ObjectTransmissionInformation::with_defaults(block_length, block.symbol_size);
        let mut decoder = SourceBlockDecoder::new(block_id, &config, block_length);

        let decoded = decoder.decode(block.packets.clone());

        match decoded {
            Some(data) => {
                // Split decoded data into individual frames using the length prefix.
                let ss = block.symbol_size as usize;
                let mut frames = Vec::with_capacity(num_source);
                for i in 0..num_source {
                    let offset = i * ss;
                    if offset + LEN_PREFIX > data.len() {
                        frames.push(Vec::new());
                        continue;
                    }
                    let payload_len = u16::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                    ]) as usize;
                    let payload_start = offset + LEN_PREFIX;
                    let payload_end = (payload_start + payload_len).min(data.len());
                    frames.push(data[payload_start..payload_end].to_vec());
                }

                let block = self.blocks.get_mut(&block_id).unwrap();
                block.decoded = true;
                block.result = Some(frames.clone());
                Ok(Some(frames))
            }
            None => Ok(None),
        }
    }

    fn expire_before(&mut self, block_id: u8) {
        // Remove blocks with IDs "older" than block_id.
        // With wrapping u8 IDs, we consider a block old if its distance
        // (in the forward direction) to block_id is > 128.
        self.blocks.retain(|&id, _| {
            let distance = block_id.wrapping_sub(id);
            // If distance is 0 or > 128, the block is current or "ahead" — keep it.
            // If distance is 1..=128, the block is behind — remove it.
            distance == 0 || distance > 128
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{repair_packets_for_block, source_packets_for_block};

    const SYMBOL_SIZE: u16 = 256;
    const FRAMES_PER_BLOCK: usize = 5;

    /// Helper: create test source symbols.
    fn make_source_symbols(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                let val = (i as u8).wrapping_mul(37).wrapping_add(7);
                vec![val; 100]
            })
            .collect()
    }

    #[test]
    fn decode_with_all_source_symbols() {
        let symbols = make_source_symbols(FRAMES_PER_BLOCK);
        let source_pkts = source_packets_for_block(0, &symbols, SYMBOL_SIZE);
        let mut decoder = RaptorQFecDecoder::new(FRAMES_PER_BLOCK, SYMBOL_SIZE);

        // Feed all source symbols (using the length-prefixed padded data).
        for (i, pkt) in source_pkts.iter().enumerate() {
            decoder
                .add_symbol(0, i as u8, false, pkt.data())
                .unwrap();
        }

        let result = decoder.try_decode(0).unwrap();
        assert!(result.is_some());
        let frames = result.unwrap();
        assert_eq!(frames.len(), FRAMES_PER_BLOCK);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame, &symbols[i]);
        }
    }

    /// Test FEC recovery using raptorq directly, validating our encoding pipeline.
    fn run_loss_test(num_frames: usize, repair_ratio: f32, drop_fraction: f32) {
        use rand::seq::SliceRandom;

        let symbols = make_source_symbols(num_frames);
        let source_pkts = source_packets_for_block(0, &symbols, SYMBOL_SIZE);
        let repair_pkts = repair_packets_for_block(0, &symbols, SYMBOL_SIZE, repair_ratio);

        let mut all: Vec<EncodingPacket> = Vec::new();
        all.extend(source_pkts);
        all.extend(repair_pkts);

        let mut rng = rand::thread_rng();
        all.shuffle(&mut rng);
        let keep = ((all.len() as f32) * (1.0 - drop_fraction)).ceil() as usize;
        all.truncate(keep);

        let block_len = (num_frames as u64) * (SYMBOL_SIZE as u64);
        let config = ObjectTransmissionInformation::new(block_len, SYMBOL_SIZE, 1, 1, 1);
        let mut dec = SourceBlockDecoder::new(0, &config, block_len);
        let decoded = dec.decode(all);
        assert!(decoded.is_some(), "Should recover with {:.0}% loss", drop_fraction * 100.0);

        let data = decoded.unwrap();
        let ss = SYMBOL_SIZE as usize;
        for i in 0..num_frames {
            let off = i * ss;
            let plen = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            assert_eq!(&data[off + 2..off + 2 + plen], &symbols[i][..], "Frame {i}");
        }
    }

    #[test]
    fn decode_with_30pct_loss() { run_loss_test(FRAMES_PER_BLOCK, 0.5, 0.3); }

    #[test]
    fn decode_with_50pct_loss() { run_loss_test(FRAMES_PER_BLOCK, 1.0, 0.5); }

    #[test]
    fn decode_with_70pct_source_loss_heavy_repair() { run_loss_test(8, 2.0, 0.5); }

    #[test]
    fn expire_removes_old_blocks() {
        let mut decoder = RaptorQFecDecoder::new(FRAMES_PER_BLOCK, SYMBOL_SIZE);

        // Add symbols to blocks 0, 1, 2
        for block_id in 0..3u8 {
            decoder
                .add_symbol(block_id, 0, false, &[block_id; 50])
                .unwrap();
        }

        assert_eq!(decoder.blocks.len(), 3);

        // Expire before block 2 — should remove blocks 0 and 1
        decoder.expire_before(2);
        assert!(!decoder.blocks.contains_key(&0));
        assert!(!decoder.blocks.contains_key(&1));
        assert!(decoder.blocks.contains_key(&2));
    }

    #[test]
    fn concurrent_blocks() {
        let symbols_a = make_source_symbols(FRAMES_PER_BLOCK);
        let symbols_b: Vec<Vec<u8>> = (0..FRAMES_PER_BLOCK)
            .map(|i| vec![(i as u8).wrapping_add(100); 80])
            .collect();

        let pkts_a = source_packets_for_block(0, &symbols_a, SYMBOL_SIZE);
        let pkts_b = source_packets_for_block(1, &symbols_b, SYMBOL_SIZE);

        let mut decoder = RaptorQFecDecoder::new(FRAMES_PER_BLOCK, SYMBOL_SIZE);

        // Interleave symbols from block 0 and block 1
        for i in 0..FRAMES_PER_BLOCK {
            decoder
                .add_symbol(0, i as u8, false, pkts_a[i].data())
                .unwrap();
            decoder
                .add_symbol(1, i as u8, false, pkts_b[i].data())
                .unwrap();
        }

        let result_a = decoder.try_decode(0).unwrap().unwrap();
        let result_b = decoder.try_decode(1).unwrap().unwrap();

        for (i, frame) in result_a.iter().enumerate() {
            assert_eq!(frame, &symbols_a[i]);
        }
        for (i, frame) in result_b.iter().enumerate() {
            assert_eq!(frame, &symbols_b[i]);
        }
    }
}
