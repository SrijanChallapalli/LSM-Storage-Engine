//! Stage 13 — Bloom filters.
//!
//! Each SSTable stores one filter so a lookup can skip the file when the key
//! is definitely absent. False positives are allowed; false negatives are not.

use crate::crc::{crc32, read_u32_le};
use crate::error::{Error, Result};

/// A compact probabilistic set of keys.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u8>,
    hash_count: u32,
}

impl BloomFilter {
    /// Builds an empty filter sized for `expected_items` at `false_positive_rate`.
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        let n = expected_items.max(1) as f64;
        let p = false_positive_rate.clamp(0.000_1, 0.5);
        // m = -n ln(p) / (ln 2)^2
        let bit_count = ((-n * p.ln()) / (std::f64::consts::LN_2.powi(2)))
            .ceil()
            .max(64.0) as usize;
        // k = (m/n) ln 2
        let hash_count =
            (((bit_count as f64 / n) * std::f64::consts::LN_2).round() as u32).clamp(1, 16);
        let byte_count = bit_count.div_ceil(8);
        Self {
            bits: vec![0; byte_count],
            hash_count,
        }
    }

    /// An empty filter that reports every key as absent.
    pub fn empty() -> Self {
        Self {
            bits: vec![0; 8],
            hash_count: 1,
        }
    }

    /// Records `key` in the filter.
    pub fn insert(&mut self, key: &[u8]) {
        let bit_count = self.bits.len() * 8;
        let (h1, h2) = hash_pair(key);
        for i in 0..self.hash_count {
            let idx = h1.wrapping_add(u64::from(i).wrapping_mul(h2)) as usize % bit_count;
            self.bits[idx / 8] |= 1 << (idx % 8);
        }
    }

    /// Returns `true` if `key` might be present. Never returns `false` for a
    /// key that was inserted.
    pub fn might_contain(&self, key: &[u8]) -> bool {
        if self.bits.is_empty() {
            return false;
        }
        let bit_count = self.bits.len() * 8;
        let (h1, h2) = hash_pair(key);
        for i in 0..self.hash_count {
            let idx = h1.wrapping_add(u64::from(i).wrapping_mul(h2)) as usize % bit_count;
            if self.bits[idx / 8] & (1 << (idx % 8)) == 0 {
                return false;
            }
        }
        true
    }

    /// Serializes the filter: `hash_count u32 | bit_len u32 | bits`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.bits.len());
        out.extend_from_slice(&self.hash_count.to_le_bytes());
        out.extend_from_slice(&(self.bits.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.bits);
        let checksum = crc32(&out);
        out.extend_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Restores a filter written by [`encode`](Self::encode).
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 12 {
            return Err(Error::CorruptedRecord);
        }
        let hash_count = read_u32_le(&bytes[0..4]);
        let bit_len = read_u32_le(&bytes[4..8]) as usize;
        if hash_count == 0 || hash_count > 16 {
            return Err(Error::CorruptedRecord);
        }
        let expected = 8 + bit_len + 4;
        if bytes.len() != expected {
            return Err(Error::CorruptedRecord);
        }
        let body = &bytes[..8 + bit_len];
        let checksum = read_u32_le(&bytes[8 + bit_len..]);
        if crc32(body) != checksum {
            return Err(Error::InvalidChecksum);
        }
        Ok(Self {
            bits: bytes[8..8 + bit_len].to_vec(),
            hash_count,
        })
    }
}

fn hash_pair(key: &[u8]) -> (u64, u64) {
    (
        fnv1a_64(key),
        splitmix(fnv1a_64(key) ^ 0x9E37_79B9_7F4A_7C15),
    )
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserted_keys_are_never_reported_absent() {
        let mut filter = BloomFilter::new(64, 0.01);
        let keys: Vec<Vec<u8>> = (0..64u32)
            .map(|i| format!("key-{i:04}").into_bytes())
            .collect();
        for key in &keys {
            filter.insert(key);
        }
        for key in &keys {
            assert!(filter.might_contain(key), "false negative for {key:?}");
        }
    }

    #[test]
    fn missing_keys_are_usually_reported_absent() {
        let mut filter = BloomFilter::new(64, 0.01);
        for i in 0..64u32 {
            filter.insert(format!("key-{i:04}").as_bytes());
        }
        let mut false_positives = 0;
        for i in 1000..2000u32 {
            if filter.might_contain(format!("miss-{i:04}").as_bytes()) {
                false_positives += 1;
            }
        }
        // 1% target over 1000 keys should be nowhere near half.
        assert!(
            false_positives < 100,
            "too many false positives: {false_positives}"
        );
    }

    #[test]
    fn encode_and_decode_preserve_behavior() {
        let mut filter = BloomFilter::new(16, 0.01);
        filter.insert(b"alpha");
        filter.insert(b"beta");
        let restored = BloomFilter::decode(&filter.encode()).unwrap();
        assert!(restored.might_contain(b"alpha"));
        assert!(restored.might_contain(b"beta"));
        assert!(!restored.might_contain(b"zzz-missing"));
    }

    #[test]
    fn empty_filter_works() {
        let filter = BloomFilter::empty();
        assert!(!filter.might_contain(b"anything"));
    }

    #[test]
    fn invalid_serialized_filter_is_rejected() {
        assert!(BloomFilter::decode(&[]).is_err());
        assert!(BloomFilter::decode(&[1, 2, 3]).is_err());
        let mut bad = BloomFilter::new(8, 0.01).encode();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(matches!(
            BloomFilter::decode(&bad),
            Err(Error::InvalidChecksum)
        ));
    }
}
