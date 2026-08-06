//! Stage 4 — the write-ahead log.
//!
//! The WAL is what makes a write survive a crash. Every mutation is encoded with
//! the Stage 3 [`record`](crate::record) format, appended to the log, and synced
//! to disk *before* it is applied to the in-memory memtable. On startup the log
//! is replayed to rebuild the exact last committed state.
//!
//! ## Durable write order
//!
//! 1. Encode the [`Operation`].
//! 2. Append the record to the log.
//! 3. `fsync` the log.
//! 4. Apply the operation to the memtable.
//! 5. Report success.
//!
//! The memtable is never updated before the append and sync succeed, so a
//! visible write is always already on disk.
//!
//! ## Recovery policy
//!
//! Records are read from oldest to newest. A crash can leave a torn record at
//! the very end of the log (a partial header or a payload cut short by the
//! process dying mid-write); that trailing fragment is ignored, since it was
//! never acknowledged. A *complete* record whose checksum fails is treated as
//! corruption and surfaced as an error rather than silently skipped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::engine::{MAX_KEY_SIZE, MAX_VALUE_SIZE};
use crate::error::{Error, Result};
use crate::record::{decode, encode, Operation};

/// Total bytes in a record's fixed header: checksum + type + key len + value len.
const RECORD_HEADER_LEN: usize = 4 + 1 + 4 + 4;
/// Offset of the little-endian key-length field within a record.
const KEY_LEN_OFFSET: usize = 5;
/// Offset of the little-endian value-length field within a record.
const VALUE_LEN_OFFSET: usize = 9;

/// An append-only write-ahead log backed by a single file.
pub struct Wal {
    file: File,
    path: PathBuf,
}

impl Wal {
    /// Opens the log at `path`, creating it if it does not exist.
    ///
    /// The file is opened for reading and writing so the same handle can both
    /// append new records and replay existing ones.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        Ok(Self { file, path })
    }

    /// The path this log is stored at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Encodes `operation` and appends it to the end of the log.
    ///
    /// Returns the byte offset at which the record begins. Encoding failures
    /// (for example an oversized key) are reported before anything is written,
    /// so a failed append never leaves a partial record behind.
    pub fn append(&mut self, operation: &Operation) -> Result<u64> {
        let bytes = encode(operation)?;
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        Ok(offset)
    }

    /// Flushes and `fsync`s the log so far to durable storage.
    pub fn sync(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Replays the log from the beginning, returning every committed operation
    /// in the order it was written.
    ///
    /// A torn trailing record is ignored; a complete but corrupt record is an
    /// error. See the [module docs](self#recovery-policy).
    pub fn replay(&mut self) -> Result<Vec<Operation>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut contents = Vec::new();
        self.file.read_to_end(&mut contents)?;
        parse_records(&contents)
    }

    /// Empties the log, producing a zero-length file on disk.
    ///
    /// Used after a flush persists the memtable elsewhere, so recovery does not
    /// replay operations that are already durable.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        Ok(())
    }

    /// The current size of the log in bytes.
    pub fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Whether the log currently holds no bytes.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Splits a flat buffer of concatenated records into operations.
///
/// Each record is self-delimiting: its header carries the key and value lengths,
/// which give the record's total size, so we can walk the buffer one record at a
/// time and hand each complete record to [`decode`] for full validation.
fn parse_records(mut data: &[u8]) -> Result<Vec<Operation>> {
    let mut operations = Vec::new();

    loop {
        if data.is_empty() {
            // Clean record boundary — nothing more to read.
            return Ok(operations);
        }
        if data.len() < RECORD_HEADER_LEN {
            // Torn final header: ignore the unacknowledged fragment.
            return Ok(operations);
        }

        let key_len = u32::from_le_bytes(four(&data[KEY_LEN_OFFSET..KEY_LEN_OFFSET + 4])) as usize;
        let value_len =
            u32::from_le_bytes(four(&data[VALUE_LEN_OFFSET..VALUE_LEN_OFFSET + 4])) as usize;

        // A wildly out-of-range length means the header itself is corrupt.
        if key_len > MAX_KEY_SIZE || value_len > MAX_VALUE_SIZE {
            return Err(Error::CorruptedRecord);
        }

        let record_len = RECORD_HEADER_LEN + key_len + value_len;
        if data.len() < record_len {
            // Torn final payload: ignore the unacknowledged fragment.
            return Ok(operations);
        }

        let (record, rest) = data.split_at(record_len);
        // A complete record must decode cleanly; a checksum failure here is
        // mid-log corruption, not a torn tail, so the error propagates.
        operations.push(decode(record)?);
        data = rest;
    }
}

/// Copies a 4-byte slice into an array; the caller guarantees the length.
fn four(bytes: &[u8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    out.copy_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A scratch directory (created on construction) that removes itself on drop.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "lsm-store-wal-{}-{nanos}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        fn wal_path(&self) -> PathBuf {
            self.0.join("wal.log")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put(key: &[u8], value: &[u8]) -> Operation {
        Operation::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn wal_file_is_created() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.wal_path()).unwrap();
        assert!(wal.path().is_file());
        assert!(wal.is_empty().unwrap());
    }

    #[test]
    fn multiple_records_append_and_replay_in_order() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        wal.append(&put(b"a", b"1")).unwrap();
        wal.append(&put(b"b", b"2")).unwrap();
        wal.append(&Operation::Delete { key: b"a".to_vec() }).unwrap();
        wal.sync().unwrap();

        let replayed = wal.replay().unwrap();
        assert_eq!(
            replayed,
            vec![
                put(b"a", b"1"),
                put(b"b", b"2"),
                Operation::Delete { key: b"a".to_vec() },
            ]
        );
    }

    #[test]
    fn wal_survives_reopening() {
        let dir = TestDir::new();
        {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"k", b"v")).unwrap();
            wal.sync().unwrap();
        }
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert_eq!(wal.replay().unwrap(), vec![put(b"k", b"v")]);
    }

    #[test]
    fn truncate_produces_an_empty_log() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        wal.append(&put(b"k", b"v")).unwrap();
        wal.sync().unwrap();
        assert!(!wal.is_empty().unwrap());

        wal.truncate().unwrap();
        assert!(wal.is_empty().unwrap());
        assert!(wal.replay().unwrap().is_empty());
    }

    #[test]
    fn truncated_final_record_is_ignored() {
        let dir = TestDir::new();
        let good_len = {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"a", b"1")).unwrap();
            let len = wal.len().unwrap();
            wal.append(&put(b"b", b"22222")).unwrap();
            wal.sync().unwrap();
            len
        };

        // Chop the file part-way through the second record, as a crash might.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.wal_path())
            .unwrap();
        file.set_len(good_len + 4).unwrap();
        drop(file);

        // The intact first record survives; the torn tail is dropped.
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert_eq!(wal.replay().unwrap(), vec![put(b"a", b"1")]);
    }

    #[test]
    fn corrupted_record_is_detected() {
        let dir = TestDir::new();
        {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"a", b"1")).unwrap();
            wal.append(&put(b"b", b"2")).unwrap();
            wal.sync().unwrap();
        }

        // Flip a byte inside the (complete) first record's payload — the first
        // byte after its fixed header is the key.
        let mut bytes = std::fs::read(dir.wal_path()).unwrap();
        bytes[RECORD_HEADER_LEN] ^= 0xff;
        std::fs::write(dir.wal_path(), &bytes).unwrap();

        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert!(matches!(
            wal.replay(),
            Err(Error::InvalidChecksum) | Err(Error::CorruptedRecord)
        ));
    }

    #[test]
    fn failed_append_does_not_grow_the_log() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        // Oversized key: encoding fails, so nothing should be written.
        let bad = Operation::Put {
            key: vec![0u8; MAX_KEY_SIZE + 1],
            value: Vec::new(),
        };
        assert!(wal.append(&bad).is_err());
        assert_eq!(wal.len().unwrap(), 0);
    }
}
