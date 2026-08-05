//! Stage 3 — the on-disk binary record format.
//!
//! Before anything is written to disk, we pin down exactly how a single write is
//! encoded. Every mutation is an [`Operation`] (a put or a delete), and
//! [`encode`] turns one into a self-describing, checksummed byte record that
//! [`decode`] can turn back into the same `Operation`.
//!
//! ## Layout
//!
//! ```text
//! ┌────────────┬─────────────┬────────────┬──────────────┬─────┬───────┐
//! │ checksum   │ record type │ key length │ value length │ key │ value │
//! │ 4 bytes    │ 1 byte      │ 4 bytes    │ 4 bytes      │ var │ var   │
//! └────────────┴─────────────┴────────────┴──────────────┴─────┴───────┘
//! ```
//!
//! - All integers are little-endian.
//! - `record type` is `1` for a put and `2` for a delete.
//! - A delete carries no value, so its `value length` is `0`.
//! - `checksum` is a CRC32 (IEEE) over every byte that follows it, so any bit
//!   flip in the header or payload is detected on decode.
//!
//! `decode` is deliberately strict: it rejects empty input, truncated headers,
//! truncated or oversized key/value regions, trailing bytes, unknown record
//! types, and checksum mismatches. A single record must round-trip exactly.

use crate::engine::{MAX_KEY_SIZE, MAX_VALUE_SIZE};
use crate::error::{Error, Result};

/// A single mutation to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Store `value` under `key`.
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Delete `key`.
    Delete { key: Vec<u8> },
}

const RECORD_TYPE_PUT: u8 = 1;
const RECORD_TYPE_DELETE: u8 = 2;

/// Bytes reserved for the leading CRC32 checksum.
const CHECKSUM_LEN: usize = 4;
/// Bytes for the checksummed header: record type + key length + value length.
const BODY_HEADER_LEN: usize = 1 + 4 + 4;
/// Smallest possible record: checksum + body header, with empty key and value.
const RECORD_HEADER_LEN: usize = CHECKSUM_LEN + BODY_HEADER_LEN;

/// Encodes an [`Operation`] into its byte record.
pub fn encode(operation: &Operation) -> Result<Vec<u8>> {
    let (record_type, key, value): (u8, &[u8], &[u8]) = match operation {
        Operation::Put { key, value } => (RECORD_TYPE_PUT, key.as_slice(), value.as_slice()),
        Operation::Delete { key } => (RECORD_TYPE_DELETE, key.as_slice(), &[]),
    };

    if key.len() > MAX_KEY_SIZE {
        return Err(Error::KeyTooLarge);
    }
    if value.len() > MAX_VALUE_SIZE {
        return Err(Error::ValueTooLarge);
    }

    // The body is everything the checksum covers.
    let mut body = Vec::with_capacity(BODY_HEADER_LEN + key.len() + value.len());
    body.push(record_type);
    body.extend_from_slice(&(key.len() as u32).to_le_bytes());
    body.extend_from_slice(&(value.len() as u32).to_le_bytes());
    body.extend_from_slice(key);
    body.extend_from_slice(value);

    let checksum = crc32(&body);

    let mut record = Vec::with_capacity(CHECKSUM_LEN + body.len());
    record.extend_from_slice(&checksum.to_le_bytes());
    record.extend_from_slice(&body);
    Ok(record)
}

/// Decodes a byte record back into an [`Operation`], validating it fully.
pub fn decode(bytes: &[u8]) -> Result<Operation> {
    // Empty input and truncated headers can't hold a complete record.
    if bytes.len() < RECORD_HEADER_LEN {
        return Err(Error::CorruptedRecord);
    }

    let checksum = u32::from_le_bytes(array4(&bytes[0..CHECKSUM_LEN]));
    let body = &bytes[CHECKSUM_LEN..];

    let record_type = body[0];
    let key_len = u32::from_le_bytes(array4(&body[1..5])) as usize;
    let value_len = u32::from_le_bytes(array4(&body[5..9])) as usize;

    // Reject implausible lengths before trusting them to size the payload.
    if key_len > MAX_KEY_SIZE || value_len > MAX_VALUE_SIZE {
        return Err(Error::CorruptedRecord);
    }

    // The body must be exactly the header plus the declared payload: shorter
    // means a truncated key/value, longer means unexpected trailing bytes.
    let expected_body_len = BODY_HEADER_LEN + key_len + value_len;
    if body.len() != expected_body_len {
        return Err(Error::CorruptedRecord);
    }

    // Only trust the contents once integrity is confirmed.
    if crc32(body) != checksum {
        return Err(Error::InvalidChecksum);
    }

    let key_start = BODY_HEADER_LEN;
    let key_end = key_start + key_len;
    let value_end = key_end + value_len;
    let key = body[key_start..key_end].to_vec();
    let value = body[key_end..value_end].to_vec();

    match record_type {
        RECORD_TYPE_PUT => Ok(Operation::Put { key, value }),
        RECORD_TYPE_DELETE => {
            // A delete must not carry a value.
            if value_len != 0 {
                return Err(Error::CorruptedRecord);
            }
            Ok(Operation::Delete { key })
        }
        _ => Err(Error::CorruptedRecord),
    }
}

/// Copies a 4-byte slice into an array. The caller guarantees the length.
fn array4(bytes: &[u8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    out.copy_from_slice(bytes);
    out
}

/// CRC32 (IEEE 802.3, reflected, polynomial `0xEDB88320`).
///
/// Implemented directly to keep the crate dependency-free at this stage.
fn crc32(bytes: &[u8]) -> u32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_round_trip(operation: Operation) {
        let encoded = encode(&operation).unwrap();
        assert_eq!(decode(&encoded).unwrap(), operation);
    }

    /// Hand-builds a record with a valid checksum for the given (possibly
    /// nonsensical) fields, so decode's structural checks can be exercised.
    fn build_record(record_type: u8, key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut body = vec![record_type];
        body.extend_from_slice(&(key.len() as u32).to_le_bytes());
        body.extend_from_slice(&(value.len() as u32).to_le_bytes());
        body.extend_from_slice(key);
        body.extend_from_slice(value);
        let mut record = crc32(&body).to_le_bytes().to_vec();
        record.extend_from_slice(&body);
        record
    }

    #[test]
    fn put_record_round_trip() {
        assert_round_trip(Operation::Put {
            key: b"name".to_vec(),
            value: b"Srijan".to_vec(),
        });
    }

    #[test]
    fn delete_record_round_trip() {
        assert_round_trip(Operation::Delete {
            key: b"name".to_vec(),
        });
    }

    #[test]
    fn empty_value_round_trip() {
        assert_round_trip(Operation::Put {
            key: b"key".to_vec(),
            value: Vec::new(),
        });
    }

    #[test]
    fn unicode_bytes_round_trip() {
        assert_round_trip(Operation::Put {
            key: "café☕".as_bytes().to_vec(),
            value: "日本語".as_bytes().to_vec(),
        });
    }

    #[test]
    fn binary_bytes_round_trip() {
        assert_round_trip(Operation::Put {
            key: vec![0x00, 0xff, 0x00, 0x01, 0x7f],
            value: vec![0xde, 0xad, 0xbe, 0xef, 0x00],
        });
    }

    #[test]
    fn empty_input_fails() {
        assert!(matches!(decode(&[]), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn truncated_header_fails() {
        let encoded = encode(&Operation::Put {
            key: b"key".to_vec(),
            value: b"val".to_vec(),
        })
        .unwrap();
        assert!(matches!(
            decode(&encoded[..5]),
            Err(Error::CorruptedRecord)
        ));
    }

    #[test]
    fn truncated_payload_fails() {
        let encoded = encode(&Operation::Put {
            key: b"key".to_vec(),
            value: b"val".to_vec(),
        })
        .unwrap();
        let short = &encoded[..encoded.len() - 1];
        assert!(matches!(decode(short), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn extra_trailing_bytes_fail() {
        let mut encoded = encode(&Operation::Put {
            key: b"key".to_vec(),
            value: b"val".to_vec(),
        })
        .unwrap();
        encoded.push(0);
        assert!(matches!(decode(&encoded), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn corrupted_payload_fails_the_checksum() {
        let mut encoded = encode(&Operation::Put {
            key: b"key".to_vec(),
            value: b"val".to_vec(),
        })
        .unwrap();
        // Flip a bit in the value without changing the length.
        let last = encoded.len() - 1;
        encoded[last] ^= 0xff;
        assert!(matches!(decode(&encoded), Err(Error::InvalidChecksum)));
    }

    #[test]
    fn unknown_record_type_fails() {
        let raw = build_record(99, b"key", b"");
        assert!(matches!(decode(&raw), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn delete_with_a_value_fails() {
        let raw = build_record(RECORD_TYPE_DELETE, b"key", b"value");
        assert!(matches!(decode(&raw), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn oversized_declared_length_fails() {
        // A valid checksum over a header that claims an absurd key length.
        let mut body = vec![RECORD_TYPE_PUT];
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        let mut raw = crc32(&body).to_le_bytes().to_vec();
        raw.extend_from_slice(&body);
        assert!(matches!(decode(&raw), Err(Error::CorruptedRecord)));
    }

    #[test]
    fn crc32_matches_known_vector() {
        // The CRC32 of "123456789" is the well-known 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
