//! Chunk hashing, the manifest root, and chunk bitmaps.
//!
//! A file of `size` bytes is split into `ceil(size / chunk_size)` chunks
//! (the last one may be short; a zero-byte file has zero chunks). Each chunk
//! is hashed with BLAKE3. The *manifest* is the concatenation of those
//! 32-byte hashes, and the root advertised in `FileOffer.blake3_root` is
//!
//! ```text
//! root = BLAKE3("wzp-files/v1/manifest" || size u64 LE || chunk_size u32 LE || hash_0 || ... || hash_{n-1})
//! ```
//!
//! The receiver gets the manifest right after it accepts, checks it against
//! the root, and from then on verifies every chunk as it arrives.

use std::io::Read;
use std::path::Path;

/// Number of chunks for a file.
pub fn chunk_count(size: u64, chunk_size: u32) -> u64 {
    size.div_ceil(chunk_size as u64)
}

/// Byte length of chunk `idx`.
pub fn chunk_len(size: u64, chunk_size: u32, idx: u32) -> usize {
    let start = idx as u64 * chunk_size as u64;
    (size.saturating_sub(start)).min(chunk_size as u64) as usize
}

pub fn hash_chunk(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

pub fn manifest_root(size: u64, chunk_size: u32, hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"wzp-files/v1/manifest");
    h.update(&size.to_le_bytes());
    h.update(&chunk_size.to_le_bytes());
    for c in hashes {
        h.update(c);
    }
    *h.finalize().as_bytes()
}

/// Hash a file chunk by chunk with a single `chunk_size` buffer (blocking).
pub fn hash_file(path: &Path, chunk_size: u32) -> std::io::Result<(u64, Vec<[u8; 32]>)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let n = chunk_count(size, chunk_size);
    let mut hashes = Vec::with_capacity(n as usize);
    let mut buf = vec![0u8; chunk_size as usize];
    for i in 0..n {
        let len = chunk_len(size, chunk_size, i as u32);
        f.read_exact(&mut buf[..len])?;
        hashes.push(hash_chunk(&buf[..len]));
    }
    Ok((size, hashes))
}

/// One bit per chunk, LSB-first within each byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    bits: Vec<u8>,
    n: u64,
}

impl Bitmap {
    pub fn new(n: u64) -> Self {
        Self {
            bits: vec![0u8; n.div_ceil(8) as usize],
            n,
        }
    }

    /// Parse a peer/disk-supplied bitmap; `None` if its length is wrong.
    pub fn from_bytes(bytes: &[u8], n: u64) -> Option<Self> {
        if bytes.len() as u64 != n.div_ceil(8) {
            return None;
        }
        let mut b = Self {
            bits: bytes.to_vec(),
            n,
        };
        // clear padding bits
        if n % 8 != 0 {
            if let Some(last) = b.bits.last_mut() {
                *last &= (1u8 << (n % 8)) - 1;
            }
        }
        Some(b)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    pub fn len(&self) -> u64 {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn get(&self, i: u64) -> bool {
        i < self.n && self.bits[(i / 8) as usize] & (1 << (i % 8)) != 0
    }

    pub fn set(&mut self, i: u64) {
        if i < self.n {
            self.bits[(i / 8) as usize] |= 1 << (i % 8);
        }
    }

    pub fn count_ones(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }

    pub fn is_full(&self) -> bool {
        self.count_ones() == self.n
    }

    pub fn missing(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.n).filter(|i| !self.get(*i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_math() {
        assert_eq!(chunk_count(0, 4), 0);
        assert_eq!(chunk_count(1, 4), 1);
        assert_eq!(chunk_count(8, 4), 2);
        assert_eq!(chunk_count(9, 4), 3);
        assert_eq!(chunk_len(9, 4, 2), 1);
        assert_eq!(chunk_len(9, 4, 1), 4);
        assert_eq!(chunk_count(10 << 30, 1 << 20), 10240);
    }

    #[test]
    fn bitmap_ops() {
        let mut b = Bitmap::new(10);
        assert_eq!(b.as_bytes().len(), 2);
        b.set(0);
        b.set(9);
        assert!(b.get(0) && b.get(9) && !b.get(5));
        assert_eq!(b.count_ones(), 2);
        assert_eq!(b.missing().count(), 8);
        let c = Bitmap::from_bytes(&[0xff, 0xff], 10).unwrap();
        assert!(c.is_full());
        assert!(Bitmap::from_bytes(&[0xff], 10).is_none());
    }

    #[test]
    fn root_binds_size_and_chunk_size() {
        let h = vec![[1u8; 32]];
        assert_ne!(manifest_root(5, 8, &h), manifest_root(6, 8, &h));
        assert_ne!(manifest_root(5, 8, &h), manifest_root(5, 16, &h));
    }
}
