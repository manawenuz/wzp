//! Temporal interleaving — spreads symbols from multiple FEC blocks across
//! transmission slots so that burst losses damage multiple blocks lightly
//! rather than one block fatally.

/// A symbol ready for transmission: (block_id, symbol_index, is_repair, data).
pub type Symbol = (u8, u8, bool, Vec<u8>);

/// Temporal interleaver that mixes symbols across multiple FEC blocks.
pub struct Interleaver {
    /// Number of blocks to interleave across (spread depth).
    depth: usize,
}

impl Interleaver {
    /// Create an interleaver with the given spread depth.
    pub fn new(depth: usize) -> Self {
        Self { depth }
    }

    /// Create with default depth of 3 blocks.
    pub fn with_default_depth() -> Self {
        Self::new(3)
    }

    /// Spread depth (number of blocks mixed together).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Interleave symbols from multiple blocks into a single transmission sequence.
    ///
    /// Each inner `Vec` contains the symbols for one FEC block.
    /// The output interleaves them in round-robin fashion: symbol 0 from block A,
    /// symbol 0 from block B, symbol 0 from block C, symbol 1 from block A, etc.
    ///
    /// This ensures a burst loss of N consecutive packets only destroys at most
    /// ceil(N/depth) symbols from any single block.
    pub fn interleave(&self, blocks: &[Vec<Symbol>]) -> Vec<Symbol> {
        if blocks.is_empty() {
            return Vec::new();
        }

        let max_len = blocks.iter().map(|b| b.len()).max().unwrap_or(0);
        let mut output = Vec::with_capacity(blocks.iter().map(|b| b.len()).sum());

        for slot in 0..max_len {
            for block in blocks {
                if slot < block.len() {
                    output.push(block[slot].clone());
                }
            }
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_mixes_blocks() {
        let interleaver = Interleaver::with_default_depth();

        let block_a: Vec<Symbol> = (0..3)
            .map(|i| (0u8, i as u8, false, vec![0xA0 + i as u8]))
            .collect();
        let block_b: Vec<Symbol> = (0..3)
            .map(|i| (1u8, i as u8, false, vec![0xB0 + i as u8]))
            .collect();
        let block_c: Vec<Symbol> = (0..3)
            .map(|i| (2u8, i as u8, false, vec![0xC0 + i as u8]))
            .collect();

        let result = interleaver.interleave(&[block_a, block_b, block_c]);

        assert_eq!(result.len(), 9);

        // Round-robin: A0, B0, C0, A1, B1, C1, A2, B2, C2
        assert_eq!(result[0].0, 0); // block A
        assert_eq!(result[1].0, 1); // block B
        assert_eq!(result[2].0, 2); // block C
        assert_eq!(result[3].0, 0); // block A
        assert_eq!(result[4].0, 1); // block B
        assert_eq!(result[5].0, 2); // block C

        // Verify symbol indices cycle correctly
        assert_eq!(result[0].1, 0); // sym 0 from A
        assert_eq!(result[3].1, 1); // sym 1 from A
        assert_eq!(result[6].1, 2); // sym 2 from A
    }

    #[test]
    fn interleave_unequal_lengths() {
        let interleaver = Interleaver::new(2);

        let block_a: Vec<Symbol> = (0..3)
            .map(|i| (0u8, i as u8, false, vec![0xA0 + i as u8]))
            .collect();
        let block_b: Vec<Symbol> = (0..1)
            .map(|i| (1u8, i as u8, false, vec![0xB0 + i as u8]))
            .collect();

        let result = interleaver.interleave(&[block_a, block_b]);

        // A0, B0, A1, A2
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].0, 0); // A0
        assert_eq!(result[1].0, 1); // B0
        assert_eq!(result[2].0, 0); // A1
        assert_eq!(result[3].0, 0); // A2
    }

    #[test]
    fn interleave_empty() {
        let interleaver = Interleaver::with_default_depth();
        let result = interleaver.interleave(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn burst_loss_distributed() {
        // With 3-block interleaving and a burst of 6 consecutive losses,
        // each block loses at most 2 symbols.
        let interleaver = Interleaver::new(3);

        let blocks: Vec<Vec<Symbol>> = (0..3)
            .map(|b| {
                (0..6)
                    .map(|i| (b as u8, i as u8, false, vec![b as u8; 10]))
                    .collect()
            })
            .collect();

        let interleaved = interleaver.interleave(&blocks);
        assert_eq!(interleaved.len(), 18);

        // Simulate burst loss of 6 consecutive packets starting at index 3
        let lost_range = 3..9;
        let mut losses_per_block = [0u32; 3];
        for idx in lost_range {
            let block_id = interleaved[idx].0 as usize;
            losses_per_block[block_id] += 1;
        }

        // Each block should lose exactly 2 (6 losses / 3 blocks)
        for &loss in &losses_per_block {
            assert_eq!(loss, 2, "Each block should lose at most 2 symbols from a burst of 6");
        }
    }
}
