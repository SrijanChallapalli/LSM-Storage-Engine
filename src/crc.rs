//! CRC32 (IEEE 802.3) used by every on-disk checksum in the engine.
//!
//! Implemented directly so the crate stays dependency-free for the storage
//! path. The well-known vector `CRC32("123456789") == 0xCBF43926` is covered
//! by a unit test.

/// CRC32 (IEEE 802.3, reflected, polynomial `0xEDB88320`).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            // `mask` is all-ones when the low bit is set, all-zeros otherwise.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Copies a 4-byte slice into an array. The caller guarantees the length.
pub fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut out = [0u8; 4];
    out.copy_from_slice(bytes);
    u32::from_le_bytes(out)
}

/// Copies an 8-byte slice into an array. The caller guarantees the length.
pub fn read_u64_le(bytes: &[u8]) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(bytes);
    u64::from_le_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
